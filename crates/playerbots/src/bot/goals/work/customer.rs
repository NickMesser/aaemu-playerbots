//! One paid player commission for the guild member's configured recipe.

use aa_proto::cs::chat_type;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use super::super::delivery::{nearby_requester, tell};
use super::super::trade::{Deal, Plan, Trade, INVITE_TTL_MS};
use super::{FailReason, GoalStatus, Order};
use crate::bot::cmd::{GoalKind, PairKind, PairOrder, PairRole};
use crate::bot::ctx::Ctx;
use crate::bot::guild::{profession, treasury};
use crate::bot::state::{dist3d, ChatIn, UnitKind};
use crate::hub::unix_ms;

const COMMISSION_MS: u64 = 60 * 60_000;
const PICKUP_MS: u64 = 30_000;
const TRADE_ITEM_SLOTS: usize = 10;
type Ingredients = SmallVec<[(u32, i32); 4]>;

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    materials: Option<Materials>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pickup_until_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Stock {
    id: u64,
    count: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Materials {
    copper: i32,
    purchased: bool,
}

impl Commission {
    pub fn ready(&self) -> bool {
        self.ready.is_some()
    }

    pub fn awaits_materials(&self) -> bool {
        self.materials.is_some_and(|terms| !terms.purchased)
    }

    pub fn needs_trade(&self) -> bool {
        self.ready() || self.awaits_materials()
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
    let Order::Craft { recipe, .. } = super::assignment::order(ctx, guild)? else {
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
    let phase = if c.awaits_materials() {
        "waiting for customer materials"
    } else if c.ready() {
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

fn quoted_order(text: &str) -> Option<(&str, i32, Option<i32>)> {
    let Some((order, copper)) = text.rsplit_once(" with materials for ") else {
        let (item, price) = offer(text)?;
        return Some((item, price, None));
    };
    let (item, price) = offer(order)?;
    let credit = copper.trim().strip_suffix('c')?.parse().ok()?;
    (credit > 0 && credit < price).then_some((item, price, Some(credit)))
}

fn ingredients(ctx: &Ctx<'_>, craft: &aa_world::Craft) -> Option<Ingredients> {
    let mut items = Ingredients::new();
    for &(template, quantity) in craft.materials.iter() {
        if template == craft.product.0
            || !ctx
                .world
                .item_facts(template)
                .is_some_and(|facts| facts.tradable())
        {
            return None;
        }
        let quantity = i32::try_from(quantity).ok().filter(|&count| count > 0)?;
        if let Some((_, count)) = items.iter_mut().find(|(item, _)| *item == template) {
            *count = count.checked_add(quantity)?;
        } else {
            items.push((template, quantity));
        }
    }
    (!items.is_empty() && items.len() <= TRADE_ITEM_SLOTS).then_some(items)
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
    let pickup = text.trim().eq_ignore_ascii_case("collect");
    if pickup || text.trim().eq_ignore_ascii_case("materials") {
        let Some(mut commission) = active(ctx).filter(|c| c.customer == customer) else {
            return false;
        };
        if pickup && stock_slot(ctx, &commission).is_none() {
            let response = if commission.awaits_materials() {
                "I need the ingredients first. Whisper 'craft materials' when you're here."
            } else {
                "I'm still working on your craft order."
            };
            tell(ctx, who, response);
        } else if !pickup && !commission.awaits_materials() {
            tell(ctx, who, "I have what I need to make your order.");
        } else if nearby_buyer(ctx, line.sender_obj, &commission).is_none() {
            tell(ctx, who, "Come a little closer for the trade.");
        } else {
            commission.pickup_until_ms = Some(unix_ms() + PICKUP_MS);
            ctx.hub
                .journal
                .update(ctx.persona.id, |j| j.craft_commission = Some(commission));
        }
        return true;
    }
    let Some((guild, craft)) = recipe(ctx) else {
        return false;
    };
    let (item, quantity) = craft.product;
    let name = ctx.world.item_name(item).unwrap_or("this item");
    let Some((wanted, price, credit)) = quoted_order(text) else {
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
            "Your order is ready. Whisper 'craft collect' when you're here."
        } else if existing.awaits_materials() {
            "I have your order. Whisper 'craft materials' when you're here."
        } else {
            "I still have your order and will tell you when it's ready."
        };
        tell(ctx, who, response);
        return true;
    }
    let materials = if let Some(copper) = credit {
        let max = i64::from(ctx.cfg.guild.player_craft_material_max_copper);
        let Some(items) = ingredients(ctx, craft).filter(|_| max >= i64::from(copper)) else {
            tell(ctx, who, "I can't buy the ingredients on those terms.");
            return true;
        };
        if ctx.state.me.money < i64::from(copper)
            || ctx.state.me.level < craft.req_level
            || ctx
                .state
                .econ
                .proficiency(u32::from(craft.actability_group))
                < craft.actability_limit
        {
            tell(ctx, who, "I can't take that materials order right now.");
            return true;
        }
        let list = items
            .iter()
            .map(|&(item, count)| {
                let name = ctx.world.item_name(item).unwrap_or("ingredient");
                format!("{count} {name}")
            })
            .collect::<Vec<_>>()
            .join(", ");
        tell(ctx, who, &format!("I'll buy {list} for {copper}c and sell the finished {name} for {price}c. Whisper 'craft materials' to trade the ingredients."));
        Some(Materials {
            copper,
            purchased: false,
        })
    } else {
        tell(
            ctx,
            who,
            &format!("I'll make {quantity} {name} for {price}c. Pay me when it's ready."),
        );
        None
    };
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
        materials,
        pickup_until_ms: None,
    };
    ctx.hub
        .journal
        .update(ctx.persona.id, |j| j.craft_commission = Some(commission));
    true
}

pub(super) fn crafted(ctx: &mut Ctx<'_>, recipe: u32) {
    let Some(mut commission) = active(ctx).filter(|c| c.recipe == recipe && !c.needs_trade())
    else {
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
        "Your craft order is ready. Come by and whisper 'craft collect'.",
    );
    ctx.hub
        .journal
        .update(ctx.persona.id, |j| j.craft_commission = Some(commission));
}

fn nearby_buyer(ctx: &Ctx<'_>, object: u32, c: &Commission) -> Option<[f32; 3]> {
    let buyer = ctx.state.units.get(&object)?;
    (buyer.kind == UnitKind::Character
        && buyer.id == c.customer
        && !buyer.dead()
        && dist3d(ctx.pos(), buyer.pos) <= 5.0)
        .then_some(buyer.pos)
}

fn customer(ctx: &Ctx<'_>, c: &Commission) -> Option<([f32; 3], PairRole)> {
    if ctx.state.trade.window.is_some() {
        return None;
    }
    if let Some((object, received)) = ctx.state.trade.incoming {
        return (ctx.now_ms.saturating_sub(received) <= INVITE_TTL_MS)
            .then(|| nearby_buyer(ctx, object, c))
            .flatten()
            .map(|pos| (pos, PairRole::Responder));
    }
    if c.pickup_until_ms.is_none_or(|until| unix_ms() >= until) {
        return None;
    }
    ctx.state
        .units
        .keys()
        .find_map(|&object| nearby_buyer(ctx, object, c).map(|pos| (pos, PairRole::Initiator)))
}

fn purchase(ctx: &Ctx<'_>, c: &Commission) -> Option<(Plan, Ingredients)> {
    let terms = c.materials.filter(|terms| !terms.purchased)?;
    if i64::from(terms.copper) > ctx.state.me.money
        || u32::try_from(terms.copper).ok()? > ctx.cfg.guild.player_craft_material_max_copper
    {
        return None;
    }
    let (_, craft) = recipe(ctx)?;
    let give = Plan {
        money: terms.copper,
        ..Plan::default()
    };
    Some((give, ingredients(ctx, craft)?))
}

pub(super) fn can_trade(ctx: &Ctx<'_>, c: &Commission) -> bool {
    customer(ctx, c).is_some() && (stock_slot(ctx, c).is_some() || purchase(ctx, c).is_some())
}

pub fn requested_trade(ctx: &Ctx<'_>) -> bool {
    active(ctx).is_some_and(|c| can_trade(ctx, &c))
}

#[derive(Debug)]
pub(super) struct Exchange {
    commission: Commission,
    trade: Trade,
    change: InventoryChange,
    money_change: i64,
    money_before: i64,
    confirm_until: Option<u64>,
}

#[derive(Debug)]
enum InventoryChange {
    Purchase(SmallVec<[(u32, i64); 4]>),
    Delivery(i64),
}

impl Exchange {
    pub(super) fn start(ctx: &mut Ctx<'_>, mut commission: Commission) -> Option<Self> {
        let ([x, y, _], role) = customer(ctx, &commission)?;
        let (deal, change, money_change) = if commission.awaits_materials() {
            let (give, want) = purchase(ctx, &commission)?;
            let after = want
                .iter()
                .map(|&(item, count)| (item, ctx.state.item_count(item) + i64::from(count)))
                .collect();
            (
                Deal::fixed(give, &want, 0),
                InventoryChange::Purchase(after),
                -i64::from(give.money),
            )
        } else {
            let quantity = i32::try_from(commission.quantity).ok()?;
            let give = Plan {
                slot: stock_slot(ctx, &commission)?,
                template: commission.item,
                amount: quantity,
                money: 0,
            };
            (
                Deal::fixed(give, &[], commission.price),
                InventoryChange::Delivery(
                    ctx.state.item_count(commission.item) - i64::from(quantity),
                ),
                i64::from(commission.price),
            )
        };
        let order = PairOrder {
            kind: PairKind::Trade,
            role,
            partner_bot: 0,
            partner_char: commission.customer,
            partner_name: commission.name.clone(),
            meet_at: (x, y),
        };
        let mut trade = Trade::for_deal(order, deal);
        trade.start(ctx);
        if commission.pickup_until_ms.take().is_some() {
            ctx.hub.journal.update(ctx.persona.id, |j| {
                j.craft_commission = Some(commission.clone());
            });
        }
        Some(Self {
            change,
            money_change,
            money_before: ctx.state.me.money,
            commission,
            trade,
            confirm_until: None,
        })
    }

    pub(super) fn tick(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        if !active(ctx).is_some_and(|mut c| {
            c.pickup_until_ms = None;
            c == self.commission
        }) {
            return GoalStatus::Failed(FailReason::NoTarget);
        }
        if self.confirm_until.is_none() {
            match self.trade.tick(ctx) {
                GoalStatus::Done => self.confirm_until = Some(ctx.now_ms + 4_000),
                status => return status,
            }
        }
        if ctx.state.me.money - self.money_before == self.money_change && self.received(ctx) {
            self.record_receipt(ctx);
            return GoalStatus::Done;
        }
        if self.confirm_until.is_some_and(|until| ctx.now_ms >= until) {
            return GoalStatus::Failed(FailReason::Timeout);
        }
        GoalStatus::Running
    }

    fn received(&self, ctx: &Ctx<'_>) -> bool {
        match &self.change {
            InventoryChange::Purchase(items) => items
                .iter()
                .all(|&(item, expected)| ctx.state.item_count(item) == expected),
            InventoryChange::Delivery(expected) => {
                ctx.state.item_count(self.commission.item) == *expected
            }
        }
    }

    fn record_receipt(&mut self, ctx: &mut Ctx<'_>) {
        let c = &mut self.commission;
        if let Some(terms) = c.materials.as_mut().filter(|terms| !terms.purchased) {
            terms.purchased = true;
            ctx.log.say(format!(
                "guild craft: paid {} copper to {} for recipe inputs",
                terms.copper, c.name
            ));
            ctx.hub.journal.update(ctx.persona.id, |j| {
                j.craft_commission = Some(c.clone());
            });
            tell(
                ctx,
                &c.name,
                "I bought the ingredients. I'll let you know when the order is ready.",
            );
        } else {
            ctx.hub
                .journal
                .update(ctx.persona.id, |j| j.craft_commission = None);
            let cost = c.materials.map_or(0, |terms| terms.copper);
            let proceeds = u64::try_from(c.price.saturating_sub(cost)).unwrap_or_default();
            treasury::note_receipt(ctx, GoalKind::Market, proceeds);
            ctx.log.say(format!(
                "guild craft: delivered item {} to {} for {} copper",
                c.item, c.name, c.price
            ));
        }
    }

    pub(super) fn purchasing(&self) -> bool {
        matches!(self.change, InventoryChange::Purchase(_))
    }

    pub(super) fn stop(&mut self, ctx: &mut Ctx<'_>) {
        self.trade.stop(ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::{offer, quoted_order};

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

    #[test]
    fn customer_material_credit_is_positive_and_below_the_delivery_price() {
        assert_eq!(
            quoted_order("Stone Brick for 100c with materials for 40c"),
            Some(("Stone Brick", 100, Some(40)))
        );
        assert_eq!(
            quoted_order("Stone Brick for 100c"),
            Some(("Stone Brick", 100, None))
        );
        for amount in ["0c", "100c", "101c", "-1c", "40"] {
            assert!(
                quoted_order(&format!("Stone Brick for 100c with materials for {amount}"))
                    .is_none()
            );
        }
    }
}
