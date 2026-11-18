//! Prices inferred from trades whose inventory and purse changes matched the agreement.

use std::collections::VecDeque;

use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

const MAX_ITEMS: usize = 256;
const MAX_RECEIPTS: usize = 12;
const MAX_AGE_MS: u64 = 7 * 24 * 60 * 60 * 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub item: u32,
    pub quantity: u32,
    pub copper: u32,
    pub at_ms: u64,
}

#[derive(Debug, Default)]
pub struct Prices(Mutex<FxHashMap<u32, VecDeque<Receipt>>>);

impl Prices {
    pub fn remember(&self, receipt: Receipt) {
        if receipt.item == 0 || receipt.quantity == 0 || receipt.copper == 0 {
            return;
        }
        let mut items = self.0.lock();
        if items.len() >= MAX_ITEMS && !items.contains_key(&receipt.item) {
            let oldest = items
                .iter()
                .min_by_key(|(_, rows)| rows.back().map(|r| r.at_ms))
                .map(|(&item, _)| item);
            if let Some(oldest) = oldest {
                items.remove(&oldest);
            }
        }
        let rows = items.entry(receipt.item).or_default();
        if rows.len() >= MAX_RECEIPTS {
            rows.pop_front();
        }
        rows.push_back(receipt);
    }

    pub fn average(&self, item: u32, now: u64) -> Option<f32> {
        let items = self.0.lock();
        let mut number = 0;
        let (quantity, copper) = items
            .get(&item)?
            .iter()
            .filter(|r| now.saturating_sub(r.at_ms) <= MAX_AGE_MS)
            .fold((0u64, 0u64), |(qty, copper), r| {
                number += 1;
                (qty + u64::from(r.quantity), copper + u64::from(r.copper))
            });
        (number >= 2 && quantity > 0).then(|| copper as f32 / quantity as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn market_prices_weight_by_quantity_and_age_out_old_trades() {
        let prices = Prices::default();
        let now = MAX_AGE_MS + 100;
        for (quantity, copper, at_ms) in [(100, 100, 1), (1, 10, now - 2), (9, 180, now)] {
            prices.remember(Receipt {
                item: 42,
                quantity,
                copper,
                at_ms,
            });
        }
        assert_eq!(prices.average(42, now), Some(19.0));
        assert_eq!(prices.average(42, now + MAX_AGE_MS), None);
    }
}
