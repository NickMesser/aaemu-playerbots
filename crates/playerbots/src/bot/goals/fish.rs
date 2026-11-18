//! Sport fishing at an active school and selling the resulting backpack to a fish stand.

use aa_proto::{cs, sc_world::BACKPACK_SLOT};
use aa_world::{doodad::FishSale, Vec3};

use super::{FailReason, GoalStatus};
use crate::bot::ctx::Ctx;
use crate::bot::pvp::walk::{Walk, Walker};
use crate::bot::state::{dist3d, UnitKind, U_MY_SUMMON};

const CAST: u32 = 21578;
const LURE: u32 = 27319;
const REEL: [u32; 5] = [21135, 21194, 21195, 21196, 21290];
const SEA_Z: f32 = 100.0;

fn backpack<'a>(ctx: &'a Ctx<'_>) -> Option<&'a aa_proto::sc_world::EquipSlot> {
    ctx.state
        .equipment
        .iter()
        .find(|item| item.slot == BACKPACK_SLOT)
}

pub fn carries_catch(ctx: &Ctx<'_>) -> bool {
    ctx.cfg.behavior.sport_fishing && carried_catch_in(ctx.world, &ctx.state.equipment)
}

pub fn carried_catch_in(
    world: &aa_world::World,
    equipment: &[aa_proto::sc_world::EquipSlot],
) -> bool {
    equipment
        .iter()
        .find(|item| item.slot == BACKPACK_SLOT)
        .is_some_and(|pack| {
            world
                .fish_sales()
                .iter()
                .any(|sale| sale.item == pack.template)
        })
}

fn cast_point(ctx: &Ctx<'_>) -> Option<([f32; 3], u32)> {
    let skill = ctx.world.skill(CAST)?;
    if backpack(ctx).is_some()
        || ctx.state.item_count(LURE) == 0
        || i32::from(ctx.state.me.labor) < i32::from(skill.labor)
        || !ctx.state.cooldown_ready(CAST, ctx.now_ms)
        || ctx.world.fish_sales().is_empty()
    {
        return None;
    }
    let me = ctx.pos();
    let school = ctx
        .state
        .doodads
        .values()
        .filter(|d| !ctx.world.school_fish(d.phase).is_empty() && dist3d(me, d.pos) < 95.0)
        .min_by(|a, b| dist3d(me, a.pos).total_cmp(&dist3d(me, b.pos)))?;
    let dx = school.pos[0] - me[0];
    let dy = school.pos[1] - me[1];
    let distance = dx.hypot(dy);
    if distance < 1.0 {
        return None;
    }
    let scale = distance.clamp(12.0, 25.0) / distance;
    let at = [me[0] + dx * scale, me[1] + dy * scale, SEA_Z];
    (ctx.world.ground_z(at[0], at[1]).is_some_and(|z| z < 99.0) && !ctx.in_war_zone(at[0], at[1]))
        .then_some((at, school.phase))
}

pub fn score(ctx: &Ctx<'_>) -> f32 {
    if !ctx.cfg.behavior.sport_fishing || ctx.state.place.in_instance() || ctx.state.place.loading()
    {
        return 0.0;
    }
    if carries_catch(ctx) {
        return 8.0;
    }
    if ctx.state.me.hp_pct() >= 60 && cast_point(ctx).is_some() {
        1.0
    } else {
        0.0
    }
}

#[derive(Debug, Default)]
enum Step {
    #[default]
    Start,
    Hook {
        at: [f32; 3],
        phase: u32,
        deadline: u64,
    },
    Reel {
        object: u32,
        due: u64,
        deadline: u64,
    },
    Loot {
        deadline: u64,
    },
    TurnIn {
        destination: [f32; 3],
        deadline: u64,
    },
    Payment {
        id: u64,
        money: i64,
        refund: i64,
        deadline: u64,
    },
}

#[derive(Debug, Default)]
pub struct Fish {
    step: Step,
    walker: Walker,
}

impl Fish {
    pub fn tick(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        use Step::*;
        if !ctx.cfg.behavior.sport_fishing || ctx.state.place.in_instance() {
            return GoalStatus::Failed(FailReason::NoTarget);
        }
        let now = ctx.now_ms;
        let timeout = GoalStatus::Failed(FailReason::Timeout);
        match self.step {
            Start => {
                if carries_catch(ctx) {
                    return self.begin_turn_in(ctx);
                }
                let Some((at, phase)) = cast_point(ctx) else {
                    return GoalStatus::Failed(FailReason::NoTarget);
                };
                ctx.motion.stop();
                ctx.motion.face(at[0], at[1]);
                ctx.send(cs::start_skill_position(
                    CAST,
                    ctx.state.me.obj,
                    (at[0], at[1], at[2]),
                    ctx.motion.yaw(),
                ));
                self.step = Hook {
                    at,
                    phase,
                    deadline: now + 45_000,
                };
            }
            Hook {
                at,
                phase,
                deadline,
            } => {
                if now >= deadline {
                    return timeout;
                }
                if let Some((&object, _)) = ctx.state.units.iter().find(|(_, fish)| {
                    fish.kind == UnitKind::Npc
                        && fish.flags & U_MY_SUMMON != 0
                        && !fish.dead()
                        && ctx.world.school_fish(phase).contains(&fish.id)
                        && dist3d(at, fish.pos) < 15.0
                }) {
                    self.step = Reel {
                        object,
                        due: now,
                        deadline: now + 120_000,
                    };
                }
            }
            Reel {
                object,
                due,
                deadline,
            } => {
                if now >= deadline {
                    return timeout;
                }
                let Some(unit) = ctx.state.units.get(&object) else {
                    return GoalStatus::Failed(FailReason::NoTarget);
                };
                if unit.kind != UnitKind::Npc || unit.flags & U_MY_SUMMON == 0 {
                    return GoalStatus::Failed(FailReason::NoTarget);
                }
                let pos = unit.pos;
                if unit.dead() {
                    if !unit.has_loot() {
                        return GoalStatus::Running;
                    }
                    match self.walker.step(ctx, [pos[0], pos[1]], 1.5) {
                        Walk::Going => return GoalStatus::Running,
                        Walk::Stuck => return GoalStatus::Failed(FailReason::Stuck),
                        Walk::There => {}
                    }
                    ctx.send(cs::loot_open_bag(object, true));
                    self.step = Loot {
                        deadline: now + 8_000,
                    };
                } else if now >= due {
                    if let Some(skill) =
                        REEL.iter()
                            .filter_map(|id| ctx.world.skill(*id))
                            .find(|skill| {
                                ctx.state.cooldown_ready(skill.id, now)
                                    && (f32::from(skill.min_range)..=f32::from(skill.max_range))
                                        .contains(&dist3d(ctx.pos(), pos))
                            })
                    {
                        ctx.send(cs::start_skill_unit(skill.id, ctx.state.me.obj, object));
                        self.step = Reel {
                            object,
                            due: now + u64::from(skill.casting_time).max(1_500),
                            deadline,
                        };
                    }
                }
            }
            Loot { deadline } => {
                if carries_catch(ctx) {
                    return self.begin_turn_in(ctx);
                }
                if now >= deadline {
                    return timeout;
                }
            }
            TurnIn {
                destination,
                deadline,
            } => {
                if now >= deadline {
                    return timeout;
                }
                let Some(pack) = backpack(ctx) else {
                    return GoalStatus::Failed(FailReason::NoTarget);
                };
                let (id, template) = (pack.id, pack.template);
                if let Some((object, sale)) = nearby_stand(ctx, template) {
                    let money = ctx.state.me.money;
                    ctx.send(cs::start_skill_doodad(sale.skill, ctx.state.me.obj, object));
                    self.step = Payment {
                        id,
                        money,
                        refund: i64::from(sale.copper),
                        deadline: now + 12_000,
                    };
                } else {
                    match self.walker.step(ctx, [destination[0], destination[1]], 1.5) {
                        Walk::Going => {}
                        Walk::Stuck => return GoalStatus::Failed(FailReason::Stuck),
                        Walk::There => {
                            self.step = TurnIn {
                                destination,
                                deadline: deadline.min(now + 15_000),
                            };
                        }
                    }
                }
            }
            Payment {
                id,
                money,
                refund,
                deadline,
            } => {
                if backpack(ctx).is_none_or(|pack| pack.id != id)
                    && ctx.state.me.money >= money.saturating_add(refund)
                {
                    ctx.hub.journal.update(ctx.persona.id, |j| {
                        j.fish_turn_ins = j.fish_turn_ins.saturating_add(1);
                        j.fish_sale_copper = j.fish_sale_copper.saturating_add(refund as u64);
                    });
                    ctx.log
                        .say(format!("sport fishing: sold a catch for {refund} copper"));
                    return GoalStatus::Done;
                }
                if now >= deadline {
                    return timeout;
                }
            }
        }
        GoalStatus::Running
    }

    fn begin_turn_in(&mut self, ctx: &Ctx<'_>) -> GoalStatus {
        let Some(pack) = backpack(ctx) else {
            return GoalStatus::Failed(FailReason::NoTarget);
        };
        let me = ctx.pos();
        let from = Vec3::new(me[0], me[1], me[2]);
        let dest = ctx
            .world
            .fish_sales()
            .iter()
            .filter(|sale| sale.item == pack.template)
            .filter_map(|sale| ctx.world.nearest_doodad_spawn(sale.stand, from))
            .filter(|p| !ctx.in_war_zone(p.x, p.y))
            .min_by(|a, b| a.dist2d(from).total_cmp(&b.dist2d(from)));
        let destination = if nearby_stand(ctx, pack.template).is_some() {
            me
        } else if let Some(p) = dest {
            [p.x, p.y, p.z]
        } else {
            return GoalStatus::Failed(FailReason::NoTarget);
        };
        self.walker.reset();
        self.step = Step::TurnIn {
            destination,
            deadline: ctx.now_ms + 20 * 60_000,
        };
        GoalStatus::Running
    }

    pub fn describe(&self, out: &mut String) {
        out.push_str(match self.step {
            Step::Start | Step::Hook { .. } => "Casting for a sport fish",
            Step::Reel { .. } => "Reeling in a sport fish",
            Step::Loot { .. } => "Taking the catch",
            Step::TurnIn { .. } | Step::Payment { .. } => "Selling the catch at a fish stand",
        });
    }
}

fn nearby_stand(ctx: &Ctx<'_>, item: u32) -> Option<(u32, FishSale)> {
    ctx.state
        .doodads
        .iter()
        .filter(|(_, d)| dist3d(ctx.pos(), d.pos) <= 3.0)
        .find_map(|(&id, d)| {
            ctx.world
                .fish_sales()
                .iter()
                .find(|s| s.stand == d.template && s.phase == d.phase && s.item == item)
                .map(|sale| (id, *sale))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archetype::Archetypes;
    use crate::bot::combat::Combat;
    use crate::bot::coordinator::CoordHandle;
    use crate::bot::ctx::Persona;
    use crate::bot::motion::{Motion, MotionCfg};
    use crate::bot::state::{BotState, Doodad};
    use crate::config::Config;
    use crate::hub::{Hub, HubParams, Logger};
    use crate::roster::{Profile, Temperament};
    use aa_proto::sc_world::EquipSlot;

    #[test]
    #[ignore = "needs AA_COMPACT and AA_SERVER_DATA"]
    fn a_fish_stand_turn_in_requires_both_the_purse_and_equipment_updates() {
        let world = aa_world::World::load(&aa_world::WorldPaths {
            compact: std::env::var_os("AA_COMPACT").expect("AA_COMPACT").into(),
            server_data: std::env::var_os("AA_SERVER_DATA")
                .expect("AA_SERVER_DATA")
                .into(),
            baked: None,
            world: "main_world".into(),
        })
        .unwrap();
        let sale = *world.fish_sales().iter().find(|s| s.item == 27602).unwrap();
        let mut cfg = Config::default();
        cfg.behavior.sport_fishing = true;
        let persona = Persona {
            id: 1,
            name: "Fisher".into(),
            archetype: Archetypes::builtin()
                .unwrap()
                .get("Darkrunner")
                .unwrap()
                .clone(),
            temper: Temperament::NEUTRAL,
            profile: Profile::NEUTRAL,
        };
        let (tx, _) = tokio::sync::mpsc::channel(1);
        let hub = Hub::new(HubParams {
            world: &world,
            baked_dir: None,
            supervisor: tx,
            llm: None,
        });
        let log = Logger::new(hub.register(pb_web::BotSnapshot {
            id: 1,
            ..Default::default()
        }));
        let coord = CoordHandle::detached();
        let mut state = BotState::new();
        state.me.obj = 75;
        state.me.pos = [100.0, 100.0, 100.0];
        state.me.money = 5;
        state.equipment.push(EquipSlot {
            slot: BACKPACK_SLOT,
            template: sale.item,
            id: 20,
            grade: 0,
        });
        state.doodads.insert(
            40,
            Doodad {
                template: sale.stand,
                phase: sale.phase,
                pos: state.me.pos,
            },
        );
        let mut motion = Motion::new(MotionCfg::default());
        let mut combat = Combat::new();
        let mut out = Vec::new();
        let mut rng = fastrand::Rng::with_seed(1);
        let mut ctx = Ctx {
            world: &world,
            cfg: &cfg,
            persona: &persona,
            state: &mut state,
            motion: &mut motion,
            combat: &mut combat,
            out: &mut out,
            coord: &coord,
            hub: &hub,
            log: &log,
            rng: &mut rng,
            now: tokio::time::Instant::now(),
            now_ms: 1000,
            real_player_near: false,
        };
        let mut goal = Fish::default();
        assert_eq!(goal.tick(&mut ctx), GoalStatus::Running);
        assert_eq!(goal.tick(&mut ctx), GoalStatus::Running);
        assert_eq!(*ctx.out, [cs::start_skill_doodad(sale.skill, 75, 40)]);
        ctx.state.equipment.clear();
        assert_eq!(goal.tick(&mut ctx), GoalStatus::Running);
        assert_eq!(hub.journal.read(1, |j| j.fish_turn_ins), 0);
        ctx.state.me.money += i64::from(sale.copper);
        assert_eq!(goal.tick(&mut ctx), GoalStatus::Done);
        assert_eq!(
            hub.journal
                .read(1, |j| (j.fish_turn_ins, j.fish_sale_copper)),
            (1, u64::from(sale.copper))
        );
        assert_eq!(ctx.out.len(), 1);
    }
}
