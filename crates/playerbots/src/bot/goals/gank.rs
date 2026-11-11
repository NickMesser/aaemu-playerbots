//! Gank: ambush the carriers of trade packs of the hostile alliance.
//!
//! A carrier walks from a specialty workbench to the NPC that buys its pack, and the pack makes
//! it a tempting target: when it dies the pack drops as a doodad that anybody can pick up with
//! skill 11361 and sell. The goal is a small machine:
//!
//! ```text
//! Plan -> ToPost -> Lurk -> Fight -> Collect -> (trade run, sells the pack)
//! ```
//!
//! * **Plan**: [`pvp::post`](crate::bot::pvp::post) picks a post on a trade route (outside the
//!   victims' home zones, where the server protects them; conflict zones and wars score higher).
//!   A bot that joined a squad takes the post of the squad's leader instead.
//! * **ToPost / Lurk**: walk there and wait, looking out for carriers
//!   ([`scan::carriers`]: the rules of [`pvp::rules`](crate::bot::pvp::rules) apply, so never a
//!   friendly character, never a name in `never_names`, never one in its home zone). A solo bot
//!   patrols a few dozen metres along the route and offers itself to the coordinator now and then;
//!   the coordinator groups bots of the same alliance that wait close together into a squad of up
//!   to `[pvp] max_squad` (a [`PartyOrder`] with the purpose
//!   [`PartyPurpose::Ambush`], which restarts this goal for all of them at one post).
//! * **Fight**: the odds decide ([`verdict`]): a fair fight is taken, a stronger target needs a
//!   squad, a stronger group is left alone. The combat controller does the fighting; the goal
//!   gives up when the odds turn hopeless, the bot retreats, the zone turns out to protect the
//!   target or the bot dies.
//! * **Collect**: the pack doodad that appears at the body is picked up within 3 m
//!   ([`CollectJob`]); a squad agrees on the carrier by the same rule ([`squad::pick_target`]) and
//!   the members take turns at the doodad, so the first one that gets there wins.
//! * The pack is then sold by the trade run's carry-only plan: [`Gank::resumes_run`] tells
//!   [`Goal::tick`](super::Goal::tick) to continue as a trade run.
//!
//! Everything is off until `[pvp] enabled` and `gank`, and the bot needs someone it may attack
//! (`attack_bots`, or real players when [`Config::may_attack_players`](crate::config::Config)).

use aa_proto::cs;
use pb_web::Activity;
use smallvec::SmallVec;

use crate::bot::combat::{CombatStatus, LostReason};
use crate::bot::ctx::Ctx;
use crate::bot::party::{PartyOrder, PartyPurpose};
use crate::bot::pvp::fight::{self, Odds};
use crate::bot::pvp::post::{self, Post, Query};
use crate::bot::pvp::rules;
use crate::bot::pvp::scan;
use crate::bot::pvp::squad::{self, Standby};
use crate::bot::pvp::walk::{Walk, Walker};
use crate::bot::state::dist2d;
use crate::config::BehaviorCfg;
use crate::roster::Temperament;

use super::trade_run::carried_pack_of;
use super::{FailReason, GoalStatus};

/// A bot that has been online for less than this does not gank yet (bot clock, ms).
const SETTLE_MS: u64 = 60_000;
/// Bots below this level do not gank.
const MIN_LEVEL: u8 = 15;
/// The whole goal gives up after this long.
const GOAL_MAX_MS: u64 = 25 * 60_000;
/// Lying in wait gives up after this long.
const LURK_MAX_MS: u64 = 10 * 60_000;
/// Workbenches within this many metres are looked at when the bot has to walk.
const PLAN_WALK_RADIUS_M: f32 = 3_000.0;
/// Posts farther than this are not walked to.
const PLAN_WALK_TRIP_M: f32 = 3_500.0;
/// Workbenches within this many metres are looked at when a GM `/move` brings the bot there.
const PLAN_TELEPORT_RADIUS_M: f32 = 12_000.0;
/// The bot counts as at its post this close (metres).
const POST_STOP_M: f32 = 6.0;
/// How far the bot looks out for carriers (metres): well inside the distance at which the combat
/// controller gives a fight with a character up.
const SEE_M: f32 = 60.0;
const _: () = assert!(SEE_M < fight::LOST_DISTANCE);
/// A solo bot walks this far either way along the route while it waits (metres) ...
const PATROL_SPAN_M: f32 = 70.0;
/// ... every so often (ms, low and high).
const PATROL_EVERY_MS: (u64, u64) = (40_000, 90_000);
/// The odds of a running fight are looked at this often (ms).
const EVAL_MS: u64 = 2_000;
/// ... and not before the fight is this old (ms).
const EVAL_AFTER_MS: u64 = 3_000;
/// A carrier the bot (or its squad) cannot take is left alone this long (ms).
const SKIP_MS: u64 = 60_000;
/// Fights lost in a row (the target slipped away) after which the goal ends.
const MAX_LOST: u8 = 2;
/// The skill that picks a dropped pack up (`DoodadFuncRecoverItem`).
const COLLECT_SKILL: u32 = 11_361;
/// A pack doodad this close to the body (metres) is the carrier's.
const PACK_NEAR_M: f32 = 20.0;
/// The doodad is used from this close (metres).
const CAST_M: f32 = 2.8;
/// The bot walks up to this close (metres).
const COLLECT_STOP_M: f32 = 1.5;
/// A use of the doodad is repeated after this long (ms).
const CAST_GAP_MS: u64 = 2_500;
/// Most uses of the doodad.
const MAX_CASTS: u8 = 4;
/// Collecting gives up after this long (ms).
const COLLECT_MAX_MS: u64 = 30_000;
/// No doodad appeared this long after the kill (ms): nothing was dropped.
const APPEAR_MS: u64 = 8_000;
/// Squad members take turns at the doodad: member `i` waits `i` times this long (ms).
const TURN_MS: u64 = 1_500;

/// Gank: ambush trade-pack carriers. See the module docs.
#[derive(Debug, Default)]
pub struct Gank {
    m: Option<Box<Machine>>,
    order: Option<Box<PartyOrder>>,
}

impl Gank {
    /// A fresh goal for a solo bot.
    pub fn new() -> Gank {
        Gank::default()
    }

    /// A goal that lies in wait with the squad of `order` (the coordinator's ambush order).
    pub fn with_order(order: Option<Box<PartyOrder>>) -> Gank {
        Gank { m: None, order }
    }

    /// The squad's order, if the goal has one.
    pub fn order(&self) -> Option<&PartyOrder> {
        self.order.as_deref()
    }

    /// Called once when the brain picks the goal.
    pub fn start(&mut self, ctx: &mut Ctx<'_>) {
        let now = ctx.now_ms;
        self.m = Some(Box::new(Machine::new(now, carried_pack_of(ctx).is_some())));
        match self.order.as_deref() {
            Some(o) => ctx.log.say(format!(
                "goal: gank with a squad of {} (leader {})",
                o.members.len(),
                o.leader
            )),
            None => ctx.log.debug("goal: gank (solo)"),
        }
    }

    /// One tick.
    pub fn tick(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        match self.m.as_deref_mut() {
            Some(m) => m.step(ctx, self.order.as_deref()),
            None => GoalStatus::Failed(FailReason::Other("gank was not started")),
        }
    }

    /// Called when the goal ends, fails or is interrupted (stop motion, leave the fight).
    pub fn stop(&mut self, ctx: &mut Ctx<'_>) {
        if let Some(m) = self.m.as_deref_mut() {
            m.finish(ctx);
        }
    }

    /// Whether the goal ended with a pack in the bag that the trade run should sell. Also ends
    /// the goal's business (the caller replaces the goal without a `stop`).
    pub fn resumes_run(&mut self, ctx: &mut Ctx<'_>) -> bool {
        if let Some(m) = self.m.as_deref_mut() {
            m.finish(ctx);
        }
        carried_pack_of(ctx).is_some()
    }

    /// Whether the goal drives the combat controller itself (see [`super::Goal::owns_combat`]):
    /// while it fights its carrier.
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
            out.push_str("gank");
            return;
        };
        match m.phase {
            Phase::Plan => out.push_str("gank: planning an ambush"),
            Phase::ToPost => out.push_str("gank: going to the ambush post"),
            Phase::Lurk if self.order.is_some() => out.push_str("gank: lying in wait with a squad"),
            Phase::Lurk => out.push_str("gank: lying in wait"),
            Phase::Fight => {
                out.push_str("gank: attacking ");
                out.push_str(&m.name);
            }
            Phase::Collect => out.push_str("gank: taking the pack"),
        }
    }
}

/// The goal's weight before the situation is looked at: 0 (the goal scores itself, see [`score`],
/// from the temperament and the situation).
pub fn base_weight(_behavior: &BehaviorCfg, _temper: &Temperament) -> f32 {
    0.0
}

/// The weight of an ambush for a bot with this temperament: ganking is for the fighting types,
/// about a third of a quest's weight for a neutral bot.
pub fn weight(temper: &Temperament) -> f32 {
    0.3 * (0.2 + 1.6 * temper.duelist)
}

/// The score of the goal for this tick: 0 unless ganking is on, someone may be attacked, the bot
/// is settled, healthy, a high enough level, in the main world, off its cooldown and not already
/// carrying a pack; the temperament's [`weight`] otherwise.
pub fn score(ctx: &Ctx<'_>, _base: f32) -> f32 {
    let cfg = ctx.cfg;
    if !(cfg.pvp.enabled && cfg.pvp.gank) || !(cfg.pvp.attack_bots || cfg.may_attack_players()) {
        return 0.0;
    }
    let me = &ctx.state.me;
    if ctx.state.place.in_instance()
        || me.dead
        || ctx.now_ms < SETTLE_MS
        || me.level < MIN_LEVEL
        || me.hp_pct() < start_hp(cfg.pvp.retreat_hp_pct)
        || ctx
            .state
            .pvp
            .as_ref()
            .is_some_and(|p| ctx.now_ms < p.gank_next_ms)
        || rules::enemy_alliance(ctx.world.mother_faction(me.faction)) == 0
        || carried_pack_of(ctx).is_some()
    {
        return 0.0;
    }
    weight(&ctx.persona.temper)
}

// ---- the pure decisions ----------------------------------------------------------------------

/// Hit points (percent) a bot wants before it starts a fight: well above the retreat line.
pub fn start_hp(retreat_pct: u8) -> u8 {
    retreat_pct.saturating_add(30).clamp(60, 95)
}

/// What the bot does about a carrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Attack.
    Attack,
    /// Keep waiting for help (a squad).
    Wait,
    /// Leave this one alone for a while.
    Skip,
    /// Too hurt to start a fight.
    Rest,
}

/// The decision for a carrier whose fight looks like `odds` to a bot with `hp_pct` percent of its
/// hit points that retreats below `retreat_pct`; `in_squad` says the bot waits with a squad.
pub fn verdict(odds: Odds, in_squad: bool, hp_pct: u8, retreat_pct: u8) -> Verdict {
    if hp_pct < start_hp(retreat_pct) {
        return Verdict::Rest;
    }
    match odds {
        Odds::Fair => Verdict::Attack,
        Odds::NeedsHelp if in_squad => Verdict::Skip,
        Odds::NeedsHelp => Verdict::Wait,
        Odds::Hopeless => Verdict::Skip,
    }
}

/// What a collecting tick sees.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CollectView {
    /// Bot clock (ms).
    pub now_ms: u64,
    /// The pack doodad at the body `(object id, distance to the bot)`, if there is one.
    pub doodad: Option<(u32, f32)>,
    /// The bot's backpack slot holds a trade pack.
    pub carrying: bool,
}

/// What a collecting tick does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectStep {
    /// Walk up to the doodad.
    Walk(u32),
    /// Use the doodad (skill 11361).
    Cast(u32),
    /// Nothing to do yet.
    Wait,
    /// The pack is in the bag.
    Collected,
    /// There is no pack to take (never dropped, or somebody else took it).
    Nothing(&'static str),
    /// Gave up.
    Timeout,
}

/// Picking the dropped pack up. Pure: the goal feeds it what it sees every tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CollectJob {
    since_ms: u64,
    wait_until_ms: u64,
    last_cast_ms: u64,
    casts: u8,
    seen: bool,
}

impl CollectJob {
    /// A job that starts at `now_ms` and waits `delay_ms` before it goes for the doodad.
    pub fn new(now_ms: u64, delay_ms: u64) -> CollectJob {
        CollectJob {
            since_ms: now_ms,
            wait_until_ms: now_ms + delay_ms,
            last_cast_ms: 0,
            casts: 0,
            seen: false,
        }
    }

    /// Uses made so far.
    pub fn casts(&self) -> u8 {
        self.casts
    }

    /// One tick.
    pub fn step(&mut self, v: &CollectView) -> CollectStep {
        if v.carrying {
            return CollectStep::Collected;
        }
        let age = v.now_ms.saturating_sub(self.since_ms);
        if age > COLLECT_MAX_MS {
            return CollectStep::Timeout;
        }
        let Some((obj, dist)) = v.doodad else {
            return if self.seen {
                CollectStep::Nothing("somebody else took the pack")
            } else if age > APPEAR_MS {
                CollectStep::Nothing("no pack dropped")
            } else {
                CollectStep::Wait
            };
        };
        self.seen = true;
        if v.now_ms < self.wait_until_ms {
            return CollectStep::Wait;
        }
        if dist > CAST_M {
            return CollectStep::Walk(obj);
        }
        if self.casts >= MAX_CASTS {
            return CollectStep::Timeout;
        }
        if self.casts == 0 || v.now_ms.saturating_sub(self.last_cast_ms) >= CAST_GAP_MS {
            self.casts += 1;
            self.last_cast_ms = v.now_ms;
            return CollectStep::Cast(obj);
        }
        CollectStep::Wait
    }
}

/// A point along the route within [`PATROL_SPAN_M`] of the post, `u` in `-1..=1`.
fn patrol_point(post: &Post, u: f32) -> [f32; 2] {
    let s = u.clamp(-1.0, 1.0) * PATROL_SPAN_M;
    [post.pos[0] + post.dir[0] * s, post.pos[1] + post.dir[1] * s]
}

// ---- the machine -------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Plan,
    ToPost,
    Lurk,
    Fight,
    Collect,
}

/// How the goal went, for the cooldown before the bot gets the idea again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Nothing,
    NoPost,
    Collected,
    GaveUp,
    Died,
}

impl Outcome {
    /// The wait before the next gank (ms, low and high).
    fn cooldown_ms(self) -> (u64, u64) {
        match self {
            Outcome::Collected => (4 * 60_000, 8 * 60_000),
            Outcome::Nothing => (5 * 60_000, 10 * 60_000),
            Outcome::NoPost => (8 * 60_000, 12 * 60_000),
            Outcome::GaveUp => (10 * 60_000, 15 * 60_000),
            Outcome::Died => (20 * 60_000, 30 * 60_000),
        }
    }
}

#[derive(Debug)]
struct Machine {
    phase: Phase,
    post: Post,
    walker: Walker,
    end_ms: u64,
    phase_ms: u64,
    offer_ms: u64,
    patrol_ms: u64,
    patrol_to: Option<[f32; 2]>,
    /// The carrier being fought, or watched while the bot waits with a squad (0 = none).
    target: u32,
    name: Box<str>,
    /// Where the carrier was last seen.
    victim: [f32; 3],
    eval_ms: u64,
    lost: u8,
    /// The bot carried a pack when it started: it cannot take another.
    had_pack: bool,
    job: CollectJob,
    /// The carrier the last sighting was logged for.
    told: u32,
    outcome: Outcome,
    finished: bool,
}

impl Machine {
    fn new(now_ms: u64, had_pack: bool) -> Machine {
        Machine {
            phase: Phase::Plan,
            post: Post {
                pos: [0.0, 0.0],
                dir: [0.0, 0.0],
                group: 0,
                price: 0,
                score: 0.0,
            },
            walker: Walker::new(),
            end_ms: now_ms + GOAL_MAX_MS,
            phase_ms: now_ms,
            offer_ms: 0,
            patrol_ms: 0,
            patrol_to: None,
            target: 0,
            name: "".into(),
            victim: [0.0; 3],
            eval_ms: 0,
            lost: 0,
            had_pack,
            job: CollectJob::new(now_ms, 0),
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

    fn step(&mut self, ctx: &mut Ctx<'_>, order: Option<&PartyOrder>) -> GoalStatus {
        if ctx.now_ms >= self.end_ms {
            return self.fail(
                ctx,
                FailReason::Timeout,
                "it took too long",
                Outcome::Nothing,
            );
        }
        match self.phase {
            Phase::Plan => self.plan(ctx, order),
            Phase::ToPost => self.go_to_post(ctx, order),
            Phase::Lurk => self.lurk(ctx, order),
            Phase::Fight => self.fight(ctx, order),
            Phase::Collect => self.collect(ctx, order),
        }
    }

    fn fail(
        &mut self,
        ctx: &mut Ctx<'_>,
        why: FailReason,
        note: &str,
        outcome: Outcome,
    ) -> GoalStatus {
        ctx.log.say(format!("gank: giving up ({note})"));
        self.outcome = outcome;
        GoalStatus::Failed(why)
    }

    // ---- planning and the way ------------------------------------------------------------------

    fn plan(&mut self, ctx: &mut Ctx<'_>, order: Option<&PartyOrder>) -> GoalStatus {
        let now = ctx.now_ms;
        if let Some(PartyPurpose::Ambush { x, y }) = order.map(|o| o.purpose) {
            self.post = Post {
                pos: [x as f32, y as f32],
                dir: [0.0, 0.0],
                group: 0,
                price: 0,
                score: 0.0,
            };
            self.enter(Phase::ToPost, now);
            return GoalStatus::Running;
        }
        let victims = rules::enemy_alliance(ctx.world.mother_faction(ctx.state.me.faction));
        if victims == 0 {
            return self.fail(
                ctx,
                FailReason::NoTarget,
                "no enemy alliance",
                Outcome::NoPost,
            );
        }
        let me = ctx.state.me.pos;
        let (radius_m, max_trip_m) = if ctx.would_teleport(1.0e9) {
            (PLAN_TELEPORT_RADIUS_M, f32::INFINITY)
        } else {
            (PLAN_WALK_RADIUS_M, PLAN_WALK_TRIP_M)
        };
        let cands = {
            let state = &*ctx.state;
            let at_war = |g: u16| state.at_war(g);
            let post_ok = |p: [f32; 2]| !state.pvp.as_ref().is_some_and(|m| m.post_blocked(p, now));
            let q = Query {
                from: [me[0], me[1]],
                radius_m,
                max_trip_m,
                victims,
                at_war: &at_war,
                post_ok: &post_ok,
            };
            post::candidates(ctx.world, ctx.world, &q)
        };
        let Some(p) = post::pick(&cands, ctx.rng) else {
            return self.fail(ctx, FailReason::NoTarget, "no ambush post", Outcome::NoPost);
        };
        ctx.log.say(format!(
            "gank: lying in wait at {:.0}, {:.0} (zone group {}, the pack pays {})",
            p.pos[0],
            p.pos[1],
            p.group,
            crate::bot::goals::trade_run::coins(i64::from(p.price)),
        ));
        self.post = p;
        self.enter(Phase::ToPost, now);
        GoalStatus::Running
    }

    fn go_to_post(&mut self, ctx: &mut Ctx<'_>, order: Option<&PartyOrder>) -> GoalStatus {
        let now = ctx.now_ms;
        if carried_pack_of(ctx).is_some() && !self.had_pack {
            return GoalStatus::Done;
        }
        if ctx.state.me.hp_pct() < start_hp(ctx.cfg.pvp.retreat_hp_pct) {
            return self.fail(ctx, FailReason::Other("hurt"), "too hurt", Outcome::GaveUp);
        }
        // (a carrier on the way is as good as one at the post)
        if let Some(obj) = self.spot(ctx, order) {
            return self.attack(ctx, obj);
        }
        match self.walker.step(ctx, self.post.pos, POST_STOP_M) {
            Walk::Going => {}
            Walk::There => {
                ctx.log.say("gank: at the post, watching the road");
                self.enter(Phase::Lurk, now);
                self.patrol_ms = now
                    + crate::bot::goals::jitter_ms(ctx.rng, PATROL_EVERY_MS.0, PATROL_EVERY_MS.1);
            }
            Walk::Stuck => {
                let pos = self.post.pos;
                ctx.state.pvp_mut().block_post(pos, now + 20 * 60_000);
                return self.fail(
                    ctx,
                    FailReason::Stuck,
                    "cannot reach the post",
                    Outcome::NoPost,
                );
            }
        }
        GoalStatus::Running
    }

    // ---- waiting -------------------------------------------------------------------------------

    fn lurk(&mut self, ctx: &mut Ctx<'_>, order: Option<&PartyOrder>) -> GoalStatus {
        let now = ctx.now_ms;
        if carried_pack_of(ctx).is_some() && !self.had_pack {
            return GoalStatus::Done;
        }
        if ctx.state.me.hp_pct() < start_hp(ctx.cfg.pvp.retreat_hp_pct) {
            // (the brain rests a hurt bot)
            return GoalStatus::Done;
        }
        if now.saturating_sub(self.phase_ms) > LURK_MAX_MS {
            ctx.log.say("gank: nobody came by; giving the post up");
            return GoalStatus::Done;
        }
        // a squad mate killed the carrier the squad agreed on: the pack is up for grabs
        if order.is_some() && self.target != 0 {
            let me = ctx.state.me.pos;
            match ctx.state.units.get(&self.target) {
                Some(u) if u.dead() && u.dist2d(me) <= 2.0 * SEE_M => {
                    self.victim = u.pos;
                    return self.begin_collect(ctx, order, "a squad mate killed the carrier");
                }
                Some(_) => {}
                None => self.target = 0,
            }
        }
        if let Some(obj) = self.spot(ctx, order) {
            return self.attack(ctx, obj);
        }
        // a solo bot looks for company now and then
        if order.is_none() && ctx.cfg.pvp.max_squad >= 2 && now >= self.offer_ms {
            let me = ctx.state.me.pos;
            let offer = Standby {
                bot: ctx.persona.id,
                at: (me[0], me[1]),
                alliance: ctx.world.mother_faction(ctx.state.me.faction),
                level: ctx.state.me.level,
                max_squad: ctx.cfg.pvp.max_squad,
            };
            if ctx.coord.standby(offer) {
                self.offer_ms = now + squad::REPEAT_MS;
            } else {
                self.offer_ms = now + 5_000;
            }
        }
        self.patrol(ctx, order.is_some());
        GoalStatus::Running
    }

    /// A short walk along the route now and then (a squad stands still).
    fn patrol(&mut self, ctx: &mut Ctx<'_>, in_squad: bool) {
        let now = ctx.now_ms;
        if let Some(p) = self.patrol_to {
            match self.walker.step(ctx, p, 3.0) {
                Walk::Going => {}
                Walk::There | Walk::Stuck => {
                    self.patrol_to = None;
                    self.walker.reset();
                }
            }
            return;
        }
        if in_squad || self.post.dir == [0.0, 0.0] || now < self.patrol_ms {
            return;
        }
        self.patrol_ms =
            now + crate::bot::goals::jitter_ms(ctx.rng, PATROL_EVERY_MS.0, PATROL_EVERY_MS.1);
        let p = patrol_point(&self.post, ctx.rng.f32() * 2.0 - 1.0);
        if crate::bot::goals::walkable(ctx.world, p[0], p[1]) {
            self.patrol_to = Some(p);
            self.walker.reset();
        }
    }

    /// Looks for a carrier to attack: `Some(obj)` when the bot takes the fight.
    fn spot(&mut self, ctx: &mut Ctx<'_>, order: Option<&PartyOrder>) -> Option<u32> {
        let now = ctx.now_ms;
        let cands = scan::carriers(ctx, self.post.pos, SEE_M);
        if cands.is_empty() {
            return None;
        }
        let view: SmallVec<[(u32, f32, f32); 8]> =
            cands.iter().map(|c| (c.obj, c.dist, c.dist_post)).collect();
        let obj = squad::pick_target(&view)?;
        if order.is_some() {
            self.target = obj;
        }
        let odds = scan::weigh(ctx, obj);
        let me = &ctx.state.me;
        let v = verdict(
            odds,
            order.is_some(),
            me.hp_pct(),
            ctx.cfg.pvp.retreat_hp_pct,
        );
        if self.told != obj {
            self.told = obj;
            let pack = cands.iter().find(|c| c.obj == obj).map_or(0, |c| c.pack);
            ctx.log.say(format!(
                "gank: {} carries a pack ({pack}); odds {odds:?}, {v:?}",
                scan::name_of(ctx, obj)
            ));
        }
        match v {
            Verdict::Attack => Some(obj),
            Verdict::Wait | Verdict::Rest => None,
            Verdict::Skip => {
                let until = now
                    + if odds == Odds::Hopeless {
                        fight::SHUN_MS
                    } else {
                        SKIP_MS
                    };
                ctx.state.pvp_mut().shun(obj, until);
                if self.target == obj {
                    self.target = 0;
                }
                None
            }
        }
    }

    // ---- the fight -----------------------------------------------------------------------------

    fn attack(&mut self, ctx: &mut Ctx<'_>, obj: u32) -> GoalStatus {
        let now = ctx.now_ms;
        if ctx.motion.is_moving() {
            ctx.motion.stop();
        }
        self.patrol_to = None;
        self.target = obj;
        self.name = scan::name_of(ctx, obj).into();
        if let Some(u) = ctx.state.units.get(&obj) {
            self.victim = u.pos;
        }
        self.eval_ms = now + EVAL_AFTER_MS;
        self.enter(Phase::Fight, now);
        ctx.log.say(format!("gank: attacking {}", self.name));
        ctx.combat.engage(obj);
        GoalStatus::Running
    }

    fn fight(&mut self, ctx: &mut Ctx<'_>, order: Option<&PartyOrder>) -> GoalStatus {
        let now = ctx.now_ms;
        let target = self.target;
        let unit = ctx.state.units.get(&target).map(|u| (u.pos, u.dead()));
        if let Some((pos, _)) = unit {
            self.victim = pos;
        }
        match ctx.combat.status() {
            CombatStatus::TargetDead { obj } if obj == target => {
                return self.begin_collect(ctx, order, "the carrier is dead");
            }
            CombatStatus::Lost => return self.lost(ctx),
            _ => {}
        }
        if unit.is_some_and(|(_, dead)| dead) {
            // (a squad mate struck the last blow)
            ctx.combat.disengage();
            return self.begin_collect(ctx, order, "the carrier is dead");
        }
        if ctx.combat.target() != Some(target) {
            if ctx.combat.busy() {
                return GoalStatus::Running; // defending against someone else
            }
            // the controller dropped the carrier without a word (it left the view)
            ctx.state.pvp_mut().shun(target, now + SKIP_MS);
            self.back_to_lurk(ctx);
            return GoalStatus::Running;
        }
        if now >= self.eval_ms {
            self.eval_ms = now + EVAL_MS;
            if scan::weigh(ctx, target) == Odds::Hopeless {
                return self.give_up(ctx, "outnumbered");
            }
        }
        GoalStatus::Running
    }

    fn lost(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let now = ctx.now_ms;
        let reason = ctx.combat.last_loss().map(|l| l.reason);
        ctx.log.say(format!(
            "gank: lost {} ({})",
            self.name,
            reason.map_or("?", reason_word)
        ));
        match reason {
            Some(LostReason::Retreat) => self.fail(
                ctx,
                FailReason::Other("retreat"),
                "retreat",
                Outcome::GaveUp,
            ),
            Some(LostReason::Protected) => {
                // (the carrier stands where the server protects it: this post is no good)
                let pos = self.post.pos;
                ctx.state.pvp_mut().block_post(pos, now + 30 * 60_000);
                self.fail(
                    ctx,
                    FailReason::Other("protected"),
                    "protected target",
                    Outcome::GaveUp,
                )
            }
            _ => {
                self.lost += 1;
                ctx.state.pvp_mut().shun(self.target, now + SKIP_MS);
                if self.lost >= MAX_LOST {
                    return self.fail(
                        ctx,
                        FailReason::Other("lost"),
                        "carriers slip away",
                        Outcome::GaveUp,
                    );
                }
                self.back_to_lurk(ctx);
                GoalStatus::Running
            }
        }
    }

    fn back_to_lurk(&mut self, ctx: &mut Ctx<'_>) {
        ctx.combat.disengage();
        self.target = 0;
        // (the lurk clock keeps running from the original arrival)
        let phase_ms = self.phase_ms;
        self.enter(Phase::Lurk, ctx.now_ms);
        self.phase_ms = phase_ms;
    }

    /// Breaks the fight off and runs.
    fn give_up(&mut self, ctx: &mut Ctx<'_>, note: &str) -> GoalStatus {
        let (target, victim) = (self.target, self.victim);
        let now = ctx.now_ms;
        ctx.combat.disengage();
        let mem = ctx.state.pvp_mut();
        mem.shun(target, now + fight::SHUN_MS);
        mem.gave_up += 1;
        fight::start_flee(ctx, target, victim);
        self.fail(ctx, FailReason::Other("gave up"), note, Outcome::GaveUp)
    }

    // ---- the pack ------------------------------------------------------------------------------

    fn begin_collect(
        &mut self,
        ctx: &mut Ctx<'_>,
        order: Option<&PartyOrder>,
        why: &str,
    ) -> GoalStatus {
        let now = ctx.now_ms;
        ctx.combat.disengage();
        if self.had_pack {
            ctx.log
                .say(format!("gank: {why}; the bot carries a pack already"));
            self.outcome = Outcome::Collected;
            return GoalStatus::Done;
        }
        let turn = order
            .and_then(|o| o.members.iter().position(|&b| b == ctx.persona.id))
            .unwrap_or(0) as u64;
        self.job = CollectJob::new(now, turn * TURN_MS);
        ctx.log.say(format!("gank: {why}; going for the pack"));
        self.enter(Phase::Collect, now);
        GoalStatus::Running
    }

    fn collect(&mut self, ctx: &mut Ctx<'_>, _order: Option<&PartyOrder>) -> GoalStatus {
        let now = ctx.now_ms;
        let me = ctx.state.me.pos;
        let around = self.victim;
        let pack = ctx
            .state
            .doodads
            .iter()
            .filter(|(_, d)| ctx.world.is_pack_doodad(d.template))
            .map(|(&obj, d)| (obj, d.template, d.pos, dist2d(d.pos, around)))
            .filter(|t| t.3 <= PACK_NEAR_M)
            .min_by(|a, b| a.3.total_cmp(&b.3));
        let view = CollectView {
            now_ms: now,
            doodad: pack.map(|(obj, _, pos, _)| (obj, dist2d(pos, me))),
            carrying: carried_pack_of(ctx).is_some(),
        };
        match self.job.step(&view) {
            CollectStep::Collected => {
                ctx.log.say(format!(
                    "gank: took the pack of {} ({} uses)",
                    self.name,
                    self.job.casts()
                ));
                self.outcome = Outcome::Collected;
                GoalStatus::Done
            }
            CollectStep::Nothing(why) => {
                ctx.log.say(format!("gank: {why}"));
                self.outcome = Outcome::Nothing;
                GoalStatus::Done
            }
            CollectStep::Timeout => self.fail(
                ctx,
                FailReason::Timeout,
                "could not take the pack",
                Outcome::Nothing,
            ),
            CollectStep::Wait => {
                if ctx.motion.is_moving() {
                    ctx.motion.stop();
                }
                GoalStatus::Running
            }
            CollectStep::Walk(_) => {
                if let Some((_, _, pos, _)) = pack {
                    if self.walker.step(ctx, [pos[0], pos[1]], COLLECT_STOP_M) == Walk::Stuck {
                        return self.fail(
                            ctx,
                            FailReason::Stuck,
                            "cannot reach the pack",
                            Outcome::Nothing,
                        );
                    }
                }
                GoalStatus::Running
            }
            CollectStep::Cast(obj) => {
                let Some((_, template, pos, _)) = pack else {
                    return GoalStatus::Running;
                };
                if ctx.motion.is_moving() {
                    ctx.motion.stop();
                }
                ctx.motion.face(pos[0], pos[1]);
                let skill = match ctx.world.doodad_use_skill(template) {
                    0 => COLLECT_SKILL,
                    s => s,
                };
                let me_obj = ctx.state.me.obj;
                ctx.send(cs::start_skill_doodad(skill, me_obj, obj));
                ctx.log
                    .debug(format!("gank: using pack doodad {obj} (skill {skill})"));
                GoalStatus::Running
            }
        }
    }

    // ---- the end -------------------------------------------------------------------------------

    /// Ends the goal's business: stops, leaves the fight, withdraws the standby offer and sets the
    /// cooldown. Runs once.
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
        ctx.coord.cancel(ctx.persona.id);
        if ctx.state.me.dead {
            self.outcome = Outcome::Died;
        }
        let (lo, hi) = self.outcome.cooldown_ms();
        let until = ctx.now_ms + crate::bot::goals::jitter_ms(ctx.rng, lo, hi);
        let mem = ctx.state.pvp_mut();
        mem.gank_next_ms = mem.gank_next_ms.max(until);
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

    fn view(now_ms: u64, doodad: Option<(u32, f32)>, carrying: bool) -> CollectView {
        CollectView {
            now_ms,
            doodad,
            carrying,
        }
    }

    #[test]
    fn a_fair_fight_is_taken_and_a_stronger_group_is_not() {
        assert_eq!(verdict(Odds::Fair, false, 100, 35), Verdict::Attack);
        assert_eq!(verdict(Odds::Fair, true, 100, 35), Verdict::Attack);
        // a stronger target waits for a squad, and a squad that is still too weak leaves it
        assert_eq!(verdict(Odds::NeedsHelp, false, 100, 35), Verdict::Wait);
        assert_eq!(verdict(Odds::NeedsHelp, true, 100, 35), Verdict::Skip);
        assert_eq!(verdict(Odds::Hopeless, false, 100, 35), Verdict::Skip);
        assert_eq!(verdict(Odds::Hopeless, true, 100, 35), Verdict::Skip);
    }

    #[test]
    fn a_hurt_bot_starts_no_fight() {
        // (65 percent is the least a bot wants when it retreats at 35)
        assert_eq!(start_hp(35), 65);
        assert_eq!(verdict(Odds::Fair, false, 64, 35), Verdict::Rest);
        assert_eq!(verdict(Odds::Fair, false, 65, 35), Verdict::Attack);
        // the bar stays between 60 and 95 whatever the retreat line says
        assert_eq!(start_hp(0), 60);
        assert_eq!(start_hp(90), 95);
        assert_eq!(start_hp(255), 95);
    }

    #[test]
    fn the_pack_is_taken_within_three_metres() {
        let mut job = CollectJob::new(1_000, 0);
        // not yet dropped
        assert_eq!(job.step(&view(1_200, None, false)), CollectStep::Wait);
        // dropped, far: walk
        assert_eq!(
            job.step(&view(2_000, Some((7, 9.0)), false)),
            CollectStep::Walk(7)
        );
        assert_eq!(
            job.step(&view(2_250, Some((7, 3.5)), false)),
            CollectStep::Walk(7)
        );
        // close: use it, then wait for the cooldown, then use it again
        assert_eq!(
            job.step(&view(2_500, Some((7, 2.5)), false)),
            CollectStep::Cast(7)
        );
        assert_eq!(
            job.step(&view(3_000, Some((7, 2.5)), false)),
            CollectStep::Wait
        );
        assert_eq!(
            job.step(&view(5_100, Some((7, 2.5)), false)),
            CollectStep::Cast(7)
        );
        assert_eq!(job.casts(), 2);
        // the pack arrived in the bag
        assert_eq!(job.step(&view(5_400, None, true)), CollectStep::Collected);
    }

    #[test]
    fn a_pack_somebody_else_took_is_not_waited_for() {
        let mut job = CollectJob::new(0, 0);
        assert_eq!(
            job.step(&view(500, Some((3, 1.0)), false)),
            CollectStep::Cast(3)
        );
        assert_eq!(
            job.step(&view(1_000, None, false)),
            CollectStep::Nothing("somebody else took the pack")
        );
    }

    #[test]
    fn no_doodad_at_all_means_nothing_was_dropped() {
        let mut job = CollectJob::new(10_000, 0);
        assert_eq!(job.step(&view(12_000, None, false)), CollectStep::Wait);
        assert_eq!(
            job.step(&view(10_000 + APPEAR_MS + 1, None, false)),
            CollectStep::Nothing("no pack dropped")
        );
    }

    #[test]
    fn the_collecting_gives_up() {
        // too many uses without a pack
        let mut job = CollectJob::new(0, 0);
        let mut now = 0;
        for _ in 0..MAX_CASTS {
            assert_eq!(
                job.step(&view(now, Some((1, 1.0)), false)),
                CollectStep::Cast(1)
            );
            now += CAST_GAP_MS;
        }
        assert_eq!(
            job.step(&view(now, Some((1, 1.0)), false)),
            CollectStep::Timeout
        );
        // or too long a wait
        let mut job = CollectJob::new(0, 0);
        assert_eq!(
            job.step(&view(COLLECT_MAX_MS + 1, Some((1, 50.0)), false)),
            CollectStep::Timeout
        );
        // having the pack beats everything, a timeout included
        assert_eq!(
            job.step(&view(COLLECT_MAX_MS * 3, None, true)),
            CollectStep::Collected
        );
    }

    #[test]
    fn squad_members_take_turns_at_the_doodad() {
        let mut first = CollectJob::new(0, 0);
        let mut second = CollectJob::new(0, TURN_MS);
        let v = view(500, Some((4, 1.0)), false);
        assert_eq!(first.step(&v), CollectStep::Cast(4));
        assert_eq!(second.step(&v), CollectStep::Wait);
        assert_eq!(
            second.step(&view(TURN_MS, Some((4, 1.0)), false)),
            CollectStep::Cast(4)
        );
    }

    #[test]
    fn the_patrol_stays_near_the_post() {
        let post = Post {
            pos: [100.0, 200.0],
            dir: [0.0, 1.0],
            group: 0,
            price: 0,
            score: 0.0,
        };
        assert_eq!(patrol_point(&post, 0.0), [100.0, 200.0]);
        assert_eq!(patrol_point(&post, 1.0), [100.0, 200.0 + PATROL_SPAN_M]);
        assert_eq!(patrol_point(&post, -1.0), [100.0, 200.0 - PATROL_SPAN_M]);
        assert_eq!(patrol_point(&post, 9.0), patrol_point(&post, 1.0));
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
        // (the weight table of the brain leaves the goal to its own score)
        assert_eq!(base_weight(&BehaviorCfg::default(), &fighter), 0.0);
    }

    #[test]
    fn a_fresh_goal_shows_as_idle_and_owns_no_combat() {
        let g = Gank::new();
        assert_eq!(g.activity(false, false), Activity::Idle);
        assert_eq!(g.activity(true, false), Activity::Traveling);
        assert_eq!(g.activity(true, true), Activity::Fighting);
        assert!(!g.owns_combat());
        assert!(g.order().is_none());
        let mut text = String::new();
        g.describe(&mut text);
        assert_eq!(text, "gank");
    }

    #[test]
    fn a_squad_goal_keeps_its_order() {
        let order = PartyOrder {
            party_id: 3,
            leader: 1,
            members: [1, 2].into_iter().collect(),
            roles: Default::default(),
            purpose: PartyPurpose::Ambush { x: 10, y: 20 },
        };
        let g = Gank::with_order(Some(Box::new(order.clone())));
        assert_eq!(g.order(), Some(&order));
    }

    #[test]
    fn the_cooldowns_grow_with_the_trouble() {
        let low = |o: Outcome| o.cooldown_ms().0;
        assert!(low(Outcome::Collected) < low(Outcome::Nothing));
        assert!(low(Outcome::Nothing) < low(Outcome::GaveUp));
        assert!(low(Outcome::GaveUp) < low(Outcome::Died));
        for o in [
            Outcome::Nothing,
            Outcome::NoPost,
            Outcome::Collected,
            Outcome::GaveUp,
            Outcome::Died,
        ] {
            let (lo, hi) = o.cooldown_ms();
            assert!(lo < hi);
        }
    }
}
