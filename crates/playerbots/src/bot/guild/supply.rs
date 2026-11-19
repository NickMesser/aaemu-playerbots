//! Finite supply orders between configured guilds, settled through native trades.

use serde::{Deserialize, Serialize};

use super::profession;
use crate::bot::{
    ctx::Ctx,
    market::ads::{Funding, Need},
};
use crate::hub::unix_ms;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Contract {
    pub id: u32,
    pub buyer_character: u32,
    pub supplier: u32,
    pub item: u32,
    pub quantity: u32,
    pub unit_copper: u32,
}

impl Contract {
    pub fn valid(self, buyer: u32) -> bool {
        self.id != 0
            && self.buyer_character != 0
            && self.supplier != 0
            && self.supplier != buyer
            && self.item != 0
            && (1..=1_000).contains(&self.quantity)
            && (1..=100_000).contains(&self.unit_copper)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct Receipt {
    pub contract: Contract,
    pub quantity: u32,
    pub copper: u64,
}

fn receipt(contract: Contract, prior: Option<Receipt>) -> Option<Receipt> {
    match prior {
        Some(prior) if prior.contract == contract => Some(prior),
        Some(prior) if prior.contract.id >= contract.id => None,
        _ => Some(Receipt {
            contract,
            quantity: 0,
            copper: 0,
        }),
    }
}

fn progress(ctx: &Ctx<'_>, guild: u32) -> Option<Receipt> {
    let contract = *ctx.cfg.guild.supply_contracts.get(&guild.to_string())?;
    if contract.buyer_character != ctx.state.me.char_id {
        return None;
    }
    let prior = ctx.hub.journal.read(ctx.persona.id, |journal| {
        journal.guild_supply_receipts.get(&guild).copied()
    });
    receipt(contract, prior)
}

fn budget(ctx: &Ctx<'_>, guild: u32) -> u64 {
    ctx.cfg
        .guild
        .treasuries
        .get(&guild.to_string())
        .map_or(0, |terms| {
            ctx.state
                .me
                .money
                .saturating_sub(i64::from(terms.reserve_copper))
                .max(0) as u64
        })
}

pub fn shopping(ctx: &Ctx<'_>) -> Option<Need> {
    if ctx.cfg.guild.supply_contracts.is_empty() {
        return None;
    }
    let (guild, _, true) = profession::membership(ctx)? else {
        return None;
    };
    let Receipt {
        contract, quantity, ..
    } = progress(ctx, guild)?;
    if !ctx
        .world
        .item_facts(contract.item)
        .is_some_and(aa_world::ItemFacts::tradable)
    {
        return None;
    }
    let missing = u64::from(contract.quantity.saturating_sub(quantity));
    let affordable = budget(ctx, guild) / u64::from(contract.unit_copper);
    let qty = missing.min(affordable) as u32;
    (qty != 0).then_some(Need {
        template: contract.item,
        qty,
        order_price: Some(contract.unit_copper),
        funding: Some(Funding::SupplyContract(guild, contract.id)),
    })
}

pub fn permits_supplier(
    cfg: &crate::config::GuildCfg,
    book: &super::GuildBook,
    funding: Option<Funding>,
    name: &str,
    character: Option<u32>,
) -> bool {
    let Some(Funding::SupplyContract(guild, id)) = funding else {
        return true;
    };
    let Some(contract) = cfg
        .supply_contracts
        .get(&guild.to_string())
        .filter(|c| c.id == id)
    else {
        return false;
    };
    book.has_member(contract.supplier, name, character, unix_ms())
}

pub fn can_pay(ctx: &Ctx<'_>, guild: u32, id: u32, item: u32, qty: u32, unit: u32) -> bool {
    shopping(ctx).is_some_and(|need| {
        need.funding == Some(Funding::SupplyContract(guild, id))
            && item == need.template
            && (1..=need.qty).contains(&qty)
            && (1..=need.order_price.unwrap_or(0)).contains(&unit)
    })
}

pub fn note_purchase(ctx: &Ctx<'_>, guild: u32, id: u32, item: u32, qty: i64, total: i64) {
    let Some(mut current) =
        progress(ctx, guild).filter(|r| r.contract.id == id && r.contract.item == item)
    else {
        return;
    };
    let Ok(qty) = u32::try_from(qty) else { return };
    let Ok(total) = u64::try_from(total) else {
        return;
    };
    if !(1..=current.contract.quantity.saturating_sub(current.quantity)).contains(&qty)
        || !(u64::from(qty)..=u64::from(qty) * u64::from(current.contract.unit_copper))
            .contains(&total)
    {
        return;
    }
    current.quantity += qty;
    current.copper += total;
    ctx.hub.journal.update(ctx.persona.id, |journal| {
        journal.guild_supply_receipts.insert(guild, current);
    });
    ctx.log.say(format!("guild contract {id}: bought {qty} of item {item} from guild {} for {total} copper, {}/{} delivered", current.contract.supplier, current.quantity, current.contract.quantity));
}

pub fn summary(ctx: &Ctx<'_>, guild: u32) -> Option<String> {
    let Receipt {
        contract,
        quantity,
        copper,
    } = progress(ctx, guild)?;
    let item = ctx.world.item_name(contract.item).unwrap_or("supplies");
    Some(format!(" Supply contract {} with guild {} has received {quantity} of {} {item}, paying {copper} copper. Its limit is {} copper per item.", contract.id, contract.supplier, contract.quantity, contract.unit_copper))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_id_keeps_progress_and_cannot_be_reused_or_rolled_back() {
        let contract = Contract {
            id: 2,
            buyer_character: 11,
            supplier: 43,
            item: 8008,
            quantity: 20,
            unit_copper: 50,
        };
        let prior = Some(Receipt {
            contract,
            quantity: 15,
            copper: 700,
        });
        assert_eq!(receipt(contract, prior), prior);
        assert!(receipt(
            Contract {
                quantity: 21,
                ..contract
            },
            prior
        )
        .is_none());
        assert!(receipt(Contract { id: 1, ..contract }, prior).is_none());
        let next = receipt(Contract { id: 3, ..contract }, prior).unwrap();
        assert_eq!((next.quantity, next.copper), (0, 0));
    }

    #[test]
    fn contract_only_accepts_the_configured_guild_and_native_character() {
        let mut cfg = crate::config::GuildCfg::default();
        cfg.supply_contracts.insert(
            "42".into(),
            Contract {
                id: 1,
                buyer_character: 11,
                supplier: 43,
                item: 8008,
                quantity: 20,
                unit_copper: 50,
            },
        );
        let guild = super::super::GuildBook::default();
        let now = unix_ms();
        let mut card = super::super::Card {
            bot: 2,
            char_id: 22,
            name: "Vendor".into(),
            mother: 101,
            level: 20,
            guild: 43,
            flags: 0,
            seen_ms: 0,
        };
        guild.note_member(
            2,
            Some(super::super::Membership {
                id: 43,
                name: "Gatherers".into(),
                mother: 101,
            }),
        );
        guild.update_card(card.clone(), now);
        let funding = Some(Funding::SupplyContract(42, 1));
        assert!(permits_supplier(&cfg, &guild, funding, "vendor", Some(22)));
        assert!(!permits_supplier(&cfg, &guild, funding, "vendor", Some(23)));
        assert!(!permits_supplier(&cfg, &guild, funding, "pretender", None));
        card.guild = 42;
        guild.update_card(card, now);
        assert!(!permits_supplier(&cfg, &guild, funding, "vendor", None));
    }
}
