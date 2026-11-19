//! Purchases of tax certificates for a garden the server has already quoted.

use aa_world::ItemFacts;

use super::{profession, treasury};
use crate::bot::ctx::Ctx;
use crate::bot::goals::housing::{certificates_for, certificates_held, TAX_CERTIFICATE};
use crate::bot::market::ads::{Funding, Need};

const QUOTE_MAX_AGE_MS: u64 = 30 * 60_000;

struct Shortfall {
    guild: u32,
    price: u32,
    missing: u32,
    budget: u64,
}

fn shortfall(ctx: &Ctx<'_>) -> Option<Shortfall> {
    if ctx.cfg.guild.farm_tax_prices.is_empty() || ctx.gm_allowed() || !ctx.state.houses.is_empty()
    {
        return None;
    }
    let (guild, profession::Profession::Potato, false) = profession::membership(ctx)? else {
        return None;
    };
    let &price = ctx.cfg.guild.farm_tax_prices.get(&guild.to_string())?;
    let quote = ctx.state.house_log.quote?;
    let design = ctx.world.housing_design(quote.design)?;
    if ctx.now_ms.saturating_sub(quote.at_ms) > QUOTE_MAX_AGE_MS
        || !matches!(design.category, 16 | 17)
        || ctx.state.find_item(design.item).is_none()
        || !ctx
            .world
            .item_facts(TAX_CERTIFICATE)
            .is_some_and(ItemFacts::tradable)
    {
        return None;
    }
    let missing = u32::try_from(certificates_for(quote.total) - certificates_held(ctx)).ok()?;
    (missing > 0).then(|| Shortfall {
        guild,
        price,
        missing,
        budget: treasury::allowance_budget(ctx, guild),
    })
}

pub fn shopping(ctx: &Ctx<'_>) -> Option<Need> {
    let order = shortfall(ctx)?;
    let qty = u64::from(order.missing).min(order.budget / u64::from(order.price)) as u32;
    (qty > 0).then_some(Need {
        template: TAX_CERTIFICATE,
        qty,
        order_price: Some(order.price),
        funding: Some(Funding::FarmLand(order.guild)),
    })
}

pub fn can_pay(ctx: &Ctx<'_>, guild: u32, item: u32, qty: u32, unit: u32) -> bool {
    item == TAX_CERTIFICATE
        && shortfall(ctx).is_some_and(|order| {
            order.guild == guild
                && (1..=order.price).contains(&unit)
                && (1..=order.missing).contains(&qty)
                && u64::from(qty) * u64::from(unit) <= order.budget
        })
}

pub fn note_purchase(ctx: &Ctx<'_>, guild: u32, qty: i64, copper: u64) {
    let charged = ctx.hub.journal.update(ctx.persona.id, |journal| {
        let Some(account) = journal.guild_treasury.get_mut(&guild) else {
            return false;
        };
        if copper == 0 || copper > account.allowance_balance() {
            return false;
        }
        account.support_land_spent += copper;
        true
    });
    if charged {
        ctx.log.say(format!(
            "guild land: bought {qty} tax certificates for {copper} copper from rank allowances"
        ));
    }
}
