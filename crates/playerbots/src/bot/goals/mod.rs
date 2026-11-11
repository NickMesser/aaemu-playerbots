//! Goals: what a bot does for a few minutes at a time.
//!
//! A goal is a small state machine the [`Brain`](super::brain::Brain) starts, ticks and stops.
//! The set is closed (an enum, no allocation per goal): [`Wander`], [`Rest`], [`Grind`],
//! [`Quest`], [`Duel`], [`Trade`], [`TradeRun`], [`Housing`], [`Dungeon`], [`Sail`], [`Auction`],
//! [`Market`], [`Guild`], [`Gank`], [`Warzone`], [`Event`], [`Companion`], [`Farm`], [`Explore`]
//! and [`Gear`] (a goal added as a stub reports [`FailReason::NotImplemented`] and is switched off
//! for good by the brain).
//!
//! A goal may be started with [`GoalArgs`] ([`Goal::with_args`]: the LLM director, the web UI's
//! `act` command, the agent API); a goal that takes none ignores them.
//!
//! # Writing a goal
//!
//! * Put the state in the goal struct; keep it small (it lives as long as the bot task).
//! * `start` runs once when the brain picks the goal, `tick` every brain tick (250 ms while the
//!   bot moves or fights, 1.5 s while it stands), `stop` when the goal ends or is interrupted
//!   (stop motion, drop targets).
//! * Everything the goal may touch is in [`Ctx`]: the world data, the bot's state, movement
//!   ([`Ctx::travel`], `ctx.motion`), combat (`ctx.combat.engage`), packets (`ctx.send`,
//!   `ctx.say`, `ctx.gm`), the pairing service (`ctx.coord`) and the hub (map markers).
//! * Return [`GoalStatus::Running`] while busy, [`GoalStatus::Done`] when finished and
//!   [`GoalStatus::Failed`] when it cannot proceed; the brain applies a cooldown to a failed goal.
//! * Do not keep borrows of `ctx` between ticks and do not allocate per tick.

pub mod auction;
pub mod companion;
pub mod duel;
pub mod dungeon;
pub mod event;
pub mod explore;
pub mod farm;
pub mod gank;
pub mod gear;
pub mod grind;
pub mod guild;
pub mod housing;
pub mod market;
pub mod quest;
pub mod rest;
pub mod sail;
pub mod social;
pub mod trade;
pub mod trade_run;
pub mod wander;
pub mod warzone;

use pb_web::Activity;

pub use auction::Auction;
pub use companion::Companion;
pub use duel::Duel;
pub use dungeon::Dungeon;
pub use event::Event;
pub use explore::Explore;
pub use farm::Farm;
pub use gank::Gank;
pub use gear::Gear;
pub use grind::Grind;
pub use guild::Guild;
pub use housing::Housing;
pub use market::Market;
pub use quest::Quest;
pub use rest::Rest;
pub use sail::Sail;
pub use trade::Trade;
pub use trade_run::TradeRun;
pub use wander::Wander;
pub use warzone::Warzone;

use super::cmd::{GoalArgs, GoalKind, PairOrder};
use super::combat::CombatStatus;
use super::ctx::Ctx;
use super::motion::SWIM_THRESHOLD;
use super::party::PartyOrder;

/// Why a goal gave up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailReason {
    /// The goal cannot run in this build; the brain never picks it again.
    NotImplemented,
    /// Nothing to work on (no monster, no quest giver, no partner).
    NoTarget,
    /// The way is blocked.
    Stuck,
    /// It took too long.
    Timeout,
    /// Anything else, with a short reason for the log.
    Other(&'static str),
}

/// What a goal tick came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalStatus {
    /// Keep going.
    Running,
    /// Finished normally.
    Done,
    /// Gave up.
    Failed(FailReason),
}

/// The running goal. See the module docs.
#[derive(Debug)]
pub enum Goal {
    /// Walk around.
    Wander(Wander),
    /// Stand still and recover.
    Rest(Rest),
    /// Kill monsters.
    Grind(Grind),
    /// Do quests.
    Quest(Quest),
    /// Duel another bot.
    Duel(Duel),
    /// Trade with another bot.
    Trade(Trade),
    /// Craft, carry and sell trade packs.
    TradeRun(TradeRun),
    /// Build and use a house.
    Housing(Housing),
    /// Clear an instance dungeon with a party.
    Dungeon(Dungeon),
    /// Summon a boat and sail.
    Sail(Sail),
    /// List goods at the auction house.
    Auction(Auction),
    /// Advertise and sell goods.
    Market(Market),
    /// Found, join and run an expedition.
    Guild(Guild),
    /// Ambush trade-pack carriers.
    Gank(Gank),
    /// Fight in zones at war.
    Warzone(Warzone),
    /// Join a world event.
    Event(Event),
    /// Follow a real player's party.
    Companion(Companion),
    /// Plant and harvest crops.
    Farm(Farm),
    /// See new places.
    Explore(Explore),
    /// Get, equip and upgrade gear.
    Gear(Gear),
}

impl Goal {
    /// A fresh goal of `kind`; `order` is the coordinator's pairing for duels and trades.
    pub fn new(kind: GoalKind, order: Option<PairOrder>) -> Goal {
        Goal::with_party(kind, order, None)
    }

    /// Like [`Goal::new`], with the coordinator's party order for a dungeon (any other goal
    /// ignores it).
    pub fn with_party(
        kind: GoalKind,
        order: Option<PairOrder>,
        party: Option<Box<PartyOrder>>,
    ) -> Goal {
        Goal::with_args(kind, order, party, None)
    }

    /// Like [`Goal::with_party`], with the [`GoalArgs`] somebody started it with (`None`: the bot's
    /// own choice). Only the goals that understand arguments keep them.
    pub fn with_args(
        kind: GoalKind,
        order: Option<PairOrder>,
        party: Option<Box<PartyOrder>>,
        args: Option<Box<GoalArgs>>,
    ) -> Goal {
        match kind {
            GoalKind::Wander => Goal::Wander(Wander::with_args(args)),
            GoalKind::Rest => Goal::Rest(Rest::default()),
            GoalKind::Grind => Goal::Grind(Grind::default()),
            GoalKind::Quest => Goal::Quest(Quest::default()),
            GoalKind::Duel => Goal::Duel(Duel::new(order)),
            GoalKind::Trade => Goal::Trade(Trade::new(order)),
            GoalKind::TradeRun => Goal::TradeRun(TradeRun::default()),
            GoalKind::Housing => Goal::Housing(Housing::default()),
            GoalKind::Dungeon => Goal::Dungeon(Dungeon::new(party)),
            GoalKind::Sail => Goal::Sail(Sail::default()),
            GoalKind::Auction => Goal::Auction(Auction::default()),
            GoalKind::Market => Goal::Market(Market::default()),
            GoalKind::Guild => Goal::Guild(Guild::new(party)),
            GoalKind::Gank => Goal::Gank(Gank::with_order(party)),
            GoalKind::Warzone => Goal::Warzone(Warzone::new()),
            GoalKind::Event => Goal::Event(Event::new()),
            GoalKind::Companion => Goal::Companion(Companion::new()),
            GoalKind::Farm => Goal::Farm(Farm::with_args(args)),
            GoalKind::Explore => Goal::Explore(Explore::new(args)),
            GoalKind::Gear => Goal::Gear(Gear::new(args)),
        }
    }

    /// Which goal this is.
    pub fn kind(&self) -> GoalKind {
        match self {
            Goal::Wander(_) => GoalKind::Wander,
            Goal::Rest(_) => GoalKind::Rest,
            Goal::Grind(_) => GoalKind::Grind,
            Goal::Quest(_) => GoalKind::Quest,
            Goal::Duel(_) => GoalKind::Duel,
            Goal::Trade(_) => GoalKind::Trade,
            Goal::TradeRun(_) => GoalKind::TradeRun,
            Goal::Housing(_) => GoalKind::Housing,
            Goal::Dungeon(_) => GoalKind::Dungeon,
            Goal::Sail(_) => GoalKind::Sail,
            Goal::Auction(_) => GoalKind::Auction,
            Goal::Market(_) => GoalKind::Market,
            Goal::Guild(_) => GoalKind::Guild,
            Goal::Gank(_) => GoalKind::Gank,
            Goal::Warzone(_) => GoalKind::Warzone,
            Goal::Event(_) => GoalKind::Event,
            Goal::Companion(_) => GoalKind::Companion,
            Goal::Farm(_) => GoalKind::Farm,
            Goal::Explore(_) => GoalKind::Explore,
            Goal::Gear(_) => GoalKind::Gear,
        }
    }

    /// Whether the goal drives the combat controller itself. For every other goal the brain lets
    /// the controller defend the bot and pauses the goal while a fight or a loot job is on.
    pub fn owns_combat(&self) -> bool {
        match self {
            Goal::Grind(_) | Goal::Quest(_) | Goal::Duel(_) => true,
            Goal::Dungeon(g) => g.owns_combat(),
            Goal::Sail(g) => g.owns_combat(),
            Goal::Auction(g) => g.owns_combat(),
            Goal::Market(g) => g.owns_combat(),
            Goal::Guild(g) => g.owns_combat(),
            Goal::Gank(g) => g.owns_combat(),
            Goal::Warzone(g) => g.owns_combat(),
            Goal::Event(g) => g.owns_combat(),
            Goal::Companion(g) => g.owns_combat(),
            Goal::Farm(g) => g.owns_combat(),
            Goal::Explore(g) => g.owns_combat(),
            Goal::Gear(g) => g.owns_combat(),
            _ => false,
        }
    }

    /// Called once when the brain picks the goal.
    pub fn start(&mut self, ctx: &mut Ctx<'_>) {
        match self {
            Goal::Wander(g) => g.start(ctx),
            Goal::Rest(g) => g.start(ctx),
            Goal::Grind(g) => g.start(ctx),
            Goal::Quest(g) => g.start(ctx),
            Goal::Duel(g) => g.start(ctx),
            Goal::Trade(g) => g.start(ctx),
            Goal::TradeRun(g) => g.start(ctx),
            Goal::Housing(g) => g.start(ctx),
            Goal::Dungeon(g) => g.start(ctx),
            Goal::Sail(g) => g.start(ctx),
            Goal::Auction(g) => g.start(ctx),
            Goal::Market(g) => g.start(ctx),
            Goal::Guild(g) => g.start(ctx),
            Goal::Gank(g) => g.start(ctx),
            Goal::Warzone(g) => g.start(ctx),
            Goal::Event(g) => g.start(ctx),
            Goal::Companion(g) => g.start(ctx),
            Goal::Farm(g) => g.start(ctx),
            Goal::Explore(g) => g.start(ctx),
            Goal::Gear(g) => g.start(ctx),
        }
    }

    /// One tick; `combat` is the controller's verdict of this tick.
    pub fn tick(&mut self, ctx: &mut Ctx<'_>, combat: CombatStatus) -> GoalStatus {
        let resume = match self {
            Goal::Wander(g) => return g.tick(ctx),
            Goal::Rest(g) => match g.tick(ctx) {
                GoalStatus::Done => g.resumes_run(ctx),
                status => return status,
            },
            Goal::Grind(g) => return g.tick(ctx, combat),
            Goal::Quest(g) => return g.tick(ctx),
            Goal::Duel(g) => return g.tick(ctx),
            Goal::Trade(g) => return g.tick(ctx),
            Goal::TradeRun(g) => return g.tick(ctx),
            Goal::Housing(g) => return g.tick(ctx),
            Goal::Dungeon(g) => return g.tick(ctx),
            Goal::Sail(g) => return g.tick(ctx),
            Goal::Auction(g) => return g.tick(ctx),
            Goal::Market(g) => return g.tick(ctx),
            Goal::Guild(g) => return g.tick(ctx),
            Goal::Gank(g) => match g.tick(ctx) {
                GoalStatus::Done => g.resumes_run(ctx),
                status => return status,
            },
            Goal::Warzone(g) => return g.tick(ctx),
            Goal::Event(g) => return g.tick(ctx, combat),
            Goal::Companion(g) => return g.tick(ctx),
            Goal::Farm(g) => return g.tick(ctx),
            Goal::Explore(g) => return g.tick(ctx),
            Goal::Gear(g) => return g.tick(ctx),
        };
        if !resume {
            return GoalStatus::Done;
        }
        // The rest cut a trade run short while the bot carried a pack (or a gank took one): the
        // run goes on and sells it.
        ctx.log.say("goal: trade run (the pack is carried)");
        let mut run = TradeRun::default();
        run.start(ctx);
        *self = Goal::TradeRun(run);
        GoalStatus::Running
    }

    /// Called when the goal ends, fails or is interrupted.
    pub fn stop(&mut self, ctx: &mut Ctx<'_>) {
        match self {
            Goal::Wander(g) => g.stop(ctx),
            Goal::Rest(g) => g.stop(ctx),
            Goal::Grind(g) => g.stop(ctx),
            Goal::Quest(g) => g.stop(ctx),
            Goal::Duel(g) => g.stop(ctx),
            Goal::Trade(g) => g.stop(ctx),
            Goal::TradeRun(g) => g.stop(ctx),
            Goal::Housing(g) => g.stop(ctx),
            Goal::Dungeon(g) => g.stop(ctx),
            Goal::Sail(g) => g.stop(ctx),
            Goal::Auction(g) => g.stop(ctx),
            Goal::Market(g) => g.stop(ctx),
            Goal::Guild(g) => g.stop(ctx),
            Goal::Gank(g) => g.stop(ctx),
            Goal::Warzone(g) => g.stop(ctx),
            Goal::Event(g) => g.stop(ctx),
            Goal::Companion(g) => g.stop(ctx),
            Goal::Farm(g) => g.stop(ctx),
            Goal::Explore(g) => g.stop(ctx),
            Goal::Gear(g) => g.stop(ctx),
        }
    }

    /// The activity the web UI shows while this goal runs.
    pub fn activity(&self, moving: bool, fighting: bool) -> Activity {
        match self {
            Goal::Wander(_) => {
                if moving {
                    Activity::Traveling
                } else {
                    Activity::Idle
                }
            }
            Goal::Rest(_) => Activity::Resting,
            Goal::Grind(_) => {
                if fighting {
                    Activity::Fighting
                } else {
                    Activity::Grinding
                }
            }
            Goal::Quest(_) => Activity::Questing,
            Goal::Duel(_) => Activity::Dueling,
            Goal::Trade(_) => Activity::Trading,
            Goal::TradeRun(_) => Activity::TradeRun,
            Goal::Housing(_) => Activity::Housing,
            Goal::Dungeon(g) => g.activity(moving, fighting),
            Goal::Sail(g) => g.activity(moving, fighting),
            Goal::Auction(g) => g.activity(moving, fighting),
            Goal::Market(g) => g.activity(moving, fighting),
            Goal::Guild(g) => g.activity(moving, fighting),
            Goal::Gank(g) => g.activity(moving, fighting),
            Goal::Warzone(g) => g.activity(moving, fighting),
            Goal::Event(g) => g.activity(moving, fighting),
            Goal::Companion(g) => g.activity(moving, fighting),
            Goal::Farm(g) => g.activity(moving, fighting),
            Goal::Explore(g) => g.activity(moving, fighting),
            Goal::Gear(g) => g.activity(moving, fighting),
        }
    }

    /// Appends a one-line description of what the goal is doing to `out`.
    pub fn describe(&self, out: &mut String) {
        match self {
            Goal::Wander(g) => g.describe(out),
            Goal::Rest(_) => out.push_str("resting"),
            Goal::Grind(g) => g.describe(out),
            Goal::Quest(g) => g.describe(out),
            Goal::Duel(g) => g.describe(out),
            Goal::Trade(g) => g.describe(out),
            Goal::Housing(g) => g.describe(out),
            Goal::TradeRun(g) => g.describe(out),
            Goal::Dungeon(g) => g.describe(out),
            Goal::Sail(g) => g.describe(out),
            Goal::Auction(g) => g.describe(out),
            Goal::Market(g) => g.describe(out),
            Goal::Guild(g) => g.describe(out),
            Goal::Gank(g) => g.describe(out),
            Goal::Warzone(g) => g.describe(out),
            Goal::Event(g) => g.describe(out),
            Goal::Companion(g) => g.describe(out),
            Goal::Farm(g) => g.describe(out),
            Goal::Explore(g) => g.describe(out),
            Goal::Gear(g) => g.describe(out),
        }
    }
}

// ---- helpers shared by the goals -----------------------------------------------------------

/// Whether a bot may stand at (x, y): on land, and inside the baked terrain when there is one.
/// Without baked terrain every point is accepted (the bots walk on flat ground).
pub fn walkable(world: &aa_world::World, x: f32, y: f32) -> bool {
    match world.ground_z(x, y) {
        Some(g) => g >= SWIM_THRESHOLD + 1.0,
        None => world.baked().terrain.is_none(),
    }
}

/// A random walkable point between `min_r` and `max_r` metres from `center`, or `None` after
/// `tries` attempts.
pub fn random_point(
    world: &aa_world::World,
    rng: &mut fastrand::Rng,
    center: [f32; 3],
    min_r: f32,
    max_r: f32,
    tries: u32,
) -> Option<(f32, f32)> {
    for _ in 0..tries {
        let ang = rng.f32() * std::f32::consts::TAU;
        let r = min_r + rng.f32() * (max_r - min_r).max(0.0);
        let (x, y) = (center[0] + ang.cos() * r, center[1] + ang.sin() * r);
        if walkable(world, x, y) {
            return Some((x, y));
        }
    }
    None
}

/// Whether a monster of `level` is a fair fight for a bot of `own` level.
pub fn level_ok(own: u8, level: u8) -> bool {
    level <= own.saturating_add(3) && level.saturating_add(10) >= own
}

/// A random number of milliseconds in `lo..hi`.
pub fn jitter_ms(rng: &mut fastrand::Rng, lo: u64, hi: u64) -> u64 {
    if hi <= lo {
        lo
    } else {
        lo + rng.u64(0..hi - lo)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fair_fights() {
        assert!(level_ok(1, 1));
        assert!(level_ok(1, 4));
        assert!(!level_ok(1, 5));
        assert!(level_ok(20, 10));
        assert!(!level_ok(20, 9));
        assert!(level_ok(255, 255));
    }

    #[test]
    fn jitter_stays_in_range() {
        let mut rng = fastrand::Rng::with_seed(1);
        for _ in 0..1000 {
            let v = jitter_ms(&mut rng, 100, 200);
            assert!((100..200).contains(&v));
        }
        assert_eq!(jitter_ms(&mut rng, 5, 5), 5);
        assert_eq!(jitter_ms(&mut rng, 9, 5), 9);
    }

    #[test]
    fn activities_follow_the_goal() {
        let w = Goal::new(GoalKind::Wander, None);
        assert_eq!(w.activity(true, false), Activity::Traveling);
        assert_eq!(w.activity(false, false), Activity::Idle);
        let g = Goal::new(GoalKind::Grind, None);
        assert_eq!(g.activity(false, true), Activity::Fighting);
        assert_eq!(g.activity(true, false), Activity::Grinding);
        assert_eq!(
            Goal::new(GoalKind::Rest, None).activity(false, false),
            Activity::Resting
        );
        assert!(g.owns_combat() && !w.owns_combat());
        for kind in GoalKind::ALL {
            assert_eq!(Goal::new(kind, None).kind(), kind);
        }
    }

    #[test]
    fn the_dungeon_and_sail_stubs_are_goals_that_own_nothing() {
        for kind in [GoalKind::Dungeon, GoalKind::Sail] {
            let g = Goal::new(kind, None);
            assert_eq!(g.kind(), kind);
            assert!(!g.owns_combat());
            let mut text = String::new();
            g.describe(&mut text);
            assert!(text.starts_with(kind.name()), "{text}");
        }
        // only a dungeon keeps the party order it is started with
        let order = crate::bot::party::PartyOrder {
            party_id: 1,
            leader: 4,
            members: [4, 5].into_iter().collect(),
            roles: Default::default(),
            purpose: crate::bot::party::PartyPurpose::Dungeon { zone_group: 2 },
        };
        match Goal::with_party(GoalKind::Dungeon, None, Some(Box::new(order.clone()))) {
            Goal::Dungeon(d) => assert_eq!(d.order(), Some(&order)),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            Goal::with_party(GoalKind::Sail, None, Some(Box::new(order))),
            Goal::Sail(_)
        ));
    }

    #[test]
    fn the_living_world_stubs_keep_their_arguments_and_own_no_combat() {
        let args = GoalArgs {
            zone: Some(12),
            why: Some("test".into()),
            ..GoalArgs::default()
        };
        for kind in [GoalKind::Explore, GoalKind::Gear] {
            let g = Goal::with_args(kind, None, None, Some(Box::new(args.clone())));
            assert_eq!(g.kind(), kind);
            assert!(!g.owns_combat());
            let kept = match &g {
                Goal::Explore(e) => e.args().cloned(),
                Goal::Gear(e) => e.args().cloned(),
                other => panic!("{other:?}"),
            };
            assert_eq!(kept.as_ref(), Some(&args));
            let mut text = String::new();
            g.describe(&mut text);
            assert!(text.starts_with(kind.name()), "{text}");
        }
        // the other goals take no arguments and do not mind getting some
        for kind in GoalKind::ALL {
            let g = Goal::with_args(kind, None, None, Some(Box::new(args.clone())));
            assert_eq!(g.kind(), kind);
        }
    }

    #[test]
    fn the_wave_one_goals_start_idle_and_own_no_combat() {
        for kind in [
            GoalKind::Guild,
            GoalKind::Event,
            GoalKind::Companion,
            GoalKind::Farm,
        ] {
            let g = Goal::new(kind, None);
            assert_eq!(g.kind(), kind);
            assert!(!g.owns_combat());
            assert_eq!(g.activity(false, false), Activity::Idle);
            assert_eq!(g.activity(true, false), Activity::Traveling);
            assert_eq!(g.activity(true, true), Activity::Fighting);
            let mut text = String::new();
            g.describe(&mut text);
            assert!(text.starts_with(kind.name()), "{text}");
        }
    }
}
