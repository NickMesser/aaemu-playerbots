//! Sport lures bought from a Gatherer guild member's rank allowance.

use crate::bot::ctx::Ctx;
use crate::bot::guild::{profession, treasury};
use crate::bot::market::ads::{Funding, Need};

use super::LURE;

const STOCK: u32 = 6;

fn terms(ctx: &Ctx<'_>) -> Option<(u32, u32)> {
    let prices = &ctx.cfg.guild.fishing_lure_prices;
    if prices.is_empty() || !ctx.cfg.behavior.sport_fishing || ctx.gm_allowed() {
        return None;
    }
    let (guild, profession::Profession::Gatherer, false) = profession::membership(ctx)? else {
        return None;
    };
    let price = *prices.get(&guild.to_string())?;
    (super::has_rod(ctx)
        && ctx
            .world
            .item_facts(LURE)
            .is_some_and(aa_world::ItemFacts::tradable))
    .then_some((guild, price))
}

pub fn reserve(ctx: &Ctx<'_>) -> Option<(u32, u32)> {
    terms(ctx).map(|_| (LURE, STOCK))
}

pub fn shopping(ctx: &Ctx<'_>) -> Option<Need> {
    let (guild, price) = terms(ctx)?;
    let missing = u32::try_from(i64::from(STOCK) - ctx.state.item_count(LURE)).ok()?;
    let qty = u64::from(missing).min(treasury::allowance_budget(ctx, guild) / u64::from(price));
    (qty > 0).then_some(Need {
        template: LURE,
        qty: qty as u32,
        order_price: Some(price),
        funding: Some(Funding::Fishing(guild)),
    })
}

pub fn can_pay(ctx: &Ctx<'_>, guild: u32, template: u32, qty: u32, unit: u32) -> bool {
    template == LURE
        && terms(ctx).is_some_and(|(member_guild, price)| {
            member_guild == guild
                && (1..=price).contains(&unit)
                && qty > 0
                && ctx.state.item_count(LURE) + i64::from(qty) <= i64::from(STOCK)
                && u64::from(qty) * u64::from(unit) <= treasury::allowance_budget(ctx, guild)
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
        account.support_supplies_spent += copper;
        true
    });
    if charged {
        ctx.log.say(format!(
            "guild fishing: bought {qty} sport lures for {copper} copper from rank allowances"
        ));
    }
}

#[cfg(test)]
mod tests {
    use crate::config::Config;

    #[test]
    fn lure_prices_need_a_funded_gatherer_guild() {
        let cfg = |role, budget, price| {
            Config::parse(&format!(
                "[guild]\nprofessions = {{ '42' = '{role}' }}\ntreasuries = {{ '42' = {{ weekly_support_copper = {budget} }} }}\nfishing_lure_prices = {{ '42' = {price} }}"
            ))
        };
        assert_eq!(
            cfg("gatherer", 5000, 1000)
                .unwrap()
                .guild
                .fishing_lure_prices["42"],
            1000
        );
        assert!(cfg("trader", 5000, 1000).is_err());
        assert!(cfg("gatherer", 0, 1000).is_err());
        assert!(cfg("gatherer", 5000, 0).is_err());
    }
}
