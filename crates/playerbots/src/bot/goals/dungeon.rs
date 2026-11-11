//! Dungeon: form a party with other bots and clear an instance dungeon (the dungeons feature).
//!
//! # The run
//!
//! A bot that picks the goal (by score, or because the operator asked) has no party yet: it
//! sends a [`Wish`](crate::bot::party::Wish) to the coordinator and waits ([`Phase::Gather`]).
//! The coordinator forms a party of bots that fit one dungeon and sends every member a
//! [`BotCmd::Party`](crate::bot::cmd::BotCmd::Party); the brain restarts the goal with that
//! [`PartyOrder`], and the run goes on:
//!
//! 1. **Prep / Team**: a level below the dungeon's range is raised with the GM `/level` (config
//!    `behavior.dungeon_level_up`, only with `bots.gm_bootstrap`); the leader invites the others
//!    one at a time, they accept the leader's invitation.
//! 2. **Travel / Muster**: walk (or `/move`) to the entrance doodad, wait until everybody is
//!    within 20 m, go in one after the other.
//! 3. **Enter / Load**: cast the entrance skill on the doodad; the server answers with
//!    `SCProcessingInstance` and `SCLoadInstance`, the bot answers `CSInstanceLoaded`.
//! 4. **Clear**: the leader walks the route (the hostile spawns of the world's data, grouped into
//!    stops, bosses last) and engages what it meets; the members follow within about 15 m and
//!    assist the leader's target (and the monsters that attack a member, see the combat
//!    controller). Everybody rests when low, the leader waits for stragglers and hurt members.
//! 5. **Leave / Out**: when the route is done, the time (25 min) is up, the party is gone or the
//!    bot died three times, everybody walks to the nearest exit doodad and casts its skill; back
//!    in the main world the members leave the team and the leader dismisses it.
//!
//! Refusals of the entry are typed ([`entry`]) and answered by kind; a dungeon the server turned
//! away is left alone for a while (per zone group, see
//! [`PartyMem`](crate::bot::party::PartyMem)). With `[dungeons] entry_limit` on, three entries per
//! zone group and four hours are tracked like the server counts them; it is off by default
//! because the 1.2 server has no entry limit. What the server does that the data and the protocol
//! contradict is filed as an issue ([`report`]).
//!
//! The pieces: [`machine`] (the phases, pure), [`clear`] (the walk through the dungeon, pure),
//! [`route`] (the route, pure), [`entry`] (the refusals), [`report`] (the issues) and [`run`]
//! (the glue to the tick context). This file is the goal itself and its scoring.

mod clear;
pub(crate) mod entry;
mod machine;
mod report;
mod route;
pub(crate) mod run;
#[cfg(test)]
mod sim;

use pb_web::Activity;

use crate::bot::cmd::GoalKind;
use crate::bot::ctx::Ctx;
use crate::bot::party::{self, PartyOrder};
use crate::config::BehaviorCfg;
use crate::roster::Temperament;

use super::{FailReason, GoalStatus};
use run::{Outcome, Run};

/// A bot starts a dungeon at most this often (bot clock, milliseconds).
pub const MIN_GAP_MS: u64 = 30 * 60_000;
/// A bot with fewer hit points (percent) does not start a dungeon.
const MIN_HP_PCT: u8 = 70;
/// A team invitation nobody asked for is turned down after this long.
const STRAY_INVITE_MS: u64 = 4_000;

/// Clearing an instance dungeon with a party. See the module docs.
#[derive(Debug, Default)]
pub struct Dungeon {
    /// The order that started the goal; `None` when the operator forced the goal or the score
    /// picked it.
    order: Option<Box<PartyOrder>>,
    /// The run, made at [`Dungeon::start`].
    run: Option<Box<Run>>,
}

impl Dungeon {
    /// A goal started by `order` (`None` for an operator command or the bot's own choice).
    pub fn new(order: Option<Box<PartyOrder>>) -> Dungeon {
        Dungeon { order, run: None }
    }

    /// The party order the goal was started with.
    pub fn order(&self) -> Option<&PartyOrder> {
        self.order.as_deref()
    }

    /// Called once when the brain picks the goal.
    pub fn start(&mut self, ctx: &mut Ctx<'_>) {
        let run = Run::new(ctx, self.order.as_deref());
        self.run = Some(Box::new(run));
    }

    /// One tick.
    pub fn tick(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        if self.run.is_none() {
            self.start(ctx);
        }
        let Some(run) = self.run.as_mut() else {
            return GoalStatus::Failed(FailReason::Other("no dungeon run"));
        };
        let order = self.order.as_deref();
        match run.tick(ctx, order) {
            None => GoalStatus::Running,
            Some(Outcome::Done) => {
                run.finish(ctx, order, Some(Outcome::Done));
                GoalStatus::Done
            }
            Some(Outcome::Failed(f)) => {
                run.finish(ctx, order, Some(Outcome::Failed(f)));
                GoalStatus::Failed(FailReason::Other(f.word()))
            }
        }
    }

    /// Called when the goal ends, fails or is interrupted (stop motion, drop targets).
    pub fn stop(&mut self, ctx: &mut Ctx<'_>) {
        if let Some(run) = self.run.as_mut() {
            run.finish(ctx, self.order.as_deref(), None);
        }
        ctx.motion.stop();
        ctx.combat.disengage();
    }

    /// Whether the goal drives the combat controller itself (see [`super::Goal::owns_combat`]):
    /// inside the dungeon, where it decides whom the bot fights and when it moves on.
    pub fn owns_combat(&self) -> bool {
        self.run.as_ref().is_some_and(|r| r.owns_combat())
    }

    /// The bot died. Returns whether the run goes on (it does inside the dungeon: the bot is
    /// brought back at the temple and rejoins the party).
    pub fn on_dead(&mut self, ctx: &mut Ctx<'_>) -> bool {
        match self.run.as_mut() {
            Some(r) if r.survives_death() => {
                r.on_dead(ctx);
                true
            }
            _ => false,
        }
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
        out.push_str("dungeon");
        if let Some(run) = &self.run {
            out.push_str(": ");
            run.describe(out);
        }
    }
}

/// The goal's weight before the situation is looked at, from the bot's temperament: bots that
/// like a fight like a dungeon (`behavior.dungeons` off: 0).
pub fn base_weight(behavior: &BehaviorCfg, temper: &Temperament) -> f32 {
    if !behavior.dungeons {
        return 0.0;
    }
    0.25 * (0.3 + 1.4 * temper.duelist)
}

/// What the scoring looks at.
#[derive(Debug, Clone, Copy, Default)]
pub struct Gate {
    /// `behavior.dungeons`.
    pub enabled: bool,
    /// The bot is inside an instance or loading one.
    pub away: bool,
    /// The bot is dead.
    pub dead: bool,
    /// Hit points in percent.
    pub hp_pct: u8,
    /// The bot clock.
    pub now_ms: u64,
    /// The bot clock at which the last run ended (0 = never).
    pub last_run_ms: u64,
    /// The bot is in a team.
    pub in_team: bool,
    /// Some dungeon fits the bot's level (or can be raised to) and is not blocked.
    pub fits: bool,
}

/// Why a bot does not pick the goal now (`None`: it may).
pub fn unavailable(g: &Gate) -> Option<&'static str> {
    if !g.enabled {
        Some("dungeons are switched off")
    } else if g.away {
        Some("already in an instance")
    } else if g.dead {
        Some("dead")
    } else if g.hp_pct < MIN_HP_PCT {
        Some("hurt")
    } else if g.in_team {
        Some("in a team")
    } else if g.last_run_ms != 0 && g.now_ms.saturating_sub(g.last_run_ms) < MIN_GAP_MS {
        Some("ran a dungeon lately")
    } else if !g.fits {
        Some("no dungeon fits")
    } else {
        None
    }
}

/// Whether a bot of `level` fits a dungeon of `min..=max`: inside the range, or below it and
/// able to raise its level.
pub fn level_fits(level: u8, min: u8, max: u8, raise: bool) -> bool {
    level <= max && (level >= min || raise)
}

fn gate(ctx: &Ctx<'_>) -> Gate {
    let st = &*ctx.state;
    let raise = ctx.gm_allowed() && ctx.cfg.behavior.dungeon_level_up;
    let mem = st.party.as_deref();
    let level = st.me.level;
    let now = ctx.now_ms;
    let enabled = ctx.cfg.behavior.dungeons;
    let entry_limit = ctx.cfg.dungeons.entry_limit;
    // (the search is the expensive part: only when everything else allows the goal)
    let fits = enabled
        && ctx.world.dungeons().iter().any(|d| {
            level_fits(level, d.level_min, d.level_max, raise)
                && party::usable(ctx.world, d)
                && !mem.is_some_and(|m| {
                    m.blocked(d.zone_group, entry_limit.then_some(d.enter_count), now)
                })
        });
    Gate {
        enabled,
        away: st.place.in_instance() || st.place.loading(),
        dead: st.me.dead || !st.me.spawned,
        hp_pct: st.me.hp_pct(),
        now_ms: now,
        last_run_ms: mem.map_or(0, |m| m.last_run_ms),
        in_team: mem.is_some_and(|m| m.team.in_team()),
        fits,
    }
}

/// The score of the goal for this tick, given its `base` weight. 0 keeps the bot from picking it.
/// Called whenever the brain scores the goals, so keep it cheap.
pub fn score(ctx: &Ctx<'_>, base: f32) -> f32 {
    if base <= 0.0 || !ctx.cfg.behavior.dungeons {
        return 0.0;
    }
    match unavailable(&gate(ctx)) {
        None => base,
        Some(_) => 0.0,
    }
}

/// Team invitations no dungeon run asked for are turned down: a player's, or a leader's that
/// arrived before the party order did and was not answered. (A pending invitation blocks every
/// other one to the same character on the server.)
pub fn unsolicited(ctx: &mut Ctx<'_>, running: Option<GoalKind>) {
    // (a founding team answers its own invitations: the guild goal)
    if matches!(running, Some(GoalKind::Dungeon | GoalKind::Guild)) {
        return;
    }
    // (a real player's invitation is the party feature's: it answers within seconds)
    if crate::bot::companion::claims(ctx) {
        return;
    }
    let Some(inv) = ctx.state.team().and_then(|t| t.invite.as_ref()) else {
        return;
    };
    if ctx.now_ms.saturating_sub(inv.at_ms) < STRAY_INVITE_MS {
        return;
    }
    run::reject_strays(ctx, None);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::party::PartyPurpose;

    fn ready() -> Gate {
        Gate {
            enabled: true,
            hp_pct: 100,
            now_ms: 10 * MIN_GAP_MS,
            fits: true,
            ..Gate::default()
        }
    }

    #[test]
    fn the_order_that_started_the_goal_is_kept() {
        assert!(Dungeon::default().order().is_none());
        let order = PartyOrder {
            party_id: 1,
            leader: 2,
            members: [2, 3].into_iter().collect(),
            purpose: PartyPurpose::Dungeon { zone_group: 5 },
        };
        let g = Dungeon::new(Some(Box::new(order.clone())));
        assert_eq!(g.order(), Some(&order));
        assert!(!g.owns_combat());
    }

    #[test]
    fn the_goal_shows_as_idle_or_traveling_or_fighting() {
        let g = Dungeon::default();
        assert_eq!(g.activity(false, false), Activity::Idle);
        assert_eq!(g.activity(true, false), Activity::Traveling);
        assert_eq!(g.activity(true, true), Activity::Fighting);
        let mut text = String::new();
        g.describe(&mut text);
        assert!(text.starts_with("dungeon"));
    }

    #[test]
    fn a_ready_bot_may_pick_the_goal_and_every_gate_closes_it() {
        assert_eq!(unavailable(&ready()), None);
        let cases: [(Gate, &str); 7] = [
            (
                Gate {
                    enabled: false,
                    ..ready()
                },
                "switched off",
            ),
            (
                Gate {
                    away: true,
                    ..ready()
                },
                "instance",
            ),
            (
                Gate {
                    dead: true,
                    ..ready()
                },
                "dead",
            ),
            (
                Gate {
                    hp_pct: MIN_HP_PCT - 1,
                    ..ready()
                },
                "hurt",
            ),
            (
                Gate {
                    in_team: true,
                    ..ready()
                },
                "team",
            ),
            (
                Gate {
                    last_run_ms: ready().now_ms - MIN_GAP_MS + 1,
                    ..ready()
                },
                "lately",
            ),
            (
                Gate {
                    fits: false,
                    ..ready()
                },
                "fits",
            ),
        ];
        for (gate, word) in cases {
            let why = unavailable(&gate).unwrap_or_else(|| panic!("{word} should close the gate"));
            assert!(why.contains(word), "{why:?} should name {word:?}");
        }
        // the gap is a gap: half an hour later the bot may go again
        let later = Gate {
            last_run_ms: ready().now_ms - MIN_GAP_MS,
            ..ready()
        };
        assert_eq!(unavailable(&later), None);
    }

    #[test]
    fn a_level_fits_inside_the_range_or_below_it_with_a_raise() {
        assert!(level_fits(20, 18, 55, false));
        assert!(level_fits(18, 18, 55, false));
        assert!(!level_fits(10, 18, 55, false));
        assert!(level_fits(10, 18, 55, true), "a GM raise lifts it to 18");
        assert!(
            !level_fits(56, 18, 55, true),
            "a raise cannot lower a level"
        );
    }

    #[test]
    fn fighters_weigh_more_and_the_switch_turns_the_goal_off() {
        let on = BehaviorCfg::default();
        let off = BehaviorCfg {
            dungeons: false,
            ..BehaviorCfg::default()
        };
        let fighter = Temperament {
            duelist: 0.9,
            ..Temperament::NEUTRAL
        };
        let homebody = Temperament {
            duelist: 0.1,
            ..Temperament::NEUTRAL
        };
        assert!(base_weight(&on, &fighter) > 2.0 * base_weight(&on, &homebody));
        assert!(base_weight(&on, &homebody) > 0.0);
        assert_eq!(base_weight(&off, &fighter), 0.0);
    }
}
