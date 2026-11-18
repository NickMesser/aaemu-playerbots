//! Nearby players can request a courier or hire a Thief to report a bot sighting.

use aa_proto::cs::chat_type;
use serde::{Deserialize, Serialize};

use super::{social, FailReason, GoalStatus};
use crate::bot::ctx::Ctx;
use crate::bot::guild::profession::{self, Profession};
use crate::bot::pvp::walk::{Walk, Walker};
use crate::bot::state::{dist2d, dist3d, ChatIn, UnitKind};
use crate::hub::unix_ms;

const MAX_TRIP_M: f32 = 850.0;
const CONTRACT_MS: u64 = 20 * 60_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Service {
    #[default]
    Courier,
    Spy,
}

fn can_spy(ctx: &Ctx<'_>) -> bool {
    matches!(
        profession::membership(ctx),
        Some((_, Profession::Thief, false))
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryOrder {
    pub requester: u32,
    pub requester_name: String,
    pub recipient: u32,
    pub recipient_name: String,
    pub message: String,
    pub expires_ms: u64,
    #[serde(default)]
    service: Service,
}

fn recipient(ctx: &Ctx<'_>, order: &DeliveryOrder) -> Option<([f32; 3], bool)> {
    if let Some(unit) = ctx
        .state
        .units
        .values()
        .find(|u| u.kind == UnitKind::Character && u.id == order.recipient && !u.dead())
    {
        return Some((unit.pos, true));
    }
    ctx.hub.slots().into_iter().find_map(|slot| {
        if slot.char_id.load(std::sync::atomic::Ordering::Relaxed) != order.recipient {
            return None;
        }
        slot.with(|g| {
            let snap = &g.snap;
            (!matches!(
                snap.activity,
                pb_web::Activity::Offline | pb_web::Activity::Connecting | pb_web::Activity::Dead
            ) && snap.name.eq_ignore_ascii_case(&order.recipient_name))
            .then_some(([snap.x, snap.y, snap.z], false))
        })
    })
}

fn tell(ctx: &mut Ctx<'_>, who: &str, text: &str) {
    ctx.out.extend(social::whisper_packet(who, text));
}

/// Consumes a nearby player's explicit delivery request or cancellation.
pub fn on_whisper(ctx: &mut Ctx<'_>, line: &ChatIn) -> bool {
    if !ctx.cfg.behavior.delivery_contracts || line.kind != chat_type::WHISPER {
        return false;
    }
    let text = line.text.trim();
    let Some((verb, rest)) = text.split_once(' ') else {
        return false;
    };
    let service = if verb.eq_ignore_ascii_case("deliver") {
        Service::Courier
    } else if verb.eq_ignore_ascii_case("spy") {
        Service::Spy
    } else {
        return false;
    };
    let who = social::strip_gm(&line.name);
    let Some(unit) = ctx.state.units.get(&line.sender_obj).filter(|u| {
        u.kind == UnitKind::Character
            && !ctx.hub.is_bot_char(u.id)
            && u.name
                .as_deref()
                .is_some_and(|n| n.eq_ignore_ascii_case(who))
            && dist3d(ctx.pos(), u.pos) <= 30.0
    }) else {
        return false;
    };
    let requester = unit.id;
    if ctx.state.team().is_some_and(|team| {
        team.in_team()
            && !team
                .members
                .iter()
                .any(|member| member.char_id == requester)
    }) {
        tell(ctx, who, "I'm with another party right now.");
        return true;
    }
    if rest.trim().eq_ignore_ascii_case("cancel") {
        let removed = ctx.hub.journal.update(ctx.persona.id, |j| {
            if j.delivery
                .as_ref()
                .is_some_and(|order| order.requester == requester && order.service == service)
            {
                j.delivery = None;
                true
            } else {
                false
            }
        });
        if removed {
            let reply = match service {
                Service::Courier => "I cancelled your delivery.",
                Service::Spy => "I cancelled your scouting job.",
            };
            tell(ctx, who, reply);
        }
        return true;
    }
    let (name, message) = match service {
        Service::Spy if can_spy(ctx) => (rest.trim(), String::new()),
        Service::Spy => {
            tell(ctx, who, "Ask a member of a Thief guild to scout for you.");
            return true;
        }
        Service::Courier => {
            let Some((name, message)) = rest.trim_start().split_once(' ') else {
                tell(
                    ctx,
                    who,
                    "Whisper deliver, the bot's name, and a short message.",
                );
                return true;
            };
            let Some(message) = pb_llm::sanitize_reply(message) else {
                tell(ctx, who, "I can only carry a short chat message.");
                return true;
            };
            (name, message)
        }
    };
    let Some((recipient, recipient_name)) = ctx.hub.slots().into_iter().find_map(|slot| {
        if slot.id == ctx.persona.id {
            return None;
        }
        let id = slot.char_id.load(std::sync::atomic::Ordering::Relaxed);
        slot.with(|g| {
            (id != 0
                && g.snap.name.eq_ignore_ascii_case(name)
                && !matches!(
                    g.snap.activity,
                    pb_web::Activity::Offline
                        | pb_web::Activity::Connecting
                        | pb_web::Activity::Dead
                )
                && dist2d(ctx.pos(), [g.snap.x, g.snap.y, g.snap.z]) <= MAX_TRIP_M)
                .then(|| (id, g.snap.name.clone()))
        })
    }) else {
        tell(ctx, who, "Pick a bot who's online nearby.");
        return true;
    };
    let now = unix_ms();
    let accepted = ctx.hub.journal.update(ctx.persona.id, |j| {
        if j.delivery
            .as_ref()
            .is_some_and(|order| order.expires_ms > now)
        {
            return false;
        }
        j.delivery = Some(DeliveryOrder {
            requester,
            requester_name: who.to_owned(),
            recipient,
            recipient_name,
            message,
            expires_ms: now + CONTRACT_MS,
            service,
        });
        true
    });
    tell(
        ctx,
        who,
        match (accepted, service) {
            (true, Service::Courier) => "I'll carry your message.",
            (true, Service::Spy) => "I'll look for them and report back.",
            (false, _) => "I'm already on a job.",
        },
    );
    true
}

pub fn ready(ctx: &Ctx<'_>) -> bool {
    ctx.cfg.behavior.delivery_contracts
        && !ctx.state.place.in_instance()
        && !ctx.state.place.loading()
        && ctx.state.me.hp_pct() >= 60
        && ctx.hub.journal.read(ctx.persona.id, |j| {
            j.delivery.as_ref().is_some_and(|order| {
                order.expires_ms > unix_ms() && (order.service == Service::Courier || can_spy(ctx))
            })
        })
}

pub fn scouting(ctx: &Ctx<'_>) -> bool {
    ctx.cfg.behavior.delivery_contracts
        && ctx.hub.journal.read(ctx.persona.id, |j| {
            j.delivery
                .as_ref()
                .is_some_and(|order| order.service == Service::Spy && order.expires_ms > unix_ms())
        })
}

#[derive(Debug, Default)]
pub struct Delivery {
    walker: Walker,
    service: Service,
}

impl Delivery {
    pub fn tick(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        if !ready(ctx) {
            return GoalStatus::Done;
        }
        let Some(order) = ctx.hub.journal.read(ctx.persona.id, |j| j.delivery.clone()) else {
            return GoalStatus::Done;
        };
        self.service = order.service;
        let Some((at, seen)) = recipient(ctx, &order) else {
            return GoalStatus::Failed(FailReason::NoTarget);
        };
        if ctx.in_war_zone(at[0], at[1]) || dist2d(ctx.pos(), at) > MAX_TRIP_M {
            return GoalStatus::Failed(FailReason::NoTarget);
        }
        let (reach, stop) = match order.service {
            Service::Spy => (24.0, 12.0),
            Service::Courier => (5.0, 1.5),
        };
        if !seen || dist3d(ctx.pos(), at) > reach {
            return match self.walker.step(ctx, [at[0], at[1]], stop) {
                Walk::Going => GoalStatus::Running,
                Walk::Stuck => GoalStatus::Failed(FailReason::Stuck),
                Walk::There if seen => GoalStatus::Running,
                Walk::There => GoalStatus::Failed(FailReason::NoTarget),
            };
        }
        if order.service == Service::Spy {
            return scout(ctx, &order);
        }
        let text = format!("Message from {}: {}", order.requester_name, order.message);
        let Some(packet) = social::whisper_packet(&order.recipient_name, &text) else {
            return GoalStatus::Failed(FailReason::Other("invalid delivery message"));
        };
        let delivered = ctx.hub.journal.update(ctx.persona.id, |j| {
            if j.delivery.as_ref() != Some(&order) {
                return false;
            }
            j.delivery = None;
            j.deliveries_completed = j.deliveries_completed.saturating_add(1);
            true
        });
        if delivered {
            ctx.send(packet);
            ctx.log.say(format!(
                "delivery: passed on a message to {}",
                order.recipient_name
            ));
            tell(
                ctx,
                &order.requester_name,
                &format!(
                    "I reached {} and passed on your message.",
                    order.recipient_name
                ),
            );
        }
        GoalStatus::Done
    }

    pub fn describe(&self, out: &mut String) {
        out.push_str(match self.service {
            Service::Courier => "Delivering a message",
            Service::Spy => "Scouting for a Thief guild client",
        });
    }
}

fn scout(ctx: &mut Ctx<'_>, order: &DeliveryOrder) -> GoalStatus {
    let Some((&object, unit)) = ctx
        .state
        .units
        .iter()
        .find(|(_, u)| u.kind == UnitKind::Character && u.id == order.recipient && !u.dead())
    else {
        return GoalStatus::Failed(FailReason::NoTarget);
    };
    let [x, y, _] = unit.pos;
    let zone = ctx
        .world
        .zone_key_at(x, y)
        .and_then(|key| ctx.world.zone_name(key))
        .unwrap_or("the area");
    let cargo = if ctx.state.pack_of(object) != 0 {
        " They carried a trade pack."
    } else {
        ""
    };
    let report = format!(
        "I saw {} in {zone} near {x:.0}, {y:.0}.{cargo}",
        order.recipient_name
    );
    let Some(packet) = social::whisper_packet(&order.requester_name, &report) else {
        return GoalStatus::Failed(FailReason::Other("invalid scouting report"));
    };
    let ready = ctx.hub.journal.update(ctx.persona.id, |j| {
        if j.delivery.as_ref() != Some(order) {
            return false;
        }
        j.delivery = None;
        j.scouting_reports = j.scouting_reports.saturating_add(1);
        j.notes.push(unix_ms() / 1000, &report);
        true
    });
    if ready {
        ctx.send(packet);
        ctx.log.say(format!(
            "scouting: reported a sighting of {}",
            order.recipient_name
        ));
    }
    GoalStatus::Done
}
