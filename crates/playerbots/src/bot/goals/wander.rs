//! Wander: stroll around the place the bot stands in, with pauses.
//!
//! The bot picks a walkable point 10 to 60 metres from where it started the goal, walks there
//! (mostly running, sometimes walking), waits a few seconds and repeats for one to three minutes.
//! It is the fallback every bot can do anywhere, and what makes the map look alive.
//!
//! Started with a place ([`GoalArgs::dest`], from the LLM director or the agent API's `act`), the
//! bot first travels there ([`Ctx::travel`] in legs, so a long trip may go by portal or GM
//! `/move` when those are on) and strolls around that place instead. A place in a zone at war that
//! the bot keeps out of is refused, and a trip that gets stuck or takes longer than
//! [`TRAVEL_MAX_MS`] fails the goal.

use std::fmt::Write;

use crate::bot::cmd::GoalArgs;
use crate::bot::ctx::Ctx;
use crate::bot::manner;
use crate::bot::motion::{MotionStatus, Pace};
use crate::bot::pvp::walk::{Walk, Walker};

use super::{jitter_ms, random_point, FailReason, GoalStatus};

/// Shortest and longest leg in metres.
const LEG_MIN_M: f32 = 10.0;
const LEG_MAX_M: f32 = 60.0;
/// Total time of the goal in milliseconds.
const TOTAL_MS: (u64, u64) = (60_000, 180_000);
/// Pause between two legs in milliseconds.
const PAUSE_MS: (u64, u64) = (2_000, 10_000);
/// Chance that a leg is walked instead of run.
const WALK_CHANCE: f32 = 0.3;
/// Failed legs in a row after which the goal gives up.
const MAX_FAILS: u8 = 3;
/// The bot counts as there within this many metres of the place it was sent to.
const ARRIVE_M: f32 = 8.0;
/// The longest trip to the place it was sent to, in milliseconds.
pub const TRAVEL_MAX_MS: u64 = 20 * 60_000;

/// A friendly market reachable on an ordinary outing.
pub fn settlement_market(ctx: &Ctx<'_>) -> Option<[f32; 2]> {
    if !ctx.cfg.behavior.settlement_visits || ctx.state.place.in_instance() {
        return None;
    }
    let [x, y, z] = ctx.pos();
    let faction = ctx.state.me.faction;
    let market = ctx
        .world
        .nearest_auctioneer(aa_world::Vec3::new(x, y, z), 850.0, |a| {
            let p = a.pos;
            p.dist2d(aa_world::Vec3::new(x, y, z)) < 25.0
                || ctx.in_war_zone(p.x, p.y)
                || !ctx.world.npc(a.template).is_some_and(|npc| {
                    ctx.world.relation(faction, npc.faction_id) != aa_world::Relation::Hostile
                })
                || super::quest::progress::cut_off(ctx.world, (x, y), (p.x, p.y))
                || super::quest::progress::deadly(
                    ctx.world,
                    faction,
                    ctx.state.me.level,
                    (x, y),
                    (p.x, p.y),
                )
        })?;
    Some([market.pos.x, market.pos.y])
}

/// A configured or authored place to linger within the bot's local area.
pub fn settlement_hangout(ctx: &Ctx<'_>) -> Option<[f32; 2]> {
    if !ctx.cfg.behavior.settlement_visits || ctx.state.place.in_instance() {
        return None;
    }
    let [x, y, z] = ctx.pos();
    let faction = ctx.state.me.faction;
    let mut best = None;
    let mut consider = |[a, b]: [f32; 2]| {
        let distance = (a - x).hypot(b - y);
        if (15.0..=850.0).contains(&distance)
            && best.is_none_or(|(nearer, _)| distance < nearer)
            && !ctx.in_war_zone(a, b)
            && super::walkable(ctx.world, a, b)
            && !super::quest::progress::cut_off(ctx.world, (x, y), (a, b))
            && !super::quest::progress::deadly(
                ctx.world,
                faction,
                ctx.state.me.level,
                (x, y),
                (a, b),
            )
        {
            best = Some((distance, [a, b]));
        }
    };
    for &point in &ctx.cfg.behavior.settlement_hangouts {
        consider(point);
    }
    // Look for actual innkeepers and quiet furnishings in the authored district.
    let here = aa_world::Vec3::new(x, y, z);
    ctx.world.npc_spawns_within(here, 425.0, |template, pos| {
        if ctx.world.npc(template).is_some_and(|npc| {
            npc.name.contains("Innkeeper")
                && ctx.world.relation(faction, npc.faction_id) != aa_world::Relation::Hostile
        }) {
            consider([pos.x, pos.y]);
        }
    });
    ctx.world
        .doodad_spawns_within(here, 225.0, |template, pos| {
            if ctx.world.doodad(template).is_some_and(|doodad| {
                matches!(
                    doodad.name.as_ref(),
                    "Campfire" | "Small Campfire" | "Long Wood Bench" | "Wooden Bench"
                )
            }) {
                consider([pos.x, pos.y]);
            }
        });
    best.map(|(_, at)| at)
}

/// Strolling around. See the module docs.
#[derive(Debug, Default, Clone)]
pub struct Wander {
    home: [f32; 3],
    end_ms: u64,
    pause_until_ms: u64,
    walking: bool,
    fails: u8,
    dest: (f32, f32),
    /// The place to go to first (`GoalArgs::dest`); `None` once the bot is there, or when it was
    /// sent nowhere.
    target: Option<[f32; 2]>,
    /// When the trip to `target` is given up.
    travel_until_ms: u64,
    walker: Walker,
    lingering: bool,
    rest_at_target: bool,
}

impl Wander {
    /// A fresh goal started with `args`: `GoalArgs::dest` is a place to travel to and stroll
    /// around (a point that is not a pair of finite numbers is ignored).
    pub fn with_args(args: Option<Box<GoalArgs>>) -> Wander {
        Wander {
            target: args
                .and_then(|a| a.dest)
                .filter(|d| d.iter().all(|v| v.is_finite())),
            ..Wander::default()
        }
    }

    /// The place the bot is on its way to, if any.
    pub fn target(&self) -> Option<[f32; 2]> {
        self.target
    }

    /// Called once when the brain picks the goal.
    pub fn start(&mut self, ctx: &mut Ctx<'_>) {
        self.rest_at_target = false;
        if self.target.is_none() && ctx.cfg.behavior.settlement_visits && ctx.rng.u8(0..4) == 0 {
            let hangout = settlement_hangout(ctx);
            self.rest_at_target = hangout.is_some();
            self.target = hangout.or_else(|| settlement_market(ctx));
        } else if let Some(target) = self.target.filter(|_| ctx.cfg.behavior.settlement_visits) {
            self.rest_at_target = ctx.cfg.behavior.settlement_hangouts.contains(&target)
                || settlement_hangout(ctx) == Some(target);
        }
        self.home = ctx.pos();
        self.end_ms = ctx.now_ms + jitter_ms(ctx.rng, TOTAL_MS.0, TOTAL_MS.1);
        self.pause_until_ms = ctx.now_ms + jitter_ms(ctx.rng, 500, 3_000);
        self.walking = false;
        self.fails = 0;
        self.travel_until_ms = ctx.now_ms + TRAVEL_MAX_MS;
        self.walker.reset();
    }

    /// One tick.
    pub fn tick(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let now = ctx.now_ms;
        if let Some(t) = self.target {
            return self.travel(ctx, t);
        }
        if self.walking {
            match ctx.motion.status() {
                MotionStatus::Moving => return GoalStatus::Running,
                MotionStatus::Failed(_) => {
                    self.walking = false;
                    self.fails += 1;
                    self.pause_until_ms = now + 1_000;
                }
                MotionStatus::Arrived | MotionStatus::Idle => {
                    self.walking = false;
                    self.fails = 0;
                    self.pause_until_ms = now + jitter_ms(ctx.rng, PAUSE_MS.0, PAUSE_MS.1);
                }
            }
        }
        if self.fails >= MAX_FAILS {
            return GoalStatus::Failed(FailReason::Stuck);
        }
        if now >= self.end_ms {
            return GoalStatus::Done;
        }
        if now < self.pause_until_ms {
            if self.lingering {
                manner::rest_tick(ctx);
                return GoalStatus::Running;
            }
            // (in front of a player the bot looks around while it pauses)
            manner::wander_pause(ctx);
            return GoalStatus::Running;
        }
        if self.lingering {
            manner::rest_end(ctx);
            self.lingering = false;
        }
        match random_point(ctx.world, ctx.rng, self.home, LEG_MIN_M, LEG_MAX_M, 8)
            .filter(|&(x, y)| !ctx.in_war_zone(x, y))
        {
            Some((x, y)) => {
                let pace = if ctx.rng.f32() < WALK_CHANCE {
                    Pace::Walk
                } else {
                    Pace::Run
                };
                self.dest = (x, y);
                ctx.motion.go_to(x, y, 1.0, pace, now);
                self.walking = true;
            }
            None => {
                // Boxed in by water or the edge of the map: count it as a failed leg.
                self.fails += 1;
                self.pause_until_ms = now + 1_000;
            }
        }
        GoalStatus::Running
    }

    /// One tick of the trip to `t`; on arrival the stroll starts around it.
    fn travel(&mut self, ctx: &mut Ctx<'_>, t: [f32; 2]) -> GoalStatus {
        let now = ctx.now_ms;
        if ctx.in_war_zone(t[0], t[1]) {
            return GoalStatus::Failed(FailReason::Other("the place is in a zone at war"));
        }
        if now >= self.travel_until_ms {
            return GoalStatus::Failed(FailReason::Timeout);
        }
        match self.walker.step(ctx, t, ARRIVE_M) {
            Walk::Going => GoalStatus::Running,
            Walk::Stuck => GoalStatus::Failed(FailReason::Stuck),
            Walk::There => {
                // (the stroll's clock starts on arrival)
                self.target = None;
                ctx.motion.face(t[0], t[1]);
                self.home = ctx.pos();
                self.end_ms = now + jitter_ms(ctx.rng, TOTAL_MS.0, TOTAL_MS.1);
                self.lingering = self.rest_at_target;
                self.pause_until_ms = if self.lingering {
                    manner::rest_begin(ctx);
                    now + jitter_ms(ctx.rng, 30_000, 55_000)
                } else {
                    now + jitter_ms(ctx.rng, PAUSE_MS.0, PAUSE_MS.1)
                };
                GoalStatus::Running
            }
        }
    }

    /// Called when the goal ends.
    pub fn stop(&mut self, ctx: &mut Ctx<'_>) {
        if self.lingering {
            manner::rest_end(ctx);
        }
        if self.walking || self.target.is_some() {
            ctx.motion.stop();
            self.walking = false;
        }
    }

    /// Appends what the bot is doing.
    pub fn describe(&self, out: &mut String) {
        if let Some(t) = self.target {
            let _ = write!(out, "heading for {:.0}, {:.0}", t[0], t[1]);
        } else if self.walking {
            let _ = write!(out, "strolling to {:.0}, {:.0}", self.dest.0, self.dest.1);
        } else {
            out.push_str("looking around");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_place_to_go_to_comes_from_the_arguments() {
        let sent = Wander::with_args(Some(Box::new(GoalArgs {
            dest: Some([11_380.0, 11_900.0]),
            zone: Some(4),
            ..GoalArgs::default()
        })));
        assert_eq!(sent.target(), Some([11_380.0, 11_900.0]));
        let mut text = String::new();
        sent.describe(&mut text);
        assert_eq!(text, "heading for 11380, 11900");

        // no place, or nonsense, is the plain stroll
        for args in [
            None,
            Some(Box::new(GoalArgs::default())),
            Some(Box::new(GoalArgs {
                dest: Some([f32::NAN, 1.0]),
                ..GoalArgs::default()
            })),
            Some(Box::new(GoalArgs {
                dest: Some([1.0, f32::INFINITY]),
                ..GoalArgs::default()
            })),
        ] {
            let w = Wander::with_args(args);
            assert_eq!(w.target(), None);
            let mut text = String::new();
            w.describe(&mut text);
            assert_eq!(text, "looking around");
        }
    }
}
