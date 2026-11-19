//! Items reserved for active quests and guild recipes.
//!
//! A quest objective that asks the bot to hold (`Gather`) or use (`UseItem`) a number of an item
//! is a claim on that item. Selling it to a player, listing it at the auction house or handing it
//! over in a trade would only send the bot out to get it again, so the sale paths keep that many
//! units back ([`QuestNeeds::count`]). Assigned recipes reserve their ingredients the same way.
//! These limits are in addition to the consumable reserve ([`super::ledger::reserve`]).

use aa_world::{Craft, ObjKind, World};
use smallvec::SmallVec;

use crate::bot::ctx::Ctx;
use crate::bot::goals::work;
use crate::bot::guild::profession;
use crate::bot::state::QuestLog;

pub fn guild_recipe<'a>(ctx: &Ctx<'a>) -> Option<&'a Craft> {
    if let Some(id) = profession::trade_pack_order(ctx) {
        return ctx.world.craft(id).filter(|craft| craft.is_pack);
    }
    let (work::Order::Craft { recipe, .. }, _) = work::selected_order(ctx)? else {
        return None;
    };
    ctx.world.craft(recipe).filter(|craft| !craft.is_pack)
}

/// Item counts reserved for quests and the bot's current guild recipe.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QuestNeeds(SmallVec<[(u32, u32); 8]>);

impl QuestNeeds {
    pub fn for_bot(ctx: &Ctx<'_>) -> Self {
        let mut needs = Self::of(ctx.world, &ctx.state.quests);
        if let Some(recipe) =
            guild_recipe(ctx).filter(|craft| ctx.state.me.level >= craft.req_level)
        {
            for &(item, count) in recipe.materials.iter() {
                needs.add(item, count);
            }
        }
        needs
    }

    /// What the quests of `quests.active` need: the `Gather` and `UseItem` counts of all their
    /// progress objectives (`world` knows the quests), summed per template. A quest the world
    /// does not know needs nothing.
    pub fn of(world: &World, quests: &QuestLog) -> QuestNeeds {
        QuestNeeds::from_kinds(
            quests
                .active
                .iter()
                .filter_map(|row| world.quest(row.template))
                .flat_map(|quest| quest.progress.iter())
                .flat_map(|step| step.objectives.iter())
                .map(|objective| &objective.kind),
        )
    }

    /// The needs of `kinds`: the `Gather` and `UseItem` objectives among them, summed per
    /// template.
    pub fn from_kinds<'a>(kinds: impl IntoIterator<Item = &'a ObjKind>) -> QuestNeeds {
        let mut needs = QuestNeeds::default();
        for kind in kinds {
            if let ObjKind::Gather { item, count, .. } | ObjKind::UseItem { item, count } = kind {
                needs.add(*item, *count);
            }
        }
        needs
    }

    fn add(&mut self, template: u32, n: u32) {
        if template == 0 || n == 0 {
            return;
        }
        match self.0.iter_mut().find(|e| e.0 == template) {
            Some(e) => e.1 = e.1.saturating_add(n),
            None => self.0.push((template, n)),
        }
    }

    /// How many units of `template` to keep in the bag.
    pub fn count(&self, template: u32) -> u32 {
        self.0.iter().find(|e| e.0 == template).map_or(0, |e| e.1)
    }

    /// Whether no active quest needs an item.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gather(item: u32, count: u32) -> ObjKind {
        ObjKind::Gather {
            item,
            count,
            droppers: Box::default(),
        }
    }

    #[test]
    fn gather_and_use_counts_are_summed_per_template() {
        let kinds = [
            gather(3712, 5),
            ObjKind::UseItem {
                item: 3712,
                count: 2,
            },
            gather(3411, 10),
            gather(0, 4),
            gather(7, 0),
            ObjKind::Talk { npc: 3712 },
            ObjKind::Hunt {
                npcs: Box::new([3712]),
                count: 9,
            },
            ObjKind::Interact {
                doodad: 3712,
                count: 3,
            },
        ];
        let needs = QuestNeeds::from_kinds(&kinds);
        assert_eq!(needs.count(3712), 7);
        assert_eq!(needs.count(3411), 10);
        // (no template, no count, and objectives that name no item add nothing)
        assert_eq!(needs.count(0), 0);
        assert_eq!(needs.count(7), 0);
        assert_eq!(needs.count(99), 0);
        assert!(!needs.is_empty());
        assert!(QuestNeeds::from_kinds(&kinds[5..]).is_empty());
    }
}
