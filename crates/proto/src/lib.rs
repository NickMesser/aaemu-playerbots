//! ArcheAge 1.2 (r208022) client-side wire protocol for the AAEmu Rust server.
//!
//! `codec`, `stream`, `pos` and `opcodes` are vendored from the AAEmu-1.2-Rust workspace
//! (`aaemu-commons`, `aaemu-game`), so this crate does not depend on the server crates.
//!
//! - [`login`]: login server client (Trion ticket auth, world list, world cookie).
//! - [`game`]: game server connection (framing, DD04 batches, send/recv).
//! - [`cs`]: client → server packet builders.
//! - [`sc`]: server → client packet parsers ([`sc::ServerEvent`]): [`sc_world`] (world, units,
//!   inventory, combat, chat, loot, plus the travel packets below), [`sc_activity`] (quests,
//!   duels, trade, teams, houses, mail, crafting).
//! - [`guild`]: the guild (expedition) requests and the packets that tell a bot about its guild.
//! - [`sc_travel`]: the payload types and readers of the travel packets: the teleport book and
//!   recall districts (`SCCharacterPortals`, `SCCharacterReturnDistricts`, `SCPortalInfoSaved`,
//!   `SCPortalDeleted`, `SCUnitPortalUsed`), instance dungeons (`SCProcessingInstance`,
//!   `SCLoadInstance`), boats (`SCSlaveCreated`, `SCMySlave`, `SCSlaveBound`,
//!   `SCUnitAttached`/`Detached`, `SCSlaveDespawn`/`Removed`, ship `SCOneUnitMovement`) and
//!   mates (`SCMateSpawned`). The
//!   matching client packets are `cs::use_portal`, `cs::delete_portal`, `cs::notify_sub_zone`,
//!   `cs::bind_slave`, `cs::discard_slave`, `cs::despawn_slave`, `cs::destroy_slave`,
//!   `cs::mount_mate`, `cs::unmount_mate`, `cs::remove_mate`, `cs::ship_request`, and the skill
//!   casts with a skill object
//!   (`cs::record_portal`, `cs::rename_portal`, `cs::open_portal`, `cs::start_skill_*_with`).

pub mod auction;
pub mod codec;
pub mod cs;
pub mod game;
pub mod guild;
pub mod login;
pub mod opcodes;
pub mod pos;
pub mod sc;
pub mod sc_activity;
pub mod sc_emote;
pub mod sc_event;
pub mod sc_travel;
pub mod sc_world;
pub mod stream;

pub use stream::{MarshalError, PacketStream};
