//! Mounts and gliders: a bot rides the mate of a mount summon item it owns on long overland
//! trips, gets off for a fight, a talk, a harvest or a dungeon door, dismisses the mate when it
//! has stood idle, wears a glider on its back and glides down a cliff in its way.
//!
//! | Piece | Where | What |
//! | --- | --- | --- |
//! | [`plan`] | pure | When to summon, ride, leave and dismiss, the refusal back-off, and [`plan::find_ledge`]. |
//! | [`duty`] | here | The glue the brain calls every tick: reads the bag, the equipment and the terrain, carries out the acts. |
//! | [`Mem`] | here | One per bot, behind the travel state's pointer: the phase, the mate, the glider and the clocks. |
//!
//! # What the server answers
//!
//! * A mount is the `SummonMate` item's use skill (special effect `SpawnPet`, type 24,
//!   `systems/skills/effects/special/misc.rs:643` -> `systems/vehicles/mates.rs:920`
//!   `spawn_mount`), then `CSMountMatePacket` (0x0a7) and `SCUnitAttached` (point 1). The mate
//!   appears 3 m in front of the owner and its unit state names its object id and `tl`.
//! * A glider is a back-slot item (`item_backpacks.backpack_type_id = 2`) whose use skill the
//!   client starts with the item as caster; the server checks the back slot and the item's own
//!   skill (`systems/skills/glider.rs:34`, `handlers.rs:183`). The flight itself is client
//!   physics; the bot moves itself and sends the glide stance (4) in its `CSMoveUnit` updates.
//! * A refusal is silent in most mount paths (`mates.rs:334` only logs). The feature therefore
//!   has a deadline for every request and a back-off after a timeout or an `SCErrorMsg`, and
//!   never sends the same request every tick.
//!
//! Memory: [`Mem`] lives in [`crate::bot::travel::Inner`], so `size_of::<Bot>()` does not grow.
//! A bot allocates the travel state on its first tick anyway (the portal duty), so the mount
//! feature adds no fixed cost to a bot that never rides.

pub mod plan;

#[cfg(test)]
mod tests;

use aa_proto::cs;
use aa_proto::sc_travel::attach_point;
use aa_proto::sc_world::BACKPACK_SLOT;
use aa_world::World;
use rustc_hash::FxHashMap;

use crate::bot::ctx::Ctx;
use crate::bot::gear::refused_since;
use crate::bot::state::Item;

pub use plan::{
    Act, Acts, Facts, ItemRef, Ledge, Mem, Phase, Rules, DISMOUNT_WAIT_MS, DRY_LAND_M,
    EQUIP_WAIT_MS, GLIDE_GAP_MS, GLIDE_MIN_REMAINING_M, GLIDE_REFUSE_MS, ITEM_WAIT_MS,
    MATE_OBJ_WAIT_MS, MOUNT_REACH_M, MOUNT_WAIT_MS, NO_ITEM_MS, REFUSE_MS, RETRY_MS,
    SUMMON_RETRY_MS, SUMMON_WAIT_MS,
};

/// The `[mounts]` rules of this process.
pub fn rules(cfg: &crate::config::MountsCfg) -> Rules {
    Rules {
        mounts: cfg.mounts,
        gliders: cfg.gliders,
        summon_over_m: cfg.summon_over_m,
        dismount_under_m: cfg.dismount_under_m,
        idle_despawn_ms: u64::from(cfg.idle_despawn_secs) * 1_000,
        glide_drop_m: cfg.glide_drop_m,
        glide_lookahead_m: cfg.glide_lookahead_m,
    }
}

/// One brain tick of the feature. Cheap when nothing is owned: one pass over the bag, the
/// equipment and (only with a worn glider and a long trip) the terrain ahead.
pub fn duty(ctx: &mut Ctx<'_>) {
    let (base_run, base_walk) = (ctx.cfg.bots.move_speed, ctx.cfg.bots.walk_speed);
    ctx.motion.set_speeds(base_run, base_walk);
    let cfg = &ctx.cfg.mounts;
    if !cfg.enabled || (!cfg.mounts && !cfg.gliders) {
        return;
    }
    if !ctx.state.me.spawned || ctx.state.me.dead || ctx.state.me.obj == 0 {
        return;
    }
    let now = ctx.now_ms;
    let rules = rules(cfg);
    let level = ctx.state.me.level;
    let me_obj = ctx.state.me.obj;
    let note = ctx.state.travel.skill_note();
    let mut mem = std::mem::take(ctx.state.travel.mount_mut());
    // A skill the server answered since the request: the summon was refused, or the glider
    // deploy was. Both back off; the physical glide already started is allowed to land.
    if mem.glide_sent_ms != 0 {
        if let Some(n) = note {
            if n.skill == mem.glide_skill && n.at_ms >= mem.glide_sent_ms {
                if n.result != 0 {
                    mem.glide_off_until_ms = now.saturating_add(GLIDE_REFUSE_MS);
                    ctx.log.debug(format!(
                        "mount: the server refused the glider (result {})",
                        n.result
                    ));
                }
                mem.glide_sent_ms = 0;
                mem.glide_skill = 0;
            }
        }
    }
    if let Some(since) = mem.awaiting_since() {
        if refused_since(&ctx.state.errors, since) {
            ctx.log
                .debug("mount: the server refused the mount request; backing off");
            mem.on_refused(now);
        }
    }
    if let Some(p) = mem.glider_pending {
        if refused_since(&ctx.state.errors, p.sent_ms) {
            ctx.log
                .debug("mount: the server refused the glider move; backing off");
            mem.glider_pending = None;
            mem.glider_off_until_ms = now.saturating_add(REFUSE_MS);
        }
    }
    let equipped = equipped_glider(ctx.world, &ctx.state.equipment);
    let back_blocked = ctx
        .state
        .equipment
        .iter()
        .any(|e| e.slot == BACKPACK_SLOT && ctx.world.mounts().glider(e.template).is_none());
    let bag_glider = if back_blocked {
        None
    } else {
        glider_in_bag(ctx.world, &ctx.state.bag, level, cfg.glider_item)
    };
    let bag_mount = mount_in_bag(ctx.world, &ctx.state.bag, level, cfg.item);
    let default_mount = if cfg.item != 0 {
        Some(cfg.item)
    } else {
        ctx.world.mounts().default_mount(level)
    };
    let default_glider = if cfg.glider_item != 0 {
        Some(cfg.glider_item)
    } else {
        ctx.world.mounts().default_glider(level)
    };
    let busy = {
        let st = &*ctx.state;
        st.trade.active()
            || st.place.in_instance()
            || st.place.loading()
            || st.travel.busy()
            || st.vehicle.boat().is_some()
            || (st.vehicle.seat().is_some() && !mem.mounted())
            || ctx.motion.teleporting()
            || ctx.motion.gliding()
    };
    let glide_path = ctx
        .motion
        .remaining()
        .is_some_and(|r| r >= GLIDE_MIN_REMAINING_M);
    let ledge = if rules.gliders
        && !mem.mounted()
        && !busy
        && !ctx.motion.gliding()
        && !ctx.motion.awaits_path()
        && glide_path
        && equipped.is_some()
    {
        let ground = ctx.state.place.ground(ctx.world);
        plan::find_ledge(
            |x, y| ground.ground_z(x, y),
            ctx.state.me.pos,
            ctx.motion.yaw(),
            rules.glide_lookahead_m,
            rules.glide_drop_m,
        )
    } else {
        None
    };
    let mut facts = Facts::idle(now, rules);
    facts.me_pos = ctx.state.me.pos;
    facts.moving = ctx.motion.is_moving();
    facts.remaining = ctx.motion.remaining();
    facts.combat = ctx.combat.busy() || !ctx.state.threats.is_empty();
    facts.busy = busy;
    facts.gm = ctx.cfg.bots.gm_bootstrap;
    facts.mount_item = bag_mount;
    facts.glider_item = bag_glider;
    facts.back_blocked = back_blocked;
    facts.glider_equipped = equipped;
    facts.skill_note = note;
    facts.ledge = ledge;
    facts.default_mount = default_mount;
    facts.default_glider = default_glider;
    let acts = plan::step(&mut mem, &facts);
    for act in acts {
        match act {
            Act::GrantMount { template } => {
                if ctx.gm(&format!("/item add self {template} 1")) {
                    ctx.log.debug(format!(
                        "mount: asking the GM for the summon item {template}"
                    ));
                } else {
                    mem.phase = Phase::Idle;
                    mem.refused_until_ms = now.saturating_add(NO_ITEM_MS);
                }
            }
            Act::Summon {
                item_id,
                template,
                skill,
            } => {
                if skill == 0 {
                    mem.phase = Phase::Idle;
                    mem.refused_until_ms = now.saturating_add(NO_ITEM_MS);
                } else {
                    ctx.out.push(cs::start_skill_item(
                        skill, me_obj, item_id, template, me_obj,
                    ));
                    ctx.log.debug(format!(
                        "mount: summoning the mount with item {template} (skill {skill})"
                    ));
                }
            }
            Act::Mount { tl } => {
                ctx.out.push(cs::mount_mate(
                    tl,
                    attach_point::DRIVER,
                    cs::MOUNT_REASON_FORWARD,
                ));
                ctx.log.debug(format!("mount: boarding the mate {tl}"));
            }
            Act::Dismount { tl } => {
                ctx.out.push(cs::unmount_mate(
                    tl,
                    attach_point::DRIVER,
                    cs::MOUNT_REASON_FORWARD,
                ));
                ctx.log.debug(format!("mount: leaving the mate {tl}"));
            }
            Act::Despawn { tl } => {
                ctx.out.push(cs::remove_mate(tl));
                ctx.log.debug(format!("mount: dismissing the mate {tl}"));
            }
            Act::GrantGlider { template } => {
                if !ctx.gm(&format!("/item add self {template} 1")) {
                    mem.glider_pending = None;
                    mem.glider_off_until_ms = now.saturating_add(NO_ITEM_MS);
                }
            }
            Act::EquipGlider {
                item_id,
                template,
                bag_slot,
            } => match u8::try_from(bag_slot) {
                Ok(slot) => {
                    ctx.out
                        .push(cs::equip_into_empty(BACKPACK_SLOT, slot, item_id));
                    ctx.log
                        .debug(format!("mount: putting on the glider {template}"));
                }
                Err(_) => {
                    mem.glider_pending = None;
                    mem.glider_off_until_ms = now.saturating_add(NO_ITEM_MS);
                }
            },
            Act::Glide {
                item_id,
                template,
                skill,
                dir,
            } => {
                if skill != 0
                    && ctx
                        .motion
                        .begin_glide(dir, cfg.glide_speed, cfg.glide_descent, now)
                {
                    ctx.out.push(cs::start_skill_item(
                        skill, me_obj, item_id, template, me_obj,
                    ));
                    ctx.log
                        .debug(format!("mount: gliding down (skill {skill})"));
                } else {
                    mem.last_glide_ms = 0;
                    mem.glide_off_until_ms = now.saturating_add(GLIDE_REFUSE_MS);
                }
            }
        }
    }
    if mem.mounted() {
        ctx.motion.set_speeds(cfg.speed, cfg.speed);
        if mem.mate_obj != 0 && ctx.motion.move_obj() != mem.mate_obj {
            // The rider and the mate are one body: the client sends the mount's moves
            // (`world/session.rs:1191` ignores a riding player's own moves). The mate appeared
            // 3 m in front, so take its position once.
            ctx.motion.set_move_obj(mem.mate_obj);
            if let Some(pos) = mem.mate_pos {
                ctx.motion.set_position(pos);
            }
        }
    } else {
        ctx.motion.set_speeds(base_run, base_walk);
        if ctx.motion.move_obj() != ctx.state.me.obj {
            ctx.motion.set_move_obj(0);
        }
    }
    *ctx.state.travel.mount_mut() = mem;
}

/// The bot died, left the world or entered an instance: its mate is gone (the server removes the
/// active mates on instance entry and logout, `systems/instances/dungeon.rs:524`,
/// `systems/vehicles/mod.rs:531`).
pub fn on_gone(
    state: &mut crate::bot::state::BotState,
    motion: &mut crate::bot::motion::Motion,
    out: &mut Vec<cs::Out>,
    base: (f32, f32),
) {
    let mut mem = std::mem::take(state.travel.mount_mut());
    if mem.mate_tl != 0 {
        // A teleport, a death or an instance load may leave the mate behind; the server removes
        // an active mate by its timeline id (and unmounts the rider first).
        out.push(cs::remove_mate(mem.mate_tl));
    }
    mem.on_gone();
    *state.travel.mount_mut() = mem;
    motion.end_glide();
    motion.set_move_obj(0);
    motion.set_speeds(base.0, base.1);
}

/// The mount summon item in the bag: the configured template when it is set, else any rideable
/// mount of the bot's level. The lowest template (then bag slot) wins, so a bot with two summons
/// always picks the same one.
fn mount_in_bag(
    world: &World,
    bag: &FxHashMap<u16, Item>,
    level: u8,
    configured: u32,
) -> Option<ItemRef> {
    let mounts = world.mounts();
    let mut best: Option<(u32, u16, ItemRef)> = None;
    for (&slot, item) in bag {
        if configured != 0 && item.template != configured {
            continue;
        }
        let Some(m) = mounts.mount(item.template) else {
            continue;
        };
        if m.level_req > level {
            continue;
        }
        let key = (item.template, slot);
        if best.as_ref().is_some_and(|b| (b.0, b.1) <= key) {
            continue;
        }
        best = Some((
            key.0,
            key.1,
            ItemRef {
                item_id: item.id,
                template: item.template,
                skill: m.use_skill,
                bag_slot: slot,
            },
        ));
    }
    best.map(|b| b.2)
}

/// The glider in the bag (not the one worn): the configured template when set, else any glider
/// of the bot's level.
fn glider_in_bag(
    world: &World,
    bag: &FxHashMap<u16, Item>,
    level: u8,
    configured: u32,
) -> Option<ItemRef> {
    let gliders = world.mounts();
    let mut best: Option<(u32, u16, ItemRef)> = None;
    for (&slot, item) in bag {
        if configured != 0 && item.template != configured {
            continue;
        }
        let Some(g) = gliders.glider(item.template) else {
            continue;
        };
        if g.level_req > level {
            continue;
        }
        let key = (item.template, slot);
        if best.as_ref().is_some_and(|b| (b.0, b.1) <= key) {
            continue;
        }
        best = Some((
            key.0,
            key.1,
            ItemRef {
                item_id: item.id,
                template: item.template,
                skill: g.use_skill,
                bag_slot: slot,
            },
        ));
    }
    best.map(|b| b.2)
}

/// The glider in the back slot.
fn equipped_glider(world: &World, equipment: &[aa_proto::sc_world::EquipSlot]) -> Option<ItemRef> {
    let e = equipment.iter().find(|e| e.slot == BACKPACK_SLOT)?;
    let g = world.mounts().glider(e.template)?;
    Some(ItemRef {
        item_id: e.id,
        template: e.template,
        skill: g.use_skill,
        bag_slot: u16::from(BACKPACK_SLOT),
    })
}
