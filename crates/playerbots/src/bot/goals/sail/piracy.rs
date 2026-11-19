//! Thieves can turn the cannon on their own ship on an eligible hostile vessel.

use aa_proto::{cs, sc_travel::attach_point};

use crate::bot::ctx::Ctx;
use crate::bot::guild::profession::{self, Profession};
use crate::bot::motion::Pace;
use crate::bot::pvp::rules::{self, Purpose, Rules, Target};
use crate::bot::state::{dist2d, dist3d, Unit, UnitKind};
use crate::bot::vehicle::{naval, wrap_pi, Boat};

const STEEL_AMMO: u32 = 23_671;
const FIRE_CANNON: u32 = 10_473;
const MOUNT_SKILLS: u32 = 16;
const MAX_SHOTS: u8 = 3;
const SHOT_GAP_MS: u64 = 3_500;
const RETRY_MS: u64 = 5 * 60_000;
const SEAT_MS: u64 = 4_000;
const BRAKE_MS: u64 = 20_000;

#[derive(Debug, Clone, Copy)]
struct Cannon {
    obj: u32,
    tl: u16,
    pos: [f32; 3],
    yaw: f32,
}

#[derive(Debug, Clone, Copy)]
enum Stage {
    Brake,
    LeaveHelm,
    ApproachCannon,
    BindCannon,
    Volley { sent: u8, hit: bool },
    LeaveCannon,
    BindHelm,
}

#[derive(Debug, Clone, Copy)]
struct Raid {
    vessel: u32,
    cannon: Cannon,
    stage: Stage,
    until: u64,
}

#[derive(Debug, Default)]
pub(super) struct Patrol {
    raid: Option<Raid>,
    retry_at: u64,
}

pub(super) fn enabled(ctx: &Ctx<'_>) -> bool {
    ctx.cfg.sail.naval_raids
        && ctx.cfg.pvp.enabled
        && (ctx.cfg.pvp.attack_bots || ctx.cfg.may_attack_players())
        && !ctx.cfg.bots.gm_bootstrap
        && ctx.state.me.level >= 35
        && ctx.state.item_count(STEEL_AMMO) > 0
        && matches!(
            profession::membership(ctx),
            Some((_, Profession::Thief, false))
        )
}

impl Patrol {
    /// Returns true while the cannon sequence has the helm.
    pub(super) fn tick(&mut self, ctx: &mut Ctx<'_>) -> bool {
        let Some(sailor) = ctx.state.vehicle.sailor() else {
            return false;
        };
        let Some(boat) = sailor.live_boat().cloned() else {
            self.raid = None;
            return false;
        };
        let seat = sailor.seat;
        let own_helm =
            seat.is_some_and(|s| s.parent == boat.obj && s.point == attach_point::DRIVER);
        let Some(track) = sailor.track.as_ref().filter(|t| t.cur.obj == boat.obj) else {
            return false;
        };
        let speed = track.cur.speed;
        let now = ctx.now_ms;
        let Some(mut raid) = self.raid.take() else {
            if own_helm && now >= self.retry_at && enabled(ctx) {
                self.retry_at = now + 1_000;
                if let Some((vessel, cannon)) = pick(ctx, &boat) {
                    ctx.send(cs::ship_request(boat.obj, now as u32, 0, 0));
                    ctx.log.say("naval raid: slowing to man the cannon");
                    self.raid = Some(Raid {
                        vessel,
                        cannon,
                        stage: Stage::Brake,
                        until: now + BRAKE_MS,
                    });
                    self.retry_at = now + RETRY_MS;
                    return true;
                }
            }
            return false;
        };
        let on_cannon =
            seat.is_some_and(|s| s.parent == raid.cannon.obj && s.point == attach_point::DRIVER);
        let hit =
            sailor.naval.as_ref().and_then(|n| n.last_hit) == Some((raid.cannon.obj, raid.vessel));
        match raid.stage {
            Stage::Brake if now >= raid.until || !own_helm => return false,
            Stage::Brake if speed <= 0.8 => {
                let Some(gun) = cannon_position(ctx, &boat, raid.cannon.obj) else {
                    return false;
                };
                if shot(ctx, &boat, gun, raid.vessel).is_none() {
                    return false;
                }
                raid.cannon = gun;
                ctx.send(cs::discard_slave(boat.tl));
                raid.stage = Stage::LeaveHelm;
                raid.until = now + SEAT_MS;
            }
            Stage::LeaveHelm if seat.is_none() => {
                if dist3d(ctx.pos(), raid.cannon.pos) > 3.6 {
                    let [x, y, _] = raid.cannon.pos;
                    ctx.motion.go_to(x, y, 2.0, Pace::Walk, now);
                    raid.stage = Stage::ApproachCannon;
                    raid.until = now + 6_000;
                } else {
                    bind_cannon(ctx, &mut raid);
                }
            }
            Stage::ApproachCannon if dist3d(ctx.pos(), raid.cannon.pos) <= 3.6 => {
                ctx.motion.stop();
                bind_cannon(ctx, &mut raid);
            }
            Stage::ApproachCannon if now >= raid.until => {
                ctx.motion.stop();
                rebind(ctx, &boat, &mut raid);
            }
            Stage::LeaveHelm if now >= raid.until => return false,
            Stage::BindCannon if on_cannon => {
                if let Some(sight) = ctx.state.vehicle.sailor_mut().naval.as_mut() {
                    sight.last_hit = None;
                }
                raid.stage = Stage::Volley {
                    sent: 0,
                    hit: false,
                };
                raid.until = now;
            }
            Stage::BindCannon if now >= raid.until => {
                rebind(ctx, &boat, &mut raid);
            }
            Stage::Volley { sent, hit: claimed } => {
                if hit && !claimed {
                    ctx.hub.journal.update(ctx.persona.id, |journal| {
                        journal.naval_hits = journal.naval_hits.saturating_add(1);
                    });
                    ctx.log.say("naval raid: the cannon hit the hostile ship");
                    raid.stage = Stage::Volley { sent, hit: true };
                }
                if now >= raid.until {
                    let aim = (on_cannon && sent < MAX_SHOTS && enabled(ctx))
                        .then(|| cannon_position(ctx, &boat, raid.cannon.obj))
                        .flatten()
                        .and_then(|gun| shot(ctx, &boat, gun, raid.vessel));
                    if let Some(pos) = aim {
                        ctx.send(cs::start_mount_skill_position(
                            FIRE_CANNON,
                            raid.cannon.obj,
                            MOUNT_SKILLS,
                            (pos[0], pos[1], pos[2]),
                        ));
                        raid.stage = Stage::Volley {
                            sent: sent + 1,
                            hit: claimed || hit,
                        };
                        raid.until = now + SHOT_GAP_MS;
                    } else if on_cannon {
                        ctx.send(cs::discard_slave(raid.cannon.tl));
                        raid.stage = Stage::LeaveCannon;
                        raid.until = now + SEAT_MS;
                    } else {
                        rebind(ctx, &boat, &mut raid);
                    }
                }
            }
            Stage::LeaveCannon if seat.is_none() => rebind(ctx, &boat, &mut raid),
            Stage::LeaveCannon if now >= raid.until => {
                ctx.send(cs::despawn_slave(boat.obj));
                ctx.log
                    .say("naval raid: recalling the ship after the cannon seat stayed occupied");
                return false;
            }
            Stage::BindHelm if own_helm || now >= raid.until => return false,
            _ => {}
        }
        self.raid = Some(raid);
        true
    }

    pub(super) fn stop(&mut self, ctx: &mut Ctx<'_>) {
        if let Some(raid) = self.raid.take() {
            if ctx
                .state
                .vehicle
                .sailor()
                .and_then(|s| s.seat)
                .is_some_and(|s| s.parent == raid.cannon.obj)
            {
                ctx.send(cs::discard_slave(raid.cannon.tl));
            }
        }
    }
}

fn rebind(ctx: &mut Ctx<'_>, boat: &Boat, raid: &mut Raid) {
    ctx.send(cs::bind_slave(boat.tl));
    raid.stage = Stage::BindHelm;
    raid.until = ctx.now_ms + SEAT_MS;
}

fn bind_cannon(ctx: &mut Ctx<'_>, raid: &mut Raid) {
    ctx.send(cs::bind_slave(raid.cannon.tl));
    raid.stage = Stage::BindCannon;
    raid.until = ctx.now_ms + SEAT_MS;
}

fn pick(ctx: &Ctx<'_>, boat: &Boat) -> Option<(u32, Cannon)> {
    let sight = ctx.state.vehicle.sailor()?.naval.as_ref()?;
    sight
        .slaves
        .iter()
        .filter(|(_, gun)| gun.template == naval::CANNON && gun.owner == ctx.state.me.char_id)
        .filter_map(|(&id, _)| cannon_position(ctx, boat, id))
        .filter(|gun| dist3d(boat.pos, gun.pos) <= 3.6)
        .flat_map(|gun| {
            sight.slaves.keys().filter_map(move |&vessel| {
                shot(ctx, boat, gun, vessel).map(|aim| (vessel, gun, dist2d(gun.pos, aim)))
            })
        })
        .min_by(|a, b| a.2.total_cmp(&b.2))
        .map(|(vessel, gun, _)| (vessel, gun))
}

fn cannon_position(ctx: &Ctx<'_>, boat: &Boat, object: u32) -> Option<Cannon> {
    let sailor = ctx.state.vehicle.sailor()?;
    let gun = sailor.naval.as_ref()?.slaves.get(&object)?;
    if gun.parent != boat.obj || gun.owner != ctx.state.me.char_id {
        return None;
    }
    let track = sailor.track.as_ref()?.cur;
    let turn = track.yaw;
    let (sin, cos) = turn.sin_cos();
    let [dx, dy, dz] = gun.origin;
    Some(Cannon {
        obj: object,
        tl: gun.tl,
        pos: [
            boat.pos[0] + cos * dx - sin * dy,
            boat.pos[1] + sin * dx + cos * dy,
            boat.pos[2] + dz,
        ],
        yaw: gun.yaw + turn,
    })
}

fn shot(ctx: &Ctx<'_>, boat: &Boat, gun: Cannon, object: u32) -> Option<[f32; 3]> {
    let sight = ctx.state.vehicle.sailor()?.naval.as_ref()?;
    let vessel = sight.slaves.get(&object)?;
    let unit = ctx.state.units.get(&object)?;
    if unit.dead()
        || object == boat.obj
        || vessel.parent != 0
        || vessel.owner == ctx.state.me.char_id
        || !ctx
            .world
            .travel()
            .boats
            .slave_template(vessel.template)
            .is_some_and(|t| aa_world::travel::boat::is_boat_kind(t.kind))
    {
        return None;
    }
    let owner = character(ctx, vessel.owner)?;
    let rules = Rules::new(ctx.world, ctx.cfg, ctx.state.me.faction);
    if !permitted(ctx, &rules, owner) || dist2d(unit.pos, owner.pos) > 18.0 {
        return None;
    }
    let group = ctx.world.zone_group_at(unit.pos[0], unit.pos[1])?;
    if !ctx.state.at_war(u16::try_from(group).ok()?)
        || ctx.world.zone_group_at(gun.pos[0], gun.pos[1]) != Some(group)
        || rules::in_peace_area(&ctx.state.buffs)
    {
        return None;
    }
    let mut aim = unit.pos;
    if let Some(motion) = vessel.motion {
        let lead = (dist3d(gun.pos, unit.pos) / 35.0).min(3.5);
        for (coordinate, velocity) in aim.iter_mut().zip(motion.velocity) {
            *coordinate += lead * velocity;
        }
    }
    let distance = dist3d(gun.pos, aim);
    let angle = (gun.pos[0] - aim[0]).atan2(aim[1] - gun.pos[1]);
    if !(6.0..=105.0).contains(&distance) || wrap_pi(angle - gun.yaw).abs() > 26.0_f32.to_radians()
    {
        return None;
    }
    if ctx.world.zone_group_at(aim[0], aim[1]) != Some(group)
        || ctx.state.units.values().any(|near| {
            near.kind == UnitKind::Character
                && near.id != owner.id
                && dist2d(near.pos, aim) < 12.0
                && rules.hostile(near.faction)
                && !permitted(ctx, &rules, near)
        })
        || sight.slaves.iter().any(|(&other, slave)| {
            if other == object || slave.owner == ctx.state.me.char_id {
                return false;
            }
            let Some(unit) = ctx
                .state
                .units
                .get(&other)
                .filter(|unit| rules.hostile(unit.faction))
            else {
                return false;
            };
            let Some(hull) = ctx
                .world
                .travel()
                .boats
                .slave_template(slave.template)
                .filter(|hull| aa_world::travel::boat::is_boat_kind(hull.kind))
            else {
                return false;
            };
            let reach = hull.ship.length.hypot(hull.ship.beam) * 0.5 + 5.0;
            dist2d(unit.pos, aim) < reach
                && character(ctx, slave.owner).is_none_or(|owner| !permitted(ctx, &rules, owner))
        })
    {
        return None;
    }
    Some(aim)
}

fn character<'a>(ctx: &'a Ctx<'_>, character: u32) -> Option<&'a Unit> {
    ctx.state
        .units
        .values()
        .find(|u| u.kind == UnitKind::Character && u.id == character)
}

fn permitted(ctx: &Ctx<'_>, rules: &Rules<'_, aa_world::World>, unit: &Unit) -> bool {
    let target = Target::of(unit, ctx.hub.is_bot_char(unit.id), 0);
    rules.check(&target, Purpose::Hunt).is_ok()
}
