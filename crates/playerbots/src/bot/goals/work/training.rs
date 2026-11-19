//! Rank allowances for ingredients consumed while raising crafting proficiency.

use std::collections::BTreeMap;

use aa_world::{Craft, ItemFacts};

use super::{selected_order, Order};
use crate::bot::ctx::Ctx;
use crate::bot::guild::{profession, treasury};

pub struct Allowance<'a> {
    pub guild: u32,
    pub copper: u64,
    craft: &'a Craft,
    prices: &'a BTreeMap<String, u32>,
}

impl<'a> Allowance<'a> {
    pub fn for_order(ctx: &Ctx<'a>, order: Option<(Order, bool)>) -> Option<Self> {
        let (Order::Craft { recipe, .. }, true) = order? else {
            return None;
        };
        let (guild, profession::Profession::Crafter, false) = profession::membership(ctx)? else {
            return None;
        };
        let prices = ctx
            .cfg
            .guild
            .training_material_prices
            .get(&guild.to_string())?;
        let craft = ctx.world.craft(recipe)?;
        if craft.is_pack || craft.req_level > ctx.state.me.level {
            return None;
        }
        Some(Self {
            guild,
            copper: treasury::allowance_budget(ctx, guild),
            craft,
            prices,
        })
    }

    pub fn price(&self, template: u32) -> Option<u32> {
        self.prices.get(&template.to_string()).copied()
    }

    fn missing(&self, ctx: &Ctx<'_>, template: u32) -> u32 {
        let required: i64 = self
            .craft
            .materials
            .iter()
            .filter(|&&(item, _)| item == template)
            .map(|&(_, count)| i64::from(count))
            .sum();
        u32::try_from(required - ctx.state.item_count(template)).unwrap_or(0)
    }
}

/// Recheck the marked order when the seller arrives, using live membership, bag and purse.
pub fn can_pay(ctx: &Ctx<'_>, guild: u32, item: u32, qty: u32, unit: u32) -> bool {
    Allowance::for_order(ctx, selected_order(ctx)).is_some_and(|allowance| {
        allowance.guild == guild
            && unit > 0
            && allowance.price(item).is_some_and(|price| unit <= price)
            && qty > 0
            && qty <= allowance.missing(ctx, item)
            && u64::from(qty) * u64::from(unit) <= allowance.copper
            && ctx.world.item_facts(item).is_some_and(ItemFacts::tradable)
    })
}

/// Book only a native trade with the exact agreed bag gain and purse debit.
pub fn note_purchase(ctx: &Ctx<'_>, guild: u32, item: u32, qty: i64, copper: u64) {
    let charged = ctx.hub.journal.update(ctx.persona.id, |journal| {
        let Some(account) = journal.guild_treasury.get_mut(&guild) else {
            return false;
        };
        if copper == 0 || copper > account.allowance_balance() {
            return false;
        }
        account.support_training_spent += copper;
        true
    });
    if charged {
        ctx.log.say(format!(
            "guild training: bought {qty} of item {item} for {copper} copper from rank allowances"
        ));
    }
}
