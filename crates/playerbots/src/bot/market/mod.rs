//! The market: what a bot may sell, and the memory of the features that sell it.
//!
//! Three features share this module and edit only their own files:
//!
//! * the **provenance ledger** ([`ledger`], done): how many of each item the bot came by honestly
//!   and may therefore sell. It is fed by the item tasks the server sends (`BotState::apply`);
//! * the **auction house** (`AuctionMemory` in [`auction`], goal `auction`);
//! * **trade-chat sales** (`AdState` in [`ads`], goal `market`): advertisements, buyers, trades
//!   with players. It hears the shout, trade and group-finding chat and the whispers through
//!   [`MarketState::on_chat`], forwarded by the social duty.
//!
//! None of them deals in an item that is not tradable (not sellable, soul-bound or a quest item,
//! see [`aa_world::ItemFacts::tradable`]) or in the part of an item the bot's active quests still
//! need ([`needs`]).
//!
//! The per-bot budget is tight (`size_of::<Bot>() < 3200` is asserted): [`MarketState`] is one
//! pointer, and the [`Inner`] behind it is allocated by the first thing that needs it (the ledger
//! when the bot first gains an item it may sell), so a bot that never gained one costs eight
//! bytes. [`BotState`](super::state::BotState) owns one (`state.market`).

pub mod ads;
pub mod auction;
pub mod ledger;
pub mod needs;

use aa_proto::sc::ServerEvent;
use aa_world::World;
use rustc_hash::FxHashMap;

use self::ads::AdState;
use self::auction::AuctionMemory;
use self::ledger::{Ledger, Touched};
use self::needs::QuestNeeds;
use super::farm::FarmMem;
use super::state::{ChatIn, Item};

/// Hands the chat lines the market features care about (see [`ads::hears`]: shouts, trade and
/// group-finding chat, whispers) to `sink` as `(chat type, sender name, text, bot clock ms)`, in
/// order, and returns how many it handed over. The social duty calls it with the bot's inbox
/// before it files the lines for its own replies; it changes nothing about them.
pub fn forward_chat(lines: &[ChatIn], mut sink: impl FnMut(i16, &str, &str, u64)) -> usize {
    let mut n = 0;
    for l in lines.iter().filter(|l| ads::hears(l.kind)) {
        sink(l.kind, &l.name, &l.text, l.at_ms);
        n += 1;
    }
    n
}

/// What a bot remembers about selling. See the module docs.
#[derive(Debug, Default)]
pub struct MarketState(Option<Box<Inner>>);

/// The memory behind [`MarketState`]: one allocation per bot that has anything to remember.
#[derive(Debug, Default)]
pub struct Inner {
    /// The goods the bot may sell.
    pub ledger: Ledger,
    /// What the auction house goal remembers (owned by the auction feature).
    pub auction: AuctionMemory,
    /// What the trade-chat sales remember (owned by the market feature).
    pub ads: AdState,
    /// What the farm goal remembers: the bot's plots (owned by the farming feature, see
    /// [`crate::bot::farm`]); made by the first crop or farm run.
    pub farm: Option<Box<FarmMem>>,
}

impl MarketState {
    /// The memory, if anything made it yet.
    pub fn get(&self) -> Option<&Inner> {
        self.0.as_deref()
    }

    /// The memory, if anything made it yet.
    pub fn get_mut(&mut self) -> Option<&mut Inner> {
        self.0.as_deref_mut()
    }

    /// The memory, made on first use.
    pub fn inner_mut(&mut self) -> &mut Inner {
        self.0.get_or_insert_with(Default::default)
    }

    /// The farm memory, if the bot has farmed or seen a crop of its own.
    pub fn farm(&self) -> Option<&FarmMem> {
        self.get().and_then(|i| i.farm.as_deref())
    }

    /// The farm memory, made on first use.
    pub fn farm_mut(&mut self) -> &mut FarmMem {
        self.inner_mut().farm.get_or_insert_with(Default::default)
    }

    /// Takes the farm memory out for the length of a tick (a new one when the bot has none), so
    /// that the goal can read the rest of the bot's state while it changes the plots. Put it back
    /// with [`MarketState::put_farm`].
    pub fn take_farm(&mut self) -> Box<FarmMem> {
        self.get_mut()
            .and_then(|i| i.farm.take())
            .unwrap_or_default()
    }

    /// Gives the farm memory back after [`MarketState::take_farm`].
    pub fn put_farm(&mut self, farm: Box<FarmMem>) {
        self.inner_mut().farm = Some(farm);
    }

    /// The farm goal starts a harvest: until [`MarketState::close_harvest`], what the bag gains
    /// from a doodad counts as the bot's own when `honest` (the seeds were bought) and as nothing
    /// otherwise, whatever reason the server names (see the ledger docs).
    pub fn open_harvest(&mut self, honest: bool) {
        self.farm_mut().harvest_honest = Some(honest);
    }

    /// The harvest is over.
    pub fn close_harvest(&mut self) {
        if let Some(f) = self.get_mut().and_then(|i| i.farm.as_deref_mut()) {
            f.harvest_honest = None;
        }
    }

    /// Whether a gain with this item task reason is the bot's own: the clean reasons, a harvest
    /// of honest seeds, and nothing at all while a dishonest harvest is under way.
    fn counts(&self, reason: u8) -> bool {
        match self.farm().and_then(|f| f.harvest_honest) {
            Some(false) => false,
            Some(true) => {
                reason == ledger::reason::DOODAD_INTERACTION || ledger::is_clean_reason(reason)
            }
            None => ledger::is_clean_reason(reason),
        }
    }

    /// How many of item `template` the bot may sell (see [`Ledger::clean_count`]).
    pub fn clean_count(&self, template: u32) -> u32 {
        self.get().map_or(0, |i| i.ledger.clean_count(template))
    }

    /// What the bot may offer for sale now, as `(template, count)`: see
    /// [`Ledger::sellable_surplus`]. `needs` is what its active quests still need.
    pub fn sellable_surplus<'a>(
        &'a self,
        world: &'a World,
        bag: &'a FxHashMap<u16, Item>,
        needs: &'a QuestNeeds,
    ) -> impl Iterator<Item = (u32, u32)> + 'a {
        self.get()
            .into_iter()
            .flat_map(move |i| i.ledger.sellable_surplus(world, bag, needs))
    }

    /// An item task was applied to the bag: `touched` holds the bag counts of the templates it
    /// named from before ([`Touched::before`]), `bag` is the bag now. Called by
    /// `BotState::apply`.
    pub fn on_item_task(&mut self, reason: u8, touched: &Touched, bag: &FxHashMap<u16, Item>) {
        if touched.is_empty() {
            return;
        }
        for &(template, before) in touched.iter() {
            let now = ledger::bag_count(bag, template);
            if now > before {
                if self.counts(reason) {
                    let gained = u32::try_from(now - before).unwrap_or(u32::MAX);
                    self.inner_mut().ledger.add(template, gained);
                }
            } else if now < before {
                if let Some(inner) = self.get_mut() {
                    let lost = u32::try_from(before - now).unwrap_or(u32::MAX);
                    inner.ledger.remove(template, lost);
                }
            }
            if let Some(inner) = self.get_mut() {
                inner.ledger.clamp(template, now);
            }
        }
    }

    /// The server sent a bag chunk: the bag is what it says, whatever the ledger believed.
    pub fn on_bag_reloaded(&mut self, bag: &FxHashMap<u16, Item>) {
        if let Some(inner) = self.get_mut() {
            inner.ledger.clamp_all(bag);
        }
    }

    /// A server event passed `BotState::apply`: the auction memory follows the answers of the
    /// auction house and the mail it sends. A bot without market memory makes it for those only.
    pub fn on_server_event(&mut self, ev: &ServerEvent, now_ms: u64) {
        if self.0.is_none() && !AuctionMemory::concerns(ev) {
            return;
        }
        self.inner_mut().auction.observe(ev, now_ms);
    }

    /// A chat line the bot heard: `kind` is the server's chat type (1 shout, 2 trade, 3 group
    /// finding, -3 whisper, ...), `sender` the character's name, `now_ms` the bot clock. The
    /// social duty forwards the lines the market cares about, besides doing its own work with
    /// them. Does nothing until the bot has market memory (see [`MarketState::inner_mut`]).
    pub fn on_chat(&mut self, kind: i16, sender: &str, text: &str, now_ms: u64) {
        if let Some(inner) = self.get_mut() {
            inner.ads.on_chat(kind, sender, text, now_ms);
        } else if ads::worth_hearing(kind, text) {
            // (a line about buying or selling: now the bot has something to remember)
            self.inner_mut().ads.on_chat(kind, sender, text, now_ms);
        }
    }

    /// Whether a whisper from `sender` belongs to a negotiation of the market, so that the social
    /// duty must not answer it as well.
    pub fn claims_whisper(&self, sender: &str) -> bool {
        self.get().is_some_and(|i| i.ads.claims_whisper(sender))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aa_proto::sc_world::{ItemChange, ItemTaskInfo};

    use crate::bot::state::SLOT_BAG;
    use ledger::reason;

    fn chat(kind: i16, name: &str, text: &str, at_ms: u64) -> ChatIn {
        ChatIn {
            kind,
            sender_obj: 7,
            name: name.into(),
            text: text.into(),
            at_ms,
        }
    }

    #[test]
    fn shouts_trade_group_finding_and_whispers_reach_the_market_and_nothing_else_does() {
        let lines = [
            chat(0, "Ann", "hello", 1),
            chat(1, "Bob", "shout", 2),
            chat(2, "Cy", "WTS copper ore 1g", 3),
            chat(3, "Di", "LFG", 4),
            chat(-3, "Ed", "psst", 5),
            chat(4, "Fay", "party", 6),
            chat(6, "Gus", "region", 7),
            chat(14, "Hal", "ally", 8),
            chat(-2, "", "system", 9),
            chat(-1, "", "notice", 10),
        ];
        let mut got = Vec::new();
        let n = forward_chat(&lines, |kind, sender, text, at| {
            got.push((kind, sender.to_owned(), text.to_owned(), at))
        });
        assert_eq!(n, 4);
        let s = |k, a: &str, b: &str, t| (k, a.to_owned(), b.to_owned(), t);
        assert_eq!(
            got,
            [
                s(1, "Bob", "shout", 2),
                s(2, "Cy", "WTS copper ore 1g", 3),
                s(3, "Di", "LFG", 4),
                s(-3, "Ed", "psst", 5),
            ]
        );
        assert_eq!(forward_chat(&[], |_, _, _, _| panic!("no line")), 0);
        // the forwarding is what the social duty does with a bot's own market memory
        let mut m = MarketState::default();
        m.inner_mut();
        forward_chat(&lines, |k, s, t, at| m.on_chat(k, s, t, at));
    }

    fn bag_of(items: &[(u16, u32, i32)]) -> FxHashMap<u16, Item> {
        items
            .iter()
            .map(|&(slot, template, count)| {
                (
                    slot,
                    Item {
                        id: u64::from(slot) + 1000,
                        template,
                        count,
                    },
                )
            })
            .collect()
    }

    fn task(reason: u8, changes: &[ItemChange]) -> ItemTaskInfo {
        ItemTaskInfo {
            reason,
            changes: changes.iter().copied().collect(),
            force_removed: Default::default(),
        }
    }

    fn added(slot: u8, template: u32, count: i32) -> ItemChange {
        ItemChange::Added {
            action: 5,
            slot_type: SLOT_BAG,
            slot,
            template,
            id: u64::from(slot) + 1000,
            grade: 0,
            count,
            treasure: None,
        }
    }

    #[test]
    fn a_harvest_counts_as_the_bots_own_only_inside_an_honest_window() {
        let gain = |m: &mut MarketState, reason: u8, before: i64, now: i64| {
            let bag = bag_of(&[(0, 7992, now as i32)]);
            let t = task(reason, &[added(0, 7992, now as i32)]);
            let touched = Touched::before(&t, &bag_of(&[(0, 7992, before as i32)]));
            m.on_item_task(t.reason, &touched, &bag);
        };
        let mut m = MarketState::default();
        // a harvest outside a window adds nothing, the usual reasons still do
        gain(&mut m, reason::DOODAD_INTERACTION, 0, 3);
        assert_eq!(m.clean_count(7992), 0);
        // an honest window counts the harvest reason
        m.open_harvest(true);
        gain(&mut m, reason::DOODAD_INTERACTION, 3, 6);
        assert_eq!(m.clean_count(7992), 3);
        gain(&mut m, reason::LOOT, 6, 7);
        assert_eq!(
            m.clean_count(7992),
            4,
            "the other clean reasons keep counting"
        );
        gain(&mut m, reason::GM, 7, 9);
        assert_eq!(m.clean_count(7992), 4, "a GM item is never clean");
        m.close_harvest();
        gain(&mut m, reason::DOODAD_INTERACTION, 9, 12);
        assert_eq!(m.clean_count(7992), 4, "the window is closed");
        // a dishonest window counts nothing at all, whatever the reason says
        m.open_harvest(false);
        gain(&mut m, reason::DOODAD_INTERACTION, 12, 15);
        gain(&mut m, reason::AUTO_LOOT_DOODAD_ITEM, 15, 17);
        gain(&mut m, reason::LOOT, 17, 18);
        assert_eq!(m.clean_count(7992), 4);
        m.close_harvest();
        gain(&mut m, reason::LOOT, 18, 19);
        assert_eq!(m.clean_count(7992), 5);
        // the farm memory can be taken out for a tick and put back with its window
        m.open_harvest(true);
        let farm = m.take_farm();
        assert_eq!(farm.harvest_honest, Some(true));
        assert!(m.farm().is_none());
        m.put_farm(farm);
        assert_eq!(m.farm().and_then(|f| f.harvest_honest), Some(true));
    }

    #[test]
    fn a_bot_that_gained_nothing_costs_a_pointer() {
        assert_eq!(
            std::mem::size_of::<MarketState>(),
            std::mem::size_of::<usize>()
        );
        let mut m = MarketState::default();
        assert!(m.get().is_none());
        assert_eq!(m.clean_count(7), 0);
        // chat about other things, reloads and losses do not make the memory
        m.on_chat(2, "Bob", "anyone seen the dragon?", 5);
        m.on_chat(0, "Bob", "WTS ore 30s", 5);
        m.on_bag_reloaded(&bag_of(&[(0, 7, 3)]));
        let bag = bag_of(&[]);
        let t = task(
            reason::STORE_SELL,
            &[ItemChange::Removed {
                slot_type: SLOT_BAG,
                slot: 0,
                id: 1000,
                template: 7,
            }],
        );
        let touched = Touched::before(&t, &bag_of(&[(0, 7, 3)]));
        m.on_item_task(t.reason, &touched, &bag);
        assert!(m.get().is_none());
        // a gain by a clean reason makes it
        let bag = bag_of(&[(0, 7, 3)]);
        let t = task(reason::LOOT, &[added(0, 7, 3)]);
        let touched = Touched::before(&t, &bag_of(&[]));
        m.on_item_task(t.reason, &touched, &bag);
        assert_eq!(m.clean_count(7), 3);
    }
}
