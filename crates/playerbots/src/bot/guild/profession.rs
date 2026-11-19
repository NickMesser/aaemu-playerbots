//! Operator-assigned professions for server guilds. The goals still check their own prerequisites.

use serde::Deserialize;

use crate::bot::{cmd::GoalKind, ctx::Ctx};

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Profession {
    Potato,
    Thief,
    Trader,
    Adventurer,
    BountyHunter,
    Crafter,
    Gatherer,
}

impl Profession {
    pub fn title(self) -> &'static str {
        match self {
            Self::Potato => "Potato",
            Self::Thief => "Thief",
            Self::Trader => "Trader",
            Self::Adventurer => "Adventurer",
            Self::BountyHunter => "Bounty hunter",
            Self::Crafter => "Crafter",
            Self::Gatherer => "Gatherer",
        }
    }

    pub fn goals(self) -> &'static [GoalKind] {
        use GoalKind::*;
        match self {
            Self::Potato => &[Farm, Market],
            Self::Thief => &[Gank, Farm, Work],
            Self::Trader => &[TradeRun, Auction, Market],
            Self::Adventurer => &[Dungeon, Grind, FieldParty, Work],
            Self::BountyHunter => &[Warzone, Work],
            Self::Crafter => &[Work, Market],
            Self::Gatherer => &[Work, Fish, Market],
        }
    }

    pub fn accepts(self, thief_history: bool) -> bool {
        !thief_history || !matches!(self, Self::Trader | Self::Crafter)
    }
}

pub fn of(ctx: &Ctx<'_>, guild: u32) -> Option<Profession> {
    if !ctx.cfg.guild.enabled || ctx.cfg.guild.professions.is_empty() || guild == 0 {
        return None;
    }
    ctx.cfg.guild.professions.get(&guild.to_string()).copied()
}

pub fn membership(ctx: &Ctx<'_>) -> Option<(u32, Profession, bool)> {
    let log = ctx.state.guild.log().filter(|log| log.in_guild())?;
    let profession = of(ctx, log.id)?;
    // Wait for the member list before starting work as a regular member.
    let member = log.member(ctx.state.me.char_id)?;
    Some((log.id, profession, log.is_owner(member.char_id)))
}

pub fn farm_order(ctx: &Ctx<'_>) -> Option<u32> {
    if ctx.cfg.guild.farm_orders.is_empty() && ctx.cfg.guild.farm_choices.is_empty() {
        return None;
    }
    let (id, Profession::Potato, false) = membership(ctx)? else {
        return None;
    };
    let key = id.to_string();
    let known = ctx.cfg.guild.farm_choices.get(&key);
    let prices = &ctx.hub.market_prices;
    let now = crate::hub::unix_ms();
    let market_order = known
        .into_iter()
        .flatten()
        .filter_map(|&seed| {
            let farm = ctx.world.farm();
            let crop = farm.crop_of_seed(seed)?;
            let yields = if crop.plantable_on_land() {
                &crop.produce[..]
            } else if ctx.cfg.farm.husbandry {
                farm.livestock_yields(crop.doodad)?
            } else {
                return None;
            };
            let market_return: f32 = yields
                .iter()
                .filter_map(|y| prices.average(y.item, now).map(|price| y.average() * price))
                .sum();
            let seed_price = ctx.world.item_facts(seed).map_or(0, |i| i.price);
            let profit = market_return - seed_price as f32;
            (profit > 0.0).then_some((seed, profit / crop.grow_ms as f32))
        })
        .max_by(|a, b| a.1.total_cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
        .map(|(seed, _)| seed);
    market_order.or_else(|| ctx.cfg.guild.farm_orders.get(&key).copied())
}

pub fn trade_pack_order(ctx: &Ctx<'_>) -> Option<u32> {
    if ctx.cfg.guild.trade_pack_orders.is_empty() {
        return None;
    }
    let (guild, Profession::Trader, false) = membership(ctx)? else {
        return None;
    };
    ctx.cfg
        .guild
        .trade_pack_orders
        .get(&guild.to_string())
        .copied()
}

/// Time spent in professional goals must be paid back with personal play. Carrying debt forward
/// lets a run finish without cutting it off during a trade or a dungeon.
#[derive(Debug, Default)]
pub struct ServiceTime {
    started: Option<u64>,
    next_work_ms: u64,
}

impl ServiceTime {
    pub fn available(&self, now: u64) -> bool {
        !self.active() && now >= self.next_work_ms
    }

    pub fn active(&self) -> bool {
        self.started.is_some()
    }

    pub fn start(&mut self, now: u64) {
        self.started = Some(now);
    }

    pub fn finish(&mut self, now: u64, percent: u8) {
        if let Some(started) = self.started.take() {
            let share = u64::from(percent.clamp(50, 75));
            let personal = now.saturating_sub(started).saturating_mul(100 - share);
            self.next_work_ms = self
                .next_work_ms
                .max(now)
                .saturating_add(personal.div_ceil(share));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guild_work_earns_personal_time_at_both_allowed_shares() {
        for (share, ready) in [(50, 700), (75, 500)] {
            let mut service = ServiceTime::default();
            service.start(100);
            assert!(!service.available(100));
            service.finish(400, share);
            assert!(!service.available(ready - 1));
            assert!(service.available(ready));
        }
        assert!(!Profession::Trader.accepts(true) && !Profession::Crafter.accepts(true));
        assert!(Profession::Potato.accepts(true) && Profession::Trader.accepts(false));
    }

    #[test]
    fn crop_and_trade_pack_orders_require_the_matching_guild() {
        let cfg = crate::config::Config::parse(
            r#"
[guild]
enabled = true
work_percent = 75
professions = { "42" = "potato", "43" = "trader" }
farm_orders = { "42" = 12345 }
trade_pack_orders = { "43" = 6223 }
"#,
        )
        .unwrap();
        assert_eq!(cfg.guild.professions["42"], Profession::Potato);
        assert_eq!(cfg.guild.farm_orders["42"], 12345);
        assert_eq!(cfg.guild.trade_pack_orders["43"], 6223);
        for invalid in [
            "[guild]\nprofessions = { '0' = 'potato' }",
            "[guild]\nwork_percent = 76",
            "[guild]\nprofessions = { '43' = 'trader' }\nfarm_orders = { '43' = 1 }",
            "[guild]\nprofessions = { '42' = 'potato' }\ntrade_pack_orders = { '42' = 6223 }",
        ] {
            assert!(crate::config::Config::parse(invalid).is_err());
        }
    }
}
