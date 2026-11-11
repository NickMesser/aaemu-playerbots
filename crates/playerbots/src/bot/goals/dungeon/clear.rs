//! Clearing the dungeon: the leader's walk along the route and the decisions the party makes on
//! the way. Pure functions and a small cursor; the goal feeds them facts from the tick.

use super::route::Stop;
use crate::bot::party::PartyRole;

/// The leader engages monsters this close (metres).
pub const ENGAGE_M: f32 = 40.0;
/// A stop counts as reached within this distance.
pub const ARRIVE_M: f32 = 14.0;
/// The leader stops this far from the centre of a stop it walks to.
pub const STOP_RADIUS_M: f32 = 6.0;
/// A stop is done after this long without a hostile around it.
pub const QUIET_MS: u64 = 3_500;
/// A stop takes at most this long (walking and fighting) before it is skipped.
pub const STOP_TIMEOUT_MS: u64 = 150_000;
/// A boss that does not show within this long after the party arrived is missing.
pub const BOSS_WAIT_MS: u64 = 10_000;
/// A member follows the leader when farther away than this ...
pub const FOLLOW_M: f32 = 12.0;
/// ... and stops this far from it.
pub const FOLLOW_STOP_M: f32 = 5.0;
/// A healer stays farther back from the leader (and so from the monsters it pulls) ...
pub const HEALER_FOLLOW_M: f32 = 24.0;
/// ... and stops this far from it.
pub const HEALER_STOP_M: f32 = 16.0;
/// The leader waits for members farther than this.
pub const GATHER_M: f32 = 28.0;
/// ... for at most this long.
pub const REGROUP_MS: u64 = 20_000;
/// A member assists a target within this distance of itself.
pub const ASSIST_M: f32 = 45.0;
/// Hit points (percent) below which a bot rests.
pub const REST_HP: u8 = 45;
/// Mana (percent) below which a bot rests.
pub const REST_MP: u8 = 15;
/// Hit points (percent) at which a rest is over.
pub const REST_DONE_HP: u8 = 85;
/// Mana (percent) at which a rest is over.
pub const REST_DONE_MP: u8 = 55;
/// The longest rest.
pub const REST_MAX_MS: u64 = 75_000;
/// A monster this many levels above the bot is left alone (bosses excepted).
pub const LEVEL_CEILING: u8 = 8;

/// Whether a bot should rest: hurt, or out of mana when it has any.
pub fn needs_rest(hp_pct: u8, mp_pct: u8, mp_max: bool) -> bool {
    hp_pct < REST_HP || (mp_max && mp_pct < REST_MP)
}

/// Whether a rest can end.
pub fn rested(hp_pct: u8, mp_pct: u8, mp_max: bool) -> bool {
    hp_pct >= REST_DONE_HP && (!mp_max || mp_pct >= REST_DONE_MP)
}

/// The largest horizontal distance between `leader` and any of `others`.
pub fn spread(leader: [f32; 3], others: impl Iterator<Item = [f32; 3]>) -> f32 {
    others
        .map(|p| ((p[0] - leader[0]).powi(2) + (p[1] - leader[1]).powi(2)).sqrt())
        .fold(0.0, f32::max)
}

/// The monster to engage: of `cands` (object id, position) the nearest to `me` among those
/// within `radius` of `anchor`.
pub fn nearest(
    cands: impl Iterator<Item = (u32, [f32; 3])>,
    anchor: [f32; 3],
    me: [f32; 3],
    radius: f32,
) -> Option<u32> {
    let flat = |a: [f32; 3], b: [f32; 3]| ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt();
    cands
        .filter(|(_, p)| flat(*p, anchor) <= radius)
        .min_by(|a, b| flat(a.1, me).total_cmp(&flat(b.1, me)))
        .map(|c| c.0)
}

/// What the leader sees at its current stop this tick.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct StopFacts {
    /// The bot clock.
    pub now_ms: u64,
    /// Horizontal distance from the leader to the stop.
    pub dist: f32,
    /// A hostile monster is in view near the leader.
    pub hostile_near: bool,
    /// A boss is expected at this stop.
    pub boss_expected: bool,
    /// A monster of the boss's template is in view.
    pub boss_in_view: bool,
}

/// What to do about the current stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopStep {
    /// Walk to the stop.
    Walk,
    /// Fight what is in view.
    Fight,
    /// Stand at the stop and wait for something to show or for the quiet.
    Hold,
    /// The stop is done; the cursor moved on.
    Next,
    /// The stop took too long; the cursor moved on.
    Skip,
    /// The boss never showed; the cursor moved on.
    BossMissing,
    /// The route is done.
    Done,
}

/// Where the leader is on the route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cursor {
    /// Index of the current stop.
    pub next: u16,
    since_ms: u64,
    arrived_ms: u64,
    quiet_ms: u64,
    /// Stops skipped (timeout or missing boss).
    pub skipped: u16,
}

impl Cursor {
    fn advance(&mut self) {
        self.next += 1;
        self.since_ms = 0;
        self.arrived_ms = 0;
        self.quiet_ms = 0;
    }

    /// One tick against the route of `len` stops.
    pub fn step(&mut self, f: &StopFacts, len: usize) -> StopStep {
        if usize::from(self.next) >= len {
            return StopStep::Done;
        }
        let now = f.now_ms;
        if self.since_ms == 0 {
            self.since_ms = now.max(1);
        }
        if now.saturating_sub(self.since_ms) > STOP_TIMEOUT_MS {
            self.skipped += 1;
            self.advance();
            return StopStep::Skip;
        }
        if f.dist > ARRIVE_M {
            self.arrived_ms = 0;
            self.quiet_ms = 0;
            return if f.hostile_near {
                StopStep::Fight
            } else {
                StopStep::Walk
            };
        }
        if self.arrived_ms == 0 {
            self.arrived_ms = now.max(1);
        }
        if f.hostile_near {
            self.quiet_ms = 0;
            return StopStep::Fight;
        }
        if self.quiet_ms == 0 {
            self.quiet_ms = now.max(1);
        }
        if f.boss_expected && !f.boss_in_view {
            if now.saturating_sub(self.arrived_ms) > BOSS_WAIT_MS {
                self.skipped += 1;
                self.advance();
                return StopStep::BossMissing;
            }
            return StopStep::Hold;
        }
        if now.saturating_sub(self.quiet_ms) >= QUIET_MS {
            self.advance();
            return StopStep::Next;
        }
        StopStep::Hold
    }

    /// The stop the cursor is at.
    pub fn current<'a>(&self, route: &'a [Stop]) -> Option<&'a Stop> {
        route.get(usize::from(self.next))
    }

    /// Stops left, the current one included.
    pub fn left(&self, len: usize) -> usize {
        len.saturating_sub(usize::from(self.next))
    }
}

// ---- one tick of clearing, for the leader and for the members -----------------------------

/// A hostile monster in view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mob {
    /// Object id.
    pub obj: u32,
    /// NPC template.
    pub template: u32,
    /// Position.
    pub pos: [f32; 3],
}

/// What a bot sees in one clearing tick.
#[derive(Debug, Clone, Copy)]
pub struct Sight<'a> {
    /// The bot clock.
    pub now_ms: u64,
    /// Where the bot is.
    pub me: [f32; 3],
    /// Own hit points (percent).
    pub hp_pct: u8,
    /// Own mana (percent).
    pub mp_pct: u8,
    /// The bot has mana at all.
    pub has_mana: bool,
    /// The monsters the bot may fight in view: alive, hostile, on its floor, not given up.
    pub mobs: &'a [Mob],
    /// Leader: the distance to the farthest member (0 = all close or unknown).
    pub spread: f32,
    /// Leader: the lowest hit points (percent) among the members (100 = unknown).
    pub party_hp: u8,
    /// Member: where the leader is, when known.
    pub leader: Option<[f32; 3]>,
    /// Member: the object the leader fights, when it did lately.
    pub board_target: Option<u32>,
    /// The bot's role in the party.
    pub role: PartyRole,
}

impl Default for Sight<'_> {
    fn default() -> Self {
        Sight {
            now_ms: 0,
            me: [0.0; 3],
            hp_pct: 100,
            mp_pct: 100,
            has_mana: false,
            mobs: &[],
            spread: 0.0,
            party_hp: 100,
            leader: None,
            board_target: None,
            role: PartyRole::Dps,
        }
    }
}

/// What a bot does in a clearing tick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    /// Stand and wait (for a quiet, for the others, for the leader).
    Hold,
    /// Stand and recover.
    Rest,
    /// Walk to (x, y), stopping `radius` short.
    Walk {
        /// Destination.
        x: f32,
        /// Destination.
        y: f32,
        /// Stop this far from it.
        radius: f32,
    },
    /// Fight the monster with this object id.
    Engage(u32),
    /// The route is done (leader).
    Done,
}

/// The state of a clearing run besides the route: where the leader is on it and the timers of
/// resting and regrouping.
#[derive(Debug, Clone, Copy, Default)]
pub struct Clearing {
    /// The leader's place on the route.
    pub cursor: Cursor,
    rest_since_ms: u64,
    no_rest_until_ms: u64,
    regroup_since_ms: u64,
    regroup_stop: u16,
    missing_boss: u32,
}

fn flat(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt()
}

impl Clearing {
    /// Whether the bot stands still to recover: starts when it (or, for the leader, a member) is
    /// low, ends when all are well again or after [`REST_MAX_MS`] (then not again for a minute).
    fn rest(&mut self, s: &Sight<'_>) -> bool {
        let now = s.now_ms;
        let low = s.hp_pct.min(s.party_hp);
        if self.rest_since_ms != 0 {
            let well = rested(low, s.mp_pct, s.has_mana);
            let timed_out = now.saturating_sub(self.rest_since_ms) > REST_MAX_MS;
            if well || timed_out {
                self.rest_since_ms = 0;
                if timed_out && !well {
                    self.no_rest_until_ms = now + 60_000;
                }
                return false;
            }
            return true;
        }
        if now >= self.no_rest_until_ms && needs_rest(low, s.mp_pct, s.has_mana) {
            self.rest_since_ms = now.max(1);
            return true;
        }
        false
    }

    /// The leader waits (a limited time per stop) while a member is far behind.
    fn must_regroup(&mut self, s: &Sight<'_>) -> bool {
        if s.spread <= GATHER_M {
            self.regroup_since_ms = 0;
            return false;
        }
        if self.regroup_stop != self.cursor.next {
            self.regroup_stop = self.cursor.next;
            self.regroup_since_ms = 0;
        }
        if self.regroup_since_ms == 0 {
            self.regroup_since_ms = s.now_ms.max(1);
        }
        s.now_ms.saturating_sub(self.regroup_since_ms) < REGROUP_MS
    }

    /// The template of a boss that never showed at its stop, once (0 = none).
    pub fn take_missing_boss(&mut self) -> u32 {
        std::mem::take(&mut self.missing_boss)
    }

    /// One tick of the leader (the combat controller is idle).
    pub fn leader(&mut self, route: &[Stop], s: &Sight<'_>) -> Action {
        if self.rest(s) {
            return Action::Rest;
        }
        let Some(stop) = self.cursor.current(route).copied() else {
            return Action::Done;
        };
        let target = nearest(s.mobs.iter().map(|m| (m.obj, m.pos)), s.me, s.me, ENGAGE_M);
        let facts = StopFacts {
            now_ms: s.now_ms,
            dist: stop.dist2d(s.me),
            hostile_near: target.is_some(),
            boss_expected: stop.is_boss(),
            boss_in_view: stop.is_boss() && s.mobs.iter().any(|m| m.template == stop.boss),
        };
        match self.cursor.step(&facts, route.len()) {
            StopStep::Walk => {
                if self.must_regroup(s) {
                    Action::Hold
                } else {
                    Action::Walk {
                        x: stop.x,
                        y: stop.y,
                        radius: STOP_RADIUS_M,
                    }
                }
            }
            StopStep::Fight => target.map_or(Action::Hold, Action::Engage),
            StopStep::BossMissing => {
                self.missing_boss = stop.boss;
                Action::Hold
            }
            StopStep::Hold | StopStep::Next | StopStep::Skip => Action::Hold,
            StopStep::Done => Action::Done,
        }
    }

    /// One tick of a member (the combat controller is idle): help the leader, else stay with it.
    pub fn member(&mut self, s: &Sight<'_>) -> Action {
        let own = Sight {
            party_hp: 100,
            ..*s
        };
        if self.rest(&own) {
            return Action::Rest;
        }
        if let Some(t) = s.board_target {
            if s.mobs
                .iter()
                .any(|m| m.obj == t && flat(m.pos, s.me) <= ASSIST_M)
            {
                return Action::Engage(t);
            }
        }
        let (follow, stop) = if s.role == PartyRole::Healer {
            (HEALER_FOLLOW_M, HEALER_STOP_M)
        } else {
            (FOLLOW_M, FOLLOW_STOP_M)
        };
        match s.leader {
            Some(l) if flat(l, s.me) > follow => Action::Walk {
                x: l[0],
                y: l[1],
                radius: stop,
            },
            _ => Action::Hold,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(now_ms: u64, dist: f32, hostile_near: bool) -> StopFacts {
        StopFacts {
            now_ms,
            dist,
            hostile_near,
            ..StopFacts::default()
        }
    }

    #[test]
    fn rest_is_for_the_hurt_and_the_drained() {
        assert!(needs_rest(30, 100, true));
        assert!(needs_rest(90, 10, true));
        assert!(!needs_rest(90, 10, false), "no mana to run out of");
        assert!(!needs_rest(60, 60, true));
        assert!(rested(90, 60, true));
        assert!(!rested(80, 100, true));
        assert!(!rested(90, 40, true));
        assert!(rested(90, 0, false));
    }

    #[test]
    fn spread_is_the_farthest_member() {
        let l = [0.0, 0.0, 0.0];
        assert_eq!(spread(l, std::iter::empty()), 0.0);
        let others = [[3.0, 4.0, 0.0], [0.0, 20.0, 9.0], [10.0, 0.0, 0.0]];
        assert_eq!(spread(l, others.into_iter()), 20.0);
    }

    #[test]
    fn the_target_is_the_nearest_to_the_bot_near_the_anchor() {
        let me = [0.0, 0.0, 0.0];
        let anchor = [30.0, 0.0, 0.0];
        let cands = [
            (1, [31.0, 0.0, 0.0]),  // next to the anchor, 31 m from me
            (2, [10.0, 0.0, 0.0]),  // 10 m from me, 20 from the anchor
            (3, [200.0, 0.0, 0.0]), // far from the anchor
            (4, [29.0, 20.0, 0.0]), // 20 m from the anchor
        ];
        assert_eq!(nearest(cands.into_iter(), anchor, me, 25.0), Some(2));
        assert_eq!(nearest(cands.into_iter(), anchor, me, 5.0), Some(1));
        assert_eq!(nearest(cands.into_iter(), [500.0, 0.0, 0.0], me, 5.0), None);
        assert_eq!(nearest(std::iter::empty(), anchor, me, 25.0), None);
    }

    #[test]
    fn the_cursor_walks_fights_waits_for_quiet_and_moves_on() {
        let mut c = Cursor::default();
        // far away: walk, or fight on the way
        assert_eq!(c.step(&at(1_000, 100.0, false), 2), StopStep::Walk);
        assert_eq!(c.step(&at(2_000, 80.0, true), 2), StopStep::Fight);
        // at the stop with monsters: fight, the quiet clock restarts
        assert_eq!(c.step(&at(5_000, 3.0, true), 2), StopStep::Fight);
        assert_eq!(c.step(&at(6_000, 3.0, false), 2), StopStep::Hold);
        assert_eq!(
            c.step(&at(6_000 + QUIET_MS - 1, 3.0, false), 2),
            StopStep::Hold
        );
        assert_eq!(c.step(&at(6_000 + QUIET_MS, 3.0, false), 2), StopStep::Next);
        assert_eq!(c.next, 1);
        // a new monster before the quiet is over resets it
        assert_eq!(c.step(&at(10_000, 3.0, false), 2), StopStep::Hold);
        assert_eq!(c.step(&at(12_000, 3.0, true), 2), StopStep::Fight);
        assert_eq!(c.step(&at(13_000, 3.0, false), 2), StopStep::Hold);
        assert_eq!(
            c.step(&at(13_000 + QUIET_MS, 3.0, false), 2),
            StopStep::Next
        );
        assert_eq!(c.step(&at(20_000, 3.0, false), 2), StopStep::Done);
        assert_eq!(c.left(2), 0);
    }

    #[test]
    fn a_stop_that_takes_too_long_is_skipped() {
        let mut c = Cursor::default();
        assert_eq!(c.step(&at(1_000, 500.0, false), 3), StopStep::Walk);
        assert_eq!(
            c.step(&at(1_000 + STOP_TIMEOUT_MS + 1, 400.0, false), 3),
            StopStep::Skip
        );
        assert_eq!((c.next, c.skipped), (1, 1));
        // the clock starts afresh for the next stop
        assert_eq!(
            c.step(&at(1_000 + STOP_TIMEOUT_MS + 2, 400.0, false), 3),
            StopStep::Walk
        );
    }

    #[test]
    fn a_boss_that_does_not_show_is_reported_missing() {
        let mut c = Cursor::default();
        let boss = |now, in_view| StopFacts {
            now_ms: now,
            dist: 2.0,
            boss_expected: true,
            boss_in_view: in_view,
            ..StopFacts::default()
        };
        assert_eq!(c.step(&boss(1_000, false), 1), StopStep::Hold);
        assert_eq!(
            c.step(&boss(1_000 + BOSS_WAIT_MS + 1, false), 1),
            StopStep::BossMissing
        );
        assert_eq!(c.skipped, 1);
        // a boss in view is fought, and after it dies the quiet ends the stop
        let mut c = Cursor::default();
        assert_eq!(
            c.step(
                &StopFacts {
                    hostile_near: true,
                    ..boss(1_000, true)
                },
                1
            ),
            StopStep::Fight
        );
        assert_eq!(c.step(&boss(5_000, true), 1), StopStep::Hold);
        assert_eq!(c.step(&boss(5_000 + QUIET_MS, true), 1), StopStep::Next);
    }

    #[test]
    fn an_empty_route_is_done_at_once() {
        let mut c = Cursor::default();
        assert_eq!(c.step(&at(1, 0.0, false), 0), StopStep::Done);
        let route = [Stop {
            x: 1.0,
            y: 2.0,
            z: 3.0,
            mobs: 1,
            boss: 0,
        }];
        assert_eq!(Cursor::default().current(&route), Some(&route[0]));
        assert_eq!(
            Cursor {
                next: 1,
                ..Cursor::default()
            }
            .current(&route),
            None
        );
    }

    fn stop(x: f32, y: f32, boss: u32) -> Stop {
        Stop {
            x,
            y,
            z: 0.0,
            mobs: 3,
            boss,
        }
    }

    fn mob(obj: u32, x: f32, y: f32) -> Mob {
        Mob {
            obj,
            template: 100 + obj,
            pos: [x, y, 0.0],
        }
    }

    #[test]
    fn the_leader_walks_to_the_stop_fights_what_is_near_and_moves_on() {
        let route = [stop(100.0, 0.0, 0), stop(200.0, 0.0, 0)];
        let mut c = Clearing::default();
        // far from the stop, nothing around: walk
        let s = Sight {
            now_ms: 1_000,
            ..Sight::default()
        };
        assert_eq!(
            c.leader(&route, &s),
            Action::Walk {
                x: 100.0,
                y: 0.0,
                radius: STOP_RADIUS_M
            }
        );
        // a monster near the way: fight it, the nearer of two
        let mobs = [mob(7, 30.0, 0.0), mob(8, 12.0, 0.0), mob(9, 90.0, 0.0)];
        let s = Sight {
            now_ms: 2_000,
            mobs: &mobs,
            ..Sight::default()
        };
        assert_eq!(c.leader(&route, &s), Action::Engage(8));
        // at the stop, quiet: hold, then the next stop is the way
        let at = Sight {
            now_ms: 10_000,
            me: [98.0, 0.0, 0.0],
            ..Sight::default()
        };
        assert_eq!(c.leader(&route, &at), Action::Hold);
        let later = Sight {
            now_ms: 10_000 + QUIET_MS,
            ..at
        };
        assert_eq!(c.leader(&route, &later), Action::Hold, "the stop is done");
        assert_eq!(c.cursor.next, 1);
        let on = Sight {
            now_ms: 14_000,
            ..later
        };
        assert_eq!(
            c.leader(&route, &on),
            Action::Walk {
                x: 200.0,
                y: 0.0,
                radius: STOP_RADIUS_M
            }
        );
        // the last stop done: the route is over
        let end = Sight {
            now_ms: 20_000,
            me: [199.0, 0.0, 0.0],
            ..Sight::default()
        };
        assert_eq!(c.leader(&route, &end), Action::Hold);
        let end2 = Sight {
            now_ms: 20_000 + QUIET_MS,
            ..end
        };
        assert_eq!(c.leader(&route, &end2), Action::Hold);
        assert_eq!(c.leader(&route, &end2), Action::Done);
    }

    #[test]
    fn the_leader_waits_for_a_boss_that_has_not_shown_and_fights_it_when_it_does() {
        let route = [stop(100.0, 0.0, 777)];
        let mut c = Clearing::default();
        let at = |now| Sight {
            now_ms: now,
            me: [99.0, 0.0, 0.0],
            ..Sight::default()
        };
        assert_eq!(c.leader(&route, &at(1_000)), Action::Hold);
        let boss = [Mob {
            obj: 5,
            template: 777,
            pos: [110.0, 0.0, 0.0],
        }];
        let seen = Sight {
            mobs: &boss,
            ..at(4_000)
        };
        assert_eq!(c.leader(&route, &seen), Action::Engage(5));
        // after the boss is dead (gone from view) the quiet ends the stop
        assert_eq!(c.leader(&route, &at(8_000)), Action::Hold);
        assert_eq!(c.leader(&route, &at(8_000 + QUIET_MS)), Action::Hold);
        assert_eq!(c.leader(&route, &at(13_000)), Action::Done);
    }

    #[test]
    fn the_leader_rests_when_it_or_a_member_is_low_and_gives_up_the_wait() {
        let route = [stop(100.0, 0.0, 0)];
        let mut c = Clearing::default();
        let low = |now, hp, party| Sight {
            now_ms: now,
            hp_pct: hp,
            party_hp: party,
            ..Sight::default()
        };
        assert_eq!(c.leader(&route, &low(1_000, 30, 100)), Action::Rest);
        // still hurt: keeps resting
        assert_eq!(c.leader(&route, &low(10_000, 70, 100)), Action::Rest);
        // well again: on the way
        assert!(matches!(
            c.leader(&route, &low(20_000, 95, 100)),
            Action::Walk { .. }
        ));
        // a member is low
        assert_eq!(c.leader(&route, &low(30_000, 100, 20)), Action::Rest);
        assert!(matches!(
            c.leader(&route, &low(40_000, 100, 90)),
            Action::Walk { .. }
        ));
        // a rest that does not help ends after the limit and does not start again at once
        let mut c = Clearing::default();
        assert_eq!(c.leader(&route, &low(1_000, 10, 100)), Action::Rest);
        assert!(matches!(
            c.leader(&route, &low(1_000 + REST_MAX_MS + 1, 10, 100)),
            Action::Walk { .. }
        ));
        assert!(matches!(
            c.leader(&route, &low(1_000 + REST_MAX_MS + 5_000, 10, 100)),
            Action::Walk { .. }
        ));
    }

    #[test]
    fn mana_matters_only_for_bots_that_have_some() {
        let route = [stop(100.0, 0.0, 0)];
        let s = |has_mana| Sight {
            now_ms: 1_000,
            mp_pct: 5,
            has_mana,
            ..Sight::default()
        };
        assert_eq!(Clearing::default().leader(&route, &s(true)), Action::Rest);
        assert!(matches!(
            Clearing::default().leader(&route, &s(false)),
            Action::Walk { .. }
        ));
    }

    #[test]
    fn the_leader_waits_for_stragglers_for_a_while() {
        let route = [stop(300.0, 0.0, 0)];
        let mut c = Clearing::default();
        let far = |now| Sight {
            now_ms: now,
            spread: GATHER_M + 10.0,
            ..Sight::default()
        };
        assert_eq!(c.leader(&route, &far(1_000)), Action::Hold);
        assert_eq!(c.leader(&route, &far(1_000 + REGROUP_MS - 1)), Action::Hold);
        // the time is up: the party goes on without waiting
        assert!(matches!(
            c.leader(&route, &far(1_000 + REGROUP_MS + 1)),
            Action::Walk { .. }
        ));
        // they caught up: no waiting
        let mut c = Clearing::default();
        let near = Sight {
            now_ms: 1_000,
            spread: 8.0,
            ..Sight::default()
        };
        assert!(matches!(c.leader(&route, &near), Action::Walk { .. }));
    }

    #[test]
    fn a_healer_follows_farther_back_than_the_damage_dealers() {
        let at = |role, leader| Sight {
            now_ms: 1_000,
            leader: Some([leader, 0.0, 0.0]),
            role,
            ..Sight::default()
        };
        // 20 m behind: a damage dealer walks in, a healer is still inside its distance
        assert!(matches!(
            Clearing::default().member(&at(PartyRole::Dps, 20.0)),
            Action::Walk { .. }
        ));
        assert_eq!(
            Clearing::default().member(&at(PartyRole::Healer, 20.0)),
            Action::Hold
        );
        // 30 m behind: the healer follows too, and stops farther out
        assert_eq!(
            Clearing::default().member(&at(PartyRole::Healer, 30.0)),
            Action::Walk {
                x: 30.0,
                y: 0.0,
                radius: HEALER_STOP_M
            }
        );
    }

    #[test]
    fn members_assist_the_leaders_target_follow_it_and_rest() {
        let mut c = Clearing::default();
        let mobs = [mob(5, 20.0, 0.0), mob(6, 200.0, 0.0)];
        let base = Sight {
            now_ms: 1_000,
            mobs: &mobs,
            leader: Some([40.0, 0.0, 0.0]),
            ..Sight::default()
        };
        // the leader's target is in view and close: help
        let help = Sight {
            board_target: Some(5),
            ..base
        };
        assert_eq!(c.member(&help), Action::Engage(5));
        // ... but not when it is out of reach of the assist, or not in view
        let far = Sight {
            board_target: Some(6),
            ..base
        };
        assert_eq!(
            c.member(&far),
            Action::Walk {
                x: 40.0,
                y: 0.0,
                radius: FOLLOW_STOP_M
            }
        );
        let unseen = Sight {
            board_target: Some(99),
            ..base
        };
        assert!(matches!(c.member(&unseen), Action::Walk { .. }));
        // close to the leader and nothing to do: stay
        let close = Sight {
            leader: Some([5.0, 0.0, 0.0]),
            ..base
        };
        assert_eq!(c.member(&close), Action::Hold);
        // no idea where the leader is: stay
        let lost = Sight {
            leader: None,
            ..base
        };
        assert_eq!(c.member(&lost), Action::Hold);
        // a hurt member rests first; the party's hit points do not concern it
        let hurt = Sight {
            hp_pct: 20,
            board_target: Some(5),
            ..base
        };
        assert_eq!(c.member(&hurt), Action::Rest);
        let party_low = Sight {
            party_hp: 10,
            ..close
        };
        assert_eq!(Clearing::default().member(&party_low), Action::Hold);
    }
}
