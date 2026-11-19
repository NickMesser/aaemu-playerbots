//! The provenance ledger: how many of each item a bot came by honestly, and may therefore sell.
//!
//! Bots with `bots.gm_bootstrap` mint goods and gold with GM commands, and the goods a bot gets
//! from a trade or a mail are not its own either. None of that may end up with a player, directly
//! or through the auction house. So the market never sells "what is in the bag" but "what the
//! ledger says the bot earned": the server tags every item change with the reason it happened
//! (`ItemTaskType`, [`aa_proto::sc_world::ItemTaskInfo::reason`]) and the ledger counts a gain
//! only for these reasons ([`CLEAN_REASONS`]):
//!
//! | Reasons | Meaning |
//! | --- | --- |
//! | 9, 10, 35 | loot of a monster or a doodad, loot all, auto-loot |
//! | 29 | craft product picked up |
//! | 37, 38 | quest reward, quest supply |
//! | 42, 43 | gathering skill effects (a skill that gives an item) |
//! | 54 | bought from a vendor |
//! | 17 | a harvest, only while the farm goal says the seeds were bought (see below) |
//!
//! Everything else adds nothing: GM (11), trade (48), mail (47), the auction house (46), swaps,
//! splits and every reason this table does not list (reason 35 is also what the server's GM
//! `/gold add` tags its purse change with; that carries no item). A gain is the growth of the
//! bag's count of a template during one item task, so a stack that merges counts like a new one.
//!
//! A harvest is the one gain whose honesty is not in the reason: the farm goal opens a *harvest
//! window* ([`MarketState::open_harvest`](super::MarketState::open_harvest)) around the harvest cast
//! and says whether the seeds of the plot were bought. In an honest window reason 17 counts like
//! the clean reasons; in a dishonest one (the seeds were minted with a GM command, or the bot
//! cannot tell where they came from) nothing counts, whatever the reason. Outside a window reason
//! 17 adds nothing.
//!
//! Whatever leaves the bag, for any reason (sold, traded, mailed, listed, used, destroyed,
//! equipped), is taken off the ledger first, even if the ledger cannot know whether the items
//! that left were the honest ones: the ledger errs on the side of selling less. The clean count is
//! also never above what the bag holds. The ledger is forgotten with the rest of the bot's state
//! when the connection is lost: after a reconnect, or a restart, nothing the bot holds counts as
//! clean until it gains more.
//!
//! What the ledger lets go is further limited by the item itself and by the bot's quests (see
//! [`Ledger::sellable_surplus`]): quest items, soul-bound and unsellable items are never offered,
//! and what an active quest still needs ([`QuestNeeds`]) stays in the bag.
//!
//! The ledger is bounded: at most [`MAX_TEMPLATES`] templates, 8 bytes each. When a new template
//! comes and the ledger is full, the template with the smallest clean count goes.

use aa_proto::sc_world::{ItemChange, ItemTaskInfo};
use aa_world::World;
use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use super::needs::QuestNeeds;
use crate::bot::state::{Item, SLOT_BAG};

/// `ItemTaskType` values (the reason byte of `SCItemTaskSuccessPacket`, the server's
/// `models/item_task.rs`) the market looks at.
pub mod reason {
    /// A monster or a doodad was looted.
    pub const LOOT: u8 = 9;
    /// Everything of a loot was taken.
    pub const LOOT_ALL: u8 = 10;
    /// A GM command (`/item add`).
    pub const GM: u8 = 11;
    /// A craft product was picked up.
    pub const CRAFT_PICKUP_PRODUCT: u8 = 29;
    /// A doodad's item was taken automatically.
    pub const AUTO_LOOT_DOODAD_ITEM: u8 = 35;
    /// A quest was completed (its reward).
    pub const QUEST_COMPLETE: u8 = 37;
    /// A quest supplied items.
    pub const QUEST_SUPPLY_ITEMS: u8 = 38;
    /// A skill effect gave an item (gathering).
    pub const SKILL_EFFECT_GAIN_ITEM: u8 = 42;
    /// A skill effect gave an item at a position (gathering).
    pub const SKILL_EFFECT_GAIN_ITEM_WITH_POS: u8 = 43;
    /// The auction house.
    pub const AUCTION: u8 = 46;
    /// A mail.
    pub const MAIL: u8 = 47;
    /// A player trade.
    pub const TRADE: u8 = 48;
    /// A seed was planted (`DoodadCreate`).
    pub const DOODAD_CREATE: u8 = 14;
    /// A doodad was used and gave an item (a harvest).
    pub const DOODAD_INTERACTION: u8 = 17;
    /// An item was sold to a vendor.
    pub const STORE_SELL: u8 = 53;
    /// An item was bought from a vendor.
    pub const STORE_BUY: u8 = 54;
}

/// The reasons whose gains count as the bot's own (see the module docs).
pub const CLEAN_REASONS: [u8; 9] = [
    reason::LOOT,
    reason::LOOT_ALL,
    reason::AUTO_LOOT_DOODAD_ITEM,
    reason::CRAFT_PICKUP_PRODUCT,
    reason::QUEST_COMPLETE,
    reason::QUEST_SUPPLY_ITEMS,
    reason::SKILL_EFFECT_GAIN_ITEM,
    reason::SKILL_EFFECT_GAIN_ITEM_WITH_POS,
    reason::STORE_BUY,
];

/// Whether a gain with this item task reason counts as the bot's own.
pub fn is_clean_reason(reason: u8) -> bool {
    CLEAN_REASONS.contains(&reason)
}

/// The most templates the ledger tracks.
pub const MAX_TEMPLATES: usize = 64;

/// How many of an item the bot keeps for itself when it may use it: a consumable (the item casts a
/// skill when used: potions, food, scrolls, books, quest props) or an item some quest asks to
/// gather. [`Ledger::sellable_surplus`] leaves this many out of what it offers.
pub const RESERVE: u32 = 10;

/// The count of item `template` in the bag (all stacks).
pub fn bag_count(bag: &FxHashMap<u16, Item>, template: u32) -> i64 {
    bag.values()
        .filter(|i| i.template == template && i.count > 0)
        .map(|i| i64::from(i.count))
        .sum()
}

/// The templates an item task touches, with their bag count from before the task was applied.
/// Take it before applying the task to the bag, hand it to
/// [`MarketState::on_item_task`](super::MarketState::on_item_task) afterwards.
#[derive(Debug, Default)]
pub struct Touched(SmallVec<[(u32, i64); 4]>);

impl Touched {
    /// The templates `task` adds to, changes or removes from the bag, counted in `bag`.
    pub fn before(task: &ItemTaskInfo, bag: &FxHashMap<u16, Item>) -> Touched {
        let mut t = Touched::default();
        let by_id = |id: u64| {
            bag.values()
                .find(|i| i.id == id && id != 0)
                .map(|i| i.template)
        };
        for change in &task.changes {
            let template = match *change {
                ItemChange::Added {
                    slot_type: SLOT_BAG,
                    template,
                    ..
                }
                | ItemChange::CountUpdate {
                    slot_type: SLOT_BAG,
                    template,
                    ..
                }
                | ItemChange::Removed {
                    slot_type: SLOT_BAG,
                    template,
                    ..
                } => Some(template),
                ItemChange::RemovedSlot {
                    slot_type: SLOT_BAG,
                    id,
                    ..
                } => by_id(id),
                // (a move inside the bag changes no count)
                ItemChange::Moved {
                    from_slot_type: SLOT_BAG,
                    to_slot_type,
                    from_id,
                    ..
                } if to_slot_type != SLOT_BAG => by_id(from_id),
                _ => None,
            };
            if let Some(template) = template {
                t.note(template, bag);
            }
        }
        for id in &task.force_removed {
            if let Some(template) = by_id(*id) {
                t.note(template, bag);
            }
        }
        t
    }

    fn note(&mut self, template: u32, bag: &FxHashMap<u16, Item>) {
        if template != 0 && !self.0.iter().any(|e| e.0 == template) {
            self.0.push((template, bag_count(bag, template)));
        }
    }

    /// Whether the task touched no template of the bag.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// `(template, bag count before the task)` of every touched template.
    pub fn iter(&self) -> std::slice::Iter<'_, (u32, i64)> {
        self.0.iter()
    }
}

/// The clean quantity per item template. See the module docs.
#[derive(Debug, Default)]
pub struct Ledger {
    /// `(template, clean count)`, oldest first; no count is 0.
    entries: Vec<(u32, u32)>,
}

impl Ledger {
    /// How many of `template` the bot came by honestly (and still, as far as it knows, has).
    pub fn clean_count(&self, template: u32) -> u32 {
        self.entries
            .iter()
            .find(|e| e.0 == template)
            .map_or(0, |e| e.1)
    }

    /// The number of templates tracked.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is tracked.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// `n` more of `template` came by honestly. A new template at [`MAX_TEMPLATES`] pushes out
    /// the template with the smallest clean count (the oldest of equals).
    pub fn add(&mut self, template: u32, n: u32) {
        if n == 0 || template == 0 {
            return;
        }
        if let Some(e) = self.entries.iter_mut().find(|e| e.0 == template) {
            e.1 = e.1.saturating_add(n);
            return;
        }
        if self.entries.len() >= MAX_TEMPLATES {
            let smallest = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.1)
                .map(|(i, _)| i);
            if let Some(i) = smallest {
                self.entries.remove(i);
            }
        }
        self.entries.push((template, n));
    }

    /// `n` of `template` left the bag: the clean count falls by as much (to 0 at the least).
    pub fn remove(&mut self, template: u32, n: u32) {
        if let Some(i) = self.entries.iter().position(|e| e.0 == template) {
            self.entries[i].1 = self.entries[i].1.saturating_sub(n);
            if self.entries[i].1 == 0 {
                self.entries.remove(i);
            }
        }
    }

    /// The bag holds `have` of `template`: the clean count is not above it.
    pub fn clamp(&mut self, template: u32, have: i64) {
        if let Some(i) = self.entries.iter().position(|e| e.0 == template) {
            let have = u32::try_from(have.max(0)).unwrap_or(u32::MAX);
            if self.entries[i].1 > have {
                self.entries[i].1 = have;
            }
            if self.entries[i].1 == 0 {
                self.entries.remove(i);
            }
        }
    }

    /// [`Ledger::clamp`] for every template, against the whole bag.
    pub fn clamp_all(&mut self, bag: &FxHashMap<u16, Item>) {
        self.entries.retain_mut(|e| {
            let have = u32::try_from(bag_count(bag, e.0).max(0)).unwrap_or(u32::MAX);
            e.1 = e.1.min(have);
            e.1 > 0
        });
    }

    /// What the bot may offer for sale, as `(template, count)`: per template the clean count (and
    /// never more than the bag holds), minus what the bot keeps ([`keep`]: a [`RESERVE`] of the
    /// items it may need itself, or what its active quests need, `needs`, if that is more), for
    /// the templates that are [`tradable`](aa_world::ItemFacts::tradable) (sellable, not bound
    /// and not quest items). Cheap enough to ask now and then (a pass over the bag per tracked
    /// template), not on every tick.
    pub fn sellable_surplus<'a>(
        &'a self,
        world: &'a World,
        bag: &'a FxHashMap<u16, Item>,
        needs: &'a QuestNeeds,
    ) -> impl Iterator<Item = (u32, u32)> + 'a {
        self.surplus_by(bag, move |template| {
            let facts = world.item_facts(template)?;
            facts.tradable().then(|| keep(world, needs, template))
        })
    }

    /// [`Ledger::sellable_surplus`] with the world's part given as `rule`: `None` for a template
    /// that must not be sold, else how many of it the bot keeps.
    pub fn surplus_by<'a>(
        &'a self,
        bag: &'a FxHashMap<u16, Item>,
        rule: impl Fn(u32) -> Option<u32> + 'a,
    ) -> impl Iterator<Item = (u32, u32)> + 'a {
        self.entries.iter().filter_map(move |&(template, clean)| {
            let keep = rule(template)?;
            let have = u32::try_from(bag_count(bag, template).max(0)).unwrap_or(u32::MAX);
            let n = clean.min(have).saturating_sub(keep);
            (n > 0).then_some((template, n))
        })
    }
}

/// How many of `template` the bot keeps for itself: the [`reserve`], or what its active quests
/// still need of it (`needs`) when that is more.
pub fn keep(world: &World, needs: &QuestNeeds, template: u32) -> u32 {
    reserve(world, template).max(needs.count(template))
}

/// How many of `template` the bot keeps for itself: [`RESERVE`] of a consumable or of an item a
/// quest asks to gather, else none.
pub fn reserve(world: &World, template: u32) -> u32 {
    if world.item_use_skill(template) != 0 || !world.gather_doodads(template).is_empty() {
        RESERVE
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aa_world::ObjKind;

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

    fn grew(slot: u8, template: u32, delta: i32) -> ItemChange {
        ItemChange::CountUpdate {
            slot_type: SLOT_BAG,
            slot,
            id: u64::from(slot) + 1000,
            delta,
            template,
        }
    }

    fn removed(slot: u8, template: u32) -> ItemChange {
        ItemChange::Removed {
            slot_type: SLOT_BAG,
            slot,
            id: u64::from(slot) + 1000,
            template,
        }
    }

    fn task(reason: u8, changes: &[ItemChange]) -> ItemTaskInfo {
        ItemTaskInfo {
            reason,
            changes: changes.iter().copied().collect(),
            force_removed: Default::default(),
        }
    }

    /// Applies `t` to `bag` the way `BotState` would (enough of it for these tests) and feeds the
    /// market state.
    fn apply(m: &mut super::super::MarketState, bag: &mut FxHashMap<u16, Item>, t: &ItemTaskInfo) {
        let touched = Touched::before(t, bag);
        for c in &t.changes {
            match *c {
                ItemChange::Added {
                    slot,
                    template,
                    id,
                    count,
                    ..
                } => {
                    bag.insert(
                        u16::from(slot),
                        Item {
                            id,
                            template,
                            count,
                        },
                    );
                }
                ItemChange::CountUpdate { slot, delta, .. } => {
                    let item = bag.get_mut(&u16::from(slot)).expect("a stack to change");
                    item.count += delta;
                    if item.count <= 0 {
                        bag.remove(&u16::from(slot));
                    }
                }
                ItemChange::Removed { slot, .. } => {
                    bag.remove(&u16::from(slot));
                }
                _ => {}
            }
        }
        bag.retain(|_, i| !t.force_removed.contains(&i.id));
        m.on_item_task(t.reason, &touched, bag);
    }

    #[test]
    fn the_clean_reasons_are_the_honest_ones_and_gm_is_not() {
        for r in [9, 10, 35, 29, 37, 38, 42, 43, 54] {
            assert!(is_clean_reason(r), "{r}");
        }
        for r in [
            reason::GM,
            reason::AUCTION,
            reason::MAIL,
            reason::TRADE,
            reason::STORE_SELL,
            0,
            1,
            5,
            6,
            17,
            36,
            90,
            255,
        ] {
            assert!(!is_clean_reason(r), "{r}");
        }
    }

    #[test]
    fn honest_gains_count_and_gm_gains_do_not() {
        let mut m = super::super::MarketState::default();
        let mut bag = bag_of(&[]);
        // a loot of 5 ore, then 3 more that merge into the stack
        apply(&mut m, &mut bag, &task(reason::LOOT, &[added(0, 3411, 5)]));
        apply(&mut m, &mut bag, &task(reason::LOOT, &[grew(0, 3411, 3)]));
        assert_eq!(m.clean_count(3411), 8);
        // 20 more from the GM do not count, however they merge
        apply(&mut m, &mut bag, &task(reason::GM, &[grew(0, 3411, 20)]));
        assert_eq!(bag_count(&bag, 3411), 28);
        assert_eq!(m.clean_count(3411), 8);
        // nor does a whole GM stack of something else
        apply(&mut m, &mut bag, &task(reason::GM, &[added(1, 500, 99)]));
        assert_eq!(m.clean_count(500), 0);
        // a trade, a mail and an auction bring nothing clean either
        for r in [reason::TRADE, reason::MAIL, reason::AUCTION] {
            apply(&mut m, &mut bag, &task(r, &[added(2, 600, 4)]));
            bag.remove(&2);
        }
        assert_eq!(m.clean_count(600), 0);
        // each clean reason adds
        for (i, r) in [9u8, 10, 35, 29, 37, 38, 42, 43, 54]
            .into_iter()
            .enumerate()
        {
            let template = 7000 + i as u32;
            apply(
                &mut m,
                &mut bag,
                &task(r, &[added(10 + i as u8, template, 2)]),
            );
            assert_eq!(m.clean_count(template), 2, "reason {r}");
        }
    }

    #[test]
    fn what_leaves_the_bag_leaves_the_ledger_and_the_count_is_clamped() {
        let mut m = super::super::MarketState::default();
        let mut bag = bag_of(&[]);
        apply(&mut m, &mut bag, &task(reason::LOOT, &[added(0, 3411, 10)]));
        // 6 used up (any reason): 4 clean left
        apply(
            &mut m,
            &mut bag,
            &task(reason::STORE_SELL, &[grew(0, 3411, -6)]),
        );
        assert_eq!(m.clean_count(3411), 4);
        // GM goods join, then half of the stack is sold: the honest count suffers first
        apply(&mut m, &mut bag, &task(reason::GM, &[grew(0, 3411, 10)]));
        assert_eq!(bag_count(&bag, 3411), 14);
        apply(&mut m, &mut bag, &task(reason::TRADE, &[grew(0, 3411, -7)]));
        assert_eq!(
            m.clean_count(3411),
            0,
            "the ledger errs on the side of selling less"
        );
        assert!(m.get().unwrap().ledger.is_empty());
        // a clean count is never above the bag: the bag shrinks behind the ledger's back
        apply(&mut m, &mut bag, &task(reason::LOOT, &[grew(0, 3411, 9)]));
        assert_eq!(m.clean_count(3411), 9);
        bag.get_mut(&0).unwrap().count = 5;
        m.on_bag_reloaded(&bag);
        assert_eq!(m.clean_count(3411), 5);
        bag.clear();
        m.on_bag_reloaded(&bag);
        assert_eq!(m.clean_count(3411), 0);
        // removing a whole stack by instance
        apply(&mut m, &mut bag, &task(reason::LOOT, &[added(3, 100, 2)]));
        assert_eq!(m.clean_count(100), 2);
        apply(&mut m, &mut bag, &task(reason::MAIL, &[removed(3, 100)]));
        assert_eq!(m.clean_count(100), 0);
        // an unknown template and a removal on an empty ledger are harmless
        let mut l = Ledger::default();
        l.remove(5, 3);
        l.clamp(5, 0);
        assert!(l.is_empty());
    }

    #[test]
    fn items_force_removed_and_equipped_leave_the_ledger() {
        let mut m = super::super::MarketState::default();
        let mut bag = bag_of(&[]);
        apply(&mut m, &mut bag, &task(reason::LOOT, &[added(0, 200, 1)]));
        apply(&mut m, &mut bag, &task(reason::LOOT, &[added(1, 201, 1)]));
        // the server drops item 1000 outright (the client must forget it)
        let mut t = task(reason::STORE_SELL, &[]);
        t.force_removed.push(1000);
        apply(&mut m, &mut bag, &t);
        assert_eq!(m.clean_count(200), 0);
        assert_eq!(m.clean_count(201), 1);
        // equipping moves the item out of the bag
        let mut t = task(
            reason::LOOT,
            &[ItemChange::Moved {
                from_slot_type: SLOT_BAG,
                from_slot: 1,
                to_slot_type: crate::bot::state::SLOT_EQUIPMENT,
                to_slot: 3,
                from_id: 1001,
                to_id: 0,
            }],
        );
        t.reason = 5;
        let touched = Touched::before(&t, &bag);
        assert_eq!(touched.iter().copied().collect::<Vec<_>>(), [(201, 1)]);
        bag.remove(&1);
        m.on_item_task(t.reason, &touched, &bag);
        assert_eq!(m.clean_count(201), 0);
        // a move inside the bag touches nothing
        let t = task(
            5,
            &[ItemChange::Moved {
                from_slot_type: SLOT_BAG,
                from_slot: 1,
                to_slot_type: SLOT_BAG,
                to_slot: 4,
                from_id: 1001,
                to_id: 0,
            }],
        );
        assert!(Touched::before(&t, &bag).is_empty());
    }

    #[test]
    fn the_ledger_holds_at_most_sixty_four_templates_and_drops_the_smallest() {
        let mut l = Ledger::default();
        for i in 0..MAX_TEMPLATES as u32 {
            l.add(1000 + i, 10 + i);
        }
        assert_eq!(l.len(), MAX_TEMPLATES);
        // the smallest (template 1000, 10) goes for a new one
        l.add(5, 1);
        assert_eq!(l.len(), MAX_TEMPLATES);
        assert_eq!(l.clean_count(1000), 0);
        assert_eq!(l.clean_count(5), 1);
        assert_eq!(l.clean_count(1001), 11);
        // a template that is there only grows
        l.add(1001, 4);
        assert_eq!(l.clean_count(1001), 15);
        assert_eq!(l.len(), MAX_TEMPLATES);
        // the newcomer is the smallest now, and the next newcomer takes its place
        l.add(6, 1);
        assert_eq!(l.clean_count(5), 0);
        assert_eq!(l.clean_count(6), 1);
        // equal counts: the oldest goes
        let mut l = Ledger::default();
        for i in 0..MAX_TEMPLATES as u32 {
            l.add(1 + i, 3);
        }
        l.add(999, 3);
        assert_eq!(l.clean_count(1), 0);
        assert_eq!(l.clean_count(2), 3);
        assert_eq!(l.clean_count(999), 3);
        // nothing and nobody
        l.add(5000, 0);
        l.add(0, 5);
        assert_eq!((l.clean_count(5000), l.clean_count(0)), (0, 0));
        // counts saturate
        l.add(2, u32::MAX);
        assert_eq!(l.clean_count(2), u32::MAX);
    }

    #[test]
    fn the_surplus_is_the_clean_tradable_part_above_the_reserve() {
        let mut l = Ledger::default();
        l.add(1, 50); // plain material
        l.add(2, 50); // consumable: keeps a reserve
        l.add(3, 50); // bound
        l.add(4, 50); // unknown to the world
        l.add(5, 8); // consumable, less than the reserve
        l.add(6, 50); // 50 clean, but only 30 in the bag
        l.add(7, 50); // none in the bag
        let bag = bag_of(&[
            (0, 1, 70),
            (1, 2, 70),
            (2, 3, 70),
            (3, 4, 70),
            (4, 5, 8),
            (5, 6, 30),
        ]);
        let rule = |t: u32| match t {
            1 | 6 | 7 => Some(0),
            2 | 5 => Some(RESERVE),
            3 => None,
            _ => None,
        };
        let got: Vec<_> = l.surplus_by(&bag, rule).collect();
        // 70 in the bag but only 50 clean; 50 - 10; 8 - 10; the bag caps the clean count
        assert_eq!(got, [(1, 50), (2, 40), (6, 30)]);
    }

    #[test]
    fn what_an_active_quest_needs_is_kept_back_in_full() {
        let mut l = Ledger::default();
        l.add(1, 50); // a quest needs 20 of it
        l.add(2, 50); // a consumable (reserve 10) that a quest needs 25 of
        l.add(3, 50); // a consumable that a quest needs 4 of: the reserve is more
        l.add(4, 15); // a quest needs 20 of it, the bot has 15
        let bag = bag_of(&[(0, 1, 50), (1, 2, 50), (2, 3, 50), (3, 4, 15)]);
        let needs = QuestNeeds::from_kinds(&[
            ObjKind::UseItem { item: 1, count: 20 },
            ObjKind::UseItem { item: 2, count: 25 },
            ObjKind::UseItem { item: 3, count: 4 },
            ObjKind::UseItem { item: 4, count: 20 },
        ]);
        let reserve_of = |t: u32| if matches!(t, 2 | 3) { RESERVE } else { 0 };
        let rule = |t: u32| Some(reserve_of(t).max(needs.count(t)));
        let got: Vec<_> = l.surplus_by(&bag, rule).collect();
        // 50 - 20; 50 - max(10, 25); 50 - max(10, 4); 15 - 20 is nothing
        assert_eq!(got, [(1, 30), (2, 25), (3, 40)]);
    }

    #[test]
    fn the_ledger_is_small() {
        assert_eq!(
            std::mem::size_of::<Ledger>(),
            std::mem::size_of::<Vec<u8>>()
        );
        // 64 templates of (u32, u32)
        assert_eq!(MAX_TEMPLATES * std::mem::size_of::<(u32, u32)>(), 512);
    }
}
