//! Gear that follows progression: a bot wears the best pieces it has for its level, gets better
//! ones as it levels (bought or minted), sockets lunagems and, if the operator allows it, regrades.
//!
//! Three parts:
//!
//! * The **equip duty** ([`duty`], every brain tick while `[gear] enabled`): every `check_secs`
//!   it compares the bag with what the bot wears ([`eval::best_move`]: item level, grade, gems and
//!   how well the piece suits the class build) and puts on the best better piece with
//!   `CSSwapItems`, one at a time. It checks that the piece arrived; a refusal (an `SCErrorMsg`
//!   or no answer within [`EQUIP_WAIT_MS`]) keeps the piece out for [`REFUSE_MS`]. It waits in a
//!   fight, a trade, an instance, while loading or travelling by portal and while the gear goal
//!   runs (which equips itself). It also works out what the goal could do ([`plan::want`]).
//! * The **gear goal** ([`crate::bot::goals::gear`], with [`plan`] and [`machine`]): tiers up by
//!   level bands, buys from merchants, mints with GM commands, sockets gems, regrades.
//! * The **quest reward** pick ([`reward_choice`]): the reward that adds the most to what the bot
//!   wears.
//!
//! The bot's memory of its gear is [`GearMem`] (`BotState::gear`, one pointer per bot). It knows
//! what the state does not keep: the grade of bag pieces and the gems of every piece whose gems
//! the bot has seen (all worn pieces at log-in, every piece the server updates, the pieces the
//! bot bought or minted, which hold none). A piece whose gems are unknown is never socketed.

pub mod eval;
pub mod machine;
pub mod plan;
#[cfg(test)]
mod tests;

use aa_proto::cs;
use aa_proto::sc_world::{InvItem, ItemChange, OwnState};
use aa_world::gear::{grade_name, slot_name, GearClass, GearItem, GradeFacts, Lunagem, SoldGear};
use aa_world::World;
use smallvec::SmallVec;

use crate::bot::cmd::GoalKind;
use crate::bot::ctx::Ctx;
use crate::bot::state::{BotState, StateError};
use crate::journal::GearNotes;

use self::eval::{BagPiece, Facts, Fit, Move, Tier, Worn, SLOTS};
use self::plan::{Catalog, Me, Rules, Vendor, Want};

/// Gear facts (grades, gems) kept at most.
pub const MAX_KNOWN: usize = 48;
/// Refused pieces remembered at most.
pub const MAX_REFUSED: usize = 8;
/// Templates bought or minted this session remembered at most.
pub const MAX_FETCHED: usize = 16;
/// Gems of a piece the bot has not seen.
pub const UNKNOWN: u8 = u8::MAX;
/// Wait for an equipped piece to show up.
pub const EQUIP_WAIT_MS: u64 = 4_000;
/// A refused piece is not tried again for this long.
pub const REFUSE_MS: u64 = 10 * 60_000;
/// After a change the duty looks again this soon (the next piece).
pub const AGAIN_MS: u64 = 1_500;
/// A blocked duty looks again this soon.
const BLOCKED_MS: u64 = 3_000;
/// The view is published at most this often.
const VIEW_GAP_MS: u64 = 2_000;
/// The longest text of a change kept for the view and the journal.
pub const MAX_CHANGE_CHARS: usize = 120;

/// What the bot knows of one piece: its grade (bag pieces) and its gems.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Known {
    /// Item instance id.
    pub id: u64,
    /// Grade.
    pub grade: u8,
    /// Gems socketed, [`UNKNOWN`] when not seen.
    pub gems: u8,
}

/// An equip the duty sent and waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingEquip {
    /// The bag item put on.
    pub id: u64,
    /// Its template.
    pub template: u32,
    /// The equipment slot.
    pub slot: u8,
    /// Bot clock when it was sent.
    pub sent_ms: u64,
}

/// The answer to a socketing or a regrade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    /// Bot clock when it arrived.
    pub at_ms: u64,
    /// The piece.
    pub item_id: u64,
    /// The result byte (socketing: 1 worked, 0 failed and wiped the gems; regrade:
    /// [`grade_enchant_result`](aa_proto::sc_world::grade_enchant_result)).
    pub result: u8,
    /// The grade before (regrade).
    pub before: u8,
    /// The grade after (regrade).
    pub after: u8,
}

/// What the bot did with its gear this session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GearStats {
    /// Pieces put on.
    pub equipped: u32,
    /// Pieces bought.
    pub bought: u32,
    /// Copper spent on gear.
    pub spent: i64,
    /// Pieces minted.
    pub minted: u32,
    /// Gems socketed.
    pub socketed: u32,
    /// Socketings that failed (and wiped the piece's gems).
    pub socket_fails: u32,
    /// Regrades that raised the grade.
    pub regraded: u32,
    /// Regrades that did not.
    pub regrade_fails: u32,
}

/// The bot's memory of its gear. See the module docs.
#[derive(Debug, Default)]
pub struct GearMem {
    known: Vec<Known>,
    /// Bag slots of the character (0: not known).
    pub bag_slots: u8,
    /// The equip the duty waits for.
    pub pending: Option<PendingEquip>,
    refused: SmallVec<[(u64, u64); 2]>,
    /// Templates bought or minted this session.
    pub fetched: SmallVec<[u32; 4]>,
    /// Bot clock of the duty's next look.
    pub next_check_ms: u64,
    /// What the goal could do (the duty's last look).
    pub want: Want,
    /// The goal does not run before this (a failed run).
    pub quiet_until_ms: u64,
    /// This session's numbers.
    pub stats: GearStats,
    /// The last socketing answer.
    pub socket_result: Option<Outcome>,
    /// The last regrade answer.
    pub grade_result: Option<Outcome>,
    /// The last change, for the view.
    pub last_change: Box<str>,
    /// The view is out of date.
    pub dirty: bool,
    view_ms: u64,
    scratch: Vec<BagPiece>,
}

impl GearMem {
    fn entry(&mut self, id: u64) -> Option<&mut Known> {
        if id == 0 {
            return None;
        }
        if let Some(i) = self.known.iter().position(|k| k.id == id) {
            return self.known.get_mut(i);
        }
        if self.known.len() >= MAX_KNOWN {
            self.known.remove(0);
        }
        self.known.push(Known {
            id,
            grade: 0,
            gems: UNKNOWN,
        });
        self.known.last_mut()
    }

    /// Records the grade of piece `id`.
    pub fn note_grade(&mut self, id: u64, grade: u8) {
        if grade == 0 && !self.known.iter().any(|k| k.id == id) {
            return; // (common is what an unknown piece is taken for)
        }
        if let Some(k) = self.entry(id) {
            k.grade = grade;
        }
    }

    /// Records the gems of piece `id`.
    pub fn note_gems(&mut self, id: u64, gems: u8) {
        if let Some(k) = self.entry(id) {
            k.gems = gems;
            self.dirty = true;
        }
    }

    /// A piece that came new (bought, minted): its grade, no gems.
    pub fn note_new(&mut self, id: u64, grade: u8) {
        if let Some(k) = self.entry(id) {
            k.grade = grade;
            k.gems = 0;
        }
    }

    /// Whether the bot has a fact about piece `id`.
    pub fn knows(&self, id: u64) -> bool {
        id != 0 && self.known.iter().any(|k| k.id == id)
    }

    /// The grade of piece `id` as far as the bot knows (common when it does not).
    pub fn grade_of(&self, id: u64) -> u8 {
        self.known
            .iter()
            .find(|k| k.id == id)
            .map_or(0, |k| k.grade)
    }

    /// The gems of piece `id`, `None` when the bot has not seen them.
    pub fn gems_of(&self, id: u64) -> Option<u8> {
        self.known
            .iter()
            .find(|k| k.id == id)
            .and_then(|k| (k.gems != UNKNOWN).then_some(k.gems))
    }

    /// Forgets the pieces `keep` says are gone.
    pub fn prune(&mut self, keep: impl Fn(u64) -> bool) {
        self.known.retain(|k| keep(k.id));
    }

    /// Whether the facts are full (time to [`prune`](Self::prune)).
    pub fn full(&self) -> bool {
        self.known.len() >= MAX_KNOWN
    }

    /// Keeps piece `id` out of the equip duty for [`REFUSE_MS`].
    pub fn refuse(&mut self, id: u64, now_ms: u64) {
        self.refused.retain(|r| r.0 != id && r.1 > now_ms);
        if self.refused.len() >= MAX_REFUSED {
            self.refused.remove(0);
        }
        self.refused.push((id, now_ms + REFUSE_MS));
    }

    /// Whether piece `id` was refused lately.
    pub fn refused(&self, id: u64, now_ms: u64) -> bool {
        self.refused.iter().any(|r| r.0 == id && r.1 > now_ms)
    }

    /// Remembers a template the bot bought or minted.
    pub fn fetched_now(&mut self, template: u32) {
        if self.fetched.len() >= MAX_FETCHED {
            self.fetched.remove(0);
        }
        self.fetched.push(template);
    }

    /// Notes a change for the view.
    pub fn changed(&mut self, text: &str) {
        self.last_change = cut(text, MAX_CHANGE_CHARS).into();
        self.dirty = true;
    }

    /// A new connection: what waited for an answer is dropped, the rest (facts by item id, the
    /// refusals, the session's numbers) stays.
    pub fn after_reset(&mut self) {
        self.pending = None;
        self.socket_result = None;
        self.grade_result = None;
        self.next_check_ms = 0;
        self.want = Want::default();
        self.dirty = true;
        self.scratch = Vec::new();
    }

    // ---- the state's events ---------------------------------------------------------------

    /// `SCCharacterState`: the bag size and the gems of every worn piece.
    pub fn on_login(&mut self, own: &OwnState) {
        self.bag_slots = own.bag_slots;
        for (i, e) in own.equipment.iter().enumerate() {
            if e.id == 0 {
                continue;
            }
            let gems = own.equipment_gems.get(i).copied().unwrap_or(0);
            if let Some(k) = self.entry(e.id) {
                k.grade = e.grade;
                k.gems = gems;
            }
        }
        self.dirty = true;
    }

    /// A chunk of the bag: the grades of its pieces.
    pub fn on_bag(&mut self, items: &[InvItem]) {
        for it in items.iter().filter(|it| it.count > 0) {
            self.note_grade(it.id, it.grade);
        }
    }

    /// One change of an item task.
    pub fn on_change(&mut self, change: &ItemChange) {
        match *change {
            ItemChange::Added { id, grade, .. } => self.note_grade(id, grade),
            ItemChange::GradeChange { id, grade, .. } => {
                self.note_grade(id, grade);
                self.dirty = true;
            }
            ItemChange::Updated {
                id, gems: Some(n), ..
            } => self.note_gems(id, n),
            // (a move or removal of a piece the bot knows changes what it wears)
            ItemChange::Moved { from_id, to_id, .. }
                if self.knows(from_id) || self.knows(to_id) =>
            {
                self.dirty = true;
            }
            ItemChange::Removed { id, .. } | ItemChange::RemovedSlot { id, .. }
                if self.knows(id) =>
            {
                self.dirty = true;
            }
            _ => {}
        }
    }

    /// `SCItemSocketingLunagemResult`.
    pub fn on_lunagem(&mut self, now_ms: u64, result: u8, item_id: u64) {
        self.socket_result = Some(Outcome {
            at_ms: now_ms,
            item_id,
            result,
            before: 0,
            after: 0,
        });
        if result == 0 {
            // a failure takes every gem out (the item task says so too)
            self.note_gems(item_id, 0);
        }
    }

    /// `SCGradeEnchantResult`.
    pub fn on_grade(&mut self, now_ms: u64, result: u8, item_id: u64, before: u8, after: u8) {
        self.grade_result = Some(Outcome {
            at_ms: now_ms,
            item_id,
            result,
            before,
            after,
        });
        self.note_grade(item_id, after);
        self.dirty = true;
    }
}

/// `text` cut to at most `max` characters.
fn cut(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((i, _)) => &text[..i],
        None => text,
    }
}

// ---- the world as the gear code reads it ------------------------------------------------------

/// The world's catalog for the planner: the gear data, prices, names and NPC spawns.
pub struct WorldCatalog<'a> {
    /// The world.
    pub world: &'a World,
    /// Where the bot stands.
    pub pos: [f32; 3],
    /// The farthest the bot goes to a merchant (metres).
    pub max_trip_m: f32,
}

impl Facts for WorldCatalog<'_> {
    fn item(&self, template: u32) -> Option<GearItem> {
        self.world.gear().item(template).copied()
    }
    fn grade_factor(&self, grade: u8) -> f32 {
        self.world.gear().grade_factor(grade)
    }
    fn grade_order(&self, grade: u8) -> u8 {
        self.world.gear().grade(grade).order
    }
    fn gem(&self, template: u32) -> Option<Lunagem> {
        self.world.gear().gem(template).copied()
    }
    fn sockets(&self, slot_type: u8, grade: u8) -> u8 {
        self.world.gear().sockets(slot_type, grade)
    }
    fn socket_chance(&self, held: u8) -> u16 {
        self.world.gear().socket_chance(held)
    }
}

impl Catalog for WorldCatalog<'_> {
    fn offers(&self) -> &[SoldGear] {
        self.world.gear().sold()
    }

    fn vendor(&self, npcs: &[u32]) -> Option<Vendor> {
        let from = aa_world::Vec3::new(self.pos[0], self.pos[1], self.pos[2]);
        npcs.iter()
            .filter_map(|&npc| {
                let at = self.world.nearest_npc_spawn(npc, from)?;
                let dist = at.dist2d(from);
                (dist <= self.max_trip_m).then_some(Vendor {
                    npc,
                    pos: [at.x, at.y, at.z],
                    dist,
                })
            })
            .min_by(|a, b| a.dist.total_cmp(&b.dist))
    }

    fn price(&self, template: u32) -> u32 {
        self.world.item_facts(template).map_or(0, |f| f.price)
    }

    fn each_mintable(&self, f: &mut dyn FnMut(u32, GearItem)) {
        for (t, item) in self.world.gear().items() {
            if item.character_gear() && mintable(self.world, t) {
                f(t, *item);
            }
        }
    }

    fn each_gem(&self, f: &mut dyn FnMut(u32, Lunagem)) {
        for (t, g) in self.world.gear().gems() {
            f(t, *g);
        }
    }

    fn use_skill(&self, item: u32) -> u32 {
        self.world.item_use_skill(item)
    }

    fn grade_facts(&self, grade: u8) -> GradeFacts {
        self.world.gear().grade(grade)
    }

    fn scroll(&self, class: GearClass) -> u32 {
        self.world.gear().scrolls().of(class)
    }
}

/// Whether a GM may mint `template` for a bot: it has a price, is no quest item and its English
/// name does not look like a test or developer item.
pub fn mintable(world: &World, template: u32) -> bool {
    let Some(f) = world.item_facts(template) else {
        return false;
    };
    if f.price == 0 || f.quest {
        return false;
    }
    let Some(name) = world.item_name(template) else {
        return false;
    };
    looks_real(name)
}

/// Whether an item name looks like a piece a player can have (not a test, developer, event
/// or placeholder item).
pub fn looks_real(name: &str) -> bool {
    let name = name.trim();
    if name.is_empty() || name.starts_with(['[', '(']) || name.contains('_') || !name.is_ascii() {
        return false;
    }
    const BAD: [&str; 7] = [
        "test",
        "dev ",
        "deprecated",
        "unused",
        "gm ",
        "dummy",
        "npc ",
    ];
    !BAD.iter().any(|w| contains_nocase(name, w))
}

/// Whether `hay` contains `needle`, ignoring ASCII case.
pub fn contains_nocase(hay: &str, needle: &str) -> bool {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    n.is_empty() || h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

/// Whether `hay` holds `word` as a whole word (or its plural), ignoring ASCII case: "Sword Box"
/// and "Swords" hold "sword", "Greatsword" does not.
pub fn has_word(hay: &str, word: &str) -> bool {
    let (h, n) = (hay.as_bytes(), word.as_bytes());
    if n.is_empty() || h.len() < n.len() {
        return false;
    }
    (0..=h.len() - n.len()).any(|i| {
        if !h[i..i + n.len()].eq_ignore_ascii_case(n) {
            return false;
        }
        let before = i == 0 || !h[i - 1].is_ascii_alphabetic();
        let rest = &h[i + n.len()..];
        let after = match rest {
            [] => true,
            [c, ..] if !c.is_ascii_alphabetic() => true,
            [b's' | b'S'] => true,
            [b's' | b'S', c, ..] => !c.is_ascii_alphabetic(),
            _ => false,
        };
        before && after
    })
}

/// The worn set of the bot (the gems from its memory).
pub fn worn_of(st: &BotState, mem: Option<&GearMem>, facts: &dyn Facts, fit: &Fit) -> Worn {
    Worn::build(
        st.equipment
            .iter()
            .filter(|e| e.id != 0 && usize::from(e.slot) < SLOTS)
            .map(|e| (e.slot, e.id, e.template, e.grade)),
        |id| mem.and_then(|m| m.gems_of(id)),
        facts,
        fit,
    )
}

/// The gear, gems and scrolls of the bag into `out` (cleared first).
fn bag_of(st: &BotState, mem: Option<&GearMem>, facts: &dyn Facts, out: &mut Vec<BagPiece>) {
    out.clear();
    for (&slot, it) in &st.bag {
        if facts.item(it.template).is_some() || facts.gem(it.template).is_some() {
            out.push(BagPiece {
                slot,
                id: it.id,
                template: it.template,
                grade: mem.map_or(0, |m| m.grade_of(it.id)),
                gems: mem.and_then(|m| m.gems_of(it.id)),
            });
        }
    }
}

/// Free bag slots, `None` when the bag size is not known.
fn free_bag(st: &BotState, mem: &GearMem) -> Option<usize> {
    (mem.bag_slots > 0).then(|| usize::from(mem.bag_slots).saturating_sub(st.bag.len()))
}

/// The planner's rules from the configuration.
pub fn rules(ctx: &Ctx<'_>) -> Rules {
    let g = &ctx.cfg.gear;
    let gm = ctx.cfg.bots.gm_bootstrap;
    Rules {
        buy: g.buy,
        mint: g.mint && gm,
        socket: g.socket,
        regrade: g.regrade && gm,
        max_spend_pct: g.max_spend_pct,
    }
}

/// The goals during which the duty leaves the gear alone: its own goal equips itself, the
/// trading goals move items around.
fn hands_off(goal: Option<GoalKind>) -> bool {
    matches!(
        goal,
        Some(
            GoalKind::Gear
                | GoalKind::Trade
                | GoalKind::Market
                | GoalKind::Auction
                | GoalKind::Dungeon
        )
    )
}

/// Whether the bot is too busy to change its gear: fighting, trading, in an instance, loading,
/// travelling by portal, starting a cast (a skill it sent that the server has not answered), dead.
pub fn busy(ctx: &Ctx<'_>) -> bool {
    let st = &*ctx.state;
    st.me.dead
        || !st.me.spawned
        || st.me.obj == 0
        || ctx.combat.busy()
        || st.trade.active()
        || st.place.in_instance()
        || st.place.loading()
        || st.travel.busy()
        || st
            .watch
            .tracker
            .pending()
            .any(|p| p.req == crate::bot::watch::Req::SkillCast)
}

// ---- the equip duty ---------------------------------------------------------------------------

/// The equip duty (see the module docs). Called every brain tick; cheap between checks.
pub fn duty(ctx: &mut Ctx<'_>, goal: Option<GoalKind>) {
    if !ctx.cfg.gear.enabled {
        return;
    }
    let now = ctx.now_ms;
    if ctx.state.gear.is_none() {
        ctx.state.gear = Some(Box::default());
    }
    if let Some(p) = ctx.state.gear.as_ref().and_then(|m| m.pending) {
        verify(ctx, p);
    }
    let Some(mem) = ctx.state.gear.as_deref_mut() else {
        return;
    };
    if mem.pending.is_some() {
        return;
    }
    if now < mem.next_check_ms {
        if mem.dirty && now >= mem.view_ms + VIEW_GAP_MS {
            publish(ctx);
        }
        return;
    }
    if hands_off(goal) || busy(ctx) {
        if let Some(mem) = ctx.state.gear.as_deref_mut() {
            mem.next_check_ms = now + BLOCKED_MS;
        }
        return;
    }
    check(ctx);
    if ctx.state.gear.as_deref().is_some_and(|m| m.dirty) {
        publish(ctx);
    }
}

/// One look: put on the best better bag piece, else work out what the goal could do.
fn check(ctx: &mut Ctx<'_>) {
    let now = ctx.now_ms;
    let r = rules(ctx);
    let facts = ctx.world.gear();
    let fit = Fit::of(&ctx.persona.archetype);
    let reserved = super::goals::work::customer::gear_reservation(ctx);
    let st: &mut BotState = ctx.state;
    let Some(mut mem) = st.gear.take() else {
        return;
    };
    mem.next_check_ms = now + u64::from(ctx.cfg.gear.check_secs.max(1)) * 1_000;
    let worn = worn_of(st, Some(&mem), facts, &fit);
    let mut bag = std::mem::take(&mut mem.scratch);
    bag_of(st, Some(&mem), facts, &mut bag);
    if let Some((template, stock)) = reserved {
        bag.retain(|item| item.template != template || stock.is_some_and(|id| id != item.id));
    }
    let (level, gender) = (st.me.level, st.me.gender);
    let pick = eval::best_move(
        &worn,
        bag.iter().copied(),
        facts,
        &fit,
        level,
        gender,
        free_bag(st, &mem),
        |id| mem.refused(id, now),
    );
    let mut send = None;
    match pick.and_then(|m| equip_packet(&m).map(|out| (m, out))) {
        Some((m, out)) => {
            mem.pending = Some(PendingEquip {
                id: m.id,
                template: m.template,
                slot: m.slot,
                sent_ms: now,
            });
            send = Some((m, out));
        }
        None => {
            let cat = WorldCatalog {
                world: ctx.world,
                pos: st.me.pos,
                max_trip_m: ctx.cfg.gear.max_trip_m,
            };
            let me = Me {
                id: ctx.persona.id,
                level,
                gender,
                money: st.me.money,
                gearhead: ctx.persona.profile.gearhead,
                fit: &fit,
                worn: &worn,
                bag: &bag,
                fetched: &mem.fetched,
                minted: mem.stats.minted,
                bought: mem.stats.bought,
            };
            let want = plan::want(&me, &r, &cat);
            mem.want = want;
        }
    }
    if mem.full() {
        let (bag_now, eq) = (&st.bag, &st.equipment);
        mem.prune(|id| bag_now.values().any(|i| i.id == id) || eq.iter().any(|e| e.id == id));
    }
    mem.scratch = bag;
    st.gear = Some(mem);
    if let Some((m, out)) = send {
        let name = ctx.world.item_name(m.template).unwrap_or("?");
        ctx.log.debug(format!(
            "gear: putting on {name} ({}) in {} (+{:.1})",
            m.template,
            slot_name(m.slot),
            m.gain
        ));
        ctx.send(out);
    }
}

/// The `CSSwapItems` of a move (`None` for a bag slot past 255, which the packet cannot name).
pub fn equip_packet(m: &Move) -> Option<cs::Out> {
    let bag_slot = u8::try_from(m.bag_slot).ok()?;
    Some(if m.worn_id == 0 {
        cs::equip_into_empty(m.slot, bag_slot, m.id)
    } else {
        cs::equip_swap(m.worn_id, m.slot, m.id, bag_slot)
    })
}

/// Whether a refusal arrived after `since_ms`.
pub fn refused_since(errors: &[(u64, StateError)], since_ms: u64) -> bool {
    errors
        .iter()
        .any(|(at, e)| *at >= since_ms && matches!(e, StateError::Msg(_)))
}

/// Follows the equip the duty sent.
fn verify(ctx: &mut Ctx<'_>, p: PendingEquip) {
    let now = ctx.now_ms;
    let st = &mut *ctx.state;
    let arrived = st
        .equipment
        .iter()
        .any(|e| e.id == p.id && e.slot == p.slot);
    let refused = !arrived && refused_since(&st.errors, p.sent_ms);
    if !arrived && !refused && now < p.sent_ms + EQUIP_WAIT_MS {
        return;
    }
    let Some(mem) = st.gear.as_deref_mut() else {
        return;
    };
    mem.pending = None;
    let name = ctx.world.item_name(p.template).unwrap_or("?");
    if arrived {
        mem.stats.equipped += 1;
        mem.next_check_ms = now + AGAIN_MS;
        let text = format!("put on {name} ({}) in {}", p.template, slot_name(p.slot));
        mem.changed(&text);
        ctx.log.say(format!("gear: {text}"));
        let unix = unix_now();
        note_journal(ctx, |j| j.changed(&text, unix));
    } else {
        mem.refuse(p.id, now);
        mem.next_check_ms = now + AGAIN_MS;
        ctx.log.say(format!(
            "gear: the server did not put on {name} ({}); leaving it for {} minutes",
            p.template,
            REFUSE_MS / 60_000
        ));
    }
}

/// Unix seconds now (for the journal).
pub fn unix_now() -> u64 {
    crate::hub::unix_ms() / 1000
}

/// Writes the bot's journal gear notes.
pub fn note_journal(ctx: &Ctx<'_>, f: impl FnOnce(&mut GearNotes)) {
    ctx.hub.journal.update(ctx.persona.id, |j| f(&mut j.gear));
}

/// Publishes the gear view of the bot (the web UI's bot detail).
pub fn publish(ctx: &mut Ctx<'_>) {
    let now = ctx.now_ms;
    let fit = Fit::of(&ctx.persona.archetype);
    let view = view(ctx.world, ctx.state, &fit);
    if let Some(mem) = ctx.state.gear.as_deref_mut() {
        mem.dirty = false;
        mem.view_ms = now;
    }
    ctx.log.with_slot(|g| g.gear = Some(view));
}

/// The web view of the bot's gear.
pub fn view(world: &World, st: &BotState, fit: &Fit) -> pb_web::GearView {
    let facts = world.gear();
    let mem = st.gear.as_deref();
    let worn = worn_of(st, mem, facts, fit);
    let tier = Tier::of_level(st.me.level);
    let mut lines = Vec::with_capacity(st.equipment.len().min(SLOTS));
    let (mut levels, mut pieces) = (0u32, 0u32);
    for slot in 0..SLOTS as u8 {
        let Some(p) = worn.get(slot) else {
            continue;
        };
        let level = p.item.map_or(0, |i| i.level);
        if p.item.is_some() {
            levels += u32::from(level);
            pieces += 1;
        }
        lines.push(pb_web::GearLine {
            slot: slot_name(slot).to_owned(),
            item: world
                .item_name(p.template)
                .map_or_else(|| p.template.to_string(), str::to_owned),
            level,
            grade: grade_name(p.grade).to_owned(),
            gems: p.gems.unwrap_or(0),
        });
    }
    let avg = if pieces > 0 {
        levels as f32 / pieces as f32
    } else {
        0.0
    };
    let deficit = eval::deficit(&worn, &tier, fit);
    let want = mem.map(|m| m.want).unwrap_or_default();
    let mut after: Vec<&str> = Vec::new();
    if want.buy {
        after.push("buy");
    }
    if want.mint {
        after.push("mint");
    }
    if want.socket {
        after.push("socket");
    }
    if want.regrade {
        after.push("regrade");
    }
    let stats = mem.map(|m| m.stats).unwrap_or_default();
    let summary = format!(
        "tier {} (item level {}+, {}): average item level {avg:.0}, {:.0} % short{}",
        tier.band,
        tier.min_item_level,
        grade_name(tier.grade),
        deficit * 100.0,
        if after.is_empty() {
            String::new()
        } else {
            format!("; could {}", after.join(", "))
        }
    );
    pb_web::GearView {
        summary,
        lines,
        tier: tier.band,
        min_item_level: tier.min_item_level,
        avg_item_level: avg,
        deficit,
        wants: after.iter().map(|s| (*s).to_owned()).collect(),
        last_change: mem.map_or_else(String::new, |m| m.last_change.to_string()),
        stats: pb_web::gear::GearStatsView {
            equipped: stats.equipped,
            bought: stats.bought,
            spent: stats.spent,
            minted: stats.minted,
            socketed: stats.socketed,
            socket_fails: stats.socket_fails,
            regraded: stats.regraded,
            regrade_fails: stats.regrade_fails,
        },
    }
}

// ---- quest rewards ----------------------------------------------------------------------------

/// The 1-based reward index to send for `quest`: the reward that adds the most to what the bot
/// wears ([`pick_reward`]); `fallback` (the quest code's own choice) when gear is off, the quest
/// offers no choice the gear data knows, or nothing tells the rewards apart.
pub fn reward_choice(ctx: &Ctx<'_>, quest: u32, fallback: i32) -> i32 {
    if fallback <= 0 || !ctx.cfg.gear.enabled {
        return fallback;
    }
    let rewards = ctx.world.gear().quest_rewards(quest);
    if rewards.is_empty() {
        return fallback;
    }
    let fit = Fit::of(&ctx.persona.archetype);
    let facts = ctx.world.gear();
    let worn = worn_of(ctx.state, ctx.state.gear.as_deref(), facts, &fit);
    let me = &ctx.state.me;
    let world = ctx.world;
    match pick_reward(rewards, &worn, facts, &fit, me.level, me.gender, |t| {
        world.item_name(t)
    }) {
        Some(i) => i32::try_from(i + 1).unwrap_or(fallback),
        None => fallback,
    }
}

/// The index of the best reward of `rewards` (in the order the client numbers them):
///
/// * a piece of gear the bot knows scores 10 plus what it adds to the worn set (a reward above
///   the bot's level counts at 80 %: it is for later), a piece that does not suit the build at
///   all never wins;
/// * a box or bundle the gear data does not know scores by its English name: the build's weapon
///   kind (5, less for the later kinds), its armor kind (2);
/// * `None` when nothing scores.
pub fn pick_reward<'n>(
    rewards: &[u32],
    worn: &Worn,
    facts: &dyn Facts,
    fit: &Fit,
    level: u8,
    gender: u8,
    name: impl Fn(u32) -> Option<&'n str>,
) -> Option<usize> {
    let mut best: Option<(f32, usize)> = None;
    for (i, &t) in rewards.iter().enumerate() {
        let score = match facts.item(t) {
            Some(item) => {
                let affinity = fit.affinity(&item);
                let v = eval::value(&item, facts.grade_factor(item.grade_at(0)), 0, affinity);
                if v <= 0.0 {
                    -1.0
                } else {
                    let later = if item.wearable_by(level, gender) {
                        1.0
                    } else {
                        0.8
                    };
                    let gain = eval::gain_of(worn, &item, v).map_or(0.0, |g| g.1);
                    // (a piece that suits the build still counts when it adds nothing: better than a guess)
                    (10.0 + gain * later).max(0.01)
                }
            }
            None => name(t).map_or(0.0, |n| name_hint(n, fit)),
        };
        if score > 0.0 && best.is_none_or(|b| score > b.0) {
            best = Some((score, i));
        }
    }
    best.map(|b| b.1)
}

/// What an item's name says about how it suits the build (see [`pick_reward`]).
pub fn name_hint(name: &str, fit: &Fit) -> f32 {
    let mut score: f32 = 0.0;
    for (word, ids) in aa_world::gear::holdable::NAMES {
        if !has_word(name, word) {
            continue;
        }
        if let Some(rank) = fit.weapons.iter().position(|h| ids.contains(h)) {
            score = score.max(5.0 - rank as f32 * 0.5);
        } else if *ids == aa_world::gear::holdable::SHIELD && fit.shield {
            score = score.max(3.0);
        }
    }
    let armor = match fit.armor {
        1 => "cloth",
        2 => "leather",
        3 => "plate",
        _ => "",
    };
    if !armor.is_empty() && has_word(name, armor) {
        score = score.max(2.0);
    }
    score.max(0.0)
}
