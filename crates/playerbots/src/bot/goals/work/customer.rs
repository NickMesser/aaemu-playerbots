//! One paid player commission for the guild member's configured recipe.

use aa_proto::cs::chat_type;
use serde::{Deserialize, Serialize};

use super::super::delivery::{nearby_requester, tell};
use super::super::trade::{Deal, Plan, Trade, INVITE_TTL_MS};
use super::{FailReason, GoalStatus, Order};
use crate::bot::cmd::{GoalKind, PairKind, PairOrder, PairRole};
use crate::bot::ctx::Ctx;
use crate::bot::guild::{profession, treasury};
use crate::bot::state::{dist3d, ChatIn, UnitKind};
use crate::hub::unix_ms;

const COMMISSION_MS: u64 = 60 * 60_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Commission {
    guild: u32,
    recipe: u32,
    item: u32,
    quantity: u32,
    price: i32,
    customer: u32,
    name: Box<str>,
    expires_ms: u64,
    ready: Option<Stock>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Stock {
    id: u64,
    count: i32,
}

impl Commission {
    pub fn ready(&self) -> bool {
        self.ready.is_some()
    }
}

fn recipe<'a>(ctx: &Ctx<'a>) -> Option<(u32, &'a aa_world::Craft)> {
    if !(ctx.cfg.guild.player_craft_orders && ctx.cfg.market.enabled)
        || ctx.state.place.in_instance()
        || ctx.state.place.loading()
        || ctx.state.me.dead
    {
        return None;
    }
    let (guild, profession::Profession::Crafter, false) = profession::membership(ctx)? else {
        return None;
    };
    let Order::Craft { recipe, .. } = ctx.cfg.guild.production_for(guild, ctx.state.me.char_id)?
    else {
        return None;
    };
    let craft = ctx.world.craft(recipe)?;
    let (item, count) = craft.product;
    (!craft.is_pack
        && count > 0
        && ctx
            .world
            .item_facts(item)
            .is_some_and(|facts| facts.tradable()))
    .then_some((guild, craft))
}

pub fn active(ctx: &Ctx<'_>) -> Option<Commission> {
    let (guild, craft) = recipe(ctx)?;
    ctx.hub.journal.read(ctx.persona.id, |j| {
        j.craft_commission
            .as_ref()
            .filter(|c| c.guild == guild && c.recipe == craft.id && c.expires_ms > unix_ms())
            .cloned()
    })
}

pub fn reserved(ctx: &Ctx<'_>) -> Option<(u32, u32)> {
    let commission = active(ctx)?;
    commission
        .ready()
        .then_some((commission.item, commission.quantity))
}

pub fn gear_reservation(ctx: &Ctx<'_>) -> Option<(u32, Option<u64>)> {
    let commission = active(ctx)?;
    Some((commission.item, commission.ready.map(|stock| stock.id)))
}

pub fn summary(ctx: &Ctx<'_>) -> Option<String> {
    let c = active(ctx)?;
    let item = ctx.world.item_name(c.item).unwrap_or("guild craft");
    let phase = if c.ready() {
        "ready for collection"
    } else {
        "in production"
    };
    Some(format!(
        " Customer order for {} {item} at {} copper is {phase}.",
        c.quantity, c.price
    ))
}

fn offer(text: &str) -> Option<(&str, i32)> {
    let (item, copper) = text.rsplit_once(" for ")?;
    let price = copper.trim().strip_suffix('c')?.parse().ok()?;
    (!item.trim().is_empty() && price > 0).then_some((item.trim(), price))
}

fn stock_slot(ctx: &Ctx<'_>, c: &Commission) -> Option<u8> {
    let ready = c.ready?;
    let (slot, _) = ctx.state.find_item_where(c.item, |item| {
        item.id == ready.id && item.count >= ready.count
    })?;
    u8::try_from(slot).ok()
}

pub(in crate::bot::goals) fn on_whisper(ctx: &mut Ctx<'_>, line: &ChatIn) -> bool {
    if line.kind != chat_type::WHISPER || !ctx.cfg.guild.player_craft_orders {
        return false;
    }
    let Some((verb, text)) = line.text.trim().split_once(' ') else {
        return false;
    };
    if !verb.eq_ignore_ascii_case("craft") {
        return false;
    }
    let Some((customer, who)) = nearby_requester(ctx, line) else {
        return false;
    };
    if text.trim().eq_ignore_ascii_case("cancel") {
        let cancelled = ctx.hub.journal.update(ctx.persona.id, |j| {
            if j.craft_commission
                .as_ref()
                .is_some_and(|c| c.customer == customer)
            {
                j.craft_commission = None;
                true
            } else {
                false
            }
        });
        if cancelled {
            tell(ctx, who, "I've cancelled your craft order.");
        }
        return cancelled;
    }
    let Some((guild, craft)) = recipe(ctx) else {
        return false;
    };
    let (item, quantity) = craft.product;
    let name = ctx.world.item_name(item).unwrap_or("this item");
    let Some((wanted, price)) = offer(text) else {
        tell(
            ctx,
            who,
            &format!("Try 'craft {name} for 100c' for one batch."),
        );
        return true;
    };
    if !name.eq_ignore_ascii_case(wanted) {
        tell(ctx, who, &format!("My guild assigned me {name}."));
        return true;
    }
    if let Some(existing) = active(ctx) {
        let response = if existing.customer != customer {
            "I'm working on another customer's order."
        } else if existing.ready() {
            "Your order is ready. Send me a trade request when you're here."
        } else {
            "I still have your order and will tell you when it's ready."
        };
        tell(ctx, who, response);
        return true;
    }
    let commission = Commission {
        guild,
        recipe: craft.id,
        item,
        quantity,
        price,
        customer,
        name: who.into(),
        expires_ms: unix_ms() + COMMISSION_MS,
        ready: None,
    };
    ctx.hub
        .journal
        .update(ctx.persona.id, |j| j.craft_commission = Some(commission));
    tell(
        ctx,
        who,
        &format!("I'll make {quantity} {name} for {price}c. Pay me when it's ready."),
    );
    true
}

pub(super) fn crafted(ctx: &mut Ctx<'_>, recipe: u32) {
    let Some(mut commission) = active(ctx).filter(|c| c.recipe == recipe && !c.ready()) else {
        return;
    };
    let Some((_, item)) = ctx.state.find_item_where(commission.item, |item| {
        item.count >= commission.quantity as i32
    }) else {
        return;
    };
    commission.ready = Some(Stock {
        id: item.id,
        count: item.count,
    });
    tell(
        ctx,
        &commission.name,
        "Your craft order is ready. Come by and send me a trade request.",
    );
    ctx.hub
        .journal
        .update(ctx.persona.id, |j| j.craft_commission = Some(commission));
}

fn customer(ctx: &Ctx<'_>, c: &Commission) -> Option<[f32; 3]> {
    let (object, received) = ctx.state.trade.incoming?;
    if ctx.state.trade.window.is_some() || ctx.now_ms.saturating_sub(received) > INVITE_TTL_MS {
        return None;
    }
    let buyer = ctx.state.units.get(&object)?;
    (buyer.kind == UnitKind::Character
        && buyer.id == c.customer
        && !buyer.dead()
        && dist3d(ctx.pos(), buyer.pos) <= 6.0)
        .then_some(buyer.pos)
}

pub(super) fn can_trade(ctx: &Ctx<'_>, c: &Commission) -> bool {
    customer(ctx, c).is_some() && stock_slot(ctx, c).is_some()
}

pub fn requested_sale(ctx: &Ctx<'_>) -> bool {
    ctx.state.trade.incoming.is_some() && active(ctx).is_some_and(|c| can_trade(ctx, &c))
}

#[derive(Debug)]
pub(super) struct Sale {
    commission: Commission,
    trade: Trade,
    items_before: i64,
    money_before: i64,
    confirm_until: Option<u64>,
}

impl Sale {
    pub(super) fn start(ctx: &mut Ctx<'_>, commission: Commission) -> Option<Self> {
        let [x, y, _] = customer(ctx, &commission)?;
        let slot = stock_slot(ctx, &commission)?;
        let order = PairOrder {
            kind: PairKind::Trade,
            role: PairRole::Responder,
            partner_bot: 0,
            partner_char: commission.customer,
            partner_name: commission.name.clone(),
            meet_at: (x, y),
        };
        let give = Plan {
            slot,
            template: commission.item,
            amount: i32::try_from(commission.quantity).ok()?,
            money: 0,
        };
        let mut trade = Trade::for_deal(order, Deal::agreed(give, &[], commission.price));
        trade.start(ctx);
        Some(Self {
            items_before: ctx.state.item_count(commission.item),
            money_before: ctx.state.me.money,
            commission,
            trade,
            confirm_until: None,
        })
    }

    pub(super) fn tick(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        if active(ctx).as_ref() != Some(&self.commission) {
            return GoalStatus::Failed(FailReason::NoTarget);
        }
        if self.confirm_until.is_none() {
            match self.trade.tick(ctx) {
                GoalStatus::Done => self.confirm_until = Some(ctx.now_ms + 4_000),
                status => return status,
            }
        }
        let c = &self.commission;
        if self.items_before - ctx.state.item_count(c.item) == i64::from(c.quantity)
            && ctx.state.me.money - self.money_before == i64::from(c.price)
        {
            ctx.hub
                .journal
                .update(ctx.persona.id, |j| j.craft_commission = None);
            treasury::note_receipt(ctx, GoalKind::Market, c.price as u64);
            ctx.log.say(format!(
                "guild craft: delivered item {} to {} for {} copper",
                c.item, c.name, c.price
            ));
            return GoalStatus::Done;
        }
        if self.confirm_until.is_some_and(|until| ctx.now_ms >= until) {
            return GoalStatus::Failed(FailReason::Timeout);
        }
        GoalStatus::Running
    }

    pub(super) fn stop(&mut self, ctx: &mut Ctx<'_>) {
        self.trade.stop(ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::offer;

    #[test]
    fn craft_order_requires_a_named_item_and_positive_copper_price() {
        assert_eq!(offer("Stone Brick for 100c"), Some(("Stone Brick", 100)));
        assert_eq!(
            offer("Tribute for Nui for 1c"),
            Some(("Tribute for Nui", 1))
        );
        for invalid in [
            "Stone Brick",
            "Stone Brick for 0c",
            "Stone Brick for -1c",
            "Stone Brick for 2147483648c",
            " for 100c",
        ] {
            assert!(offer(invalid).is_none(), "accepted {invalid:?}");
        }
    }
}
