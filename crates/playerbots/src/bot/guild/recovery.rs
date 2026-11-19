//! Rank-funded potions and other restorative consumables for combat guild members.

use aa_proto::{cs, guild::role};
use serde::Deserialize;
use smallvec::SmallVec;

use super::profession::{self, Profession};
use crate::bot::{ctx::Ctx, market::ads::Need};

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Supply {
    pub item: u32,
    pub unit_copper: u32,
    pub stock: u8,
    #[serde(default)]
    pub min_rank: u8,
}

impl Supply {
    pub fn valid(self) -> bool {
        self.item > 0
            && (1..=100_000).contains(&self.unit_copper)
            && (1..=20).contains(&self.stock)
            && self.min_rank <= role::COMMANDER
    }
}

pub fn supports(profession: Profession) -> bool {
    matches!(
        profession,
        Profession::Adventurer | Profession::BountyHunter | Profession::Thief
    )
}

type Supplies<'a> = [Option<&'a Supply>; 2];

fn supplies<'a>(ctx: &Ctx<'a>) -> Option<(u32, Supplies<'a>)> {
    if ctx.cfg.guild.recovery_supplies.is_empty() {
        return None;
    }
    let (guild, profession, false) = profession::membership(ctx)? else {
        return None;
    };
    if !supports(profession) {
        return None;
    }
    let rank = ctx.state.guild.log()?.member(ctx.state.me.char_id)?.role;
    let mut supplies: Supplies<'_> = [None, None];
    for supply in ctx.cfg.guild.recovery_supplies.get(&guild.to_string())? {
        if supply.min_rank > rank {
            continue;
        }
        let recovery = ctx.world.item_recovery(supply.item);
        for (slot, restores) in supplies.iter_mut().zip([recovery.health, recovery.mana]) {
            if restores && slot.is_none_or(|current| supply.min_rank > current.min_rank) {
                *slot = Some(supply);
            }
        }
    }
    Some((guild, supplies))
}

pub fn stock(ctx: &Ctx<'_>) -> SmallVec<[(u32, u32); 2]> {
    let mut stock = SmallVec::new();
    if let Some((_, supplies)) = supplies(ctx) {
        for supply in supplies.into_iter().flatten() {
            let entry = (supply.item, u32::from(supply.stock));
            if !stock.contains(&entry) {
                stock.push(entry);
            }
        }
    }
    stock
}

pub fn shopping(ctx: &Ctx<'_>) -> SmallVec<[Need; 2]> {
    let mut needs = SmallVec::new();
    let Some((guild, supplies)) = supplies(ctx) else {
        return needs;
    };
    let Some(terms) = ctx.cfg.guild.treasuries.get(&guild.to_string()) else {
        return needs;
    };
    let ready_cash = ctx
        .state
        .me
        .money
        .saturating_sub(i64::from(terms.reserve_copper))
        .max(0) as u64;
    let mut balance = ctx
        .hub
        .journal
        .read(ctx.persona.id, |journal| {
            journal.guild_treasury.get(&guild).map_or(0, |a| {
                a.support_received.saturating_sub(a.support_supplies_spent)
            })
        })
        .min(ready_cash);
    for supply in supplies.into_iter().flatten() {
        if needs.iter().any(|need: &Need| need.template == supply.item)
            || !ctx
                .world
                .item_facts(supply.item)
                .is_some_and(aa_world::ItemFacts::tradable)
        {
            continue;
        }
        let missing = i64::from(supply.stock)
            .saturating_sub(ctx.state.item_count(supply.item))
            .max(0) as u64;
        let qty = missing.min(balance / u64::from(supply.unit_copper)) as u32;
        if qty > 0 {
            needs.push(Need {
                template: supply.item,
                qty,
                order_price: Some(supply.unit_copper),
            });
            balance -= u64::from(qty) * u64::from(supply.unit_copper);
        }
    }
    needs
}

/// Called only after the ordinary market verifies both the goods and the purse debit.
pub fn note_purchase(ctx: &Ctx<'_>, item: u32, quantity: i64, total: i64) {
    let Some((guild, supplies)) = supplies(ctx) else {
        return;
    };
    if !supplies.into_iter().flatten().any(|supply| {
        supply.item == item
            && quantity > 0
            && quantity <= i64::from(supply.stock)
            && total == quantity * i64::from(supply.unit_copper)
    }) {
        return;
    }
    let charged = ctx.hub.journal.update(ctx.persona.id, |journal| {
        let Some(account) = journal.guild_treasury.get_mut(&guild) else {
            return false;
        };
        let total = total as u64;
        if total
            > account
                .support_received
                .saturating_sub(account.support_supplies_spent)
        {
            return false;
        }
        account.support_supplies_spent += total;
        true
    });
    if charged {
        ctx.log.say(format!("guild supplies: bought {quantity} of item {item} for {total} copper from rank allowances"));
    }
}

pub fn use_during_rest(ctx: &mut Ctx<'_>) -> bool {
    let Some((_, supplies)) = supplies(ctx) else {
        return false;
    };
    let needs = [ctx.state.me.hp_pct() < 70, ctx.state.me.mp_pct() < 55];
    let ready = supplies
        .into_iter()
        .zip(needs)
        .find_map(|(supply, needed)| {
            let supply = supply.filter(|_| needed)?;
            let (_, item) = ctx.state.find_item(supply.item)?;
            let skill = ctx.world.item_use_skill(supply.item);
            (skill != 0 && ctx.state.cooldown_ready(skill, ctx.now_ms)).then_some((skill, *item))
        });
    let Some((skill, item)) = ready else {
        return false;
    };
    let me = ctx.state.me.obj;
    ctx.send(cs::start_skill_item(skill, me, item.id, item.template, me));
    ctx.log.debug(format!(
        "guild supplies: taking item {} while resting",
        item.template
    ));
    true
}
