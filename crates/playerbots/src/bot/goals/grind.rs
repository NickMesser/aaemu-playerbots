//! Grind: find monsters of the bot's level, kill them and loot the corpses.
//!
//! The goal picks the nearest fair monster in view ([`attackable`], level window
//! [`level_ok`], normal grade) and hands it to the combat controller, which walks into range,
//! fights and loots. With no monster in view it walks to the nearest static spawn of a fair
//! monster (`World::npc_spawns_within`), remembering the spots it has already searched, and gives
//! up after a few empty searches so the brain can choose something else. It ends after three to
//! eight minutes or when hit points get low.

use std::fmt::Write;

use aa_world::{Relation, Vec3};
use smallvec::SmallVec;

use crate::bot::avoid::UNREACHABLE_MS;
use crate::bot::combat::{attackable, CombatStatus, Loss, FIGHT_TIMEOUT_MS};
use crate::bot::ctx::Ctx;
use crate::bot::motion::{MotionStatus, Pace};

use super::{jitter_ms, level_ok, random_point, walkable, FailReason, GoalStatus};

/// Length of the goal in milliseconds.
const TOTAL_MS: (u64, u64) = (180_000, 480_000);
/// Monsters further away than this (metres) are not considered when in view.
const VIEW_RADIUS: f32 = 70.0;
/// How far to look for spawns when nothing is in view.
const SPAWN_RADIUS: f32 = 160.0;
/// Spawns closer than this are in view already; if nothing is there, they are empty.
const SPAWN_MIN_M: f32 = 12.0;
/// The bot stops this far from a spawn so the monster sees it come.
const SPAWN_STOP_M: f32 = 6.0;
/// Empty searches in a row after which the goal gives up.
const MAX_SEARCHES: u8 = 4;
/// Below this many percent of hit points the goal ends (the brain rests).
const LOW_HP_PCT: u8 = 40;
/// A target that was lost is not picked again for this long. Longer than a fight can last
/// ([`FIGHT_TIMEOUT_MS`]): with a shorter pause the bot walked back to the monster it just gave
/// up and fought it again, for minutes. A target the bot could not reach is left alone for
/// [`UNREACHABLE_MS`].
const IGNORE_MS: u64 = FIGHT_TIMEOUT_MS + 30_000;
const _: () = assert!(IGNORE_MS >= FIGHT_TIMEOUT_MS);
/// Searched spots are remembered in cells of this size.
const CELL_M: f32 = 20.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Phase {
    #[default]
    Scan,
    Walk,
}

/// Hunting monsters. See the module docs.
#[derive(Debug, Default, Clone)]
pub struct Grind {
    end_ms: u64,
    phase: Phase,
    searches: u8,
    kills: u32,
    spot: Option<(f32, f32)>,
    visited: SmallVec<[(i32, i32); 4]>,
    ignore: SmallVec<[(u32, u64); 4]>,
    fighting: Option<u32>,
}

fn cell(x: f32, y: f32) -> (i32, i32) {
    ((x / CELL_M).floor() as i32, (y / CELL_M).floor() as i32)
}

impl Grind {
    /// Called once when the brain picks the goal.
    pub fn start(&mut self, ctx: &mut Ctx<'_>) {
        *self = Grind::default();
        self.end_ms = ctx.now_ms + jitter_ms(ctx.rng, TOTAL_MS.0, TOTAL_MS.1);
    }

    /// One tick; `combat` is the controller's verdict of this tick.
    pub fn tick(&mut self, ctx: &mut Ctx<'_>, combat: CombatStatus) -> GoalStatus {
        let now = ctx.now_ms;
        match combat {
            CombatStatus::TargetDead { .. } => {
                self.kills += 1;
                self.searches = 0;
                self.fighting = None;
            }
            CombatStatus::Lost => {
                if let Some(obj) = self.fighting.take() {
                    self.give_up(ctx, obj);
                }
            }
            _ => {}
        }
        if ctx.combat.busy() {
            return GoalStatus::Running;
        }
        if now >= self.end_ms || ctx.state.me.hp_pct() < LOW_HP_PCT {
            return GoalStatus::Done;
        }
        if let Some(obj) = self.pick_target(ctx) {
            if self.phase == Phase::Walk {
                ctx.motion.stop();
                self.phase = Phase::Scan;
            }
            self.searches = 0;
            self.fighting = Some(obj);
            ctx.combat.engage(obj);
            return GoalStatus::Running;
        }
        match self.phase {
            Phase::Walk => match ctx.motion.status() {
                MotionStatus::Moving => GoalStatus::Running,
                _ => {
                    // Arrived (or blocked) and nothing to fight: the spot is empty.
                    if let Some((x, y)) = self.spot.take() {
                        self.mark_visited(x, y);
                    }
                    self.phase = Phase::Scan;
                    self.empty_search()
                }
            },
            Phase::Scan => {
                if let Some((x, y)) = self.find_spawn(ctx) {
                    ctx.travel(x, y, SPAWN_STOP_M, Pace::Run);
                    self.spot = Some((x, y));
                    self.phase = Phase::Walk;
                    return GoalStatus::Running;
                }
                // No known spawn to go to: move on a little to see more of the world.
                let me = ctx.pos();
                match random_point(ctx.world, ctx.rng, me, 25.0, 60.0, 8) {
                    Some((x, y)) => {
                        ctx.motion.go_to(x, y, 2.0, Pace::Run, now);
                        self.spot = None;
                        self.phase = Phase::Walk;
                        GoalStatus::Running
                    }
                    None => self.empty_search(),
                }
            }
        }
    }

    /// Called when the goal ends.
    pub fn stop(&mut self, ctx: &mut Ctx<'_>) {
        ctx.combat.disengage();
        if self.phase == Phase::Walk {
            ctx.motion.stop();
        }
        self.fighting = None;
    }

    /// Appends what the bot is doing.
    pub fn describe(&self, out: &mut String) {
        let _ = match (self.fighting, self.phase) {
            (Some(_), _) => write!(out, "hunting, {} kills", self.kills),
            (None, Phase::Walk) => write!(out, "looking for monsters, {} kills", self.kills),
            (None, Phase::Scan) => write!(out, "scanning, {} kills", self.kills),
        };
    }

    /// The controller gave up monster `obj`: leave it alone for a while, a long while when the
    /// bot could not hurt it (and its whole kind after the second time).
    fn give_up(&mut self, ctx: &mut Ctx<'_>, obj: u32) {
        let now = ctx.now_ms;
        if !ctx.combat.last_loss().is_some_and(Loss::unreachable) {
            self.ignore_for(obj, now + IGNORE_MS);
            return;
        }
        let until = now + UNREACHABLE_MS;
        let template = ctx.state.units.get(&obj).map(|u| u.id);
        let avoid = ctx.state.avoid_mut();
        avoid.ignore_obj(obj, until);
        if let Some(t) = template {
            if avoid.strike(t, now) {
                ctx.log.say(format!(
                    "grind: leaving monsters of kind {t} alone, unreachable"
                ));
            }
        }
        ctx.log.debug(format!("grind: unit {obj} is out of reach"));
    }

    fn empty_search(&mut self) -> GoalStatus {
        self.searches += 1;
        if self.searches >= MAX_SEARCHES {
            GoalStatus::Failed(FailReason::NoTarget)
        } else {
            GoalStatus::Running
        }
    }

    fn mark_visited(&mut self, x: f32, y: f32) {
        if self.visited.len() >= 4 {
            self.visited.remove(0);
        }
        self.visited.push(cell(x, y));
    }

    fn ignore_for(&mut self, obj: u32, until_ms: u64) {
        if self.ignore.len() >= 4 {
            self.ignore.remove(0);
        }
        self.ignore.push((obj, until_ms));
    }

    fn ignored(&self, obj: u32, now_ms: u64) -> bool {
        self.ignore.iter().any(|i| i.0 == obj && now_ms < i.1)
    }

    /// The nearest fair monster in view.
    fn pick_target(&self, ctx: &Ctx<'_>) -> Option<u32> {
        let me = ctx.state.me.pos;
        let (own_faction, own_level) = (ctx.state.me.faction, ctx.state.me.level);
        let mut best: Option<(u32, bool, f32)> = None;
        for (&obj, u) in &ctx.state.units {
            if self.ignored(obj, ctx.now_ms) || ctx.state.avoids(obj, u.id, ctx.now_ms) {
                continue;
            }
            let d = u.dist2d(me);
            if d > VIEW_RADIUS {
                continue;
            }
            let Some(t) = attackable(ctx.world, own_faction, u) else {
                continue;
            };
            let level = if u.level != 0 { u.level } else { t.level };
            if t.grade > 1 || !level_ok(own_level, level) {
                continue;
            }
            // Monsters another character tagged (or is fighting) are left for them while a
            // free one is in view: the kill would bring the grinder no credit anyway.
            let taken = ctx.state.taken_by_other(obj);
            if best.is_none_or(|b| better_target(taken, d, b.1, b.2)) {
                best = Some((obj, taken, d));
            }
        }
        best.map(|b| b.0)
    }

    /// The nearest static spawn of a fair monster that has not been searched.
    fn find_spawn(&self, ctx: &Ctx<'_>) -> Option<(f32, f32)> {
        let me = ctx.state.me.pos;
        let (own_faction, own_level) = (ctx.state.me.faction, ctx.state.me.level);
        let mut best: Option<(f32, f32, f32)> = None;
        ctx.world.npc_spawns_within(
            Vec3::new(me[0], me[1], me[2]),
            SPAWN_RADIUS,
            |template, p| {
                let d2 = (p.x - me[0]).powi(2) + (p.y - me[1]).powi(2);
                if d2 < SPAWN_MIN_M * SPAWN_MIN_M || best.is_some_and(|b| d2 >= b.2) {
                    return;
                }
                if self.visited.contains(&cell(p.x, p.y))
                    || ctx.state.avoids(0, template, ctx.now_ms)
                    || ctx.in_war_zone(p.x, p.y)
                {
                    return;
                }
                let Some(t) = ctx.world.npc(template) else {
                    return;
                };
                if t.grade > 1
                    || !level_ok(own_level, t.level)
                    || ctx.world.relation(own_faction, t.faction_id) != Relation::Hostile
                    || !walkable(ctx.world, p.x, p.y)
                {
                    return;
                }
                best = Some((p.x, p.y, d2));
            },
        );
        best.map(|b| (b.0, b.1))
    }
}

/// Whether target A (`taken` by another character, `d` metres away) is a better pick than the
/// best so far (`best_taken`, `best_d`): free beats taken, then the nearer wins.
fn better_target(taken: bool, d: f32, best_taken: bool, best_d: f32) -> bool {
    match (taken, best_taken) {
        (false, true) => true,
        (true, false) => false,
        _ => d < best_d,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn searched_cells_and_ignored_targets_are_bounded() {
        let mut g = Grind::default();
        for i in 0..10 {
            g.mark_visited(i as f32 * 100.0, 0.0);
            g.ignore_for(i, 1000);
        }
        assert_eq!(g.visited.len(), 4);
        assert_eq!(g.ignore.len(), 4);
        assert!(g.visited.contains(&cell(900.0, 0.0)));
        assert!(!g.visited.contains(&cell(0.0, 0.0)));
    }

    #[test]
    fn ignored_targets_come_back_after_their_time() {
        let mut g = Grind::default();
        g.ignore_for(7, 5000);
        assert!(g.ignored(7, 4999));
        assert!(!g.ignored(7, 5000));
        assert!(!g.ignored(8, 0));
    }

    #[test]
    fn empty_searches_end_the_goal() {
        let mut g = Grind::default();
        for _ in 0..MAX_SEARCHES - 1 {
            assert_eq!(g.empty_search(), GoalStatus::Running);
        }
        assert_eq!(g.empty_search(), GoalStatus::Failed(FailReason::NoTarget));
    }

    #[test]
    fn cells_group_nearby_points() {
        assert_eq!(cell(1.0, 1.0), cell(19.9, 19.9));
        assert_ne!(cell(19.9, 0.0), cell(20.1, 0.0));
        assert_eq!(cell(-0.1, 0.0).0, -1);
    }

    #[test]
    fn describe_reports_the_phase() {
        let mut g = Grind::default();
        let mut s = String::new();
        g.describe(&mut s);
        assert_eq!(s, "scanning, 0 kills");
        g.fighting = Some(3);
        g.kills = 2;
        s.clear();
        g.describe(&mut s);
        assert_eq!(s, "hunting, 2 kills");
    }
}
