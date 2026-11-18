//! Sport fishing at an active school and selling the resulting backpack to a fish stand.

use aa_proto::{cs, sc_world::BACKPACK_SLOT};
use aa_world::{
    doodad::{FishChum, FishSale},
    gear::equip_slot,
    Vec3,
};

use super::{FailReason, GoalStatus};
use crate::bot::ctx::Ctx;
use crate::bot::pvp::walk::{Walk, Walker};
use crate::bot::state::{dist3d, UnitKind, U_MY_SUMMON};

const CAST: u32 = 21578;
const LURE: u32 = 27319;
const FRESHWATER_SCHOOL: u32 = 6447;
const FRESHWATER_CHUM: u32 = 27313;
const BANK_SEARCH_M: f32 = 450.0;
// The sport-cast plot channels for up to 220 seconds and rolls for a bite every 20 seconds.
const HOOK_WAIT_MS: u64 = 230_000;
// Hooked fish can stay in the fight for up to ten minutes. Their escape removes them from view.
const FIGHT_WAIT_MS: u64 = 600_000;
// Each reel action is a 5.6-second repeating plot. Start it on a new cue to
// keep its checks in the same direction and avoid overlapping two plots.
const REEL_CHANNEL_MS: u64 = 5_600;
const REEL_CUE_MAX_AGE_MS: u64 = 1_500;
const FOLLOW_FISH_M: f32 = 45.0;
const KEEP_FISH_M: f32 = 30.0;
const LOOT_REACH_M: f32 = 4.0;
// A fish can surface at the bobber when it dies, some distance from the pursuer.
const LAND_WAIT_MS: u64 = 90_000;
const ROD_CHANGE_BUFF: u32 = 5795;
const ROD_CHANGE_MS: u64 = 40_000;
// Unwrapped sport rods in the r208022 item catalogue.
const RODS: [u32; 13] = [
    27308, 27309, 27310, 27311, 27312, 27367, 27368, 27370, 29702, 30764, 30765, 30772, 30773,
];

pub(crate) fn response_to(buff: u32) -> Option<u32> {
    match buff {
        5264 => Some(21135), // right
        5265 => Some(21194), // left
        5266 => Some(21196), // dive
        5267 => Some(21195), // run
        5508 => Some(21290), // jump
        _ => None,
    }
}

fn rod_changing(ctx: &Ctx<'_>) -> bool {
    ctx.state.buffs.iter().any(|&(_, id)| id == ROD_CHANGE_BUFF)
}

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

fn has_supplies(ctx: &Ctx<'_>) -> bool {
    ctx.world.skill(CAST).is_some_and(|skill| {
        backpack(ctx).is_none()
            && ctx.state.item_count(LURE) > 0
            && i32::from(ctx.state.me.labor) >= i32::from(skill.labor)
            && ctx.state.cooldown_ready(CAST, ctx.now_ms)
            && !ctx.world.fish_sales().is_empty()
            && (mainhand(ctx).is_some_and(|item| RODS.contains(&item.template))
                || bag_rod(ctx).is_some())
    })
}

fn mainhand<'a>(ctx: &'a Ctx<'_>) -> Option<&'a aa_proto::sc_world::EquipSlot> {
    ctx.state
        .equipment
        .iter()
        .find(|item| item.slot == equip_slot::MAINHAND)
}

fn bag_rod(ctx: &Ctx<'_>) -> Option<(u8, u64)> {
    RODS.iter()
        .filter_map(|&template| {
            let (slot, item) = ctx.state.find_item(template)?;
            let slot = u8::try_from(slot).ok()?;
            let (group, required) = ctx
                .world
                .item_table()
                .proficiency_requirement(template)
                .unwrap_or((0, 0));
            (ctx.state.econ.proficiency(group) >= required).then_some(((slot, item.id), required))
        })
        .max_by_key(|(_, required)| *required)
        .map(|(rod, _)| rod)
}

fn cast_point(ctx: &Ctx<'_>) -> Option<[f32; 3]> {
    let me = ctx.pos();
    ctx.state
        .doodads
        .values()
        .filter(|d| !ctx.world.school_fish(d.phase).is_empty() && dist3d(me, d.pos) < 95.0)
        .filter_map(|school| {
            let at = water_towards(ctx, me, school.pos)?;
            Some((dist3d(me, school.pos), at))
        })
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, at)| at)
}

fn water_towards(ctx: &Ctx<'_>, me: [f32; 3], school: [f32; 3]) -> Option<[f32; 3]> {
    let dx = school[0] - me[0];
    let dy = school[1] - me[1];
    let distance = dx.hypot(dy);
    if distance < 1.0 {
        return None;
    }
    let scale = distance.clamp(12.0, 25.0) / distance;
    let at = [me[0] + dx * scale, me[1] + dy * scale, school[2]];
    let skill = ctx.world.skill(CAST)?;
    ((f32::from(skill.min_range)..=f32::from(skill.max_range)).contains(&dist3d(me, at))
        && ctx
            .world
            .ground_z(at[0], at[1])
            .is_some_and(|z| z < at[2] - 0.75)
        && !ctx.in_war_zone(at[0], at[1]))
    .then_some(at)
}

fn chum_target(ctx: &Ctx<'_>) -> Option<(u32, [f32; 3], FishChum, u64)> {
    ctx.state.doodads.iter().find_map(|(&object, school)| {
        let options = ctx.world.school_chum(school.phase);
        if options.is_empty() || !ctx.world.school_fish(school.phase).is_empty() {
            return None;
        }
        // The chum affects a 25 m circle at the cast point.
        let at = water_towards(ctx, ctx.pos(), school.pos)?;
        if dist3d(at, school.pos) > 24.0 {
            return None;
        }
        let distance = dist3d(ctx.pos(), at);
        options.iter().find_map(|&chum| {
            let skill = ctx.world.skill(chum.skill)?;
            if !(f32::from(skill.min_range)..=f32::from(skill.max_range)).contains(&distance)
                || !ctx.state.cooldown_ready(chum.skill, ctx.now_ms)
            {
                return None;
            }
            ctx.state
                .find_item(chum.item)
                .map(|(_, item)| (object, at, chum, item.id))
        })
    })
}

fn bank_near_school(ctx: &Ctx<'_>) -> Option<[f32; 2]> {
    if ctx.state.item_count(FRESHWATER_CHUM) == 0 {
        return None;
    }
    let [x, y, z] = ctx.pos();
    let from = Vec3::new(x, y, z);
    let school = ctx.world.nearest_doodad_spawn(FRESHWATER_SCHOOL, from)?;
    if school.z <= super::super::motion::SEA_LEVEL
        || school.dist2d(from) > BANK_SEARCH_M
        || ctx.in_war_zone(school.x, school.y)
    {
        return None;
    }
    (0..32)
        .filter_map(|index| {
            let angle = index as f32 * std::f32::consts::TAU / 32.0;
            let (sin, cos) = angle.sin_cos();
            let (x, y) = (school.x + 43.0 * cos, school.y + 43.0 * sin);
            let z = ctx.world.ground_z(x, y)?;
            if !(school.z + 0.25..=school.z + 6.0).contains(&z) || ctx.in_war_zone(x, y) {
                return None;
            }
            water_towards(ctx, [x, y, z], [school.x, school.y, school.z])?;
            Some(([x, y], Vec3::new(x, y, z).dist2d_sq(from)))
        })
        .min_by(|(_, left), (_, right)| left.total_cmp(right))
        .map(|(pos, _)| pos)
}

pub fn score(ctx: &Ctx<'_>) -> f32 {
    if !ctx.cfg.behavior.sport_fishing || ctx.state.place.in_instance() || ctx.state.place.loading()
    {
        return 0.0;
    }
    if carries_catch(ctx) || nearby_owned_catch(ctx).is_some() {
        return 8.0;
    }
    if ctx.state.me.hp_pct() < 60 || !has_supplies(ctx) {
        return 0.0;
    }
    if cast_point(ctx).is_some() || chum_target(ctx).is_some() {
        1.0
    } else if bank_near_school(ctx).is_some() {
        0.4
    } else {
        0.0
    }
}

fn nearby_owned_catch(ctx: &Ctx<'_>) -> Option<u32> {
    let pos = ctx.pos();
    ctx.state
        .units
        .iter()
        .filter(|(_, fish)| {
            fish.kind == UnitKind::Npc
                && fish.flags & U_MY_SUMMON != 0
                && fish.dead()
                && fish.has_loot()
                && ctx.world.is_sport_fish(fish.id)
        })
        .min_by(|(_, left), (_, right)| dist3d(pos, left.pos).total_cmp(&dist3d(pos, right.pos)))
        .map(|(&object, _)| object)
}

#[derive(Debug, Default)]
enum Step {
    #[default]
    Start,
    Seek {
        bank: [f32; 2],
        deadline: u64,
    },
    Equipping {
        deadline: u64,
    },
    SettlingRod {
        ready: u64,
        deadline: u64,
    },
    Chumming {
        object: u32,
        expected: u32,
        deadline: u64,
    },
    Hook {
        at: [f32; 3],
        deadline: u64,
    },
    Reel {
        object: u32,
        due: u64,
        deadline: u64,
        hp: i32,
    },
    Land {
        object: u32,
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
    restore_weapon: Option<(u32, u64)>,
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
                if let Some(object) = nearby_owned_catch(ctx) {
                    if let Some(at) = cast_point(ctx) {
                        ctx.motion.note_water_surface(at);
                    }
                    self.step = Land {
                        object,
                        deadline: now + LAND_WAIT_MS,
                    };
                    return GoalStatus::Running;
                }
                if !has_supplies(ctx) {
                    return GoalStatus::Failed(FailReason::NoTarget);
                }
                let cast_at = cast_point(ctx);
                let chum_at = chum_target(ctx);
                if cast_at.is_none() && chum_at.is_none() {
                    let Some(bank) = bank_near_school(ctx) else {
                        return GoalStatus::Failed(FailReason::NoTarget);
                    };
                    self.step = Seek {
                        bank,
                        deadline: now + 150_000,
                    };
                    return GoalStatus::Running;
                }
                if !mainhand(ctx).is_some_and(|item| RODS.contains(&item.template)) {
                    let Some((slot, rod)) = bag_rod(ctx) else {
                        return GoalStatus::Failed(FailReason::NoTarget);
                    };
                    self.restore_weapon = mainhand(ctx).map(|item| (item.template, item.id));
                    let packet = match self.restore_weapon {
                        Some((_, old)) => cs::equip_swap(old, equip_slot::MAINHAND, rod, slot),
                        None => cs::equip_into_empty(equip_slot::MAINHAND, slot, rod),
                    };
                    ctx.send(packet);
                    self.step = Equipping {
                        deadline: now + 6_000,
                    };
                    return GoalStatus::Running;
                }
                if rod_changing(ctx) {
                    ctx.log
                        .debug("sport fishing: waiting for the equipped rod's lockout");
                    self.step = SettlingRod {
                        ready: now,
                        deadline: now + ROD_CHANGE_MS + 5_000,
                    };
                    return GoalStatus::Running;
                }
                if let Some(at) = cast_at {
                    self.cast(ctx, at);
                } else if let Some((object, [x, y, z], chum, item)) = chum_at {
                    ctx.motion.stop();
                    ctx.motion.face(x, y);
                    ctx.send(cs::start_skill_item_at(
                        chum.skill,
                        ctx.state.me.obj,
                        item,
                        chum.item,
                        (x, y, z),
                        ctx.motion.yaw(),
                    ));
                    self.step = Chumming {
                        object,
                        expected: chum.next_phase,
                        deadline: now + 12_000,
                    };
                } else {
                    return GoalStatus::Failed(FailReason::NoTarget);
                }
            }
            Seek { bank, deadline } => {
                if now >= deadline {
                    return timeout;
                }
                if cast_point(ctx).is_some() || chum_target(ctx).is_some() {
                    ctx.motion.stop();
                    self.walker.reset();
                    self.step = Start;
                } else {
                    return match self.walker.step(ctx, bank, 1.0) {
                        Walk::Going => GoalStatus::Running,
                        Walk::There => GoalStatus::Failed(FailReason::NoTarget),
                        Walk::Stuck => GoalStatus::Failed(FailReason::Stuck),
                    };
                }
            }
            Equipping { deadline } => {
                if mainhand(ctx).is_some_and(|item| RODS.contains(&item.template)) {
                    ctx.log
                        .debug("sport fishing: rod equipped, waiting out swap lockout");
                    self.step = SettlingRod {
                        ready: now + ROD_CHANGE_MS,
                        deadline: now + ROD_CHANGE_MS + 5_000,
                    };
                } else if now >= deadline {
                    return timeout;
                }
            }
            SettlingRod { ready, deadline } => {
                if now >= ready && !rod_changing(ctx) {
                    ctx.log.debug("sport fishing: the rod is ready");
                    self.step = Start;
                } else if now >= deadline {
                    return timeout;
                }
            }
            Chumming {
                object,
                expected,
                deadline,
            } => {
                if now >= deadline {
                    return timeout;
                }
                if let Some(school) = ctx
                    .state
                    .doodads
                    .get(&object)
                    .filter(|d| d.phase == expected)
                {
                    let Some(at) = water_towards(ctx, ctx.pos(), school.pos) else {
                        return GoalStatus::Failed(FailReason::NoTarget);
                    };
                    self.cast(ctx, at);
                }
            }
            Hook { at, deadline } => {
                if let Some(note) = ctx.state.travel.skill_note().filter(|n| {
                    n.skill == CAST
                        && n.result != 0
                        && n.at_ms >= deadline.saturating_sub(HOOK_WAIT_MS)
                }) {
                    ctx.log.say(format!(
                        "sport fishing: cast refused with result {}",
                        note.result
                    ));
                    return GoalStatus::Failed(FailReason::Other("sport fishing cast refused"));
                }
                if now >= deadline {
                    return timeout;
                }
                if let Some((&object, fish)) = ctx.state.units.iter().find(|(_, fish)| {
                    fish.kind == UnitKind::Npc
                        && fish.flags & U_MY_SUMMON != 0
                        && !fish.dead()
                        && ctx.world.is_sport_fish(fish.id)
                        && dist3d(at, fish.pos) < 15.0
                }) {
                    ctx.motion.note_water_surface(at);
                    ctx.log.say(format!(
                        "sport fishing: hooked owned fish {object} (template {}, {} HP)",
                        fish.id, fish.hp
                    ));
                    self.step = Reel {
                        object,
                        due: now,
                        deadline: now + FIGHT_WAIT_MS,
                        hp: fish.hp,
                    };
                }
            }
            Reel {
                object,
                mut due,
                deadline,
                hp,
            } => {
                if now >= deadline {
                    return timeout;
                }
                let Some(unit) = ctx.state.units.get(&object) else {
                    ctx.log.debug(format!(
                        "sport fishing: fish {object} left view with last observed HP {hp}"
                    ));
                    return GoalStatus::Failed(FailReason::NoTarget);
                };
                if unit.kind != UnitKind::Npc || unit.flags & U_MY_SUMMON == 0 {
                    return GoalStatus::Failed(FailReason::NoTarget);
                }
                let pos = unit.pos;
                let current_hp = unit.hp;
                let distance = dist3d(ctx.pos(), pos);
                if unit.dead() {
                    ctx.log.debug(format!(
                        "sport fishing: fish {object} died at {pos:?}, lootable={}",
                        unit.has_loot()
                    ));
                    self.step = Land {
                        object,
                        deadline: now + LAND_WAIT_MS,
                    };
                    return GoalStatus::Running;
                } else if distance > FOLLOW_FISH_M
                    || (ctx.motion.is_moving() && distance > KEEP_FISH_M)
                {
                    if ctx.in_war_zone(pos[0], pos[1])
                        || self.walker.step(ctx, [pos[0], pos[1]], KEEP_FISH_M - 2.0) == Walk::Stuck
                    {
                        return GoalStatus::Failed(FailReason::Stuck);
                    }
                } else if now >= due && !rod_changing(ctx) {
                    let skill = ctx
                        .state
                        .fish_cue
                        .as_ref()
                        .filter(|cue| {
                            cue.object == object
                                && now.saturating_sub(cue.seen_ms) <= REEL_CUE_MAX_AGE_MS
                        })
                        .and_then(|cue| ctx.world.skill(cue.skill));
                    if let Some(skill) = skill.filter(|skill| {
                        ctx.state.cooldown_ready(skill.id, now)
                            && (f32::from(skill.min_range)..=f32::from(skill.max_range))
                                .contains(&distance)
                    }) {
                        ctx.motion.stop();
                        ctx.send(cs::start_skill_unit(skill.id, ctx.state.me.obj, object));
                        due = now + REEL_CHANNEL_MS.max(u64::from(skill.casting_time));
                    }
                }
                self.step = Reel {
                    object,
                    due,
                    deadline,
                    hp: current_hp,
                };
            }
            Land { object, deadline } => {
                if carries_catch(ctx) {
                    return self.begin_turn_in(ctx);
                }
                if now >= deadline {
                    return timeout;
                }
                let Some(unit) = ctx.state.units.get(&object) else {
                    return GoalStatus::Failed(FailReason::NoTarget);
                };
                if unit.kind != UnitKind::Npc
                    || unit.flags & U_MY_SUMMON == 0
                    || !unit.dead()
                    || !ctx.world.is_sport_fish(unit.id)
                {
                    return GoalStatus::Failed(FailReason::NoTarget);
                }
                let pos = unit.pos;
                if !unit.has_loot() {
                    return GoalStatus::Running;
                }
                if dist3d(ctx.pos(), pos) > LOOT_REACH_M {
                    return match self.walker.step(ctx, [pos[0], pos[1]], 1.5) {
                        Walk::Going => GoalStatus::Running,
                        Walk::Stuck | Walk::There => GoalStatus::Failed(FailReason::Stuck),
                    };
                }
                ctx.motion.stop();
                ctx.send(cs::loot_open_bag(object, true));
                ctx.log
                    .debug(format!("sport fishing: taking loot from fish {object}"));
                self.step = Loot {
                    deadline: now + 8_000,
                };
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
                    ctx.motion.stop();
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
                    use crate::bot::{cmd::GoalKind, guild::treasury};
                    treasury::note_receipt(ctx, GoalKind::Fish, refund as u64);
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

    fn cast(&mut self, ctx: &mut Ctx<'_>, [x, y, z]: [f32; 3]) {
        ctx.motion.stop();
        ctx.motion.face(x, y);
        ctx.send(cs::start_skill_position(
            CAST,
            ctx.state.me.obj,
            (x, y, z),
            ctx.motion.yaw(),
        ));
        self.step = Step::Hook {
            at: [x, y, z],
            deadline: ctx.now_ms + HOOK_WAIT_MS,
        };
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
            Step::Seek { .. } => "Looking for a fishing bank",
            Step::Equipping { .. } => "Equipping a sport fishing rod",
            Step::SettlingRod { .. } => "Waiting to use the fishing rod",
            Step::Chumming { .. } => "Chumming a sport fishing school",
            Step::Start | Step::Hook { .. } => "Casting for a sport fish",
            Step::Reel { .. } => "Reeling in a sport fish",
            Step::Land { .. } | Step::Loot { .. } => "Taking the catch",
            Step::TurnIn { .. } | Step::Payment { .. } => "Selling the catch at a fish stand",
        });
    }

    pub fn stop(&mut self, ctx: &mut Ctx<'_>) {
        ctx.motion.stop();
        let Some((template, old)) = self.restore_weapon.take() else {
            return;
        };
        let Some(rod) = mainhand(ctx)
            .filter(|item| RODS.contains(&item.template))
            .map(|item| item.id)
        else {
            return;
        };
        if let Some((slot, _)) = ctx.state.find_item_where(template, |item| item.id == old) {
            if let Ok(slot) = u8::try_from(slot) {
                ctx.send(cs::equip_swap(rod, equip_slot::MAINHAND, old, slot));
            }
        }
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
