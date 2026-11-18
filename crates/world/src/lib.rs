//! Static ArcheAge world knowledge for player bots, loaded once and shared read-only.
//!
//! [`World::load`] reads the server's `compact.sqlite3` (read-only), the server's spawn files and
//! the data baked by `aa-bake`, and builds compact indexes: NPC / doodad templates and spawn
//! positions with a grid for "what is near me" queries, faction relations, quests compiled into
//! bot-friendly steps, skills, crafts and trade-pack buyers, housing designs and areas, zone
//! names and terrain heights. Wrap it in an `Arc` and hand it to every bot.

pub mod auctioneers;
pub mod baked;
pub mod nav;
pub mod terrain;

pub mod chars;
pub mod doodad;
pub mod economy;
pub mod event;
pub mod faction;
pub mod farm;
pub mod gear;
pub mod geom;
pub mod housing;
pub mod items;
pub mod jsonc;
pub mod landmarks;
pub mod npc;
pub mod quest;
pub mod quest_atlas;
pub mod reqs;
pub mod skill;
pub mod spatial;
pub mod spawns;
mod sql;
mod text;
pub mod travel;
pub mod uses;
pub mod water;
mod world;

pub use auctioneers::{Auctioneer, Auctioneers};
pub use chars::CharRow;
pub use doodad::{DoodadFlags, DoodadTemplate};
pub use economy::Craft;
pub use event::{EventDef, Events};
pub use faction::Relation;
pub use farm::{farm_limit, Crop, FarmArea, FarmKind, Farming, StockAction, Yield};
pub use gear::{GearClass, GearData, GearItem, Lunagem};
pub use geom::Vec3;
pub use housing::HousingDesign;
pub use items::{ItemFacts, ItemTable};
pub use npc::{NpcRoles, NpcTemplate};
pub use quest::{
    ObjKind, Objective, Quest, QuestReport, QuestStart, QuestStats, QuestStep, CATEGORY_TUTORIAL,
};
pub use quest_atlas::{AtlasPoint, AtlasZone, Chain, QuestAtlas, CATEGORY_EVENT};
pub use reqs::{Gate, StartGates, Who};
pub use skill::{mana_level_scale, SkillInfo};
pub use spawns::SpawnStats;
pub use travel::Travel;
pub use uses::{ItemUses, SkillNeed};
pub use world::{World, WorldPaths, ZoneInfo};
