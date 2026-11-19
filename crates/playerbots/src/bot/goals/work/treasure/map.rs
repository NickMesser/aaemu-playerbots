//! Follow a coordinate map to its authored dig spot, then take the chest loot.

use aa_proto::cs;
use aa_world::geom::Vec3;
use aa_world::water::SEA_LEVEL_M;

use super::{dist3d, Ctx, FailReason, GoalStatus, Walk, Walker};
use crate::contest::doodad_key;

pub(crate) const MAP: u32 = 24_581;
const DIG: u32 = 18_349;
const LABOR: i16 = 50;
const MAX_DISTANCE: f32 = 600.0;

#[derive(Clone, Copy, Debug)]
struct Chest {
    template: u32,
    buried: u32,
    exposed: u32,
}

const CHESTS: [Chest; 5] = [
    Chest {
        template: 5_089,
        buried: 13_212,
        exposed: 13_209,
    },
    Chest {
        template: 5_172,
        buried: 13_478,
        exposed: 13_479,
    },
    Chest {
        template: 5_173,
        buried: 13_627,
        exposed: 13_628,
    },
    Chest {
        template: 5_174,
        buried: 13_482,
        exposed: 13_483,
    },
    Chest {
        template: 5_175,
        buried: 13_631,
        exposed: 13_632,
    },
];

#[derive(Debug)]
enum Stage {
    Approach { arrived_ms: Option<u64> },
    Digging { object: u32 },
    Taking { object: u32, receipts: u64 },
}

#[derive(Debug)]
pub(crate) struct Hunt {
    coordinates: [i64; 2],
    maps: usize,
    chest: Chest,
    spot: [f32; 3],
    deadline: u64,
    stage: Stage,
}

fn matching_maps(ctx: &Ctx<'_>, coordinates: [i64; 2]) -> usize {
    ctx.state
        .treasure_maps
        .values()
        .filter(|&&pos| pos == coordinates)
        .count()
}

pub(super) fn find(ctx: &Ctx<'_>) -> Option<Hunt> {
    if ctx.state.me.labor < LABOR || ctx.state.item_count(MAP) == 0 {
        return None;
    }
    let me = ctx.pos();
    let (coordinates, chest, spot) = ctx
        .state
        .treasure_maps
        .values()
        .copied()
        .filter_map(|pos| {
            let [x, y] = pos.map(|n| n as f32 / 4_096.0);
            if (me[0] - x).hypot(me[1] - y) > MAX_DISTANCE
                || ctx.world.zone_closed_at(x, y)
                || ctx.in_war_zone(x, y)
            {
                return None;
            }
            let pin = Vec3::new(x, y, 0.0);
            CHESTS.into_iter().find_map(|chest| {
                ctx.world
                    .doodad_spawns(chest.template)
                    .iter()
                    .find(|point| point.z >= SEA_LEVEL_M && point.dist2d(pin) < 0.75)
                    .map(|point| (pos, chest, [point.x, point.y, point.z]))
            })
        })
        .min_by(|(a, _, p), (b, _, q)| dist3d(me, *p).total_cmp(&dist3d(me, *q)).then(a.cmp(b)))?;
    Some(Hunt {
        coordinates,
        maps: matching_maps(ctx, coordinates),
        chest,
        spot,
        deadline: ctx.now_ms + 150_000,
        stage: Stage::Approach { arrived_ms: None },
    })
}

impl Hunt {
    pub(super) fn tick(&mut self, ctx: &mut Ctx<'_>, walker: &mut Walker) -> GoalStatus {
        if let Stage::Taking { object, receipts } = self.stage {
            let gone = ctx
                .state
                .doodads
                .get(&object)
                .is_none_or(|chest| chest.phase != self.chest.exposed);
            if gone && ctx.state.doodad_loot_receipts > receipts {
                ctx.hub.journal.update(ctx.persona.id, |journal| {
                    journal.treasure_chests = journal.treasure_chests.saturating_add(1);
                });
                ctx.log
                    .say("treasure: took loot from a chest marked on a map");
                return GoalStatus::Done;
            }
        }
        if ctx.now_ms >= self.deadline {
            return GoalStatus::Failed(FailReason::Timeout);
        }
        match &mut self.stage {
            Stage::Approach { arrived_ms } => {
                if matching_maps(ctx, self.coordinates) == 0 || ctx.state.me.labor < LABOR {
                    return GoalStatus::Failed(FailReason::NoTarget);
                }
                let [x, y, _] = self.spot;
                if dist3d(ctx.pos(), self.spot) > 3.5 {
                    return match walker.step(ctx, [x, y], 1.5) {
                        Walk::Stuck => GoalStatus::Failed(FailReason::Stuck),
                        _ => GoalStatus::Running,
                    };
                }
                let object = ctx.state.doodads.iter().find_map(|(&object, d)| {
                    (d.template == self.chest.template
                        && d.phase == self.chest.buried
                        && dist3d(d.pos, self.spot) <= 1.0
                        && dist3d(d.pos, ctx.pos()) <= 3.5)
                        .then_some(object)
                });
                let Some(object) = object else {
                    let since = *arrived_ms.get_or_insert(ctx.now_ms);
                    return if ctx.now_ms - since < 3_000 {
                        GoalStatus::Running
                    } else {
                        GoalStatus::Failed(FailReason::NoTarget)
                    };
                };
                if ctx.in_war_zone(x, y)
                    || !ctx
                        .hub
                        .contest
                        .try_claim(doodad_key(object), ctx.persona.id)
                {
                    return GoalStatus::Failed(FailReason::NoTarget);
                }
                ctx.motion.stop();
                self.maps = matching_maps(ctx, self.coordinates);
                self.stage = Stage::Digging { object };
                self.deadline = ctx.now_ms + 10_000;
                ctx.send(cs::start_skill_doodad(DIG, ctx.state.me.obj, object));
            }
            Stage::Digging { object } => {
                if ctx
                    .state
                    .doodads
                    .get(object)
                    .is_some_and(|d| d.phase == self.chest.exposed)
                    && matching_maps(ctx, self.coordinates) < self.maps
                {
                    ctx.send(cs::loot_open_bag(*object, true));
                    self.stage = Stage::Taking {
                        object: *object,
                        receipts: ctx.state.doodad_loot_receipts,
                    };
                    self.deadline = ctx.now_ms + 8_000;
                }
            }
            Stage::Taking { .. } => {}
        }
        GoalStatus::Running
    }

    pub(super) fn stop(&self, ctx: &Ctx<'_>) {
        if let Stage::Digging { object } | Stage::Taking { object, .. } = self.stage {
            ctx.hub.contest.release(doodad_key(object), ctx.persona.id);
        }
    }
}
