//! One funded resale per Trader guild member, accounted for after the sale mail pays.

use aa_proto::auction::{duration, listing_fee, Lot};
use serde::{Deserialize, Serialize};

use super::Stack;
use crate::bot::ctx::Ctx;
use crate::bot::guild::{profession, treasury};
use crate::bot::market::auction::{
    mail::SaleDetails,
    price::{self, Ask},
};
use crate::hub::unix_ms;

const TRACK_MS: u64 = 7 * 24 * 60 * 60 * 1_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resale {
    guild: u32,
    item_id: u64,
    template: u32,
    quantity: u32,
    cost: i64,
    expires: u64,
    listing: Option<Listing>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Listing {
    name: String,
    buyout: i32,
    fee: i32,
}

impl Resale {
    fn sale_profit(&self, details: &SaleDetails, copper: i64) -> Option<i64> {
        let listing = self.listing.as_ref()?;
        // Journals written before the deposit rounding was corrected can have a lower estimate.
        let fees = [listing.fee, listing_fee(listing.buyout, duration::H48)];
        (self.quantity == details.quantity
            && listing.buyout == details.buyout
            && fees.contains(&details.fee)
            && listing.name.eq_ignore_ascii_case(&details.item)
            && copper == i64::from(details.paid))
        .then(|| copper - self.cost - i64::from(details.fee))
    }
}

fn guild(ctx: &Ctx<'_>) -> Option<u32> {
    let (guild, profession::Profession::Trader, false) = profession::membership(ctx)? else {
        return None;
    };
    (ctx.cfg.guild.auction_flips && ctx.cfg.guild.treasuries.contains_key(&guild.to_string()))
        .then_some(guild)
}

fn current(ctx: &Ctx<'_>) -> Option<Resale> {
    let now = unix_ms();
    ctx.hub.journal.read(ctx.persona.id, |j| {
        j.auction_resale
            .as_ref()
            .filter(|r| r.expires > now)
            .cloned()
    })
}

pub(super) fn candidate(ctx: &Ctx<'_>, lot: &Lot, budget: i64) -> bool {
    let Some(guild) = guild(ctx) else {
        return false;
    };
    let funds =
        ctx.state.me.money - i64::from(ctx.cfg.guild.treasuries[&guild.to_string()].reserve_copper);
    lot.item_id != 0
        && lot.client_id != ctx.state.me.char_id
        && !ctx.hub.is_bot_char(lot.client_id)
        && !lot.has_bid()
        && lot.count > 0
        && i64::from(lot.buyout) <= funds.min(budget)
        && ctx.state.item_count(lot.template) == 0
        && !super::useful_item(ctx, lot.template)
        && !ctx
            .state
            .market
            .get()
            .is_some_and(|m| m.auction.has_lot_of(lot.template))
        && current(ctx).is_none()
        && ask(ctx, lot.template, lot.count as u32, i64::from(lot.buyout)).is_some()
}

fn ask(ctx: &Ctx<'_>, template: u32, quantity: u32, cost: i64) -> Option<Ask> {
    let unit = ctx.hub.market_prices.average(template, unix_ms())?;
    price::resale_ask(unit, quantity, cost)
}

pub(super) fn bought(ctx: &Ctx<'_>, lot: &Lot, cost: i64) {
    let Some(guild) = guild(ctx) else { return };
    if cost <= 0 || lot.count <= 0 {
        return;
    }
    ctx.hub.journal.update(ctx.persona.id, |j| {
        j.auction_resale = Some(Resale {
            guild,
            item_id: lot.item_id,
            template: lot.template,
            quantity: lot.count as u32,
            cost,
            expires: unix_ms() + TRACK_MS,
            listing: None,
        });
    });
}

pub(super) fn stack(ctx: &Ctx<'_>) -> Option<Stack> {
    let resale = current(ctx)?;
    if resale.guild != guild(ctx)? || resale.listing.is_some() {
        return None;
    }
    let (_, item) = ctx.state.find_item_where(resale.template, |item| {
        item.id == resale.item_id && item.count == resale.quantity as i32
    })?;
    let target = ask(ctx, item.template, resale.quantity, resale.cost)?;
    (i64::from(target.fee) <= ctx.state.me.money).then_some(Stack {
        item_id: item.id,
        template: item.template,
        count: resale.quantity,
    })
}

pub(super) fn listing_ask(ctx: &Ctx<'_>, stack: Stack) -> Option<Option<Ask>> {
    let resale = current(ctx)?;
    (resale.item_id == stack.item_id).then(|| {
        (resale.quantity == stack.count && resale.listing.is_none())
            .then(|| ask(ctx, stack.template, stack.count, resale.cost))
            .flatten()
    })
}

pub(super) fn is_listed(ctx: &Ctx<'_>, item_id: u64) -> bool {
    current(ctx).is_some_and(|r| r.item_id == item_id && r.listing.is_some())
}

pub(super) fn posted(ctx: &Ctx<'_>, stack: Stack, name: &str, ask: Ask) {
    let Some(resale) = current(ctx).filter(|r| r.item_id == stack.item_id) else {
        return;
    };
    ctx.hub.journal.update(ctx.persona.id, |j| {
        if let Some(r) = j.auction_resale.as_mut().filter(|r| **r == resale) {
            r.listing = Some(Listing {
                name: name.to_owned(),
                buyout: ask.buyout,
                fee: ask.fee,
            });
        }
    });
}

/// Returns true when this receipt belongs to a flip and has already been accounted for.
pub(super) fn settle(ctx: &Ctx<'_>, details: &SaleDetails, copper: i64) -> bool {
    let Some(resale) = current(ctx) else {
        return false;
    };
    let Some(profit) = resale.sale_profit(details, copper) else {
        return false;
    };
    let contribute = guild(ctx) == Some(resale.guild) && profit > 0;
    ctx.hub.journal.update(ctx.persona.id, |j| {
        j.auction_resale = None;
        j.auction_resale_profit = j.auction_resale_profit.saturating_add(profit);
        if contribute {
            treasury::queue_copper(j, resale.guild, profit as u64);
        }
    });
    ctx.log.say(format!(
        "auction: a guild resale earned {profit} copper after costs"
    ));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_native_receipt_uses_the_charged_fee_even_with_an_old_estimate() {
        let resale = Resale {
            guild: 42,
            item_id: 10,
            template: 14702,
            quantity: 1,
            cost: 200,
            expires: 1_000,
            listing: Some(Listing {
                name: "Kobold Haversack".to_owned(),
                buyout: 945,
                fee: 37,
            }),
        };
        let mut sale = SaleDetails {
            item: "Kobold Haversack".to_owned(),
            quantity: 1,
            paid: 850,
            buyout: 945,
            fee: 38,
        };
        assert_eq!(resale.sale_profit(&sale, 850), Some(612));
        assert_eq!(resale.sale_profit(&sale, 849), None);
        sale.fee = 39;
        assert_eq!(resale.sale_profit(&sale, 850), None);
    }
}
