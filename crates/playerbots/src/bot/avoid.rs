//! Targets a bot gave up on and should leave alone for a good while, and monsters a travelling
//! bot walks around.
//!
//! A monster the bot could not hurt (it stands on a ledge, behind a wall, evades, or every cast at
//! it is refused for a missing line of sight) must not be picked again the moment a short ignore
//! timer runs out: the bot would stand next to it, fighting nothing, for minutes. [`Avoid`] keeps
//! two bounded lists in [`BotState`](super::state::BotState): single units (by object id) and
//! whole kinds (by NPC template, after repeated failures against that kind). The grind goal and
//! the quest goal both read it when they choose a monster.
//!
//! [`Avoid::route_detour`] is the travel rule: a bot on its way somewhere takes one waypoint to
//! the side of a monster far above its level whose aggro radius the straight line crosses, instead
//! of walking through its pack. The waypoint is kept for one trip ([`DETOUR_LIFE_MS`]), so the
//! detour cannot become a loop.

use smallvec::SmallVec;

/// A unit that could not be reached is left alone this long.
pub const UNREACHABLE_MS: u64 = 600_000;
/// Failures against one kind within [`KIND_WINDOW_MS`] after which the whole kind is avoided.
pub const KIND_STRIKES: u8 = 2;
/// Strikes older than this are forgotten.
pub const KIND_WINDOW_MS: u64 = 600_000;
/// A kind that earned its strikes is avoided this long.
pub const KIND_AVOID_MS: u64 = 600_000;
/// Most units remembered.
const MAX_OBJS: usize = 8;
/// Most kinds remembered.
const MAX_KINDS: usize = 4;
/// A travel detour is used this long at most (it cannot become a loop).
pub const DETOUR_LIFE_MS: u64 = 30_000;
/// The detour waypoint counts as reached this close.
pub const DETOUR_ARRIVE_M: f32 = 8.0;
/// The detour keeps this much more than the aggro radius free (metres).
const DETOUR_MARGIN_M: f32 = 8.0;

/// A spawn a travelling bot should keep out of: where it stands and how strong it is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Danger {
    /// Position.
    pub x: f32,
    /// Position.
    pub y: f32,
    /// Level of the monster.
    pub level: u8,
}

/// How much a travel detour keeps clear (`[survival] travel_*`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RouteLimits {
    /// Monsters more than this many levels above the bot are dangerous.
    pub gap: u8,
    /// The aggro radius kept free (metres).
    pub aggro_m: f32,
    /// Nothing farther ahead than this is looked at (metres).
    pub lookahead_m: f32,
}

/// A waypoint around a danger, for the trip to `dest`.
#[derive(Debug, Clone, Copy)]
struct Detour {
    dest: (f32, f32),
    via: (f32, f32),
    until_ms: u64,
}

#[derive(Debug, Clone, Copy)]
struct Kind {
    template: u32,
    strikes: u8,
    /// End of the strike window, or of the avoidance once the strikes are in.
    until_ms: u64,
}

/// What a bot avoids. See the module docs.
#[derive(Debug, Default, Clone)]
pub struct Avoid {
    objs: SmallVec<[(u32, u64); MAX_OBJS]>,
    kinds: SmallVec<[Kind; MAX_KINDS]>,
    detour: Option<Detour>,
}

impl Avoid {
    /// Leaves unit `obj` alone until `until_ms`.
    pub fn ignore_obj(&mut self, obj: u32, until_ms: u64) {
        if let Some(e) = self.objs.iter_mut().find(|e| e.0 == obj) {
            e.1 = e.1.max(until_ms);
            return;
        }
        if self.objs.len() >= MAX_OBJS {
            // the one that expires first goes first
            if let Some(i) = self
                .objs
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.1)
                .map(|(i, _)| i)
            {
                self.objs.remove(i);
            }
        }
        self.objs.push((obj, until_ms));
    }

    /// Whether unit `obj` is left alone at `now_ms`.
    pub fn ignores(&self, obj: u32, now_ms: u64) -> bool {
        self.objs.iter().any(|e| e.0 == obj && now_ms < e.1)
    }

    /// The bot could not reach a monster of `template`: the second strike inside the window makes
    /// the bot avoid the whole kind. Returns whether the kind is avoided now.
    pub fn strike(&mut self, template: u32, now_ms: u64) -> bool {
        self.kinds.retain(|k| now_ms < k.until_ms);
        let i = match self.kinds.iter().position(|k| k.template == template) {
            Some(i) => i,
            None => {
                if self.kinds.len() >= MAX_KINDS {
                    self.kinds.remove(0);
                }
                self.kinds.push(Kind {
                    template,
                    strikes: 0,
                    until_ms: now_ms + KIND_WINDOW_MS,
                });
                self.kinds.len() - 1
            }
        };
        let k = &mut self.kinds[i];
        k.strikes = k.strikes.saturating_add(1);
        if k.strikes >= KIND_STRIKES {
            k.until_ms = now_ms + KIND_AVOID_MS;
        }
        k.strikes >= KIND_STRIKES
    }

    /// Whether monsters of `template` are avoided at `now_ms`.
    pub fn avoids_kind(&self, template: u32, now_ms: u64) -> bool {
        self.kinds
            .iter()
            .any(|k| k.template == template && k.strikes >= KIND_STRIKES && now_ms < k.until_ms)
    }

    /// Whether a monster `obj` of `template` should not be picked at `now_ms`.
    pub fn skips(&self, obj: u32, template: u32, now_ms: u64) -> bool {
        self.ignores(obj, now_ms) || self.avoids_kind(template, now_ms)
    }

    /// The waypoint of the trip to `dest` when a monster more than `gap` levels above
    /// `own_level` stands within `aggro_m` of the straight line and less than `lookahead_m`
    /// ahead: a point beside the monster, beyond its aggro radius. The waypoint of the trip in
    /// progress wins until it is reached or [`DETOUR_LIFE_MS`] runs out; a new trip drops it.
    pub fn route_detour(
        &mut self,
        now_ms: u64,
        from: (f32, f32),
        dest: (f32, f32),
        dangers: &[Danger],
        own_level: u8,
        limits: RouteLimits,
    ) -> Option<(f32, f32)> {
        if let Some(d) = self.detour {
            let same_trip = (d.dest.0 - dest.0).abs() <= 2.0 && (d.dest.1 - dest.1).abs() <= 2.0;
            if now_ms >= d.until_ms || !same_trip {
                self.detour = None;
            } else if dist(from, d.via) > DETOUR_ARRIVE_M {
                return Some(d.via);
            } else {
                self.detour = None;
            }
        }
        if limits.aggro_m <= 0.0 {
            return None;
        }
        let (dx, dy) = (dest.0 - from.0, dest.1 - from.1);
        let len = (dx * dx + dy * dy).sqrt();
        if len < 1.0 {
            return None;
        }
        let (ux, uy) = (dx / len, dy / len);
        let reach = limits.lookahead_m.min(len);
        let mut best: Option<(f32, Detour)> = None;
        for danger in dangers {
            if danger.level <= own_level.saturating_add(limits.gap) {
                continue;
            }
            let rx = danger.x - from.0;
            let ry = danger.y - from.1;
            let along = rx * ux + ry * uy;
            if along <= 0.0 || along > reach {
                continue;
            }
            let off = (rx * -uy + ry * ux).abs();
            if off > limits.aggro_m {
                continue;
            }
            // the side the monster is on: step to the other one
            let side = if rx * -uy + ry * ux >= 0.0 { -1.0 } else { 1.0 };
            let clear = limits.aggro_m + DETOUR_MARGIN_M;
            let via = (
                from.0 + ux * along + -uy * side * clear,
                from.1 + uy * along + ux * side * clear,
            );
            if best.is_none_or(|b| along < b.0) {
                best = Some((
                    along,
                    Detour {
                        dest,
                        via,
                        until_ms: now_ms + DETOUR_LIFE_MS,
                    },
                ));
            }
        }
        let (_, detour) = best?;
        self.detour = Some(detour);
        Some(detour.via)
    }
}

/// Distance between two horizontal points.
fn dist(a: (f32, f32), b: (f32, f32)) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(aggro_m: f32, lookahead_m: f32) -> RouteLimits {
        RouteLimits {
            gap: 4,
            aggro_m,
            lookahead_m,
        }
    }

    #[test]
    fn an_unreachable_unit_is_left_alone_until_its_time_is_up() {
        let mut a = Avoid::default();
        a.ignore_obj(7, 10_000);
        assert!(a.ignores(7, 9_999) && !a.ignores(7, 10_000));
        assert!(!a.ignores(8, 0));
        // a longer ignore wins, a shorter one does not shorten it
        a.ignore_obj(7, 20_000);
        a.ignore_obj(7, 12_000);
        assert!(a.ignores(7, 19_999));
    }

    #[test]
    fn units_are_bounded_and_the_oldest_ignore_goes_first() {
        let mut a = Avoid::default();
        for i in 0..20u32 {
            a.ignore_obj(i, 1_000 + u64::from(i));
        }
        assert_eq!(a.objs.len(), MAX_OBJS);
        assert!(a.ignores(19, 0), "the newest stays");
        assert!(!a.ignores(0, 0), "the one that expired first went");
    }

    #[test]
    fn the_second_strike_avoids_the_whole_kind_for_a_while() {
        let mut a = Avoid::default();
        assert!(!a.strike(55, 1_000), "one failure is bad luck");
        assert!(!a.avoids_kind(55, 1_000));
        assert!(a.strike(55, 5_000), "two are a pattern");
        assert!(a.avoids_kind(55, 5_001));
        assert!(a.skips(1, 55, 5_001), "any unit of that kind is skipped");
        assert!(!a.skips(1, 56, 5_001));
        assert!(
            !a.avoids_kind(55, 5_000 + KIND_AVOID_MS),
            "the avoidance runs out"
        );
    }

    #[test]
    fn a_higher_level_monster_on_the_way_gets_a_detour_beside_it() {
        let mut a = Avoid::default();
        let danger = Danger {
            x: 50.0,
            y: 0.0,
            level: 30,
        };
        // 20 levels above: the line passes through its aggro radius
        let via = a
            .route_detour(
                1_000,
                (0.0, 0.0),
                (100.0, 0.0),
                &[danger],
                10,
                limits(15.0, 60.0),
            )
            .expect("a detour");
        assert!(dist(via, (danger.x, danger.y)) > 15.0, "{via:?}");
        assert!(via.0 > 40.0 && via.0 < 60.0, "beside it: {via:?}");
        // the waypoint is kept for this trip
        assert_eq!(
            a.route_detour(
                2_000,
                (10.0, 0.0),
                (100.0, 0.0),
                &[danger],
                10,
                limits(15.0, 60.0)
            ),
            Some(via)
        );
        // reached: the way to the destination goes on
        let at_via = (via.0, via.1);
        assert_eq!(
            a.route_detour(
                3_000,
                at_via,
                (100.0, 0.0),
                &[danger],
                10,
                limits(15.0, 60.0)
            ),
            None
        );
    }

    #[test]
    fn a_fair_monster_a_far_one_or_one_behind_is_no_reason_to_detour() {
        let mut a = Avoid::default();
        let fair = Danger {
            x: 50.0,
            y: 0.0,
            level: 11,
        };
        assert_eq!(
            a.route_detour(
                1_000,
                (0.0, 0.0),
                (100.0, 0.0),
                &[fair],
                10,
                limits(15.0, 60.0)
            ),
            None,
            "within the level gap"
        );
        let beside = Danger {
            x: 50.0,
            y: 40.0,
            level: 40,
        };
        assert_eq!(
            a.route_detour(
                1_000,
                (0.0, 0.0),
                (100.0, 0.0),
                &[beside],
                10,
                limits(15.0, 60.0)
            ),
            None,
            "outside the aggro radius"
        );
        let behind = Danger {
            x: -50.0,
            y: 0.0,
            level: 40,
        };
        assert_eq!(
            a.route_detour(
                1_000,
                (0.0, 0.0),
                (100.0, 0.0),
                &[behind],
                10,
                limits(15.0, 60.0)
            ),
            None,
            "behind the bot"
        );
        let far_ahead = Danger {
            x: 150.0,
            y: 0.0,
            level: 40,
        };
        assert_eq!(
            a.route_detour(
                1_000,
                (0.0, 0.0),
                (200.0, 0.0),
                &[far_ahead],
                10,
                limits(15.0, 60.0)
            ),
            None,
            "beyond the lookahead"
        );
    }

    #[test]
    fn a_detour_expires_and_a_new_trip_drops_it() {
        let mut a = Avoid::default();
        let danger = Danger {
            x: 50.0,
            y: 0.0,
            level: 30,
        };
        assert!(a
            .route_detour(
                1_000,
                (0.0, 0.0),
                (100.0, 0.0),
                &[danger],
                10,
                limits(15.0, 60.0)
            )
            .is_some());
        // another destination: the old waypoint is dropped, this line needs none
        let other_danger = Danger {
            x: 50.0,
            y: 40.0,
            level: 30,
        };
        assert_eq!(
            a.route_detour(
                2_000,
                (0.0, 0.0),
                (100.0, 50.0),
                &[other_danger],
                10,
                limits(5.0, 60.0)
            ),
            None
        );
        // the same trip after its life time asks afresh (and gets a fresh waypoint)
        let via = a
            .route_detour(
                1_000,
                (0.0, 0.0),
                (100.0, 0.0),
                &[danger],
                10,
                limits(15.0, 60.0),
            )
            .unwrap();
        assert_eq!(
            a.route_detour(
                1_000 + DETOUR_LIFE_MS,
                (0.0, 0.0),
                (100.0, 0.0),
                &[danger],
                10,
                limits(15.0, 60.0)
            ),
            Some(via),
            "an expired detour is made again for the same danger"
        );
    }

    #[test]
    fn strikes_expire_and_kinds_are_bounded() {
        let mut a = Avoid::default();
        a.strike(1, 0);
        assert!(
            !a.strike(1, KIND_WINDOW_MS + 1),
            "an old strike does not count"
        );
        for t in 100..120u32 {
            a.strike(t, 10);
        }
        assert!(a.kinds.len() <= MAX_KINDS);
    }
}
