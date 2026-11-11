//! Evaluating a compiled rotation for one bot.
//!
//! [`decide`] looks at the facts of the moment ([`Inputs`]), walks the rules from the highest
//! relevance down, then the weighted filler, and returns a [`Decision`]: a skill to cast and how,
//! or a request to move, face or fall back to the basic attack. The only mutable part is the
//! tiny per-bot [`RotState`] (combo, chain, group cooldowns, timers, the buffs the bot has seen
//! come and go); everything else is read-only, so the module is deterministic and testable
//! without a server or the game data (skills come through the [`SkillBook`] trait).
//!
//! Gate for every cast (port of the original `BotSkillGate`): the skill is learned, not backed
//! off after a refusal, off cooldown, affordable, the bot is not stunned or silenced (unless the
//! row says it may cast while controlled), and the target is present, alive and within the
//! skill's range. The global cooldown is checked per row so `ignoreGlobalDelay` rows can use
//! the gaps.

use smallvec::SmallVec;

use super::model::{Action, Cond, Entry, MoveMode, Rotation, Stat};
use crate::bot::combat::Backoffs;

/// Global cooldown between two skills of the rotation.
pub const GCD_MS: u64 = 800;
/// How long a combo opener waits for its follow-up.
pub const COMBO_WINDOW_MS: u64 = 4_000;
/// A chain that did not advance for this long starts over.
pub const CHAIN_WINDOW_MS: u64 = 3_000;
/// Reach of skills that list no range (melee).
pub const MELEE_REACH: f32 = 4.0;
/// Slack on every range check (the server measures in 3D).
pub const RANGE_SLACK: f32 = 0.25;
/// Reach assumed for self-centred area skills that list no range.
const AREA_REACH: f32 = 6.0;
/// A self buff cast this recently counts as running when no buff event was ever linked to it.
const SELF_BUFF_FALLBACK_MS: u64 = 30_000;
/// A debuff cast this recently counts as running when no buff event was ever linked to it.
const DEBUFF_FALLBACK_MS: u64 = 12_000;
/// A buff event this soon after a cast is attributed to that cast.
const LINK_WINDOW_MS: u64 = 4_000;
/// How long a refused cast (stun, silence, ...) keeps the bot "controlled".
const CONTROL_MS: u64 = 1_500;
/// A refused cast is taken back (combo, chain, group stamps) if the refusal comes this soon.
const UNDO_WINDOW_MS: u64 = 2_000;
/// `SkillResult` values meaning the bot is stunned / cannot act (`systems/skills/types.rs`).
const RES_STUN: u8 = 0x13;
const RES_CONTROL: [u8; 3] = [0x16, 0x17, 0x18];

// ---- skill data ------------------------------------------------------------------------------

/// The facts of a skill the rotation needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SkillFacts {
    /// Cooldown in ms.
    pub cooldown_ms: u32,
    /// Cast time in ms.
    pub cast_ms: u32,
    /// Mana cost.
    pub mana: u32,
    /// Minimum distance in metres.
    pub min_range: u16,
    /// Maximum distance in metres (0 = melee or self).
    pub max_range: u16,
    /// `skills.target_type_id`.
    pub target_type: u8,
    /// `skills.target_relation_id`.
    pub relation: u8,
    /// The class ability the skill belongs to (0 = none).
    pub ability_id: u8,
    /// Ability level at which the skill becomes available.
    pub ability_level: u8,
    /// The skill must be learned (`CSLearnSkill`) before it can be cast.
    pub need_learn: bool,
}

/// Where the facts of skills come from: the world data in production, a table in tests.
pub trait SkillBook {
    /// The facts of `skill`, if it exists.
    fn facts(&self, skill: u32) -> Option<SkillFacts>;
}

impl SkillBook for aa_world::World {
    fn facts(&self, skill: u32) -> Option<SkillFacts> {
        self.skill(skill).map(|s| SkillFacts {
            cooldown_ms: s.cooldown_time,
            cast_ms: s.casting_time,
            mana: s.mana_cost,
            min_range: s.min_range,
            max_range: s.max_range,
            target_type: s.target_type_id,
            relation: s.target_relation_id,
            ability_id: s.ability_id,
            ability_level: s.ability_level,
            need_learn: s.need_learn,
        })
    }
}

/// What a skill is aimed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aim {
    /// The bot itself (self buffs, self heals).
    SelfOnly,
    /// A unit target.
    Unit,
    /// A ground position.
    Position,
    /// Doodads, items, pets, buildings: not for the rotation.
    Unsupported,
}

/// The aim of a skill from its `target_type_id` (`SkillTargetType`) and relation.
pub fn aim_of(f: &SkillFacts) -> Aim {
    match f.target_type {
        0..=3 | 17 => Aim::SelfOnly,
        4 | 7 => Aim::Unit,
        // Any unit / others: an enemy unless the skill is meant for friends.
        5 | 16 => match f.relation {
            1..=3 if f.target_type == 5 => Aim::SelfOnly,
            1..=3 => Aim::Unsupported,
            _ => Aim::Unit,
        },
        6 | 11..=15 | 18 => Aim::Position,
        _ => Aim::Unsupported,
    }
}

/// The distance within which a skill reaches its target.
pub fn reach_of(f: &SkillFacts) -> f32 {
    if f.max_range > 0 {
        f32::from(f.max_range)
    } else if f.target_type == 0 && f.relation == 4 {
        AREA_REACH
    } else {
        MELEE_REACH
    }
}

// ---- inputs and outputs ----------------------------------------------------------------------

/// The target as the rotation sees it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TargetView {
    /// Object id.
    pub obj: u32,
    /// Position.
    pub pos: [f32; 3],
    /// Horizontal distance from the bot.
    pub dist: f32,
    /// A player character (duels).
    pub character: bool,
    /// The bot saw it start a cast that is still running.
    pub casting: bool,
}

/// A party member as the rotation sees it (the bot itself is one of them).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PartyView {
    /// Object id.
    pub obj: u32,
    /// Position.
    pub pos: [f32; 3],
    /// Horizontal distance from the bot.
    pub dist: f32,
    /// Hit points in percent.
    pub hp_pct: u8,
    /// The member is dead (or its hit points are not known to be above zero).
    pub dead: bool,
}

/// Everything [`decide`] looks at.
pub struct Inputs<'a> {
    /// The bot clock in ms.
    pub now_ms: u64,
    /// Skill data.
    pub book: &'a dyn SkillBook,
    /// Learned skills `(skill, level)`.
    pub known: &'a [(u32, u8)],
    /// The three active abilities (11 = none).
    pub abilities: [u8; 3],
    /// Character level.
    pub level: u8,
    /// Skill cooldowns `(skill, ready at ms)`.
    pub cooldowns: &'a [(u32, u64)],
    /// Skills the server refused recently.
    pub backoffs: &'a Backoffs,
    /// The bot's object id.
    pub me_obj: u32,
    /// Hit points.
    pub hp: i32,
    /// Hit points in percent.
    pub hp_pct: u8,
    /// Mana.
    pub mp: i32,
    /// Labor points.
    pub labor: i16,
    /// The target, if any (alive).
    pub target: Option<TargetView>,
    /// The party members the bot can see, the bot itself included (empty without a party).
    pub party: &'a [PartyView],
    /// Distances to the hostile units around the bot (the target included), at least up to
    /// [`Rotation::max_enemy_radius`].
    pub enemies: &'a [f32],
    /// The global cooldown ends at this ms.
    pub gcd_until_ms: u64,
    /// The running cast ends at this ms.
    pub casting_until_ms: u64,
}

/// How a skill is sent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CastKind {
    /// On the bot itself.
    SelfCast,
    /// On a unit.
    Unit(u32),
    /// On a party member: the combat controller switches its target to the member for the cast
    /// and back to the enemy after it.
    Ally(u32),
    /// At a ground position.
    Position([f32; 3]),
}

/// A skill to cast.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cast {
    /// Skill id.
    pub skill: u32,
    /// How to send it.
    pub kind: CastKind,
}

/// What the rotation wants this tick.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Decision {
    /// Cast this now.
    pub cast: Option<Cast>,
    /// Get within this many metres of the target (a `reachAndCast` out of range, a `move` row).
    pub reach: Option<f32>,
    /// Face the target.
    pub face: bool,
    /// Back away from the target.
    pub retreat: bool,
    /// An explicit `autoAttack` row applies.
    pub auto: bool,
}

/// Why a skill cannot be cast now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// It can.
    Open(Aim),
    /// Unknown skill data.
    Unknown,
    /// Not learned.
    Unlearned,
    /// Refused recently.
    Backoff,
    /// Cooling down.
    Cooldown,
    /// Not enough mana.
    Mana,
    /// The bot is stunned or silenced.
    Controlled,
    /// No (living) target.
    NoTarget,
    /// The target is too far or too close.
    Range,
    /// Not a skill for the rotation.
    Unsupported,
}

impl Gate {
    /// Whether the skill can be cast.
    pub fn is_open(self) -> bool {
        matches!(self, Gate::Open(_))
    }
}

// ---- per-bot state ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
struct Combo {
    opener: u32,
    follow_up: u32,
    since_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Chain {
    id: u8,
    stage: u8,
    at_ms: u64,
}

/// A buff the bot saw appear on itself, or on a unit it cast at.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Tracked {
    obj: u32,
    index: u32,
    /// Skill the buff was attributed to.
    skill: u32,
    until_ms: u64,
}

/// What a rotation remembers about one bot. About 400 bytes (the heap is used past a few buffs); reset with the combat controller.
#[derive(Debug, Clone, Default)]
pub struct RotState {
    combo: Option<Combo>,
    chain: Option<Chain>,
    groups: SmallVec<[(u8, u64); 3]>,
    timers: SmallVec<[(u8, u64); 2]>,
    /// The last casts `(skill, ms)`, to attribute buff events.
    recent: SmallVec<[(u32, u64); 3]>,
    /// Casts of buff and debuff skills `(skill, target, ms)`.
    stamps: SmallVec<[(u32, u32, u64); 4]>,
    tracked: SmallVec<[Tracked; 4]>,
    /// Skills a buff event was ever attributed to (their buffs are judged by events).
    observed: SmallVec<[u32; 4]>,
    controlled_until: u64,
    stunned_until: u64,
    /// The last cast `(skill, ms)` for [`RotState::on_cast_result`].
    last: Option<(u32, u64)>,
    /// Mana the server's refusals showed a skill to need `(skill, mana)`: the data holds only
    /// the base cost, the real one grows with the ability level.
    mana_floor: SmallVec<[(u32, i32); 3]>,
}

impl RotState {
    /// Forgets the fight (new target): combo, chain and everything about other units.
    pub fn reset_fight(&mut self, me_obj: u32) {
        self.combo = None;
        self.chain = None;
        self.tracked.retain(|t| t.obj == me_obj);
        self.stamps.retain(|s| s.1 == 0);
    }

    /// Whether the bot was refused a cast for being stunned.
    pub fn stunned(&self, now_ms: u64) -> bool {
        now_ms < self.stunned_until
    }

    /// Whether the bot cannot act normally.
    pub fn controlled(&self, now_ms: u64) -> bool {
        now_ms < self.controlled_until
    }

    /// Whether a combo with `opener` (any combo for `None`) is waiting for its follow-up.
    pub fn combo_active(&self, opener: Option<u32>, now_ms: u64) -> bool {
        self.combo.is_some_and(|c| {
            now_ms.saturating_sub(c.since_ms) < COMBO_WINDOW_MS
                && opener.is_none_or(|o| o == c.opener)
        })
    }

    fn chain_stage(&self, id: u8, now_ms: u64) -> Option<(u8, u64)> {
        self.chain
            .filter(|c| c.id == id && now_ms.saturating_sub(c.at_ms) < CHAIN_WINDOW_MS)
            .map(|c| (c.stage, now_ms.saturating_sub(c.at_ms)))
    }

    fn group_ready(&self, group: u8, ms: u32, now_ms: u64) -> bool {
        self.groups
            .iter()
            .find(|g| g.0 == group)
            .is_none_or(|g| now_ms.saturating_sub(g.1) >= u64::from(ms))
    }

    fn stamp_group(&mut self, group: u8, now_ms: u64) {
        match self.groups.iter_mut().find(|g| g.0 == group) {
            Some(g) => g.1 = now_ms,
            None => {
                if self.groups.len() >= 3 {
                    self.groups.remove(0);
                }
                self.groups.push((group, now_ms));
            }
        }
    }

    /// The bot's own buff of `skill` runs for longer than `refresh_ms`.
    fn self_buff(&self, skill: u32, me_obj: u32, now_ms: u64, refresh_ms: u32) -> bool {
        if self.observed.contains(&skill) {
            return self.tracked.iter().any(|t| {
                t.obj == me_obj
                    && t.skill == skill
                    && t.until_ms > now_ms.saturating_add(u64::from(refresh_ms))
            });
        }
        self.stamps
            .iter()
            .any(|s| s.0 == skill && now_ms.saturating_sub(s.2) < SELF_BUFF_FALLBACK_MS)
    }

    /// The target carries the debuff of `skill` for longer than `min_life_ms`.
    fn target_debuff(&self, skill: u32, target: u32, now_ms: u64, min_life_ms: u32) -> bool {
        if self.observed.contains(&skill) {
            return self.tracked.iter().any(|t| {
                t.obj == target
                    && t.skill == skill
                    && t.until_ms > now_ms.saturating_add(u64::from(min_life_ms))
            });
        }
        self.stamps.iter().any(|s| {
            s.0 == skill && s.1 == target && now_ms.saturating_sub(s.2) < DEBUFF_FALLBACK_MS
        })
    }

    /// Notes a buff event. Buffs on the bot, and buffs the bot put on others, are attributed to
    /// the skill that most recently started (or to the toggle skill the packet names).
    #[allow(clippy::too_many_arguments)]
    pub fn on_buff_created(
        &mut self,
        now_ms: u64,
        me_obj: u32,
        obj: u32,
        index: u32,
        caster_obj: u32,
        skill_id: u32,
        duration_ms: i32,
    ) {
        if me_obj == 0 || (obj != me_obj && caster_obj != me_obj) {
            return;
        }
        let skill = if skill_id != 0 {
            skill_id
        } else {
            match self
                .recent
                .iter()
                .filter(|r| now_ms.saturating_sub(r.1) <= LINK_WINDOW_MS)
                .max_by_key(|r| r.1)
            {
                Some(r) => r.0,
                None => return,
            }
        };
        if !self.observed.contains(&skill) {
            if self.observed.len() >= 6 {
                self.observed.remove(0);
            }
            self.observed.push(skill);
        }
        let until_ms = if duration_ms > 0 {
            now_ms + u64::try_from(duration_ms).unwrap_or(0)
        } else {
            u64::MAX
        };
        self.tracked.retain(|t| !(t.obj == obj && t.index == index));
        if self.tracked.len() >= 6 {
            self.tracked.remove(0);
        }
        self.tracked.push(Tracked {
            obj,
            index,
            skill,
            until_ms,
        });
    }

    /// Notes that a buff is gone.
    pub fn on_buff_removed(&mut self, obj: u32, index: u32) {
        self.tracked.retain(|t| !(t.obj == obj && t.index == index));
    }

    /// The server refused `skill` for lack of mana while the bot had `mp`: the skill costs more
    /// than that (a little is added for the regeneration since).
    pub fn learn_mana_floor(&mut self, skill: u32, mp: i32) {
        let need = mp.saturating_add(1).saturating_add(mp / 8);
        match self.mana_floor.iter_mut().find(|m| m.0 == skill) {
            Some(m) => m.1 = m.1.max(need),
            None => {
                if self.mana_floor.len() >= 12 {
                    self.mana_floor.remove(0);
                }
                self.mana_floor.push((skill, need));
            }
        }
    }

    fn mana_short(&self, skill: u32, mp: i32) -> bool {
        self.mana_floor.iter().any(|m| m.0 == skill && mp < m.1)
    }

    /// The server answered a cast: a refusal takes the bookkeeping of that cast back, and a stun
    /// or silence result marks the bot as controlled for a moment.
    pub fn on_cast_result(&mut self, skill: u32, result: u8, now_ms: u64) {
        if result == 0 {
            return;
        }
        if result == RES_STUN {
            self.stunned_until = now_ms + CONTROL_MS;
            self.controlled_until = now_ms + CONTROL_MS;
        } else if RES_CONTROL.contains(&result) {
            self.controlled_until = now_ms + CONTROL_MS;
        }
        let Some((last, at)) = self.last else { return };
        if last != skill || now_ms.saturating_sub(at) > UNDO_WINDOW_MS {
            return;
        }
        if self.combo.is_some_and(|c| c.since_ms == at) {
            self.combo = None;
        }
        if self.chain.is_some_and(|c| c.at_ms == at) {
            self.chain = None;
        }
        self.groups.retain(|g| g.1 != at);
        self.stamps.retain(|s| !(s.0 == skill && s.2 == at));
        self.last = None;
    }

    /// Books a cast the engine just decided on.
    fn after_cast(&mut self, rot: &Rotation, e: &Entry, target: u32, now_ms: u64) {
        let consumed = self.combo.is_some_and(|c| c.follow_up == e.skill);
        if consumed {
            self.combo = None;
        }
        if let Some((opener, follow_up)) = e.combo {
            self.combo = Some(Combo {
                opener,
                follow_up,
                since_ms: now_ms,
            });
        }
        if let Some((id, stage)) = e.chain {
            self.chain = Some(Chain {
                id,
                stage,
                at_ms: now_ms,
            });
        }
        if !consumed && e.combo.is_none() && e.chain.is_none() {
            self.combo = None;
        }
        for &g in e.claims.iter() {
            self.stamp_group(g, now_ms);
        }
        if self.recent.len() >= 3 {
            self.recent.remove(0);
        }
        self.recent.push((e.skill, now_ms));
        if rot.buff_skills().binary_search(&e.skill).is_ok() {
            self.stamps.retain(|s| !(s.0 == e.skill && s.1 == target));
            if self.stamps.len() >= 6 {
                self.stamps.remove(0);
            }
            self.stamps.push((e.skill, target, now_ms));
        }
        self.last = Some((e.skill, now_ms));
    }

    fn timer_fires(
        &mut self,
        idx: u8,
        every_ms: u32,
        probability: f32,
        now_ms: u64,
        rng: &mut fastrand::Rng,
    ) -> bool {
        let last = self.timers.iter().find(|t| t.0 == idx).map(|t| t.1);
        if last.is_some_and(|l| now_ms.saturating_sub(l) < u64::from(every_ms)) {
            return false;
        }
        if probability < 1.0 && rng.f32() >= probability {
            return false;
        }
        match self.timers.iter_mut().find(|t| t.0 == idx) {
            Some(t) => t.1 = now_ms,
            None => self.timers.push((idx, now_ms)),
        }
        true
    }
}

// ---- the decision ----------------------------------------------------------------------------

/// Whether the bot may cast `skill`: it learned it, or the server grants it without learning
/// (`need_learn` off: chain pieces and variants of an active ability's skills).
fn is_known(inp: &Inputs<'_>, f: &SkillFacts, skill: u32) -> bool {
    inp.known.iter().any(|s| s.0 == skill)
        || (!f.need_learn
            && f.ability_id != 0
            && inp.abilities.contains(&f.ability_id)
            && f.ability_level <= inp.level)
}

/// Checks whether `skill` can be cast now (see the module docs).
pub fn gate(inp: &Inputs<'_>, st: &RotState, skill: u32, while_controlled: bool) -> Gate {
    let Some(f) = inp.book.facts(skill) else {
        return Gate::Unknown;
    };
    if let Some(g) = base_gate(inp, st, &f, skill, while_controlled) {
        return g;
    }
    let aim = aim_of(&f);
    let in_reach =
        |t: &TargetView| t.dist >= f32::from(f.min_range) && t.dist <= reach_of(&f) + RANGE_SLACK;
    match aim {
        Aim::Unsupported => Gate::Unsupported,
        Aim::SelfOnly => {
            // A self-centred area skill that hits enemies needs one within its reach.
            if f.target_type == 0 && f.relation == 4 {
                match inp.target {
                    None => Gate::NoTarget,
                    Some(t) if t.dist > reach_of(&f) + RANGE_SLACK => Gate::Range,
                    Some(_) => Gate::Open(aim),
                }
            } else {
                Gate::Open(aim)
            }
        }
        Aim::Unit | Aim::Position => match inp.target {
            None => Gate::NoTarget,
            Some(t) if in_reach(&t) => Gate::Open(aim),
            Some(_) => Gate::Range,
        },
    }
}

fn cond(c: &Cond, inp: &Inputs<'_>, st: &mut RotState, rng: &mut fastrand::Rng) -> bool {
    let now = inp.now_ms;
    match c {
        Cond::Always => true,
        Cond::Never => false,
        Cond::CanCast(s) | Cond::CooldownReady(s) => gate(inp, st, *s, false).is_open(),
        Cond::Range { skill, min, max } => {
            let Some(t) = inp.target else { return false };
            let facts = skill.and_then(|s| inp.book.facts(s));
            let lo = min.unwrap_or_else(|| facts.map_or(0.0, |f| f32::from(f.min_range)));
            let hi = max.unwrap_or_else(|| facts.map_or(f32::MAX, |f| reach_of(&f)));
            t.dist >= lo && t.dist <= hi
        }
        Cond::EnemyCount { min, radius } => {
            inp.enemies.iter().filter(|d| **d <= *radius).count() as u32 >= *min
        }
        Cond::BuffMissing { skill, refresh_ms } => {
            !st.self_buff(*skill, inp.me_obj, now, *refresh_ms)
        }
        Cond::BuffPresent(skill) => st.self_buff(*skill, inp.me_obj, now, 0),
        Cond::DebuffMissing { skill, min_life_ms } => match inp.target {
            Some(t) => !st.target_debuff(*skill, t.obj, now, *min_life_ms),
            None => false,
        },
        Cond::HealthBand { min, max } => {
            let p = f32::from(inp.hp_pct);
            p >= *min && p <= *max
        }
        Cond::PartyLowest { min, max } => {
            // (the lowest living member the bot can see; the bot itself without a party)
            let p = f32::from(lowest_party_hp(inp));
            p >= *min && p <= *max
        }
        Cond::HasTarget => inp.target.is_some(),
        Cond::Pvp => inp.target.is_some_and(|t| t.character),
        Cond::Stunned => st.stunned(now),
        Cond::Controlled | Cond::Cleansable => st.controlled(now),
        Cond::GroupCooldown { group, ms } => st.group_ready(*group, *ms, now),
        Cond::ComboActive(opener) => st.combo_active(*opener, now),
        Cond::ChainStep {
            id,
            steps,
            delay_ms,
        } => st
            .chain_stage(*id, now)
            .map_or(*steps == 0, |(stage, age)| {
                stage == *steps && age >= u64::from(*delay_ms)
            }),
        Cond::Timer {
            idx,
            every_ms,
            probability,
        } => st.timer_fires(*idx, *every_ms, *probability, now, rng),
        Cond::Resource { stat, min, max } => {
            let v = match stat {
                Stat::Hp => inp.hp as f32,
                Stat::Mp => inp.mp as f32,
                Stat::Labor => f32::from(inp.labor),
            };
            v >= *min && v <= *max
        }
        Cond::TargetCasting => inp.target.is_some_and(|t| t.casting),
        Cond::All(kids) => kids.iter().all(|k| cond(k, inp, st, rng)),
        Cond::Any(kids) => kids.iter().any(|k| cond(k, inp, st, rng)),
        Cond::Not(kid) => !cond(kid, inp, st, rng),
    }
}

/// The hit points (percent) of the lowest living member the bot can see, the bot included.
fn lowest_party_hp(inp: &Inputs<'_>) -> u8 {
    inp.party
        .iter()
        .filter(|p| !p.dead)
        .map(|p| p.hp_pct)
        .fold(inp.hp_pct, u8::min)
}

/// The gate checks of a cast that do not depend on the target's range and relation.
fn base_gate(
    inp: &Inputs<'_>,
    st: &RotState,
    f: &SkillFacts,
    skill: u32,
    while_controlled: bool,
) -> Option<Gate> {
    if !is_known(inp, f, skill) {
        return Some(Gate::Unlearned);
    }
    let now = inp.now_ms;
    if inp.backoffs.blocked(skill, now) {
        return Some(Gate::Backoff);
    }
    if inp.cooldowns.iter().any(|c| c.0 == skill && now < c.1) {
        return Some(Gate::Cooldown);
    }
    if i64::from(f.mana) > i64::from(inp.mp) || st.mana_short(skill, inp.mp) {
        return Some(Gate::Mana);
    }
    if !while_controlled && st.controlled(now) {
        return Some(Gate::Controlled);
    }
    None
}

/// Builds a heal on the lowest living member in the skill's reach. The bot itself is the target
/// without a party, and is used when its own hit points are the lowest.
fn try_heal(e: &Entry, inp: &Inputs<'_>, st: &RotState) -> Result<Cast, Gate> {
    let Some(f) = inp.book.facts(e.skill) else {
        return Err(Gate::Unknown);
    };
    if let Some(g) = base_gate(inp, st, &f, e.skill, e.while_controlled) {
        return Err(g);
    }
    let aim = aim_of(&f);
    if !matches!(aim, Aim::SelfOnly | Aim::Unit) {
        return Err(Gate::Unsupported);
    }
    // a skill that can only heal the caster is not spent while somebody else is lower
    if aim == Aim::SelfOnly && inp.hp_pct > lowest_party_hp(inp) {
        return Err(Gate::NoTarget);
    }
    let reach = reach_of(&f) + RANGE_SLACK;
    let low = f32::from(f.min_range);
    let mut best: Option<(u8, f32, u32, bool)> = None;
    let mut me_listed = false;
    for p in inp.party {
        if p.dead {
            continue;
        }
        let is_me = p.obj == inp.me_obj;
        me_listed |= is_me;
        if aim == Aim::SelfOnly && !is_me {
            continue;
        }
        if p.dist < low || p.dist > reach {
            continue;
        }
        let cand = (p.hp_pct, p.dist, p.obj, is_me);
        if best.is_none_or(|b| (cand.0, cand.1) < (b.0, b.1)) {
            best = Some(cand);
        }
    }
    if !me_listed && low == 0.0 {
        let cand = (inp.hp_pct, 0.0, inp.me_obj, true);
        if best.is_none_or(|b| (cand.0, cand.1) < (b.0, b.1)) {
            best = Some(cand);
        }
    }
    let Some((_, _, obj, is_me)) = best else {
        return Err(Gate::Range);
    };
    Ok(Cast {
        skill: e.skill,
        kind: if is_me {
            CastKind::SelfCast
        } else {
            CastKind::Ally(obj)
        },
    })
}

/// Builds the cast for `e` if its gate is open.
fn try_cast(e: &Entry, inp: &Inputs<'_>, st: &RotState) -> Result<Cast, Gate> {
    if e.action == Action::CastHeal {
        return try_heal(e, inp, st);
    }
    let g = gate(inp, st, e.skill, e.while_controlled);
    let Gate::Open(aim) = g else { return Err(g) };
    let kind = match (aim, inp.target) {
        (Aim::SelfOnly, _) => CastKind::SelfCast,
        (Aim::Unit, Some(t)) => CastKind::Unit(t.obj),
        (Aim::Position, Some(t)) => CastKind::Position(t.pos),
        _ => return Err(Gate::NoTarget),
    };
    Ok(Cast {
        skill: e.skill,
        kind,
    })
}

/// The range an anchor or `maintainRange` skill keeps the bot at.
fn hold_range(inp: &Inputs<'_>, skill: u32) -> Option<f32> {
    let f = inp.book.facts(skill)?;
    let r = reach_of(&f);
    (r > 2.0).then_some(r)
}

/// One step of a row; returns `true` when the row decided the tick (a cast).
fn run_row(
    e: &Entry,
    inp: &Inputs<'_>,
    st: &mut RotState,
    rot: &Rotation,
    d: &mut Decision,
) -> bool {
    match e.action {
        Action::Move(mode) => {
            match mode {
                MoveMode::Facing => d.face = true,
                MoveMode::Away => d.retreat = true,
                MoveMode::Melee => {
                    d.reach.get_or_insert(MELEE_REACH - 1.0);
                }
                MoveMode::SpellRange => {
                    if let Some(r) = rot.anchor().and_then(|s| hold_range(inp, s)) {
                        d.reach.get_or_insert(r - 1.0);
                    }
                }
                MoveMode::Behind => {}
            }
            false
        }
        Action::MaintainRange => {
            let skill = if e.skill != 0 {
                e.skill
            } else {
                rot.anchor().unwrap_or(0)
            };
            if let (Some(r), Some(t)) = (hold_range(inp, skill), inp.target) {
                if t.dist > r {
                    d.reach.get_or_insert(r - 1.0);
                } else if t.dist < r - 3.0 {
                    d.retreat = true;
                }
            }
            false
        }
        Action::AutoAttack => {
            d.auto = true;
            false
        }
        _ => {
            if inp.now_ms < inp.gcd_until_ms && !e.ignore_gcd {
                return false;
            }
            match try_cast(e, inp, st) {
                Ok(cast) => {
                    let target = inp.target.map_or(0, |t| t.obj);
                    let target = if matches!(cast.kind, CastKind::SelfCast) {
                        0
                    } else {
                        target
                    };
                    st.after_cast(rot, e, target, inp.now_ms);
                    d.cast = Some(cast);
                    true
                }
                Err(Gate::Range) if e.action == Action::ReachAndCast => {
                    if let (Some(f), Some(_)) = (inp.book.facts(e.skill), inp.target) {
                        d.reach.get_or_insert((reach_of(&f) - 1.0).max(1.0));
                    }
                    false
                }
                Err(_) => false,
            }
        }
    }
}

/// Decides what the bot does this tick. See the module docs.
pub fn decide(
    rot: &Rotation,
    st: &mut RotState,
    inp: &Inputs<'_>,
    rng: &mut fastrand::Rng,
) -> Decision {
    let mut d = Decision::default();
    if inp.now_ms < inp.casting_until_ms {
        return d;
    }
    // Rules at or above the filler's relevance, the filler, then the rest.
    let split = rot.rules.partition_point(|e| e.rel >= FILLER_REL);
    for e in &rot.rules[..split] {
        if cond(&e.cond, inp, st, rng) && run_row(e, inp, st, rot, &mut d) {
            return d;
        }
    }
    if let Some(e) = pick_filler(rot, inp, st, rng) {
        let target = inp.target.map_or(0, |t| t.obj);
        if let Ok(cast) = try_cast(e, inp, st) {
            let target = if matches!(cast.kind, CastKind::SelfCast) {
                0
            } else {
                target
            };
            st.after_cast(rot, e, target, inp.now_ms);
            d.cast = Some(cast);
            return d;
        }
    }
    for e in &rot.rules[split..] {
        if cond(&e.cond, inp, st, rng) && run_row(e, inp, st, rot, &mut d) {
            return d;
        }
    }
    d
}

/// Relevance of the filler rows (the original's default relevance).
pub const FILLER_REL: f32 = 11.0;

/// Draws one of the eligible filler rows, weighted.
fn pick_filler<'r>(
    rot: &'r Rotation,
    inp: &Inputs<'_>,
    st: &mut RotState,
    rng: &mut fastrand::Rng,
) -> Option<&'r Entry> {
    let weight = |e: &Entry| (e.weight.round() as u32).max(1);
    let mut total = 0u32;
    let mut eligible: SmallVec<[&Entry; 8]> = SmallVec::new();
    for e in rot.filler.iter() {
        if !e.action.casts() {
            continue;
        }
        if inp.now_ms < inp.gcd_until_ms && !e.ignore_gcd {
            continue;
        }
        if cond(&e.cond, inp, st, rng) && try_cast(e, inp, st).is_ok() {
            total = total.saturating_add(weight(e));
            eligible.push(e);
        }
    }
    if eligible.is_empty() {
        return None;
    }
    let mut roll = rng.u32(0..total.max(1));
    for e in &eligible {
        let w = weight(e);
        if roll < w {
            return Some(e);
        }
        roll -= w;
    }
    eligible.last().copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[derive(Default)]
    struct Book(HashMap<u32, SkillFacts>);

    impl SkillBook for Book {
        fn facts(&self, skill: u32) -> Option<SkillFacts> {
            self.0.get(&skill).copied()
        }
    }

    fn hostile(max_range: u16) -> SkillFacts {
        SkillFacts {
            max_range,
            target_type: 4,
            ..SkillFacts::default()
        }
    }

    fn selfcast() -> SkillFacts {
        SkillFacts::default()
    }

    fn target(dist: f32) -> Option<TargetView> {
        Some(TargetView {
            obj: 77,
            pos: [dist, 0.0, 0.0],
            dist,
            character: false,
            casting: false,
        })
    }

    struct Fixture {
        book: Book,
        known: Vec<(u32, u8)>,
        cooldowns: Vec<(u32, u64)>,
        backoffs: Backoffs,
        enemies: Vec<f32>,
        target: Option<TargetView>,
        party: Vec<PartyView>,
        now: u64,
        mp: i32,
        hp_pct: u8,
        gcd_until: u64,
    }

    impl Fixture {
        fn new(skills: &[(u32, SkillFacts)]) -> Fixture {
            Fixture {
                book: Book(skills.iter().copied().collect()),
                known: skills.iter().map(|s| (s.0, 1)).collect(),
                cooldowns: Vec::new(),
                backoffs: Backoffs::default(),
                enemies: Vec::new(),
                target: target(3.0),
                party: Vec::new(),
                now: 10_000,
                mp: 100,
                hp_pct: 100,
                gcd_until: 0,
            }
        }

        fn inputs(&self) -> Inputs<'_> {
            Inputs {
                now_ms: self.now,
                book: &self.book,
                known: &self.known,
                abilities: [1, 11, 11],
                level: 10,
                cooldowns: &self.cooldowns,
                backoffs: &self.backoffs,
                me_obj: 5,
                hp: i32::from(self.hp_pct) * 10,
                hp_pct: self.hp_pct,
                mp: self.mp,
                labor: 50,
                target: self.target,
                party: &self.party,
                enemies: &self.enemies,
                gcd_until_ms: self.gcd_until,
                casting_until_ms: 0,
            }
        }
    }

    fn rot(json: &str) -> Rotation {
        let r = Rotation::compile(json).unwrap();
        assert!(r.warnings().is_empty(), "{:?}", r.warnings());
        r
    }

    fn decide_once(r: &Rotation, st: &mut RotState, f: &Fixture) -> Decision {
        decide(r, st, &f.inputs(), &mut fastrand::Rng::with_seed(1))
    }

    const BASIC: &str = r#"{
      "id": "t", "skills": {"bolt": 1, "nuke": 2, "shield": 3},
      "default": [{"action": "cast", "skill": "bolt", "rel": 11}],
      "rules": [
        {"when": {"kind": "buffMissing", "spell": "shield"}, "then": [{"action": "castBuff", "skill": "shield", "rel": 29}]},
        {"when": {"kind": "cooldownReady", "skill": "nuke"}, "then": [{"action": "cast", "skill": "nuke", "rel": 20}]}
      ]
    }"#;

    fn basic_fixture() -> Fixture {
        Fixture::new(&[(1, hostile(20)), (2, hostile(20)), (3, selfcast())])
    }

    #[test]
    fn rules_go_by_relevance_then_the_filler() {
        let r = rot(BASIC);
        let mut f = basic_fixture();
        let mut st = RotState::default();
        // the buff first (29), then the nuke (20), then the filler bolt
        assert_eq!(decide_once(&r, &mut st, &f).cast.unwrap().skill, 3);
        f.now += 1000;
        let d = decide_once(&r, &mut st, &f);
        assert_eq!(
            d.cast,
            Some(Cast {
                skill: 2,
                kind: CastKind::Unit(77)
            })
        );
        f.cooldowns.push((2, f.now + 5000));
        f.now += 1000;
        assert_eq!(decide_once(&r, &mut st, &f).cast.unwrap().skill, 1);
    }

    #[test]
    fn unlearned_cooling_mana_range_and_backoff_gates() {
        let r = rot(BASIC);
        let mut st = RotState::default();
        let mut f = basic_fixture();
        f.known = vec![(1, 1)];
        // only the bolt is learned
        assert_eq!(decide_once(&r, &mut st, &f).cast.unwrap().skill, 1);
        f.known.push((2, 1));
        f.cooldowns.push((2, f.now + 1));
        assert_eq!(gate(&f.inputs(), &st, 2, false), Gate::Cooldown);
        f.cooldowns.clear();
        f.mp = 0;
        f.book.0.insert(
            2,
            SkillFacts {
                mana: 10,
                ..hostile(20)
            },
        );
        assert_eq!(gate(&f.inputs(), &st, 2, false), Gate::Mana);
        f.mp = 100;
        f.target = target(25.0);
        assert_eq!(gate(&f.inputs(), &st, 2, false), Gate::Range);
        f.target = None;
        assert_eq!(gate(&f.inputs(), &st, 2, false), Gate::NoTarget);
        f.target = target(5.0);
        assert!(gate(&f.inputs(), &st, 2, false).is_open());
        f.backoffs.fail(2, f.now);
        assert_eq!(gate(&f.inputs(), &st, 2, false), Gate::Backoff);
        assert_eq!(gate(&f.inputs(), &st, 99, false), Gate::Unknown);
        f.book.0.insert(
            7,
            SkillFacts {
                target_type: 8,
                ..SkillFacts::default()
            },
        );
        f.known.push((7, 1));
        assert_eq!(gate(&f.inputs(), &st, 7, false), Gate::Unsupported);
    }

    #[test]
    fn the_global_cooldown_blocks_rows_without_ignore_gcd() {
        let r = rot(r#"{"id": "t", "skills": {"a": 1, "b": 2},
              "rules": [
                {"when": {"kind": "hasTarget"}, "then": [{"action": "cast", "skill": "a", "rel": 20}]},
                {"when": {"kind": "hasTarget"}, "then": [{"action": "cast", "skill": "b", "rel": 19, "ignoreGlobalDelay": true}]}
              ]}"#);
        let mut f = Fixture::new(&[(1, hostile(10)), (2, hostile(10))]);
        f.gcd_until = f.now + 500;
        let mut st = RotState::default();
        assert_eq!(decide_once(&r, &mut st, &f).cast.unwrap().skill, 2);
        f.gcd_until = 0;
        assert_eq!(decide_once(&r, &mut st, &f).cast.unwrap().skill, 1);
        // a cast in progress blocks everything
        let mut inp = f.inputs();
        inp.casting_until_ms = f.now + 10;
        assert_eq!(
            decide(&r, &mut st, &inp, &mut fastrand::Rng::with_seed(1)),
            Decision::default()
        );
    }

    #[test]
    fn buffs_are_judged_by_events_once_observed_and_by_a_timer_before() {
        let r = rot(BASIC);
        let mut f = basic_fixture();
        let mut st = RotState::default();
        // the first cast of the shield books a stamp: the buff counts as running
        let first = decide_once(&r, &mut st, &f);
        assert_eq!(first.cast.unwrap().skill, 3);
        f.now += 1000;
        assert_ne!(decide_once(&r, &mut st, &f).cast.unwrap().skill, 3);
        // ... for 30 s
        f.now += 31_000;
        f.cooldowns.clear();
        assert_eq!(decide_once(&r, &mut st, &f).cast.unwrap().skill, 3);
        // the server's buff event is attributed to the skill and then wins over the timer
        st.on_buff_created(f.now + 200, 5, 5, 40, 5, 0, 60_000);
        assert!(st.observed.contains(&3));
        f.now += 2000;
        f.cooldowns.clear();
        assert_ne!(decide_once(&r, &mut st, &f).cast.unwrap().skill, 3);
        // when the buff is removed the rotation recasts it, long before the 30 s timer
        st.on_buff_removed(5, 40);
        f.now += 1000;
        f.cooldowns.clear();
        assert_eq!(decide_once(&r, &mut st, &f).cast.unwrap().skill, 3);
    }

    #[test]
    fn a_buff_id_is_never_compared_with_a_skill_id() {
        // skill 3 creates buff 3: the tracked entry holds the skill, the buff id is irrelevant
        let r = rot(BASIC);
        let mut f = basic_fixture();
        let mut st = RotState::default();
        st.on_buff_created(f.now, 5, 5, 1, 5, 3, 0); // toggle skill 3, permanent
        f.cooldowns.clear();
        assert_ne!(decide_once(&r, &mut st, &f).cast.unwrap().skill, 3);
        // another unit's buff or an unrelated caster is ignored
        let mut st = RotState::default();
        st.on_buff_created(f.now, 5, 9, 1, 9, 3, 0);
        assert!(st.tracked.is_empty());
    }

    #[test]
    fn debuffs_follow_the_target_and_expire() {
        let r = rot(r#"{"id": "t", "skills": {"a": 1, "dot": 2},
              "default": [{"action": "cast", "skill": "a", "rel": 11}],
              "rules": [{"when": {"kind": "debuffMissing", "spell": "dot"},
                         "then": [{"action": "castDebuff", "skill": "dot", "rel": 25}]}]}"#);
        let mut f = Fixture::new(&[(1, hostile(10)), (2, hostile(10))]);
        let mut st = RotState::default();
        assert_eq!(decide_once(&r, &mut st, &f).cast.unwrap().skill, 2);
        f.now += 1000;
        f.cooldowns.clear();
        assert_eq!(decide_once(&r, &mut st, &f).cast.unwrap().skill, 1);
        // a new target: the debuff is not on it
        st.reset_fight(5);
        f.target = Some(TargetView {
            obj: 99,
            ..f.target.unwrap()
        });
        f.now += 1000;
        assert_eq!(decide_once(&r, &mut st, &f).cast.unwrap().skill, 2);
        // observed on target 99 with 5 s left, then gone
        st.on_buff_created(f.now, 5, 99, 3, 5, 0, 5000);
        f.now += 1000;
        f.cooldowns.clear();
        assert_eq!(decide_once(&r, &mut st, &f).cast.unwrap().skill, 1);
        f.now += 5000;
        assert_eq!(decide_once(&r, &mut st, &f).cast.unwrap().skill, 2);
    }

    #[test]
    fn enemy_count_health_pvp_and_resource_conditions() {
        let r = rot(
            r#"{"id": "t", "skills": {"a": 1, "aoe": 2, "heal": 3, "pk": 4, "mana": 5},
              "default": [{"action": "cast", "skill": "a", "rel": 11}],
              "rules": [
                {"when": {"kind": "enemyCount", "min": 3, "radius": 8}, "then": [{"action": "castAoe", "skill": "aoe", "rel": 27}]},
                {"when": {"kind": "healthBand", "min": 0, "max": 40}, "then": [{"action": "castHeal", "skill": "heal", "rel": 35}]},
                {"when": {"kind": "pvp"}, "then": [{"action": "cast", "skill": "pk", "rel": 21}]},
                {"when": {"kind": "resource", "stat": "mp", "max": 20}, "then": [{"action": "cast", "skill": "mana", "rel": 15}]}
              ]}"#,
        );
        let mut f = Fixture::new(&[
            (1, hostile(10)),
            (2, hostile(10)),
            (
                3,
                SkillFacts {
                    target_type: 1,
                    ..SkillFacts::default()
                },
            ),
            (4, hostile(10)),
            (5, selfcast()),
        ]);
        let mut st = RotState::default();
        assert_eq!(decide_once(&r, &mut st, &f).cast.unwrap().skill, 1);
        f.enemies = vec![2.0, 5.0, 9.0];
        assert_eq!(
            decide_once(&r, &mut st, &f).cast.unwrap().skill,
            1,
            "one too far"
        );
        f.enemies = vec![2.0, 5.0, 7.9];
        assert_eq!(decide_once(&r, &mut st, &f).cast.unwrap().skill, 2);
        f.enemies.clear();
        f.hp_pct = 30;
        let d = decide_once(&r, &mut st, &f);
        assert_eq!(
            d.cast,
            Some(Cast {
                skill: 3,
                kind: CastKind::SelfCast
            })
        );
        f.hp_pct = 100;
        f.target = Some(TargetView {
            character: true,
            ..f.target.unwrap()
        });
        assert_eq!(decide_once(&r, &mut st, &f).cast.unwrap().skill, 4);
        f.target = target(3.0);
        f.mp = 10;
        assert_eq!(decide_once(&r, &mut st, &f).cast.unwrap().skill, 5);
    }

    #[test]
    fn combos_run_opener_then_follow_up_and_expire() {
        let r = rot(r#"{"id": "t", "skills": {"open": 1, "next": 2, "fill": 3},
              "default": [{"action": "cast", "skill": "fill", "rel": 11}],
              "rules": [
                {"when": {"kind": "comboActive", "opener": "open"},
                 "then": [{"action": "castMelee", "skill": "next", "rel": 49, "ignoreGlobalDelay": true}]},
                {"when": {"kind": "canCast", "skill": "open"},
                 "then": [{"action": "castMelee", "skill": "open", "rel": 26, "combo": {"opener": "open", "followUp": "next"}}]}
              ]}"#);
        let mut f = Fixture::new(&[(1, hostile(10)), (2, hostile(10)), (3, hostile(10))]);
        let mut st = RotState::default();
        assert_eq!(decide_once(&r, &mut st, &f).cast.unwrap().skill, 1);
        assert!(st.combo_active(Some(1), f.now));
        f.cooldowns.push((1, f.now + 12_000));
        f.now += 200;
        f.gcd_until = f.now + 600; // the follow-up ignores the global cooldown
        assert_eq!(decide_once(&r, &mut st, &f).cast.unwrap().skill, 2);
        assert!(
            !st.combo_active(None, f.now),
            "the follow-up ended the combo"
        );
        // an opener whose follow-up never comes: the window closes
        let mut st = RotState::default();
        f.cooldowns.clear();
        f.gcd_until = 0;
        decide_once(&r, &mut st, &f);
        f.now += COMBO_WINDOW_MS + 1;
        assert!(!st.combo_active(Some(1), f.now));
    }

    #[test]
    fn a_refused_opener_takes_its_combo_back() {
        let r = rot(r#"{"id": "t", "skills": {"open": 1, "next": 2},
              "rules": [{"when": {"kind": "canCast", "skill": "open"},
                 "then": [{"action": "castMelee", "skill": "open", "rel": 26, "combo": {"opener": "open", "followUp": "next"}}]}]}"#);
        let f = Fixture::new(&[(1, hostile(10)), (2, hostile(10))]);
        let mut st = RotState::default();
        decide_once(&r, &mut st, &f);
        assert!(st.combo_active(None, f.now));
        st.on_cast_result(1, 8, f.now + 100);
        assert!(!st.combo_active(None, f.now + 100));
        // a refusal of another skill, or a late one, leaves things alone
        decide_once(&r, &mut st, &f);
        st.on_cast_result(9, 8, f.now + 100);
        assert!(st.combo_active(None, f.now));
        st.on_cast_result(1, 8, f.now + UNDO_WINDOW_MS + 1);
        assert!(st.combo_active(None, f.now + 100));
    }

    #[test]
    fn chains_advance_after_their_delay() {
        let r = rot(r#"{"id": "t", "skills": {"s1": 1, "s2": 2, "s3": 3},
              "rules": [
                {"when": {"kind": "range", "skill": "s1", "min": 1.5, "max": 4},
                 "then": [{"action": "castMelee", "skill": "s1", "rel": 30, "chain": {"id": "tri", "stage": 1}}]},
                {"when": {"kind": "chainStep", "id": "tri", "steps": 1, "stepDelayMs": 800},
                 "then": [{"action": "castMelee", "skill": "s2", "rel": 44, "chain": {"id": "tri", "stage": 2}}]},
                {"when": {"kind": "chainStep", "id": "tri", "steps": 2, "stepDelayMs": 800},
                 "then": [{"action": "castMelee", "skill": "s3", "rel": 44, "chain": {"id": "tri", "stage": 0}}]}
              ]}"#);
        let mut f = Fixture::new(&[(1, hostile(4)), (2, hostile(4)), (3, hostile(4))]);
        let mut st = RotState::default();
        let cast = |f: &mut Fixture, st: &mut RotState| {
            let c = decide_once(&r, st, f).cast.map(|c| c.skill);
            if c.is_some() {
                f.gcd_until = f.now + GCD_MS;
            }
            c
        };
        assert_eq!(cast(&mut f, &mut st), Some(1));
        f.now += 400;
        assert_eq!(
            cast(&mut f, &mut st),
            None,
            "the global cooldown holds the opener back"
        );
        f.now += 500;
        assert_eq!(
            cast(&mut f, &mut st),
            Some(2),
            "stage 2 once the delay has passed"
        );
        f.now += 900;
        assert_eq!(cast(&mut f, &mut st), Some(3));
        f.now += 900;
        // chain back at 0: the opener again
        assert_eq!(cast(&mut f, &mut st), Some(1));
        // an abandoned chain times out and starts over
        f.now += 5000;
        assert_eq!(cast(&mut f, &mut st), Some(1));
    }

    #[test]
    fn group_cooldowns_are_claimed_by_the_rows_that_cast() {
        let r = rot(r#"{"id": "t", "skills": {"a": 1, "b": 2},
              "rules": [
                {"when": {"kind": "groupCooldown", "group": "g", "ms": 4000}, "then": [{"action": "cast", "skill": "a", "rel": 23}]},
                {"when": {"kind": "groupCooldown", "group": "g", "ms": 4000}, "then": [{"action": "cast", "skill": "b", "rel": 22}]}
              ]}"#);
        let mut f = Fixture::new(&[(1, hostile(10)), (2, hostile(10))]);
        let mut st = RotState::default();
        assert_eq!(decide_once(&r, &mut st, &f).cast.unwrap().skill, 1);
        f.now += 1500;
        assert_eq!(
            decide_once(&r, &mut st, &f).cast,
            None,
            "the group is quiet for 4 s"
        );
        f.now += 3000;
        assert_eq!(decide_once(&r, &mut st, &f).cast.unwrap().skill, 1);
        // a refused cast releases the group again
        st.on_cast_result(1, 8, f.now + 100);
        assert!(st.group_ready(0, 4000, f.now + 100));
    }

    #[test]
    fn the_filler_is_drawn_by_weight() {
        let r = rot(r#"{"id": "t", "skills": {"a": 1, "b": 2, "c": 3},
              "default": [
                {"action": "cast", "skill": "a", "rel": 11, "weight": 75},
                {"action": "cast", "skill": "b", "rel": 11, "weight": 25},
                {"action": "cast", "skill": "c", "rel": 11, "weight": 500, "when": {"kind": "pvp"}}
              ]}"#);
        let f = Fixture::new(&[(1, hostile(10)), (2, hostile(10)), (3, hostile(10))]);
        let mut rng = fastrand::Rng::with_seed(9);
        let mut counts = [0u32; 4];
        for _ in 0..4000 {
            let mut st = RotState::default();
            let d = decide(&r, &mut st, &f.inputs(), &mut rng);
            counts[d.cast.unwrap().skill as usize] += 1;
        }
        assert_eq!(counts[3], 0, "the pvp row needs a character");
        let ratio = f64::from(counts[1]) / f64::from(counts[2]);
        assert!((2.5..3.6).contains(&ratio), "{counts:?}");
    }

    #[test]
    fn aim_decides_how_a_skill_is_sent() {
        let r = rot(r#"{"id": "t", "skills": {"ground": 1, "me": 2, "foe": 3},
              "rules": [
                {"when": {"kind": "canCast", "skill": "ground"}, "then": [{"action": "castAoe", "skill": "ground", "rel": 30}]},
                {"when": {"kind": "canCast", "skill": "me"}, "then": [{"action": "castBuff", "skill": "me", "rel": 29}]},
                {"when": {"kind": "canCast", "skill": "foe"}, "then": [{"action": "cast", "skill": "foe", "rel": 28}]}
              ]}"#);
        let mut f = Fixture::new(&[
            (
                1,
                SkillFacts {
                    target_type: 6,
                    max_range: 30,
                    ..SkillFacts::default()
                },
            ),
            (2, selfcast()),
            (3, hostile(30)),
        ]);
        f.target = Some(TargetView {
            pos: [9.0, 8.0, 1.0],
            ..f.target.unwrap()
        });
        let mut st = RotState::default();
        let sent = |f: &Fixture, st: &mut RotState| decide_once(&r, st, f).cast.unwrap();
        assert_eq!(sent(&f, &mut st).kind, CastKind::Position([9.0, 8.0, 1.0]));
        f.cooldowns.push((1, f.now + 9000));
        f.now += 1000;
        assert_eq!(sent(&f, &mut st).kind, CastKind::SelfCast);
        f.cooldowns.push((2, f.now + 9000));
        f.now += 1000;
        assert_eq!(sent(&f, &mut st).kind, CastKind::Unit(77));
        assert_eq!(
            aim_of(&SkillFacts {
                target_type: 5,
                relation: 1,
                ..SkillFacts::default()
            }),
            Aim::SelfOnly
        );
        assert_eq!(
            aim_of(&SkillFacts {
                target_type: 16,
                relation: 1,
                ..SkillFacts::default()
            }),
            Aim::Unsupported
        );
        assert_eq!(
            aim_of(&SkillFacts {
                target_type: 5,
                relation: 0,
                ..SkillFacts::default()
            }),
            Aim::Unit
        );
    }

    #[test]
    fn move_rows_set_flags_without_ending_the_tick() {
        let r = rot(
            r#"{"id": "t", "skills": {"bolt": 1}, "meta": {"homeAnchorSkill": "bolt"},
              "default": [{"action": "cast", "skill": "bolt", "rel": 11}],
              "rules": [
                {"when": {"kind": "range", "skill": "facing", "min": 0, "max": 2}, "then": [{"action": "move", "skill": "facing", "rel": 12}]},
                {"when": {"kind": "hasTarget"}, "then": [{"action": "maintainRange", "skill": "bolt", "rel": 14}]},
                {"when": {"kind": "hasTarget"}, "then": [{"action": "autoAttack", "rel": 5}]}
              ]}"#,
        );
        let mut f = Fixture::new(&[(1, hostile(20))]);
        f.target = target(1.0);
        let mut st = RotState::default();
        let d = decide_once(&r, &mut st, &f);
        assert!(d.face && d.retreat && d.cast.is_some(), "{d:?}");
        f.target = target(30.0);
        let d = decide_once(&r, &mut st, &f);
        assert_eq!(d.reach, Some(19.0));
        assert!(d.cast.is_none(), "out of range, nothing to cast");
        assert!(
            d.auto,
            "the low relevance autoAttack row applies once nothing else did"
        );
    }

    #[test]
    fn reach_and_cast_asks_to_close_in() {
        let r = rot(r#"{"id": "t", "skills": {"a": 1},
              "rules": [{"when": {"kind": "hasTarget"}, "then": [{"action": "reachAndCast", "skill": "a", "rel": 30}]}]}"#);
        let mut f = Fixture::new(&[(1, hostile(8))]);
        f.target = target(15.0);
        let mut st = RotState::default();
        let d = decide_once(&r, &mut st, &f);
        assert_eq!((d.cast, d.reach), (None, Some(7.0)));
        f.target = target(6.0);
        assert!(decide_once(&r, &mut st, &f).cast.is_some());
    }

    #[test]
    fn stun_results_mark_the_bot_controlled() {
        let r = rot(r#"{"id": "t", "skills": {"a": 1, "cleanse": 2},
              "default": [{"action": "cast", "skill": "a", "rel": 11}],
              "rules": [{"when": {"kind": "hasCleansableDebuff"},
                         "then": [{"action": "castDebuff", "skill": "cleanse", "rel": 90, "castWhileControlled": true}]}]}"#);
        let mut f = Fixture::new(&[(1, hostile(10)), (2, selfcast())]);
        let mut st = RotState::default();
        st.on_cast_result(1, RES_STUN, f.now);
        assert!(st.stunned(f.now) && st.controlled(f.now));
        // controlled: the normal skill is held back, the cleanse goes
        assert_eq!(decide_once(&r, &mut st, &f).cast.unwrap().skill, 2);
        f.cooldowns.push((2, f.now + 5000));
        assert_eq!(decide_once(&r, &mut st, &f).cast, None);
        f.now += CONTROL_MS + 1;
        assert_eq!(decide_once(&r, &mut st, &f).cast.unwrap().skill, 1);
        st.on_cast_result(1, 0x17, f.now);
        assert!(st.controlled(f.now) && !st.stunned(f.now));
    }

    #[test]
    fn timers_fire_at_most_once_per_interval() {
        let r = rot(r#"{"id": "t", "skills": {"a": 1, "b": 2},
              "default": [{"action": "cast", "skill": "b", "rel": 11}],
              "rules": [{"when": {"kind": "timer", "every": 5000}, "then": [{"action": "cast", "skill": "a", "rel": 20}]}]}"#);
        let mut f = Fixture::new(&[(1, hostile(10)), (2, hostile(10))]);
        let mut st = RotState::default();
        assert_eq!(decide_once(&r, &mut st, &f).cast.unwrap().skill, 1);
        f.now += 1000;
        assert_eq!(decide_once(&r, &mut st, &f).cast.unwrap().skill, 2);
        f.now += 4500;
        assert_eq!(decide_once(&r, &mut st, &f).cast.unwrap().skill, 1);
    }

    #[test]
    fn a_mana_refusal_teaches_the_real_cost() {
        let mut st = RotState::default();
        st.learn_mana_floor(7, 80);
        assert!(st.mana_short(7, 80));
        assert!(st.mana_short(7, 89));
        assert!(
            !st.mana_short(7, 91),
            "the floor is the refusal's mana plus a margin"
        );
        assert!(!st.mana_short(8, 0), "other skills are not affected");
        st.learn_mana_floor(7, 120);
        assert!(st.mana_short(7, 130));
        st.learn_mana_floor(7, 10);
        assert!(st.mana_short(7, 130), "a floor never drops");
        for skill in 100..120 {
            st.learn_mana_floor(skill, 5);
        }
        assert!(st.mana_floor.len() <= 12);
    }

    #[test]
    fn the_state_stays_small() {
        assert!(
            std::mem::size_of::<RotState>() <= 512,
            "{}",
            std::mem::size_of::<RotState>()
        );
    }

    fn heal(max_range: u16) -> SkillFacts {
        SkillFacts {
            max_range,
            target_type: 5,
            relation: 0,
            ..SkillFacts::default()
        }
    }

    fn member(obj: u32, dist: f32, hp_pct: u8) -> PartyView {
        PartyView {
            obj,
            pos: [dist, 0.0, 0.0],
            dist,
            hp_pct,
            dead: false,
        }
    }

    const HEAL: &str = r#"{
      "id": "t", "skills": {"heal": 2},
      "rules": [
        {"when": {"kind": "partyLowest", "min": 0, "max": 50},
         "then": [{"action": "castHeal", "skill": "heal", "rel": 35}]}
      ]
    }"#;

    #[test]
    fn party_lowest_reads_the_members_and_the_heal_goes_to_the_lowest_in_reach() {
        let r = rot(HEAL);
        let mut f = Fixture::new(&[(2, heal(25))]);
        f.hp_pct = 100;
        f.party = vec![member(5, 0.0, 100), member(9, 10.0, 40)];
        let mut st = RotState::default();
        assert_eq!(
            decide_once(&r, &mut st, &f).cast,
            Some(Cast {
                skill: 2,
                kind: CastKind::Ally(9)
            })
        );
        // the lowest is out of the skill's reach: the lowest one in reach is healed
        f.party = vec![
            member(5, 0.0, 100),
            member(9, 40.0, 10),
            member(10, 8.0, 35),
        ];
        assert_eq!(
            decide_once(&r, &mut st, &f).cast,
            Some(Cast {
                skill: 2,
                kind: CastKind::Ally(10)
            })
        );
        // the bot itself is the lowest: it heals itself
        f.hp_pct = 20;
        f.party = vec![member(5, 0.0, 20), member(9, 8.0, 80)];
        assert_eq!(
            decide_once(&r, &mut st, &f).cast,
            Some(Cast {
                skill: 2,
                kind: CastKind::SelfCast
            })
        );
        // without a party the condition reads the bot's own hit points
        f.hp_pct = 100;
        f.party.clear();
        assert_eq!(decide_once(&r, &mut st, &f).cast, None);
        f.hp_pct = 40;
        assert_eq!(
            decide_once(&r, &mut st, &f).cast,
            Some(Cast {
                skill: 2,
                kind: CastKind::SelfCast
            })
        );
    }

    #[test]
    fn a_self_only_heal_is_not_spent_while_a_member_is_lower() {
        let r = rot(r#"{
          "id": "t", "skills": {"heal": 2},
          "rules": [
            {"when": {"kind": "partyLowest", "min": 0, "max": 50},
             "then": [{"action": "castHeal", "skill": "heal", "rel": 35}]}
          ]
        }"#);
        let mut f = Fixture::new(&[(2, selfcast())]);
        f.hp_pct = 90;
        f.party = vec![member(5, 0.0, 90), member(9, 8.0, 20)];
        let mut st = RotState::default();
        assert_eq!(
            decide_once(&r, &mut st, &f).cast,
            None,
            "the self heal cannot reach the hurt member"
        );
        // dead members are not heal targets and do not lower the band
        f.hp_pct = 40;
        f.party = vec![member(5, 0.0, 40)];
        let mut dead = member(9, 8.0, 0);
        dead.dead = true;
        f.party.push(dead);
        assert_eq!(
            decide_once(&r, &mut st, &f).cast,
            Some(Cast {
                skill: 2,
                kind: CastKind::SelfCast
            }),
            "the dead member is skipped, the bot itself is lowest"
        );
    }

    #[test]
    fn a_heal_waits_for_its_cooldown_and_mana_like_any_other_cast() {
        let r = rot(HEAL);
        let mut f = Fixture::new(&[(
            2,
            SkillFacts {
                mana: 10,
                ..heal(25)
            },
        )]);
        f.hp_pct = 100;
        f.party = vec![member(9, 8.0, 20)];
        let mut st = RotState::default();
        f.mp = 5;
        assert_eq!(decide_once(&r, &mut st, &f).cast, None);
        f.mp = 100;
        f.cooldowns.push((2, f.now + 1_000));
        assert_eq!(decide_once(&r, &mut st, &f).cast, None);
        f.cooldowns.clear();
        assert!(decide_once(&r, &mut st, &f).cast.is_some());
    }
}
