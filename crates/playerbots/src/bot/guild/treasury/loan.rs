//! Copper advances for the ingredients of an assigned guild trade pack.

use serde::{Deserialize, Serialize};

use super::{account_mut, Terms};
use crate::bot::ctx::Ctx;
use crate::bot::guild::profession::{self, Profession};
use crate::bot::market::ads::price::{PriceSource, VendorPrices};
use crate::hub::unix_ms;

const REQUEST_MS: u64 = 10 * 60_000;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Loan {
    pub principal: u32,
    pub repayment: u32,
    received_week: u64,
    request_copper: u32,
    request_until: u64,
}

impl Loan {
    pub(super) fn unused(&self) -> bool {
        self.received_week == 0 && self.request_copper == 0
    }

    pub(super) fn requested(&self, now: u64) -> u32 {
        if self.principal == 0 && now < self.request_until {
            self.request_copper
        } else {
            0
        }
    }

    pub(super) fn credit(&mut self, week: u64, copper: u32) -> bool {
        if week <= self.received_week {
            return false;
        }
        self.received_week = week;
        self.principal = self.principal.saturating_add(copper);
        self.request_copper = 0;
        self.request_until = 0;
        true
    }

    pub(super) fn reserve_from_receipt(&mut self, copper: u64) {
        let balance = self.principal.saturating_sub(self.repayment);
        self.repayment += copper.min(u64::from(balance)) as u32;
    }

    pub(super) fn paid(&mut self, copper: u32) {
        let paid = self.repayment.min(copper);
        self.repayment -= paid;
        self.principal = self.principal.saturating_sub(paid);
    }
}

pub(super) fn request(ctx: &Ctx<'_>, guild: u32, role: Profession, terms: Terms) {
    if terms.trade_loan_copper == 0
        || role != Profession::Trader
        || ctx.state.place.in_instance()
        || !ctx.cfg.market.chat_ads
    {
        return;
    }
    let now = unix_ms();
    let wanted = ingredient_budget(ctx, terms);
    let changed = ctx
        .hub
        .journal
        .read(ctx.persona.id, |j| match j.guild_treasury.get(&guild) {
            None => wanted != 0,
            Some(a) if a.loan.principal > 0 => false,
            Some(a) => {
                a.loan.request_copper != wanted
                    || wanted != 0 && a.loan.request_until.saturating_sub(now) < 60_000
            }
        });
    if changed {
        ctx.hub.journal.update(ctx.persona.id, |j| {
            if let Some(a) = account_mut(j, guild) {
                a.loan.request_copper = wanted;
                a.loan.request_until = now + REQUEST_MS;
            }
        });
    }
}

fn ingredient_budget(ctx: &Ctx<'_>, terms: Terms) -> u32 {
    let Some(recipe) = profession::trade_pack_order(ctx).and_then(|id| ctx.world.craft(id)) else {
        return 0;
    };
    if !recipe.is_pack || ctx.state.me.level < recipe.req_level {
        return 0;
    }
    let prices = VendorPrices(ctx.world);
    let mut cost = 0u64;
    let mut highest = 0u64;
    for &(item, qty) in recipe.materials.iter() {
        let left = i64::from(qty) - ctx.state.item_count(item);
        if left <= 0 {
            continue;
        }
        let Some(quote) = prices.quote(item) else {
            return 0;
        };
        if !ctx
            .world
            .item_facts(item)
            .is_some_and(aa_world::ItemFacts::tradable)
        {
            return 0;
        }
        highest = highest.max(u64::from(quote.ask));
        cost = cost.saturating_add(u64::from(quote.ask).saturating_mul(left as u64));
    }
    if cost == 0 {
        return 0;
    }
    let cash = (ctx.state.me.money - i64::from(terms.reserve_copper)).max(0) as u64;
    let shortfall = cost.max(highest * 3).max(500).saturating_sub(cash);
    if shortfall <= u64::from(terms.trade_loan_copper) {
        shortfall as u32
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipts_reserve_principal_until_the_contribution_is_confirmed() {
        let mut loan = Loan::default();
        assert!(loan.credit(12, 4_000));
        assert!(!loan.credit(12, 4_000));
        loan.reserve_from_receipt(1_200);
        assert_eq!((loan.principal, loan.repayment), (4_000, 1_200));
        loan.paid(1_200);
        loan.reserve_from_receipt(9_000);
        assert_eq!((loan.principal, loan.repayment), (2_800, 2_800));
        loan.paid(2_800);
        assert!(!loan.credit(12, 4_000));
        assert_eq!((loan.principal, loan.repayment), (0, 0));
    }
}
