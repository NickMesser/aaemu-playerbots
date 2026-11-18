//! Warzone: fight for the faction in a zone group that is at war.
//!
//! The server runs a conflict state per zone group (`SCConflictZoneStatePacket`, kept in
//! [`BotState::zone_states`](crate::bot::state::BotState)); in the states conflict and war the
//! zone is fought over. The goal is scored while a reachable zone group is at war with enough time
//! left and the bot has not yet killed its [`PvpCfg::war_kill_cap`](crate::config::PvpCfg) in that
//! war:
//!
//! ```text
//! Plan -> Go -> Patrol <-> Fight -> (Regroup) -> leaves when the war ends or the cap is reached
//! ```
//!
//! * **Go / Patrol**: walk to a spot of the zone group ([`World::zone_group_spots`]), then from
//!   spot to spot, looking for characters the rules allow ([`scan::hunted`]: a hostile alliance,
//!   never one in its home zone, never a name in `never_names`, bots only with `attack_bots`,
//!   real players only when [`Config::may_attack_players`](crate::config::Config) says so).
//! * **Fight**: the odds decide ([`engage`]); the combat controller fights, the goal gives up on
//!   hopeless odds, a retreat or a zone that protects the target.
//! * **Regroup**: a hurt bot runs from the fight ([`fight::start_flee`]) and rests until it is fit
//!   again.
//! * The goal ends at the next tick after the zone group turns to peace (or is about to), and
//!   the kills of the war are counted per `(zone group, end of the state)`.

use aa_proto::sc_world::conflict_state;
use aa_world::World;
use pb_web::Activity;
use smallvec::SmallVec;

use crate::bot::combat::{CombatStatus, LostReason};
use crate::bot::ctx::Ctx;
use crate::bot::guild::profession::{self, Profession};
use crate::bot::pvp::bounties::Case;
use crate::bot::pvp::fight::{self, Odds};
use crate::bot::pvp::rules;
use crate::bot::pvp::scan;
use crate::bot::pvp::walk::{Walk, Walker};
use crate::config::BehaviorCfg;
use crate::roster::Temperament;

use super::trade_run::carried_pack_of;
use super::{FailReason, GoalStatus};

/// A bot that has been online for less than this does not go to war yet (bot clock, ms).
const SETTLE_MS: u64 = 60_000;
/// Bots below this level do not go to war.
const MIN_LEVEL: u8 = 15;
/// Hit points (percent) a bot wants before it sets out.
const MIN_HP_PCT: u8 = 70;
/// ... and before it goes on after a regroup.
const FIT_HP_PCT: u8 = 85;
/// A war needs this many seconds left for the bot to set out.
const MIN_LEFT_S: i64 = 150;
/// The goal ends when this few seconds of the war are left.
const LEAVE_LEFT_S: i64 = 15;
/// A war whose end the server did not tell is taken to have this many seconds left.
const UNKNOWN_LEFT_S: i64 = 600;
/// Zone groups with a spot within this many metres are reachable on foot.
const WALK_REACH_M: f32 = 3_500.0;
/// A zone group has to have at least this many spots, and ...
const MIN_LAND_SPOTS: usize = 8;
/// ... at least this share (percent) of them has to be land the bot can stand on: the sea
/// groups of the zone conflicts are for ships.
const MIN_LAND_PCT: usize = 40;
/// The goal gives up after this long (ms).
const GOAL_MAX_MS: u64 = 45 * 60_000;
/// A spot counts as reached this close (metres).
const SPOT_STOP_M: f32 = 12.0;
/// The next patrol spot is at least this far (metres) ...
const PATROL_MIN_M: f32 = 120.0;
/// ... and at most this far.
const PATROL_MAX_M: f32 = 500.0;
/// How far the bot looks for characters to fight (metres): well inside the distance at which the
/// combat controller gives a fight with a character up.
const HUNT_M: f32 = 60.0;
const _: () = assert!(HUNT_M < fight::LOST_DISTANCE);
/// The odds of a running fight are looked at this often (ms) ...
const EVAL_MS: u64 = 2_000;
/// ... and not before the fight is this old (ms).
const EVAL_AFTER_MS: u64 = 3_000;
/// A character the bot cannot take is left alone this long (ms).
const SKIP_MS: u64 = 45_000;
/// A regroup lasts at most this long (ms).
const REGROUP_MAX_MS: u64 = 3 * 60_000;
/// Lost fights in a row (the target got away) after which the bot looks elsewhere.
const MAX_LOST: u8 = 4;

/// Warzone: fight for the faction in zones at war. See the module docs.
#[derive(Debug, Default)]
pub struct Warzone {
    m: Option<Box<Machine>>,
}

impl Warzone {
    /// A fresh goal (the brain starts it with [`Goal::new`](super::Goal::new)).
    pub fn new() -> Warzone {
        Warzone::default()
    }

    /// Called once when the brain picks the goal.
    pub fn start(&mut self, ctx: &mut Ctx<'_>) {
        self.m = Some(Box::new(Machine::new(ctx.now_ms)));
    }

    /// One tick.
    pub fn tick(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        match self.m.as_deref_mut() {
            Some(m) => m.step(ctx),
            None => GoalStatus::Failed(FailReason::Other("warzone was not started")),
        }
    }

    /// Called when the goal ends, fails or is interrupted (stop motion, leave the fight).
    pub fn stop(&mut self, ctx: &mut Ctx<'_>) {
        if let Some(m) = self.m.as_deref_mut() {
            m.finish(ctx);
        }
    }

    /// Whether the goal drives the combat controller itself (see [`super::Goal::owns_combat`]):
    /// while it fights.
    pub fn owns_combat(&self) -> bool {
        self.m.as_deref().is_some_and(|m| m.phase == Phase::Fight)
    }

    /// The activity the web UI shows while the goal runs.
    pub fn activity(&self, moving: bool, fighting: bool) -> Activity {
        if fighting {
            Activity::Fighting
        } else if moving {
            Activity::Traveling
        } else {
            Activity::Idle
        }
    }

    /// Appends a one-line description of what the goal is doing to `out`.
    pub fn describe(&self, out: &mut String) {
        let Some(m) = self.m.as_deref() else {
            out.push_str("warzone");
            return;
        };
        match m.phase {
            Phase::Plan => out.push_str("warzone: choosing a war"),
            Phase::Go => out.push_str(&format!("warzone: heading for zone group {}", m.group)),
            Phase::Patrol => out.push_str(&format!(
                "warzone: patrolling zone group {} ({} kills)",
                m.group, m.kills
            )),
            Phase::Fight => {
                out.push_str("warzone: fighting ");
                out.push_str(&m.name);
            }
            Phase::Regroup => out.push_str("warzone: regrouping"),
        }
    }
}

/// The goal's weight before the situation is looked at: 0 (the goal scores itself, see [`score`],
/// from the temperament and the situation).
pub fn base_weight(_behavior: &BehaviorCfg, _temper: &Temperament) -> f32 {
    0.0
}

/// The weight of a war for a bot with this temperament: the fighting types go, a neutral bot as
/// readily as it quests (a war is rare and short).
pub fn weight(temper: &Temperament) -> f32 {
    0.9 * (0.3 + 1.4 * temper.duelist)
}

/// The score of the goal for this tick: 0 unless warzones are on, someone may be attacked, the
/// bot is settled, healthy, a high enough level, in the main world, off its cooldown, not carrying
/// a pack and a zone group is at war that it can reach with time left; the temperament's
/// [`weight`] otherwise. The wars are looked at here, so the brain asks only when it picks.
pub fn score(ctx: &Ctx<'_>, _base: f32) -> f32 {
    let cfg = ctx.cfg;
    if !(cfg.pvp.enabled && cfg.pvp.warzone) || !(cfg.pvp.attack_bots || cfg.may_attack_players()) {
        return 0.0;
    }
    let me = &ctx.state.me;
    if ctx.state.place.in_instance()
        || me.dead
        || ctx.now_ms < SETTLE_MS
        || me.level < MIN_LEVEL
        || me.hp_pct() < MIN_HP_PCT
        || ctx
            .state
            .pvp
            .as_ref()
            .is_some_and(|p| ctx.now_ms < p.war_next_ms)
        || rules::enemy_alliance(ctx.world.mother_faction(me.faction)) == 0
        || carried_pack_of(ctx).is_some()
    {
        return 0.0;
    }
    if wars(ctx).is_empty() {
        return 0.0;
    }
    weight(&ctx.persona.temper)
}

// ---- the pure decisions ----------------------------------------------------------------------

/// A zone group at war, as one bot sees it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct War {
    /// The zone group.
    pub group: u16,
    /// When the state ends (Unix seconds; 0 = the server did not say).
    pub end_unix_s: i64,
    /// The way to the nearest spot of the zone group (metres).
    pub dist_m: f32,
    /// Characters the bot killed in this war so far.
    pub kills: u8,
}

/// Seconds of the state left at `now_unix_s` ([`UNKNOWN_LEFT_S`] when the end is not known).
pub fn time_left_s(end_unix_s: i64, now_unix_s: i64) -> i64 {
    if end_unix_s <= 0 {
        UNKNOWN_LEFT_S
    } else {
        end_unix_s - now_unix_s
    }
}

/// Whether a bot that killed `kills` in a war with `left_s` seconds left sets out for it.
pub fn worth_joining(left_s: i64, kills: u8, cap: u8) -> bool {
    left_s >= MIN_LEFT_S && kills < cap
}

/// Whether the war is over for a bot in it: the zone group is at peace, or about to be.
pub fn war_over(at_war: bool, left_s: i64) -> bool {
    !at_war || left_s <= LEAVE_LEFT_S
}

/// The war to go to among `wars` (nearest first, `roll` in `0..2` picks the second nearest for
/// variety when there is one).
pub fn pick_war(wars: &[War], roll: usize) -> Option<War> {
    let mut sorted: SmallVec<[War; 8]> = wars.iter().copied().collect();
    sorted.sort_by(|a, b| a.dist_m.total_cmp(&b.dist_m).then(a.group.cmp(&b.group)));
    sorted.get(roll.min(1)).or_else(|| sorted.first()).copied()
}

/// What the bot does about a character it met in the war.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engage {
    /// Fight it.
    Attack,
    /// Leave it alone for a while.
    Skip,
    /// Too hurt to fight: regroup.
    Regroup,
}

/// The decision for a character whose fight looks like `odds` to a bot with `hp_pct` percent of
/// its hit points that retreats below `retreat_pct`.
pub fn engage(odds: Odds, hp_pct: u8, retreat_pct: u8) -> Engage {
    if hp_pct < floor_pct(retreat_pct) {
        return Engage::Regroup;
    }
    match odds {
        Odds::Fair => Engage::Attack,
        Odds::NeedsHelp | Odds::Hopeless => Engage::Skip,
    }
}

/// The hit points (percent) below which a bot in the war regroups: a margin above the retreat line.
pub fn floor_pct(retreat_pct: u8) -> u8 {
    retreat_pct.saturating_add(20).clamp(20, 90)
}

/// Whether a zone group with these `spots` is land the bot can fight on.
pub fn land_group(spots: &[[f32; 2]], ok: impl Fn([f32; 2]) -> bool) -> bool {
    spots.len() >= MIN_LAND_SPOTS
        && spots.iter().filter(|&&s| ok(s)).count() * 100 >= spots.len() * MIN_LAND_PCT
}

/// The `n` spots nearest to `from` that pass `ok`, nearest first, with their distances.
pub fn nearest_spots<const N: usize>(
    spots: &[[f32; 2]],
    from: [f32; 2],
    ok: impl Fn([f32; 2]) -> bool,
) -> SmallVec<[([f32; 2], f32); N]> {
    let mut out: SmallVec<[([f32; 2], f32); N]> = SmallVec::new();
    for &s in spots {
        let d = ((s[0] - from[0]).powi(2) + (s[1] - from[1]).powi(2)).sqrt();
        if out.len() == N && d >= out[N - 1].1 {
            continue;
        }
        if !ok(s) {
            continue;
        }
        let at = out.iter().position(|o| d < o.1).unwrap_or(out.len());
        out.insert(at, (s, d));
        out.truncate(N);
    }
    out
}

/// The next patrol spot: a random one between [`PATROL_MIN_M`] and [`PATROL_MAX_M`] from `from`,
/// else the nearest one that is not where the bot stands.
pub fn patrol_spot(
    spots: &[[f32; 2]],
    from: [f32; 2],
    ok: impl Fn([f32; 2]) -> bool,
    rng: &mut fastrand::Rng,
) -> Option<[f32; 2]> {
    let dist = |s: &[f32; 2]| ((s[0] - from[0]).powi(2) + (s[1] - from[1]).powi(2)).sqrt();
    let mut far: SmallVec<[[f32; 2]; 16]> = SmallVec::new();
    let mut near: Option<([f32; 2], f32)> = None;
    for s in spots {
        let d = dist(s);
        if d < 20.0 || !ok(*s) {
            continue;
        }
        if (PATROL_MIN_M..=PATROL_MAX_M).contains(&d) && far.len() < far.inline_size() {
            far.push(*s);
        }
        if near.is_none_or(|n| d < n.1) {
            near = Some((*s, d));
        }
    }
    if far.is_empty() {
        near.map(|n| n.0)
    } else {
        Some(far[rng.usize(..far.len())])
    }
}

// ---- looking at the wars -------------------------------------------------------------------------

fn unix_now_s() -> i64 {
    (crate::hub::unix_ms() / 1000) as i64
}

/// Whether the bot can stand at a spot.
fn standable(world: &World) -> impl Fn([f32; 2]) -> bool + '_ {
    move |s| super::walkable(world, s[0], s[1])
}

pub fn bounty(ctx: &Ctx<'_>, matches: impl Fn(&Case) -> bool) -> Option<Case> {
    if !matches!(
        profession::membership(ctx),
        Some((_, Profession::BountyHunter, false))
    ) {
        return None;
    }
    ctx.hub.bounties.find(
        crate::hub::unix_ms(),
        ctx.world.mother_faction(ctx.state.me.faction),
        matches,
    )
}

/// The zone groups at war that the bot may join now, with their distance and the bot's kills.
fn wars(ctx: &Ctx<'_>) -> SmallVec<[War; 8]> {
    let mut out: SmallVec<[War; 8]> = SmallVec::new();
    let now_s = unix_now_s();
    let me = ctx.state.me.pos;
    let cap = ctx.cfg.pvp.war_kill_cap;
    for e in ctx.state.zone_states.iter() {
        if out.len() == out.inline_size() {
            break;
        }
        if !conflict_state::at_war(e.state) || !ctx.world.is_conflict_group(e.zone_group) {
            continue;
        }
        let kills = ctx
            .state
            .pvp
            .as_ref()
            .map_or(0, |p| p.war_kills(e.zone_group, e.end_unix_s));
        if !worth_joining(time_left_s(e.end_unix_s, now_s), kills, cap) {
            continue;
        }
        let spots = ctx.world.zone_group_spots(e.zone_group);
        if !land_group(spots, standable(ctx.world)) {
            continue;
        }
        let Some(&(_, dist_m)) =
            nearest_spots::<1>(spots, [me[0], me[1]], standable(ctx.world)).first()
        else {
            continue;
        };
        if !(ctx.would_teleport(dist_m) || dist_m <= WALK_REACH_M) {
            continue;
        }
        out.push(War {
            group: e.zone_group,
            end_unix_s: e.end_unix_s,
            dist_m,
            kills,
        });
    }
    out
}

// ---- the machine -------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Plan,
    Go,
    Patrol,
    Fight,
    Regroup,
}

/// How the goal went, for the cooldown before the bot joins a war again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Over,
    Nothing,
    Died,
}

impl Outcome {
    /// The wait before the next war (ms, low and high).
    fn cooldown_ms(self) -> (u64, u64) {
        match self {
            Outcome::Over => (2 * 60_000, 4 * 60_000),
            Outcome::Nothing => (4 * 60_000, 8 * 60_000),
            Outcome::Died => (8 * 60_000, 12 * 60_000),
        }
    }
}

#[derive(Debug)]
struct Machine {
    phase: Phase,
    group: u16,
    end_unix_s: i64,
    walker: Walker,
    end_ms: u64,
    phase_ms: u64,
    /// Where the bot walks to (the spot of the zone group).
    spot: Option<[f32; 2]>,
    target: u32,
    name: Box<str>,
    eval_ms: u64,
    lost: u8,
    kills: u8,
    told: u32,
    outcome: Outcome,
    finished: bool,
}

impl Machine {
    fn new(now_ms: u64) -> Machine {
        Machine {
            phase: Phase::Plan,
            group: 0,
            end_unix_s: 0,
            walker: Walker::new(),
            end_ms: now_ms + GOAL_MAX_MS,
            phase_ms: now_ms,
            spot: None,
            target: 0,
            name: "".into(),
            eval_ms: 0,
            lost: 0,
            kills: 0,
            told: 0,
            outcome: Outcome::Nothing,
            finished: false,
        }
    }

    fn enter(&mut self, phase: Phase, now_ms: u64) {
        self.phase = phase;
        self.phase_ms = now_ms;
        self.walker.reset();
    }

    fn fail(&mut self, ctx: &mut Ctx<'_>, why: FailReason, note: &str) -> GoalStatus {
        ctx.log.say(format!("warzone: giving up ({note})"));
        self.outcome = Outcome::Nothing;
        GoalStatus::Failed(why)
    }

    fn step(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        if ctx.now_ms >= self.end_ms {
            return self.fail(ctx, FailReason::Timeout, "it took too long");
        }
        if self.phase != Phase::Plan {
            let at_war = ctx.state.at_war(self.group);
            let left = time_left_s(self.end_unix_s, unix_now_s());
            if war_over(at_war, left) {
                ctx.log.say(format!(
                    "warzone: the war of zone group {} is over ({} kills)",
                    self.group, self.kills
                ));
                self.outcome = Outcome::Over;
                return GoalStatus::Done;
            }
            // (the server announced a new state: this is another war)
            if ctx
                .state
                .zone_state(self.group)
                .is_some_and(|(_, end)| end != self.end_unix_s)
            {
                self.end_unix_s = ctx.state.zone_state(self.group).map_or(0, |(_, e)| e);
            }
        }
        match self.phase {
            Phase::Plan => self.plan(ctx),
            Phase::Go => self.go(ctx),
            Phase::Patrol => self.patrol(ctx),
            Phase::Fight => self.fight(ctx),
            Phase::Regroup => self.regroup(ctx),
        }
    }

    fn plan(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let now = ctx.now_ms;
        let list = wars(ctx);
        let roll = ctx.rng.usize(..2);
        let me = ctx.state.me.pos;
        let lead = bounty(ctx, |c| {
            let dist = (c.last_seen[0] - me[0]).hypot(c.last_seen[1] - me[1]);
            list.iter().any(|war| war.group == c.group)
                && (ctx.would_teleport(dist) || dist <= WALK_REACH_M)
                && standable(ctx.world)(c.last_seen)
        });
        let chosen = lead
            .as_ref()
            .and_then(|case| list.iter().find(|war| war.group == case.group).copied())
            .or_else(|| pick_war(&list, roll));
        let Some(war) = chosen else {
            return self.fail(ctx, FailReason::NoTarget, "no war to join");
        };
        let near: SmallVec<[([f32; 2], f32); 3]> = nearest_spots(
            ctx.world.zone_group_spots(war.group),
            [me[0], me[1]],
            standable(ctx.world),
        );
        if near.is_empty() {
            return self.fail(ctx, FailReason::NoTarget, "no spot in the zone group");
        }
        let spot = if let Some(case) = lead {
            ctx.log.say(format!(
                "bounty patrol: checking the last sighting of {}",
                case.name
            ));
            case.last_seen
        } else {
            near[ctx.rng.usize(..near.len())].0
        };
        self.group = war.group;
        self.end_unix_s = war.end_unix_s;
        self.kills = war.kills;
        self.spot = Some(spot);
        ctx.log.say(format!(
            "warzone: zone group {} is at war ({} s left, {:.0} m away); joining",
            war.group,
            time_left_s(war.end_unix_s, unix_now_s()),
            war.dist_m
        ));
        self.enter(Phase::Go, now);
        GoalStatus::Running
    }

    fn go(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let now = ctx.now_ms;
        let Some(spot) = self.spot else {
            self.enter(Phase::Patrol, now);
            return GoalStatus::Running;
        };
        // (anyone met on the way is fair game once the bot is in the zone group)
        if self.in_zone(ctx) {
            if let Some(status) = self.look_around(ctx) {
                return status;
            }
        }
        match self.walker.step(ctx, spot, SPOT_STOP_M) {
            Walk::Going => {}
            Walk::There => {
                ctx.log
                    .say(format!("warzone: in zone group {}", self.group));
                self.spot = None;
                self.enter(Phase::Patrol, now);
            }
            Walk::Stuck => return self.fail(ctx, FailReason::Stuck, "cannot reach the war"),
        }
        GoalStatus::Running
    }

    fn in_zone(&self, ctx: &Ctx<'_>) -> bool {
        let me = ctx.state.me.pos;
        ctx.world
            .zone_info_at(me[0], me[1])
            .is_some_and(|z| z.group == self.group)
    }

    fn patrol(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let now = ctx.now_ms;
        if ctx.state.me.hp_pct() < floor_pct(ctx.cfg.pvp.retreat_hp_pct) {
            let here = ctx.state.me.pos;
            return self.begin_regroup(ctx, 0, here);
        }
        if let Some(status) = self.look_around(ctx) {
            return status;
        }
        if self.kills >= ctx.cfg.pvp.war_kill_cap {
            ctx.log.say(format!(
                "warzone: {} kills in this war; leaving it",
                self.kills
            ));
            self.outcome = Outcome::Over;
            return GoalStatus::Done;
        }
        if self.spot.is_none() {
            let me = ctx.state.me.pos;
            self.spot = patrol_spot(
                ctx.world.zone_group_spots(self.group),
                [me[0], me[1]],
                standable(ctx.world),
                ctx.rng,
            );
            self.walker.reset();
            if self.spot.is_none() {
                return self.fail(ctx, FailReason::NoTarget, "no spot to patrol");
            }
        }
        if let Some(spot) = self.spot {
            match self.walker.step(ctx, spot, SPOT_STOP_M) {
                Walk::Going => {}
                Walk::There | Walk::Stuck => {
                    self.spot = None;
                    self.phase_ms = now;
                }
            }
        }
        GoalStatus::Running
    }

    /// Looks for a character to fight; `Some` when the tick is decided.
    fn look_around(&mut self, ctx: &mut Ctx<'_>) -> Option<GoalStatus> {
        let now = ctx.now_ms;
        let cands = scan::hunted(ctx, HUNT_M);
        let lead = bounty(ctx, |case| {
            cands.iter().any(|c| {
                ctx.state
                    .units
                    .get(&c.obj)
                    .is_some_and(|u| u.id == case.target)
            })
        });
        let c = lead
            .and_then(|case| {
                cands.iter().find(|c| {
                    ctx.state
                        .units
                        .get(&c.obj)
                        .is_some_and(|u| u.id == case.target)
                })
            })
            .or_else(|| cands.iter().min_by(|a, b| a.dist.total_cmp(&b.dist)))?;
        let obj = c.obj;
        let odds = scan::weigh(ctx, obj);
        let hp = ctx.state.me.hp_pct();
        let verdict = engage(odds, hp, ctx.cfg.pvp.retreat_hp_pct);
        if self.told != obj {
            self.told = obj;
            ctx.log.say(format!(
                "warzone: {} (level {}) in view; odds {odds:?}, {verdict:?}",
                scan::name_of(ctx, obj),
                c.level
            ));
        }
        match verdict {
            Engage::Attack => {
                if ctx.motion.is_moving() {
                    ctx.motion.stop();
                }
                self.target = obj;
                self.name = scan::name_of(ctx, obj).into();
                self.eval_ms = now + EVAL_AFTER_MS;
                self.spot = None;
                self.enter(Phase::Fight, now);
                ctx.log.say(format!("warzone: attacking {}", self.name));
                ctx.combat.engage(obj);
                Some(GoalStatus::Running)
            }
            Engage::Skip => {
                let until = now
                    + if odds == Odds::Hopeless {
                        fight::SHUN_MS
                    } else {
                        SKIP_MS
                    };
                ctx.state.pvp_mut().shun(obj, until);
                None
            }
            Engage::Regroup => {
                let pos = ctx
                    .state
                    .units
                    .get(&obj)
                    .map_or(ctx.state.me.pos, |u| u.pos);
                Some(self.begin_regroup(ctx, obj, pos))
            }
        }
    }

    fn fight(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let now = ctx.now_ms;
        let target = self.target;
        let dead = ctx.state.units.get(&target).map(|u| u.dead());
        match ctx.combat.status() {
            CombatStatus::TargetDead { obj } if obj == target => {
                self.kills = self.kills.saturating_add(1);
                let (group, end) = (self.group, self.end_unix_s);
                ctx.state.pvp_mut().add_war_kill(group, end);
                ctx.log.say(format!(
                    "warzone: killed {} ({} in this war)",
                    self.name, self.kills
                ));
                self.lost = 0;
                self.enter(Phase::Patrol, now);
                return GoalStatus::Running;
            }
            CombatStatus::Lost => return self.lost(ctx),
            _ => {}
        }
        if dead == Some(true) {
            // (someone else struck the last blow)
            ctx.combat.disengage();
            self.enter(Phase::Patrol, now);
            return GoalStatus::Running;
        }
        if ctx.combat.target() != Some(target) {
            if ctx.combat.busy() {
                return GoalStatus::Running; // defending against someone else
            }
            ctx.state.pvp_mut().shun(target, now + SKIP_MS);
            self.enter(Phase::Patrol, now);
            return GoalStatus::Running;
        }
        if now >= self.eval_ms {
            self.eval_ms = now + EVAL_MS;
            if scan::weigh(ctx, target) == Odds::Hopeless {
                let pos = ctx
                    .state
                    .units
                    .get(&target)
                    .map_or(ctx.state.me.pos, |u| u.pos);
                ctx.combat.disengage();
                let mem = ctx.state.pvp_mut();
                mem.shun(target, now + fight::SHUN_MS);
                mem.gave_up += 1;
                ctx.log.say(format!(
                    "warzone: {} is too strong; breaking off",
                    self.name
                ));
                return self.begin_regroup(ctx, target, pos);
            }
        }
        GoalStatus::Running
    }

    fn lost(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let now = ctx.now_ms;
        let reason = ctx.combat.last_loss().map(|l| l.reason);
        ctx.log.say(format!(
            "warzone: lost {} ({})",
            self.name,
            reason.map_or("?", reason_word)
        ));
        match reason {
            Some(LostReason::Retreat) => {
                // (the controller already shunned the target and started the run)
                self.enter(Phase::Regroup, now);
                GoalStatus::Running
            }
            Some(LostReason::Protected) => {
                self.enter(Phase::Patrol, now);
                GoalStatus::Running
            }
            _ => {
                self.lost += 1;
                ctx.state.pvp_mut().shun(self.target, now + SKIP_MS);
                if self.lost >= MAX_LOST {
                    return self.fail(ctx, FailReason::Other("lost"), "everyone gets away");
                }
                self.enter(Phase::Patrol, now);
                GoalStatus::Running
            }
        }
    }

    /// Runs from `from_obj` at `from` and rests.
    fn begin_regroup(&mut self, ctx: &mut Ctx<'_>, from_obj: u32, from: [f32; 3]) -> GoalStatus {
        let now = ctx.now_ms;
        if from_obj != 0 {
            fight::start_flee(ctx, from_obj, from);
        }
        ctx.combat.disengage();
        self.target = 0;
        self.spot = None;
        ctx.log.say("warzone: regrouping");
        self.enter(Phase::Regroup, now);
        GoalStatus::Running
    }

    fn regroup(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let now = ctx.now_ms;
        if ctx.combat.busy() {
            return GoalStatus::Running; // running, or defending
        }
        if ctx.state.me.hp_pct() >= FIT_HP_PCT {
            ctx.log.say("warzone: fit again; back to the patrol");
            self.enter(Phase::Patrol, now);
            return GoalStatus::Running;
        }
        if now.saturating_sub(self.phase_ms) > REGROUP_MAX_MS {
            return self.fail(ctx, FailReason::Timeout, "could not recover");
        }
        if ctx.motion.is_moving() {
            ctx.motion.stop();
        }
        GoalStatus::Running
    }

    /// Ends the goal's business: stops, leaves the fight and sets the cooldown. Runs once.
    fn finish(&mut self, ctx: &mut Ctx<'_>) {
        if self.finished {
            return;
        }
        self.finished = true;
        if ctx.motion.is_moving() {
            ctx.motion.stop();
        }
        if self.target != 0 && ctx.combat.target() == Some(self.target) {
            ctx.combat.disengage();
        }
        if ctx.state.me.dead {
            self.outcome = Outcome::Died;
        }
        let (lo, hi) = self.outcome.cooldown_ms();
        let until = ctx.now_ms + super::jitter_ms(ctx.rng, lo, hi);
        let mem = ctx.state.pvp_mut();
        mem.war_next_ms = mem.war_next_ms.max(until);
        if let Some(line) = mem.standing() {
            ctx.log.say(line);
        }
    }
}

fn reason_word(r: LostReason) -> &'static str {
    match r {
        LostReason::Gone => "out of sight",
        LostReason::Far => "too far",
        LostReason::Timeout => "took too long",
        LostReason::Stalled => "no damage",
        LostReason::Blocked => "blocked",
        LostReason::Travel => "travelling",
        LostReason::Retreat => "retreat",
        LostReason::Protected => "protected",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn war(group: u16, dist_m: f32) -> War {
        War {
            group,
            end_unix_s: 1_000,
            dist_m,
            kills: 0,
        }
    }

    #[test]
    fn a_war_needs_time_left_and_room_for_kills() {
        assert!(worth_joining(MIN_LEFT_S, 0, 10));
        assert!(!worth_joining(MIN_LEFT_S - 1, 0, 10));
        assert!(worth_joining(3_600, 9, 10));
        assert!(!worth_joining(3_600, 10, 10), "the cap is reached");
        assert!(!worth_joining(-5, 0, 10), "a state that ended is no war");
    }

    #[test]
    fn the_time_left_follows_the_servers_schedule() {
        assert_eq!(time_left_s(1_000, 400), 600);
        assert_eq!(time_left_s(1_000, 1_000), 0);
        assert_eq!(time_left_s(1_000, 1_200), -200);
        // no end announced: a plausible stretch, not forever and not nothing
        assert_eq!(time_left_s(0, 5_000), UNKNOWN_LEFT_S);
        assert!(worth_joining(time_left_s(0, 5_000), 0, 10));
    }

    #[test]
    fn the_war_is_over_at_peace_or_just_before_it_ends() {
        assert!(!war_over(true, 3_600));
        assert!(!war_over(true, LEAVE_LEFT_S + 1));
        assert!(war_over(true, LEAVE_LEFT_S));
        assert!(war_over(true, -3));
        assert!(
            war_over(false, 3_600),
            "peace ends it whatever the clock says"
        );
    }

    #[test]
    fn the_nearest_war_is_picked_and_the_second_for_variety() {
        let wars = [war(20, 900.0), war(15, 200.0), war(17, 5_000.0)];
        assert_eq!(pick_war(&wars, 0).map(|w| w.group), Some(15));
        assert_eq!(pick_war(&wars, 1).map(|w| w.group), Some(20));
        assert_eq!(pick_war(&wars, 7).map(|w| w.group), Some(20));
        // one war: the second choice is the first
        assert_eq!(pick_war(&wars[..1], 1).map(|w| w.group), Some(20));
        assert_eq!(pick_war(&[], 0), None);
    }

    #[test]
    fn a_fair_fight_is_taken_and_a_hurt_bot_regroups() {
        assert_eq!(engage(Odds::Fair, 100, 35), Engage::Attack);
        assert_eq!(engage(Odds::NeedsHelp, 100, 35), Engage::Skip);
        assert_eq!(engage(Odds::Hopeless, 100, 35), Engage::Skip);
        // below the retreat line plus a margin the bot regroups before it starts anything
        assert_eq!(engage(Odds::Fair, 54, 35), Engage::Regroup);
        assert_eq!(engage(Odds::Fair, 55, 35), Engage::Attack);
        // retreat 0 keeps a floor
        assert_eq!(engage(Odds::Fair, 19, 0), Engage::Regroup);
        assert_eq!(engage(Odds::Fair, 20, 0), Engage::Attack);
    }

    #[test]
    fn spots_are_sorted_by_distance_and_filtered() {
        let spots = [
            [100.0, 0.0],
            [10.0, 0.0],
            [50.0, 0.0],
            [30.0, 0.0],
            [5.0, 0.0],
        ];
        let n: SmallVec<[([f32; 2], f32); 3]> = nearest_spots(&spots, [0.0, 0.0], |_| true);
        let d: Vec<f32> = n.iter().map(|s| s.1).collect();
        assert_eq!(d, [5.0, 10.0, 30.0]);
        // spots the bot cannot stand on do not count
        let n: SmallVec<[([f32; 2], f32); 2]> =
            nearest_spots(&spots, [0.0, 0.0], |s| s[0] != 5.0 && s[0] != 10.0);
        let d: Vec<f32> = n.iter().map(|s| s.1).collect();
        assert_eq!(d, [30.0, 50.0]);
        let n: SmallVec<[([f32; 2], f32); 2]> = nearest_spots(&[], [0.0, 0.0], |_| true);
        assert!(n.is_empty());
    }

    #[test]
    fn only_land_groups_are_wars_for_a_bot_on_foot() {
        let land: Vec<[f32; 2]> = (0..20).map(|i| [i as f32 * 256.0, 0.0]).collect();
        assert!(land_group(&land, |_| true));
        // a coast: 8 of 20 spots on land is still a land group, 7 is not
        assert!(land_group(&land, |s| s[0] < 8.0 * 256.0));
        assert!(!land_group(&land, |s| s[0] < 7.0 * 256.0));
        // the open sea, and a group too small to patrol
        assert!(!land_group(&land, |_| false));
        assert!(!land_group(&land[..7], |_| true));
        assert!(!land_group(&[], |_| true));
    }

    #[test]
    fn the_patrol_moves_on_by_a_few_hundred_metres() {
        let spots = [
            [0.0, 0.0],
            [50.0, 0.0],
            [200.0, 0.0],
            [400.0, 0.0],
            [900.0, 0.0],
        ];
        let mut rng = fastrand::Rng::with_seed(4);
        for _ in 0..50 {
            let s = patrol_spot(&spots, [0.0, 0.0], |_| true, &mut rng).unwrap();
            assert!(s == [200.0, 0.0] || s == [400.0, 0.0], "{s:?}");
        }
        // nothing in range: the nearest other spot
        let s = patrol_spot(&spots[..2], [0.0, 0.0], |_| true, &mut rng).unwrap();
        assert_eq!(s, [50.0, 0.0]);
        // no spot at all, or none standable
        assert_eq!(patrol_spot(&[], [0.0, 0.0], |_| true, &mut rng), None);
        assert_eq!(patrol_spot(&spots, [0.0, 0.0], |_| false, &mut rng), None);
    }

    #[test]
    fn the_weight_follows_the_fighting_temper() {
        let mut fighter = Temperament::NEUTRAL;
        fighter.duelist = 1.0;
        let mut pacifist = Temperament::NEUTRAL;
        pacifist.duelist = 0.0;
        assert!(weight(&fighter) > weight(&Temperament::NEUTRAL));
        assert!(weight(&Temperament::NEUTRAL) > weight(&pacifist));
        assert!(weight(&pacifist) > 0.0);
        assert_eq!(base_weight(&BehaviorCfg::default(), &fighter), 0.0);
    }

    #[test]
    fn a_fresh_goal_shows_as_idle_and_owns_no_combat() {
        let g = Warzone::new();
        assert_eq!(g.activity(false, false), Activity::Idle);
        assert_eq!(g.activity(true, false), Activity::Traveling);
        assert_eq!(g.activity(true, true), Activity::Fighting);
        assert!(!g.owns_combat());
        let mut text = String::new();
        g.describe(&mut text);
        assert_eq!(text, "warzone");
    }

    #[test]
    fn the_cooldowns_grow_with_the_trouble() {
        let low = |o: Outcome| o.cooldown_ms().0;
        assert!(low(Outcome::Over) < low(Outcome::Nothing));
        assert!(low(Outcome::Nothing) < low(Outcome::Died));
        for o in [Outcome::Over, Outcome::Nothing, Outcome::Died] {
            let (lo, hi) = o.cooldown_ms();
            assert!(lo < hi);
        }
    }
}
