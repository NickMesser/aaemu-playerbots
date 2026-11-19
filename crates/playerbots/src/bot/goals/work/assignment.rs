//! Production instructions from the member's current guildmaster.

use aa_proto::cs::chat_type;
use serde::{Deserialize, Serialize};

use crate::bot::ctx::Ctx;
use crate::bot::goals::{delivery, social};
use crate::bot::guild::{log::GuildLog, profession};
use crate::bot::state::{ChatIn, Unit, UnitKind};

use super::Order;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dispatch {
    guild: u32,
    master: u32,
    order: Order,
}

pub fn active(ctx: &Ctx<'_>, guild: u32) -> Option<Order> {
    if !ctx.cfg.guild.dispatch_orders {
        return None;
    }
    let log = ctx.state.guild.log()?;
    let dispatch = ctx
        .hub
        .journal
        .read(ctx.persona.id, |j| j.production_dispatch)?;
    (dispatch.guild == guild
        && log.id == guild
        && log.member(dispatch.master).is_some()
        && current_master(log, dispatch.master)
        && ctx.hub.is_bot_char(dispatch.master))
    .then_some(dispatch.order)
}

pub fn order(ctx: &Ctx<'_>, guild: u32) -> Option<Order> {
    active(ctx, guild).or_else(|| ctx.cfg.guild.production_for(guild, ctx.state.me.char_id))
}

fn parse(words: &[&str]) -> Option<Option<Order>> {
    match words {
        ["clear"] => Some(None),
        ["craft", recipe] => Some(Some(Order::Craft {
            recipe: recipe.parse().ok()?,
            training_recipe: None,
        })),
        ["craft", recipe, training] => Some(Some(Order::Craft {
            recipe: recipe.parse().ok()?,
            training_recipe: Some(training.parse().ok()?),
        })),
        ["gather", doodad, item] => Some(Some(Order::Gather {
            doodad: doodad.parse().ok()?,
            item: item.parse().ok()?,
        })),
        ["hunt", item, count] => Some(Some(Order::Hunt {
            item: item.parse().ok()?,
            count: count.parse().ok()?,
        })),
        _ => None,
    }
}

fn authored(world: &aa_world::World, order: Order) -> bool {
    match order {
        Order::Craft {
            recipe,
            training_recipe,
        } => {
            let Some(craft) = world
                .craft(recipe)
                .filter(|c| !c.is_pack && c.product.0 != 0)
            else {
                return false;
            };
            training_recipe.is_none_or(|id| {
                world.craft(id).is_some_and(|training| {
                    !training.is_pack
                        && training.labor > 0
                        && training.actability_group == craft.actability_group
                })
            })
        }
        Order::Gather { doodad, item } => world.gather_doodads(item).contains(&doodad),
        Order::Hunt { item, .. } => !world.work_droppers(item).is_empty(),
    }
}

fn sender_master(
    log: &GuildLog,
    name: &str,
    visible: Option<&Unit>,
    is_bot: impl Fn(u32) -> bool,
) -> Option<u32> {
    let member = log
        .members
        .iter()
        .find(|member| member.name.eq_ignore_ascii_case(name))?;
    let character = member.char_id;
    (current_master(log, character)
        && is_bot(character)
        && visible.is_none_or(|unit| unit.kind == UnitKind::Character && unit.id == character))
    .then_some(character)
}

fn current_master(log: &GuildLog, character: u32) -> bool {
    if log.owner == 0 {
        log.is_owner(character)
    } else {
        log.owner == character
    }
}

pub(in crate::bot::goals) fn on_whisper(ctx: &mut Ctx<'_>, line: &ChatIn) -> bool {
    if !ctx.cfg.guild.dispatch_orders || line.kind != chat_type::WHISPER {
        return false;
    }
    let mut words = line.text.split_ascii_whitespace();
    if !words
        .next()
        .is_some_and(|word| word.eq_ignore_ascii_case("guild"))
        || !words
            .next()
            .is_some_and(|word| word.eq_ignore_ascii_case("work"))
    {
        return false;
    }
    let Some((guild, role, false)) = profession::membership(ctx) else {
        return true;
    };
    let who = social::strip_gm(&line.name);
    let master = ctx.state.guild.log().and_then(|log| {
        sender_master(log, who, ctx.state.units.get(&line.sender_obj), |id| {
            ctx.hub.is_bot_char(id)
        })
    });
    let Some(master) = master else {
        return true;
    };
    let words = words.collect::<smallvec::SmallVec<[_; 3]>>();
    let Some(request) = parse(&words).filter(|request| {
        request.is_none_or(|order| order.allowed_in(role) && authored(ctx.world, order))
    }) else {
        delivery::tell(
            ctx,
            who,
            "That order does not match my profession or an authored recipe or resource.",
        );
        return true;
    };
    if super::customer::active(ctx).is_some() {
        delivery::tell(
            ctx,
            who,
            "I'll finish my current customer order before changing assignments.",
        );
        return true;
    }
    let dispatch = request.map(|order| Dispatch {
        guild,
        master,
        order,
    });
    let changed = ctx.hub.journal.update(ctx.persona.id, |j| {
        let changed = j.production_dispatch != dispatch;
        j.production_dispatch = dispatch;
        changed
    });
    if changed {
        let summary = request.map_or_else(
            || "cleared the production assignment".into(),
            |order| format!("assigned {order}"),
        );
        ctx.log.say(format!("guild dispatch: {who} {summary}"));
        let reply = if request.is_some() {
            "I recorded your production assignment."
        } else {
            "I cleared the assignment and will use the configured order."
        };
        delivery::tell(ctx, who, reply);
    }
    true
}

#[cfg(test)]
mod tests {
    use crate::bot::guild::log::Member;

    use super::*;

    #[test]
    fn only_the_current_bot_guildmaster_can_dispatch() {
        let member = |id, name: &str| Member {
            char_id: id,
            name: name.into(),
            level: 40,
            role: 0,
            online: true,
            since_ms: 0,
        };
        let mut log = GuildLog::default();
        log.id = 44;
        log.owner = 6;
        log.members = vec![member(6, "Vonurn"), member(8, "Farsignal")];
        let unit = Unit {
            kind: UnitKind::Character,
            id: 8,
            pos: [0.0; 3],
            faction: 101,
            hp: 100,
            level: 40,
            flags: 0,
            target: 0,
            name: Some("Farsignal".into()),
        };
        assert_eq!(sender_master(&log, "Vonurn", None, |id| id == 6), Some(6));
        assert_eq!(sender_master(&log, "Vonurn", Some(&unit), |_| true), None);
        assert_eq!(sender_master(&log, "Vonurn", None, |_| false), None);
        assert_eq!(sender_master(&log, "Farsignal", None, |_| true), None);
        log.members[0].role = aa_proto::guild::role::OWNER;
        log.owner = 8;
        assert_eq!(sender_master(&log, "Vonurn", None, |_| true), None);
    }

    #[test]
    fn dispatch_has_the_same_profession_rules_as_a_configured_order() {
        let selected = parse(&["gather", "1671", "8022"]).flatten().unwrap();
        assert!(selected.allowed_in(profession::Profession::Gatherer));
        assert!(!selected.allowed_in(profession::Profession::Crafter));
        assert_eq!(parse(&["clear"]), Some(None));
        assert_eq!(parse(&["gather", "mine"]), None);
    }
}
