//! A player's party can arrange a guard for a carried trade pack or sport catch.

pub mod payment;

use crate::bot::ctx::Ctx;
use crate::bot::goals::{fish, social::whisper_packet};
use crate::bot::guild::profession::{membership, Profession};
use crate::bot::pvp::rules::{self, Purpose, Rules, Target};
use crate::bot::state::{dist3d, BotState, Unit, UnitKind};

use super::{duty::mem_of, Mode};

const ESCORT_MS: u64 = 20 * 60_000;
const FISH_STAND_ARRIVAL_M: f32 = 8.0;

#[derive(Debug, Clone)]
pub struct Escort {
    leader: u32,
    guild: u32,
    name: Box<str>,
    team: u32,
    pack: u32,
    sport_catch: bool,
    fee_copper: u32,
    until: u64,
    active: bool,
    attacker: Option<(u32, u64)>,
}

fn nearby_carrier<'a>(ctx: &'a Ctx<'_>, leader: u32) -> Option<(u32, &'a Unit)> {
    ctx.state.units.iter().find_map(|(&object, unit)| {
        (unit.kind == UnitKind::Character
            && unit.id == leader
            && dist3d(ctx.pos(), unit.pos) <= 45.0
            && !unit.dead())
        .then_some((ctx.state.pack_of(object), unit))
    })
}

fn at_buyer(ctx: &Ctx<'_>, leader: &Unit, pack: u32) -> bool {
    let buyers = ctx.world.specialty_buyers(pack);
    !buyers.is_empty()
        && ctx.state.units.values().any(|npc| {
            npc.kind == UnitKind::Npc
                && !npc.dead()
                && buyers.contains(&npc.id)
                && dist3d(leader.pos, npc.pos) <= 10.0
        })
}

fn at_destination(ctx: &Ctx<'_>, carrier: &Unit, pack: u32, sport_catch: bool) -> bool {
    if sport_catch {
        fish::nearby_stand(ctx, pack, carrier.pos, FISH_STAND_ARRIVAL_M).is_some()
    } else {
        at_buyer(ctx, carrier, pack)
    }
}

fn tell(ctx: &mut Ctx<'_>, name: &str, message: &str) {
    ctx.out.extend(whisper_packet(name, message));
}

pub fn request(ctx: &mut Ctx<'_>, name: &str, leader: u32, text: &str) -> bool {
    if !ctx.cfg.party.trade_escorts {
        return false;
    }
    let text = text.trim();
    if text.eq_ignore_ascii_case("escort pay") {
        payment::request(ctx, leader, name);
        return true;
    }
    if text.eq_ignore_ascii_case("escort cancel") {
        let m = mem_of(ctx.state);
        if m.escort.as_ref().is_some_and(|job| job.leader == leader) {
            m.escort = None;
            tell(ctx, name, "I ended the escort.");
        }
        return true;
    }
    let accept = text.eq_ignore_ascii_case("escort accept");
    if !accept && !text.eq_ignore_ascii_case("escort") {
        return false;
    }
    let Some((guild, Profession::Adventurer, false)) = membership(ctx) else {
        tell(
            ctx,
            name,
            "A member of an Adventurer guild can take that job.",
        );
        return true;
    };
    if let Some(invoice) = payment::outstanding(ctx) {
        let reply = if invoice.customer == leader {
            "Your last escort is complete. Whisper 'escort pay' to settle it."
        } else {
            "I have an unsettled escort and can't take another yet."
        };
        tell(ctx, name, reply);
        return true;
    }
    let Some(team) = ctx.state.team().filter(|t| t.in_team()).map(|t| t.team_id) else {
        return false;
    };
    let Some((pack, carrier)) = nearby_carrier(ctx, leader) else {
        tell(
            ctx,
            name,
            "Stand near me with your trade pack or sport catch.",
        );
        return true;
    };
    let sport_catch = ctx.world.fish_sales().iter().any(|sale| sale.item == pack);
    if (!sport_catch && ctx.world.specialty_buyers(pack).is_empty())
        || at_destination(ctx, carrier, pack, sport_catch)
    {
        tell(
            ctx,
            name,
            "Bring a trade pack or sport catch that still needs a trip to a buyer.",
        );
        return true;
    }
    let fee_copper = ctx.cfg.party.trade_escort_fee_copper;
    if fee_copper != 0 && !accept {
        tell(
            ctx,
            name,
            &format!(
                "I charge {fee_copper} copper after arrival. Whisper 'escort accept' to hire me."
            ),
        );
        return true;
    }
    let until = ctx.now_ms + ESCORT_MS;
    let mem = mem_of(ctx.state);
    if mem.escort.is_none() {
        mem.escort = Some(Escort {
            leader,
            guild,
            name: name.into(),
            team,
            pack,
            sport_catch,
            fee_copper,
            until,
            active: false,
            attacker: None,
        });
    }
    true
}

pub fn note_hit(state: &mut BotState, caster: u32, target: u32) {
    let Some(job) = state
        .party
        .as_mut()
        .and_then(|p| p.companion.as_deref_mut())
        .and_then(|m| m.escort.as_mut())
        .filter(|job| job.active)
    else {
        return;
    };
    if state
        .units
        .get(&target)
        .is_some_and(|unit| unit.kind == UnitKind::Character && unit.id == job.leader)
    {
        job.attacker = Some((caster, state.now_ms));
    }
}

pub fn raider(ctx: &Ctx<'_>) -> Option<u32> {
    if !ctx.cfg.party.trade_escorts
        || !ctx.cfg.pvp.defend
        || !ctx.cfg.pvp.enabled
        || rules::in_peace_area(&ctx.state.buffs)
        || ctx.state.me.hp_pct() <= ctx.cfg.pvp.retreat_hp_pct
    {
        return None;
    }
    let job = super::peek(ctx.state)?
        .escort
        .as_ref()
        .filter(|job| job.active)?;
    let (object, hit_at) = job.attacker?;
    if ctx.now_ms.saturating_sub(hit_at) > 5_000
        || ctx.state.team().is_none_or(|t| t.team_id != job.team)
    {
        return None;
    }
    let unit = ctx.state.units.get(&object)?;
    let rules = Rules::new(ctx.world, ctx.cfg, ctx.state.me.faction);
    let target = Target::of(
        unit,
        ctx.hub.is_bot_char(unit.id),
        ctx.state.pack_of(object),
    );
    (dist3d(ctx.pos(), unit.pos) <= 30.0
        && rules.check(&target, Purpose::Hunt).is_ok()
        && !ctx.state.avoids(object, 0, ctx.now_ms))
    .then_some(object)
}

pub fn tick(ctx: &mut Ctx<'_>, accompanying: bool, available: bool) -> bool {
    let Some(mut job) = ctx
        .state
        .party
        .as_mut()
        .and_then(|p| p.companion.as_deref_mut())
        .and_then(|m| m.escort.take())
    else {
        return false;
    };
    let valid = ctx.cfg.party.trade_escorts
        && accompanying
        && !ctx.state.place.in_instance()
        && ctx.now_ms < job.until
        && ctx.state.team().is_some_and(|t| t.team_id == job.team)
        && super::duty::leader_char(ctx) == Some(job.leader)
        && membership(ctx) == Some((job.guild, Profession::Adventurer, false));
    if !valid {
        tell(
            ctx,
            &job.name,
            "I ended the escort when our trip was interrupted.",
        );
        return false;
    }
    let destination = if job.sport_catch {
        "a fish stand"
    } else {
        "a buyer"
    };
    if !job.active {
        if !available {
            tell(ctx, &job.name, "I'm off duty right now.");
            return false;
        }
        let mem = mem_of(ctx.state);
        mem.mode = Mode::Follow;
        mem.come = 0;
        mem.dungeon = None;
        job.active = true;
        let cargo = if job.sport_catch {
            "sport catch"
        } else {
            "trade pack"
        };
        tell(
            ctx,
            &job.name,
            &format!("I'll guard you on the way to {destination}."),
        );
        ctx.log
            .say(format!("escort: guarding {} and their {cargo}", job.name));
    }
    if let Some((pack, carrier)) = nearby_carrier(ctx, job.leader) {
        if pack != job.pack {
            tell(ctx, &job.name, "Your cargo is gone, so the escort is over.");
            return false;
        }
        if dist3d(ctx.pos(), carrier.pos) <= 14.0
            && at_destination(ctx, carrier, pack, job.sport_catch)
        {
            let invoice = payment::Invoice::new(job.guild, job.leader, job.fee_copper);
            ctx.hub.journal.update(ctx.persona.id, |j| {
                j.escorts_completed = j.escorts_completed.saturating_add(1);
                if let Some(invoice) = invoice {
                    j.escort_invoice = Some(invoice);
                }
            });
            ctx.log
                .say(format!("escort: reached {destination} with {}", job.name));
            let fee = invoice.map_or_else(String::new, |fee| {
                format!(
                    " Whisper 'escort pay' when you're nearby to pay {} copper.",
                    fee.copper
                )
            });
            tell(
                ctx,
                &job.name,
                &format!("We reached {destination}. Your escort is complete.{fee}"),
            );
            return false;
        }
    }
    mem_of(ctx.state).escort = Some(job);
    true
}
