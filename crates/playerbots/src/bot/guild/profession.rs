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
            Self::Thief => &[Gank],
            Self::Trader => &[TradeRun, Auction, Market],
            Self::Adventurer => &[Dungeon, Grind],
            Self::BountyHunter => &[Warzone],
            Self::Crafter | Self::Gatherer => &[Work],
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
    if ctx.cfg.guild.farm_orders.is_empty() {
        return None;
    }
    let (id, Profession::Potato, false) = membership(ctx)? else {
        return None;
    };
    ctx.cfg.guild.farm_orders.get(&id.to_string()).copied()
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
    fn crop_orders_are_only_for_configured_potato_guilds() {
        let cfg = crate::config::Config::parse(
            r#"
[guild]
enabled = true
work_percent = 75
professions = { "42" = "potato", "43" = "trader" }
farm_orders = { "42" = 12345 }
"#,
        )
        .unwrap();
        assert_eq!(cfg.guild.professions["42"], Profession::Potato);
        assert_eq!(cfg.guild.farm_orders["42"], 12345);
        for invalid in [
            "[guild]\nprofessions = { '0' = 'potato' }",
            "[guild]\nwork_percent = 76",
            "[guild]\nprofessions = { '43' = 'trader' }\nfarm_orders = { '43' = 1 }",
        ] {
            assert!(crate::config::Config::parse(invalid).is_err());
        }
    }
}
