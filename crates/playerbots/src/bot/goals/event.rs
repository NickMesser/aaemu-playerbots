//! Event: take part in a world event, a rift (tower defence) of the server.
//!
//! The server runs a rift in waves: controller NPCs summon monsters that march on a defended
//! target, and the next wave begins when the monsters of this one are dead (anybody's kills count).
//! The bots help: when an event runs, a bot that is high enough, healthy and not too far away
//! takes a slot on the hub's [`EventBoard`](crate::events::EventBoard) (which keeps the crowd
//! small, see the board's docs), walks to the defended target, stays within [`HOLD_M`] of it and
//! kills the monsters the current wave waits for, each bot taking one nobody else has claimed.
//! It leaves when the event ends, when its hit points fall below [`LEAVE_HP_PCT`], after three
//! deaths or when nothing has come to fight for [`plan::IDLE_LEAVE_MS`] (longer in a wave with a timer).
//!
//! How a bot gets into the goal:
//!
//! * [`duty`]: called by the brain on every tick. With no event running it is one atomic read.
//!   Otherwise a bot that is between small things (nothing, wandering, resting, grinding,
//!   questing) and passes [`offer`] starts the goal at once, so a rift pulls bots out of their
//!   quests instead of waiting for the end of a three-minute grind.
//! * [`score`]: the same test when the brain draws a goal.
//!
//! [`offer`] is the whole eligibility: `[events] enabled`, a running event whose monsters the bot
//! can fight (level), hit points, not in an instance, not a war and not in a zone group at war
//! (`avoid_war_zones`), a way short enough to go, the join delay over and a free slot.
//!
//! The decisions are in [`plan`] (who may join, where to stand) and [`pick`] (which monster); this
//! file is the glue to the tick context.

pub mod pick;
pub mod plan;

use std::fmt::Write;

use aa_proto::sc_event::EventId;
use aa_world::event::EventDef;
use aa_world::Vec3;
use pb_web::Activity;
use smallvec::SmallVec;

use crate::bot::cmd::GoalKind;
use crate::bot::combat::{attackable, CombatStatus};
use crate::bot::ctx::Ctx;
use crate::bot::motion::{MotionStatus, MoveFailure, Pace};
use crate::bot::state::dist2d;
use crate::config::BehaviorCfg;
use crate::contest::unit_key;
use crate::events::{Caps, Join, Leave, View};
use crate::roster::Temperament;

use super::{FailReason, GoalStatus};
use pick::Cand;
use plan::{Who, HOLD_M, LEAVE_HP_PCT};

/// What an eligible bot's score is: far above the everyday goals, so a bot that may join does.
const SCORE: f32 = 12.0;
/// The bot tells the board it stands at the target this often.
const OBSERVE_GAP_MS: u64 = 1_000;
/// A walk is re-issued at most this often.
const GO_GAP_MS: u64 = 2_500;
/// A trip cut off by the controller's time limit is continued this many times while it gains.
const MAX_LEGS: u8 = 12;
/// A leg must bring the bot this much closer to count as progress.
const LEG_MIN_GAIN_M: f32 = 30.0;
/// A monster the controller gave up is left alone this long.
const IGNORE_MS: u64 = 90_000;
/// Candidates looked at per pick.
const MAX_CANDS: usize = 48;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// On the way to the target.
    Go,
    /// At the target, fighting what comes.
    Hold,
}

/// What a bot in the goal remembers. Allocated when it joins.
#[derive(Debug)]
struct Run {
    id: EventId,
    name: Box<str>,
    phase: Phase,
    /// The defended target.
    rally: [f32; 3],
    /// The bot's own place around it.
    spot: [f32; 2],
    step: u32,
    dist_m: f32,
    deadline_ms: u64,
    go_ms: u64,
    legs: u8,
    leg_from: f32,
    walking: bool,
    kills: u16,
    target: Option<u32>,
    idle_since_ms: u64,
    last_observe_ms: u64,
    /// Monsters the controller gave up: `(object, until)`.
    ignore: SmallVec<[(u32, u64); 4]>,
    /// The board knows the bot left.
    left: bool,
}

/// Join a world event. See the module docs.
#[derive(Debug, Default)]
pub struct Event {
    run: Option<Box<Run>>,
}

/// An event a bot may join right now.
#[derive(Debug, Clone, Copy)]
struct Offer {
    view: View,
    rally: [f32; 3],
    dist_m: f32,
}

impl Event {
    /// A fresh goal (the brain starts it with [`Goal::new`](super::Goal::new)).
    pub fn new() -> Event {
        Event { run: None }
    }

    /// Called once when the brain picks the goal: takes a slot.
    pub fn start(&mut self, ctx: &mut Ctx<'_>) {
        self.run = None;
        let Some(o) = offer(ctx, false) else {
            return;
        };
        let Some(def) = ctx.world.events().get(o.view.id.def) else {
            return;
        };
        let caps = caps_of(ctx, def);
        let bot = ctx.persona.id;
        if !matches!(
            ctx.hub.events.join(o.view.id, bot, &caps),
            Join::Joined | Join::Already
        ) {
            return;
        }
        let now = ctx.now_ms;
        ctx.log.say(format!(
            "event: joining the {} (wave {}, {:.0} m away)",
            def.name, o.view.step, o.dist_m
        ));
        self.run = Some(Box::new(Run {
            id: o.view.id,
            name: def.name.clone(),
            phase: Phase::Go,
            rally: o.rally,
            spot: plan::spot(o.rally, bot, o.view.id.def),
            step: o.view.step,
            dist_m: o.dist_m,
            deadline_ms: now + plan::trip_limit_ms(o.dist_m),
            go_ms: 0,
            legs: 0,
            leg_from: o.dist_m,
            walking: false,
            kills: 0,
            target: None,
            idle_since_ms: now,
            last_observe_ms: 0,
            ignore: SmallVec::new(),
            left: false,
        }));
    }

    /// One tick; `combat` is the controller's verdict of this tick.
    pub fn tick(&mut self, ctx: &mut Ctx<'_>, combat: CombatStatus) -> GoalStatus {
        let Some(run) = self.run.as_deref_mut() else {
            // (no event to join, or no free slot)
            return GoalStatus::Failed(FailReason::NoTarget);
        };
        let now = ctx.now_ms;
        let bot = ctx.persona.id;

        // the event, and the lease on the bot's slot
        let Some(view) = ctx.hub.events.view(run.id) else {
            return finish(run, ctx, Leave::Done, "the event ended");
        };
        let Some(def) = ctx.world.events().get(run.id.def) else {
            return finish(run, ctx, Leave::Done, "no data for the event");
        };
        if !ctx.hub.events.renew(run.id, bot) {
            let caps = caps_of(ctx, def);
            if !matches!(
                ctx.hub.events.join(run.id, bot, &caps),
                Join::Joined | Join::Already
            ) {
                return finish(run, ctx, Leave::Done, "the slot is gone");
            }
        }
        if ctx.state.me.hp_pct() < LEAVE_HP_PCT {
            return finish(run, ctx, Leave::Hurt, "hurt");
        }
        if ctx.cfg.events.avoid_war_zones && ctx.state.at_war(run.id.zone_group) {
            return finish(run, ctx, Leave::Done, "the zone is at war");
        }

        // what the fight came to
        match combat {
            CombatStatus::TargetDead { obj } => {
                run.kills = run.kills.saturating_add(1);
                run.idle_since_ms = now;
                release(run, ctx, obj);
            }
            CombatStatus::Lost => {
                if let Some(obj) = run.target {
                    run.ignore_for(obj, now + IGNORE_MS);
                    release(run, ctx, obj);
                }
            }
            _ => {}
        }
        if let Some(obj) = run.target {
            ctx.hub.contest.claim(unit_key(obj), bot);
        }

        // where the target is
        if run.step != view.step {
            // a new wave: the wait starts again
            run.idle_since_ms = now;
        }
        run.step = view.step;
        if let Some(rally) = live_rally(ctx, def, &view) {
            // (the target may walk: the own spot follows once it is out of the ring)
            let off = ((run.spot[0] - rally[0]).powi(2) + (run.spot[1] - rally[1]).powi(2)).sqrt();
            if off > plan::SPOT_M.1 + 4.0 {
                run.spot = plan::spot(rally, bot, run.id.def);
            }
            run.rally = rally;
        }
        run.dist_m = dist2d(ctx.state.me.pos, run.rally);
        let near = run.dist_m <= HOLD_M;
        if now.saturating_sub(run.last_observe_ms) >= OBSERVE_GAP_MS {
            run.last_observe_ms = now;
            ctx.hub
                .events
                .observe(run.id, near && run.phase == Phase::Hold);
        }

        if ctx.combat.busy() {
            // fighting (the target or whatever attacks the bot), or looting
            if ctx.combat.target().is_some_and(|t| Some(t) == run.target) {
                run.idle_since_ms = now;
            }
            return GoalStatus::Running;
        }
        run.target = None;

        match run.phase {
            Phase::Go => {
                if run.dist_m <= HOLD_M * 0.55 {
                    if ctx.motion.is_moving() {
                        ctx.motion.stop();
                    }
                    run.walking = false;
                    run.phase = Phase::Hold;
                    run.idle_since_ms = now;
                    ctx.log
                        .say(format!("event: at the {} (wave {})", run.name, run.step));
                    return GoalStatus::Running;
                }
                if now >= run.deadline_ms {
                    return GoalStatus::Failed(FailReason::Timeout);
                }
                let spot = run.spot;
                match walk(run, ctx, spot[0], spot[1], 3.0) {
                    Walk::Failed => GoalStatus::Failed(FailReason::Stuck),
                    Walk::Arrived => {
                        run.phase = Phase::Hold;
                        run.idle_since_ms = now;
                        GoalStatus::Running
                    }
                    Walk::Moving => GoalStatus::Running,
                }
            }
            Phase::Hold => {
                if let Some(obj) = pick_target(run, ctx, def, &view) {
                    if ctx.motion.is_moving() {
                        ctx.motion.stop();
                    }
                    run.walking = false;
                    run.target = Some(obj);
                    ctx.hub.contest.claim(unit_key(obj), bot);
                    ctx.combat.engage(obj);
                    run.idle_since_ms = now;
                    return GoalStatus::Running;
                }
                if now.saturating_sub(run.idle_since_ms) >= plan::idle_limit_ms(def.step(view.step))
                {
                    return finish(run, ctx, Leave::Idle, "nothing came");
                }
                if run.dist_m > HOLD_M {
                    // a fight carried the bot off: back to the target
                    run.phase = Phase::Go;
                    run.deadline_ms = now + plan::trip_limit_ms(run.dist_m);
                    run.go_ms = 0;
                    run.legs = 0;
                    run.leg_from = run.dist_m;
                    return GoalStatus::Running;
                }
                // back to the own spot after a fight
                let d_spot = dist2d(ctx.state.me.pos, [run.spot[0], run.spot[1], 0.0]);
                if d_spot > 12.0 && !ctx.motion.is_moving() {
                    ctx.motion
                        .go_to(run.spot[0], run.spot[1], 3.0, Pace::Run, now);
                }
                GoalStatus::Running
            }
        }
    }

    /// Called when the goal ends, fails or is interrupted: stops the fight and the walk and gives
    /// the slot back.
    pub fn stop(&mut self, ctx: &mut Ctx<'_>) {
        let Some(run) = self.run.as_deref_mut() else {
            return;
        };
        ctx.combat.disengage();
        if ctx.motion.is_moving() {
            ctx.motion.stop();
        }
        if let Some(obj) = run.target.take() {
            ctx.hub.contest.release(unit_key(obj), ctx.persona.id);
        }
        if !run.left {
            run.left = true;
            let why = if ctx.state.me.dead {
                Leave::Died
            } else {
                Leave::Done
            };
            ctx.hub.events.leave(run.id, ctx.persona.id, why);
            if why == Leave::Died {
                ctx.log.say(format!("event: died at the {}", run.name));
            }
        }
    }

    /// Whether the goal drives the combat controller itself (see [`super::Goal::owns_combat`]):
    /// once the bot has joined.
    pub fn owns_combat(&self) -> bool {
        self.run.is_some()
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
        let Some(run) = self.run.as_deref() else {
            out.push_str("event (no event to join)");
            return;
        };
        let _ = match run.phase {
            Phase::Go => write!(
                out,
                "event: going to the {} (wave {}, {:.0} m to go)",
                run.name, run.step, run.dist_m
            ),
            Phase::Hold if run.target.is_some() => write!(
                out,
                "event: fighting at the {} (wave {}), {} kills",
                run.name, run.step, run.kills
            ),
            Phase::Hold => write!(
                out,
                "event: holding the {} (wave {}), {} kills",
                run.name, run.step, run.kills
            ),
        };
    }
}

impl Run {
    fn ignore_for(&mut self, obj: u32, until_ms: u64) {
        if self.ignore.len() >= 4 {
            self.ignore.remove(0);
        }
        self.ignore.push((obj, until_ms));
    }

    fn ignored(&self, obj: u32, now_ms: u64) -> bool {
        self.ignore.iter().any(|i| i.0 == obj && now_ms < i.1)
    }
}

/// Leaves the event: tells the board and ends the goal.
fn finish(run: &mut Run, ctx: &mut Ctx<'_>, why: Leave, note: &str) -> GoalStatus {
    if !run.left {
        run.left = true;
        ctx.hub.events.leave(run.id, ctx.persona.id, why);
    }
    ctx.log.say(format!(
        "event: leaving the {} ({note}), {} kills",
        run.name, run.kills
    ));
    GoalStatus::Done
}

fn release(run: &mut Run, ctx: &Ctx<'_>, obj: u32) {
    ctx.hub.contest.release(unit_key(obj), ctx.persona.id);
    if run.target == Some(obj) {
        run.target = None;
    }
}

enum Walk {
    Moving,
    Arrived,
    Failed,
}

/// Walks to (x, y), stopping `radius` metres short. A trip the controller cut off after its time
/// limit is continued while every leg gets closer; a far trip may go by portal (see
/// [`Ctx::travel`]).
fn walk(run: &mut Run, ctx: &mut Ctx<'_>, x: f32, y: f32, radius: f32) -> Walk {
    let now = ctx.now_ms;
    let me = ctx.state.me.pos;
    let left = dist2d(me, [x, y, 0.0]);
    if left <= radius + 0.6 {
        if ctx.motion.is_moving() {
            ctx.motion.stop();
        }
        run.walking = false;
        return Walk::Arrived;
    }
    if run.walking {
        match ctx.motion.status() {
            MotionStatus::Moving => {
                ctx.combat.note_travelling(now);
                return Walk::Moving;
            }
            MotionStatus::Failed(MoveFailure::Timeout)
                if run.legs < MAX_LEGS && run.leg_from - left >= LEG_MIN_GAIN_M =>
            {
                run.legs += 1;
                run.leg_from = left;
                run.go_ms = 0;
            }
            MotionStatus::Failed(_) => {
                run.walking = false;
                return Walk::Failed;
            }
            _ => {}
        }
    }
    if run.walking && now.saturating_sub(run.go_ms) < GO_GAP_MS {
        return Walk::Moving;
    }
    ctx.travel(x, y, radius, Pace::Run);
    ctx.combat.note_travelling(now);
    run.walking = true;
    run.go_ms = now;
    Walk::Moving
}

/// The monsters in view that are the event's business, and the one to fight.
fn pick_target(run: &Run, ctx: &Ctx<'_>, def: &EventDef, view: &View) -> Option<u32> {
    let events = ctx.world.events();
    let me = ctx.state.me.pos;
    let (faction, bot, now) = (ctx.state.me.faction, ctx.persona.id, ctx.now_ms);
    let mut cands: SmallVec<[Cand; 16]> = SmallVec::new();
    for (&obj, u) in &ctx.state.units {
        if run.ignored(obj, now) || ctx.state.avoids(obj, u.id, now) {
            continue;
        }
        let d_bot = u.dist2d(me);
        let d_rally = dist2d(u.pos, run.rally);
        if d_bot > plan::VIEW_M || d_rally > plan::ENGAGE_M {
            continue;
        }
        if attackable(ctx.world, faction, u).is_none() {
            continue;
        }
        let Some(class) = pick::classify(events, def, view.step, u.id) else {
            continue;
        };
        cands.push(Cand {
            obj,
            class,
            d_bot,
            d_rally,
            held: ctx.hub.contest.others(unit_key(obj), bot) > 0,
        });
        if cands.len() >= MAX_CANDS {
            break;
        }
    }
    pick::choose(&cands)
}

/// Where the defended target is: the position the server's list names, else the nearest in the
/// data.
fn rally_of(def: &EventDef, view: &View, from: [f32; 3]) -> Option<[f32; 3]> {
    if let Some((_, p)) = view.target {
        return Some(p);
    }
    def.nearest_target(view.id.zone_group, Vec3::new(from[0], from[1], from[2]))
        .map(|p| [p.x, p.y, p.z])
}

/// [`rally_of`], but the defended unit's own position when the bot sees it: the target of a rift
/// walks a path, and the server's list only gives where it stood when the list was sent.
fn live_rally(ctx: &Ctx<'_>, def: &EventDef, view: &View) -> Option<[f32; 3]> {
    if let Some(u) = view.target.and_then(|(obj, _)| ctx.state.unit(obj)) {
        return Some(u.pos);
    }
    rally_of(def, view, ctx.state.me.pos)
}

fn caps_of(ctx: &Ctx<'_>, def: &EventDef) -> Caps {
    let cfg = &ctx.cfg.events;
    Caps {
        per_event: cfg.max_bots_per_event,
        max_share: cfg.max_share,
        min_level: def.min_level,
    }
}

/// The running event the bot may join right now, the one nearest to it. Nothing when the feature
/// is off, no event runs or the bot does not qualify (see [`plan::may_join`]). With `ask` the
/// join delay starts (and the offer is made only once it is over); without, the delay is only
/// looked at.
fn offer(ctx: &Ctx<'_>, ask: bool) -> Option<Offer> {
    let cfg = &ctx.cfg.events;
    let board = &ctx.hub.events;
    if !cfg.enabled || !board.any() {
        return None;
    }
    // (a trade pack carrier does not leave the run for a rift, and neither does a bot that is
    // going back for a pack it dropped)
    if crate::bot::goals::trade_run::pack_busy(ctx) {
        return None;
    }
    let me = &ctx.state.me;
    let place = &ctx.state.place;
    let away = !me.spawned || me.dead || place.in_instance() || place.loading();
    let bot = ctx.persona.id;
    let mut best: Option<Offer> = None;
    for view in board.views() {
        let Some(def) = ctx.world.events().get(view.id.def) else {
            continue;
        };
        let Some(rally) = live_rally(ctx, def, &view) else {
            continue;
        };
        let dist_m = dist2d(me.pos, rally);
        let who = Who {
            level: me.level,
            hp_pct: me.hp_pct(),
            away,
            dist_m,
            carried: ctx.would_teleport(dist_m),
            portals: ctx.cfg.behavior.portals,
        };
        let war = ctx.state.at_war(view.id.zone_group);
        if plan::may_join(cfg, def, &who, war).is_err() {
            continue;
        }
        if best.is_some_and(|b| b.dist_m <= dist_m) {
            continue;
        }
        let caps = caps_of(ctx, def);
        if !matches!(
            board.can_join(view.id, bot, &caps),
            Join::Joined | Join::Already
        ) {
            continue;
        }
        if ask && !board.ready(view.id, bot, cfg.join_delay_secs) {
            continue;
        }
        best = Some(Offer {
            view,
            rally,
            dist_m,
        });
    }
    best
}

/// The goals a bot gives up for an event: the small, restartable ones.
fn preemptible(running: Option<GoalKind>) -> bool {
    matches!(
        running,
        None | Some(GoalKind::Wander | GoalKind::Rest | GoalKind::Grind | GoalKind::Quest)
    )
}

/// Called by the brain on every tick: whether the bot should start the event goal now. With no
/// event running, one atomic read.
pub fn duty(ctx: &mut Ctx<'_>, running: Option<GoalKind>) -> bool {
    if !ctx.hub.events.any() || !preemptible(running) || ctx.combat.busy() {
        return false;
    }
    offer(ctx, true).is_some()
}

/// The goal's weight before the situation is looked at: it has none, the goal scores itself.
pub fn base_weight(_behavior: &BehaviorCfg, _temper: &Temperament) -> f32 {
    0.0
}

/// The score of the goal for this tick: [`SCORE`] for a bot that may join a running event, else 0.
pub fn score(ctx: &Ctx<'_>, _base: f32) -> f32 {
    if offer(ctx, true).is_some() {
        SCORE
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests;
