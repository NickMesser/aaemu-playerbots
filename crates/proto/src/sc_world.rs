// Ported from the S->C writers of AAEmu-1.2-Rust rust/crates/aaemu-game/src (cited per variant), GPL-3.0-or-later.
//! Server → client parsers for the world, units, the own character, inventory, combat, chat and
//! loot (see [`crate::sc`]).
//!
//! Every reader mirrors the server writer cited in its variant's doc comment (paths are relative to
//! `rust/crates/aaemu-game/src` of AAEmu-1.2-Rust, GPL-3.0-or-later like this crate). The
//! dispatcher matches on the opcode first, so a packet bots do not need costs one `match` and no
//! allocation; [`interesting`] answers the same question without a body.
//!
//! Conventions: object ids are the 24-bit `bc` ids of the wire; hit points and mana are returned
//! as real values (the wire carries them times 100); positions are world coordinates in metres.
//! Events own their strings as `Box<str>`/small vectors, and the large ones are boxed, so a
//! [`WorldEvent`] stays small.

use smallvec::SmallVec;

use crate::opcodes::{pp, sc};
use crate::pos::convert_long_to_coord;
use crate::sc_event::{self, EventId, TowerDefInfo};
use crate::sc_travel::{self, MateInfo, MySlaveInfo, PortalEntry, ShipMotion};
use crate::stream::{MarshalError, PacketStream};

pub(crate) type Res<T> = Result<T, MarshalError>;

/// `(x, y, z)` in world coordinates.
pub type Pos = (f32, f32, f32);

/// Level byte of the game packets (`sc::*`).
const LEVEL_GAME: u8 = 1;
/// Level byte of the proxy packets (`pp::*`).
const LEVEL_PROXY: u8 = 2;

/// Hit points and mana travel multiplied by this factor (`SCUnitStatePacket`, `SCUnitPointsPacket`).
const POINTS_SCALE: i32 = 100;

/// `BaseUnitType` values of `SCUnitStatePacket` (aaemu-game models/unit.rs:32).
pub mod unit_type {
    /// A player character.
    pub const CHARACTER: u8 = 0;
    /// An NPC (`template_id` = NPC template).
    pub const NPC: u8 = 1;
    /// A slave (vehicle).
    pub const SLAVE: u8 = 2;
    /// A house (`id` = house design template).
    pub const HOUSING: u8 = 3;
    /// A fixed-route transfer.
    pub const TRANSFER: u8 = 4;
    /// A mate (pet / mount).
    pub const MATE: u8 = 5;
    /// A shipyard.
    pub const SHIPYARD: u8 = 6;
}

/// Number of equipment slots written by `Character.Write` / `SCUnitStatePacket`
/// (`EQUIPMENT_CONTAINER_SIZE`, models/container.rs:78).
const EQUIPMENT_SLOTS: u8 = 28;
/// Appearance slots (face .. beard) are written as the template id only (models/container.rs:66).
const BODY_PART_SLOTS: std::ops::RangeInclusive<u8> = 19..=25;
/// The backpack slot of the equipment (`equip_slot::BACKPACK`, models/container.rs:73): where a
/// carried trade pack (or a glider) sits.
pub const BACKPACK_SLOT: u8 = 26;

/// `GradeEnchantResult`: the `result` of [`WorldEvent::GradeEnchantResult`]
/// (systems/economy/item_effect_packets.rs:15).
pub mod grade_enchant_result {
    /// The item broke (it is gone).
    pub const BREAK: u8 = 0;
    /// The grade went down.
    pub const DOWNGRADE: u8 = 1;
    /// Nothing changed (the scroll is used up).
    pub const FAIL: u8 = 2;
    /// One grade up.
    pub const SUCCESS: u8 = 3;
    /// Two grades up (lucky scrolls).
    pub const GREAT_SUCCESS: u8 = 4;
}

/// Detail body lengths after the type byte, indexed by `ItemDetailType`
/// (aaemu-game models/item.rs:158).
const DETAIL_BODY_LENS: [usize; 10] = [0, 55, 29, 6, 9, 24, 16, 16, 8, 4];

// ---- event payloads ------------------------------------------------------------------------

/// One character of the lobby list (`SCCharacterListPacket`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CharSummary {
    /// Character id.
    pub id: u32,
    /// Character name.
    pub name: Box<str>,
    /// Race id.
    pub race: u8,
    /// Gender id.
    pub gender: u8,
    /// Character level.
    pub level: u8,
}

/// One occupied equipment slot of the own character.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EquipSlot {
    /// `EquipmentItemSlot` index (0..28; 19..=25 are appearance parts).
    pub slot: u8,
    /// Item template id.
    pub template: u32,
    /// Item instance id (0 for appearance parts, which carry the template only).
    pub id: u64,
    /// Item grade.
    pub grade: u8,
}

/// The own character as sent once on entering the world (`SCCharacterStatePacket`).
///
/// Skipped (not reliable or not needed): the character's guid, the per-ability experience block
/// (the server writes zeros), unread-mail counters, bounty/jail/play-time tail and the model
/// appearance.
#[derive(Debug, Clone, PartialEq)]
pub struct OwnState {
    /// Character id.
    pub id: u32,
    /// Character name.
    pub name: Box<str>,
    /// Race id.
    pub race: u8,
    /// Gender id.
    pub gender: u8,
    /// Level.
    pub level: u8,
    /// Hit points (real value).
    pub hp: i32,
    /// Mana (real value).
    pub mp: i32,
    /// Zone key the character stands in.
    pub zone_id: u32,
    /// Faction id.
    pub faction_id: u32,
    /// Expedition (guild) id, 0 when none.
    pub expedition_id: u32,
    /// Family id, 0 when none.
    pub family_id: u32,
    /// The three active abilities (11 = none).
    pub abilities: [u8; 3],
    /// Position.
    pub pos: Pos,
    /// Yaw in radians (`Transform.rot_z`).
    pub yaw: f32,
    /// World instance id.
    pub instance_id: u32,
    /// Current experience.
    pub exp: i32,
    /// Experience that can still be recovered at a temple.
    pub recoverable_exp: i32,
    /// Labor points.
    pub labor: i16,
    /// Money in copper.
    pub money: i64,
    /// Bank money in copper (`Money2`).
    pub bank_money: i64,
    /// Loyalty tokens (`bm_point`).
    pub loyalty: i64,
    /// Number of bag slots.
    pub bag_slots: u8,
    /// Number of bank slots.
    pub bank_slots: i16,
    /// Occupied equipment slots.
    pub equipment: Box<[EquipSlot]>,
    /// The number of lunagems socketed in each piece of [`OwnState::equipment`] (same order; 0
    /// for appearance parts).
    pub equipment_gems: Box<[u8]>,
}

/// One buff of a unit, as listed in `SCUnitStatePacket`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuffInfo {
    /// 0 = good, 1 = bad, 2 = hidden list.
    pub list: u8,
    /// Buff instance index on the unit (what `BuffRemoved` refers to).
    pub index: u32,
    /// Buff template id.
    pub buff_id: u32,
    /// Total duration in ms (0 = permanent).
    pub duration_ms: i32,
    /// Charge counter.
    pub charge: i32,
}

/// A unit announced by `SCUnitStatePacket`.
///
/// `obj`, `unit_type`, `id`, `name`, `pos` and `level` are always read. `hp`/`mp` follow the
/// equipment block and are read for every well-formed packet. `faction_id`, `expedition_id`,
/// `yaw`, `skills` and `buffs` are in the tail; `complete` says whether the whole body was
/// consumed exactly (a character packet the server had to truncate, or a layout this parser does
/// not know, leaves the tail fields at their defaults).
///
/// Not available on the wire: maximum hit points (the client derives them from its own stats).
#[derive(Debug, Clone, PartialEq)]
pub struct UnitInfo {
    /// Object id.
    pub obj: u32,
    /// One of [`unit_type`].
    pub unit_type: u8,
    /// Character id (characters) or template id (NPC, slave, house design, transfer, mate,
    /// shipyard).
    pub id: u32,
    /// Display name (a house/NPC may have an empty one).
    pub name: Box<str>,
    /// Name of the owning character (summoned NPC, slave, mate, house); empty when none.
    pub owner_name: Box<str>,
    /// Owner/summoner character id for NPCs, slaves and mates; 0 otherwise.
    pub owner_id: u32,
    /// Transport-layer id of slaves, houses, transfers and mates; 0 otherwise.
    pub tl_id: u16,
    /// Position.
    pub pos: Pos,
    /// Level.
    pub level: u8,
    /// Hit points (real value, wire / 100).
    pub hp: i32,
    /// Mana (real value, wire / 100).
    pub mp: i32,
    /// `hp <= 0`: derived, the packet has no dead flag.
    pub dead: bool,
    /// The unit still has loot to take (posture `has_loot_left`).
    pub has_loot: bool,
    /// Faction id.
    pub faction_id: u32,
    /// Expedition (guild) id.
    pub expedition_id: u32,
    /// Yaw as the signed-byte direction of `Transform.roll_pitch_yaw_sbytes`; see
    /// [`unit_state_yaw_radians`].
    pub yaw: i8,
    /// Learned skills `(skill id, level)`; characters only.
    pub skills: Box<[(u32, u8)]>,
    /// Active buffs.
    pub buffs: Box<[BuffInfo]>,
    /// Template of the item in the backpack slot ([`BACKPACK_SLOT`]) of a character: a carried
    /// trade pack, a glider; 0 when the slot is empty and for every other kind of unit.
    pub backpack: u32,
    /// Whether the whole body was parsed.
    pub complete: bool,
}

/// A doodad of `SCDoodadCreatedPacket` / `SCDoodadsCreatedPacket`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DoodadInfo {
    /// Object id.
    pub obj: u32,
    /// Doodad template id.
    pub template: u32,
    /// Position.
    pub pos: Pos,
    /// Object id of the owning unit (0 = none).
    pub owner_obj: u32,
    /// Object id of the parent object (0 = none).
    pub parent_obj: u32,
    /// Current phase (`func_group_id`).
    pub phase: u32,
    /// Owner character id (0 = none).
    pub owner_id: u32,
    /// Quest glow value (non-zero = highlighted for a quest).
    pub quest_glow: u32,
    /// The doodad has an item to loot.
    pub has_loot: bool,
}

/// One crop of `SCResponseCommonFarmListPacket`: a plant of the character on a public farm.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CommonFarmEntry {
    /// Farm kind (`FarmGroupKind`: 1 farm, 2 nursery, 3 ranch, 4 stable).
    pub farm_type: u32,
    /// Doodad template of the crop.
    pub template: u32,
    /// Milliseconds until its current phase ends.
    pub time_left: u32,
    /// Its current phase (`func_group_id`).
    pub phase: u32,
    /// Position.
    pub pos: Pos,
    /// When it was planted, Unix seconds.
    pub plant_time: i64,
}

/// One item slot of `SCCharacterInvenContentsPacket`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvItem {
    /// Slot index inside the container (`chunk * 10 + position`).
    pub slot: u16,
    /// Item instance id.
    pub id: u64,
    /// Item template id.
    pub template: u32,
    /// Item grade.
    pub grade: u8,
    /// Stack count.
    pub count: i32,
}

/// One task of an `SCItemTaskSuccessPacket` (`ItemAction`, models/item_task.rs:127).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemChange {
    /// Money delta in copper (action 1).
    Money(i32),
    /// Bank money delta in copper (action 2).
    BankMoney(i32),
    /// Game point (honor/vocation) change (action 3).
    GamePoint {
        /// 0 = honor, 1 = vocation.
        kind: u8,
        /// Amount as sent.
        amount: i32,
    },
    /// AA point delta (action 16).
    AaPoint(i32),
    /// Bank AA point delta (action 17).
    BankAaPoint(i32),
    /// Auto-use-AA-point toggle (action 18).
    AutoUseAaPoint(u8),
    /// An item appeared in a slot: create (5), take back (6) or change owner (15).
    Added {
        /// `ItemAction` byte (5, 6 or 15).
        action: u8,
        /// `SlotType` of the container.
        slot_type: u8,
        /// Slot inside the container.
        slot: u8,
        /// Item template id.
        template: u32,
        /// Item instance id.
        id: u64,
        /// Item grade.
        grade: u8,
        /// Stack count.
        count: i32,
    },
    /// A stack changed by `delta` (action 4).
    CountUpdate {
        /// `SlotType` of the container.
        slot_type: u8,
        /// Slot inside the container.
        slot: u8,
        /// Item instance id.
        id: u64,
        /// Change of the count (negative = consumed).
        delta: i32,
        /// Item template id.
        template: u32,
    },
    /// An item left a slot (action 7).
    Removed {
        /// `SlotType` of the container.
        slot_type: u8,
        /// Slot inside the container.
        slot: u8,
        /// Item instance id.
        id: u64,
        /// Item template id.
        template: u32,
    },
    /// Two slots swapped or an item moved (action 8).
    Moved {
        /// Source `SlotType`.
        from_slot_type: u8,
        /// Source slot.
        from_slot: u8,
        /// Destination `SlotType`.
        to_slot_type: u8,
        /// Destination slot.
        to_slot: u8,
        /// Moved item id.
        from_id: u64,
        /// Item that was in the destination (0 = empty).
        to_id: u64,
    },
    /// Item grade changed (action 14).
    GradeChange {
        /// `SlotType` of the container.
        slot_type: u8,
        /// Slot inside the container.
        slot: u8,
        /// Item instance id.
        id: u64,
        /// New grade.
        grade: u8,
    },
    /// Item details changed (action 9: a socketing, a lunastone, durability); of the details
    /// only the socketed gems of gear are decoded.
    Updated {
        /// `SlotType` of the container.
        slot_type: u8,
        /// Slot inside the container.
        slot: u8,
        /// Item instance id.
        id: u64,
        /// Lunagems socketed in it when the details are gear's (`ItemDetailType.Equipment`),
        /// `None` for other details.
        gems: Option<u8>,
    },
    /// Item flag bits changed (action 10) or security state changed (action 11).
    FlagsUpdated {
        /// `SlotType` of the container.
        slot_type: u8,
        /// Slot inside the container.
        slot: u8,
        /// Item instance id.
        id: u64,
        /// New flag bits.
        bits: u8,
    },
    /// Crafting item removed (action 12).
    RemovedCrafting(u64),
    /// Item removed from a slot without a template (seize, action 13).
    RemovedSlot {
        /// `SlotType` of the container.
        slot_type: u8,
        /// Slot inside the container.
        slot: u8,
        /// Item instance id.
        id: u64,
    },
}

/// The decoded `SCItemTaskSuccessPacket`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ItemTaskInfo {
    /// `ItemTaskType` (why the items changed; models/item_task.rs:17).
    pub reason: u8,
    /// The tasks in order.
    pub changes: SmallVec<[ItemChange; 2]>,
    /// Item ids the client must drop regardless of the tasks.
    pub force_removed: SmallVec<[u64; 1]>,
}

/// One running cooldown of `SCCooldownsPacket`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cooldown {
    /// Skill id or shared tag id.
    pub id: u32,
    /// Total length in ms.
    pub total_ms: u32,
    /// Remaining time in ms.
    pub remaining_ms: u32,
}

/// One item of a loot bag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LootItem {
    /// Loot index (low 16 bits of the fake item id); what a take request names.
    pub index: u16,
    /// Item template id.
    pub template: u32,
    /// Stack count.
    pub count: i32,
}

/// What a bot needs from the server's world packets.
///
/// Level-1 (game) and level-2 (proxy) packets map to the variants below; every other packet
/// parses to `None`.
#[derive(Debug, Clone, PartialEq)]
pub enum WorldEvent {
    // ---- session -----------------------------------------------------------------------
    /// `X2EnterWorldResponsePacket` (packets/g2c.rs:16, writer line 32): the game server accepted
    /// the connection; `token` and `stream_port` address the stream connection.
    EnterWorldResponse {
        /// Reason code (0 = accepted).
        reason: i16,
        /// GM flag.
        gm: bool,
        /// Stream token.
        token: u32,
        /// Stream server port.
        stream_port: u16,
    },
    /// Proxy `ChangeStatePacket` (packets/proxy.rs:22): the connection state machine step.
    ChangeState(i32),
    /// `SCCharacterListPacket` (packets/g2c.rs:121, `Character.Write` models/character.rs:449),
    /// two characters per packet; `last` marks the final packet.
    CharacterList {
        /// Final packet of the list.
        last: bool,
        /// Characters of this packet.
        chars: SmallVec<[CharSummary; 2]>,
    },
    /// `SCCreateCharacterResponsePacket` (world/unit_state.rs:87): the new character.
    CharacterCreated {
        /// Character id.
        id: u32,
        /// Character name.
        name: Box<str>,
    },
    /// `SCCharacterCreationFailedPacket` (packets/world.rs:805).
    CharacterCreateFailed {
        /// Failure reason code.
        reason: u8,
    },
    /// `SCCharacterStatePacket` (world/unit_state.rs:43): the own character on entering the world.
    CharacterState(Box<OwnState>),
    /// `SCPrepareLeaveWorldPacket` (packets/world.rs:769): logout countdown started.
    PrepareLeaveWorld {
        /// Countdown in ms.
        time_ms: i32,
        /// Leave target (quit / character select).
        target: u8,
        /// The server is kicking an idle player.
        idle_kick: bool,
    },
    /// `SCLeaveWorldGrantedPacket` (packets/world.rs:789).
    LeaveWorldGranted {
        /// Leave target.
        target: u8,
    },
    /// `SCLeaveWorldCanceledPacket` (packets/world.rs:1054).
    LeaveWorldCanceled,
    /// `SCLoadInstancePacket` (systems/skills/packets_special.rs:155): the client must load the
    /// instance and answer `CSInstanceLoaded`.
    LoadInstance {
        /// Instance id.
        instance_id: u32,
        /// Zone key.
        zone_id: u32,
        /// Arrival position.
        pos: Pos,
        /// Arrival yaw in radians (the packet's third angle, `ang_z`).
        yaw: f32,
        /// First angle of the packet (`ang_x`, radians; the destination roll, normally 0).
        ang_x: f32,
        /// Second angle of the packet (`ang_y`, radians; the destination pitch, normally 0).
        ang_y: f32,
    },
    /// `SCProcessingInstancePacket` (systems/instances/packets.rs:387, writer :397): the server is
    /// still building the dungeon the character asked for; `SCLoadInstance` follows when it is
    /// ready (systems/instances/dungeon.rs:370).
    ProcessingInstance {
        /// Zone key of the instance being built.
        zone_id: i32,
    },
    /// `SCKickedPacket` (packets/g2c_misc.rs:150).
    Kicked {
        /// Kick reason code.
        reason: u8,
        /// Message shown to the player.
        msg: Box<str>,
    },

    // ---- units -------------------------------------------------------------------------
    /// `SCUnitStatePacket` (world/unit_state.rs:108 for characters, :413 for the other kinds).
    UnitState(Box<UnitInfo>),
    /// `SCUnitsRemovedPacket` (packets/world.rs:554).
    UnitsRemoved {
        /// Object ids that left view.
        objs: SmallVec<[u32; 4]>,
    },
    /// `SCOneUnitMovementPacket` (packets/world.rs:577, models/movement.rs:715), for the unit
    /// (1), vehicle (2), ship (3) and transfer (5) move types; other types parse to `None`. A
    /// ship's heading, speed and helm are in [`WorldEvent::ShipMoved`] ([`parse_ship`]).
    UnitMoved {
        /// Object id.
        obj: u32,
        /// Move type (1 = unit, 2 = vehicle, 3 = ship, 5 = transfer).
        kind: u8,
        /// New position.
        pos: Pos,
        /// Signed-byte yaw of unit moves (0 for the other kinds); see
        /// [`movement_direction_radians`].
        yaw: i8,
        /// Game stance of unit moves (0 = combat, 1 = relaxed, 2 = swim, ...; 0 otherwise).
        stance: u8,
    },
    /// `SCTeleportUnitPacket` (systems/skills/packets_special.rs:188): the own character was
    /// moved.
    TeleportUnit {
        /// Teleport reason code.
        reason: u8,
        /// Error message id (0 = none).
        error: i16,
        /// Destination.
        pos: Pos,
    },
    /// A ship's `SCOneUnitMovementPacket` (move type 3, models/movement.rs:552; built by
    /// systems/ai/physics/replication.rs:198) with its heading, speed and helm. Only produced by
    /// [`parse_ship`]: [`parse`] keeps returning [`WorldEvent::UnitMoved`] for the same packet.
    ShipMoved(Box<ShipMotion>),
    /// `SCUnitPointsPacket` (packets/world.rs:534); the wire values are times 100.
    UnitPoints {
        /// Object id.
        obj: u32,
        /// Hit points (real value).
        hp: i32,
        /// Mana (real value).
        mp: i32,
    },
    /// `SCUnitDamagedPacket` (systems/skills/packets.rs:334): a landed hit, broadcast to the
    /// players around the target. The bot reads who hit whom to know the first hitter of a
    /// monster (the server credits the tag, not the killing blow).
    UnitDamaged {
        /// Object id of the caster.
        caster: u32,
        /// Object id of the unit that took the hit.
        target: u32,
    },
    /// `SCUnitDeathPacket` (systems/skills/packets.rs:394).
    UnitDeath {
        /// Dead unit.
        obj: u32,
        /// Killer object id (0 = none).
        killer: u32,
        /// Respawn wait in ms (0 for NPCs without a timer).
        respawn_wait_ms: i32,
        /// Experience lost.
        lost_exp: i32,
        /// `KillReason` byte.
        reason: u8,
    },
    /// `SCCharacterResurrectedPacket` (systems/skills/packets_special.rs:53).
    Resurrected {
        /// Resurrected unit.
        obj: u32,
        /// Position it stands at again.
        pos: Pos,
        /// Yaw in radians.
        yaw: f32,
    },
    /// `SCNotifyResurrectionPacket` (systems/skills/packets_special.rs:37): a resurrection is
    /// offered to the own character (a caster's spell, or the free offer below level 10).
    ResurrectOffer {
        /// Casting unit/doodad object id.
        caster_obj: u32,
    },
    /// `SCTargetChangedPacket` (packets/world_objects.rs:209).
    TargetChanged {
        /// Unit that changed target.
        obj: u32,
        /// New target (0 = none).
        target: u32,
    },
    /// `SCExpChangedPacket` (systems/skills/packets.rs:862): experience gained or lost.
    ExpChanged {
        /// Character object id.
        obj: u32,
        /// Experience delta (negative = lost).
        delta: i32,
    },
    /// `SCLevelChangedPacket` (systems/skills/progress.rs:96).
    LevelChanged {
        /// Unit object id.
        obj: u32,
        /// New level.
        level: u8,
    },
    /// `SCCombatEngagedPacket` (systems/skills/packets.rs:643).
    CombatEngaged {
        /// Unit that entered combat.
        obj: u32,
    },
    /// `SCCombatClearedPacket` (systems/skills/packets.rs:638).
    CombatCleared {
        /// Unit that left combat.
        obj: u32,
    },
    /// `SCUnitFactionChangedPacket` (systems/social/packets/faction.rs:13), e.g. a duel flag.
    FactionChanged {
        /// Unit object id.
        obj: u32,
        /// Previous faction.
        old: u32,
        /// New faction.
        faction: u32,
        /// The change is temporary.
        temp: bool,
    },
    /// `SCUnitEquipmentsChangedPacket` (systems/economy/item_packets.rs:64): equipment slots of
    /// a unit changed; a trade pack picked up or dropped is a change of [`BACKPACK_SLOT`].
    UnitEquipmentsChanged {
        /// Object id of the unit.
        obj: u32,
        /// `(slot, item template)` per changed slot; template 0 = the slot is empty now.
        slots: SmallVec<[(u8, u32); 2]>,
    },
    /// `SCUnitPvPPointsChangedPacket` (packets/g2c_misc.rs:104): a unit's honor or hostile
    /// kills changed (broadcast to the unit's neighbours after a player kill).
    PvpPoints {
        /// Object id of the unit.
        obj: u32,
        /// 0 = honor gained in combat, 1 = hostile faction kills (see [`pvp_points`]).
        kind: u8,
        /// The new total.
        points: i32,
    },
    /// `SCConflictZoneStatePacket` (packets/world.rs:334): the conflict state of a zone group,
    /// sent once per zone group at world entry and again on every change. `state` is one of
    /// [`conflict_state`].
    ConflictZoneState {
        /// Zone group id.
        zone_group: u16,
        /// Conflict state, see [`conflict_state`].
        state: u8,
        /// When the state ends, in Unix seconds (0 when it does not).
        end_unix_s: i64,
    },
    /// `SCTowerDefListPacket` (systems/instances/packets.rs:735): every tower defence event
    /// ("rift") running in the world instance, with its wave and the position of its target.
    /// Sent on each start, wave and end and when the character changes zone.
    TowerDefList(Box<[TowerDefInfo]>),
    /// `SCTowerDefStartPacket` (systems/instances/packets.rs:756): an event started.
    TowerDefStart {
        /// Which event.
        id: EventId,
        /// The zone key it was started in.
        zone_key: u32,
    },
    /// `SCTowerDefEndPacket` (systems/instances/packets.rs:774): an event ended.
    TowerDefEnd {
        /// Which event.
        id: EventId,
        /// The zone key it was started in.
        zone_key: u32,
    },
    /// `SCTowerDefWaveStartPacket` (systems/instances/packets.rs:793): a wave of an event began.
    TowerDefWave {
        /// Which event.
        id: EventId,
        /// The zone key it was started in.
        zone_key: u32,
        /// The wave (from 1).
        step: u32,
    },

    // ---- doodads -----------------------------------------------------------------------
    /// `SCDoodadCreatedPacket` (one doodad, `Doodad.Write` models/doodad.rs:247) or
    /// `SCDoodadsCreatedPacket` (packets/world_objects.rs:21, up to 30 doodads).
    DoodadsCreated(SmallVec<[DoodadInfo; 1]>),
    /// `SCDoodadRemovedPacket` (packets/world_objects.rs:44) or `SCDoodadsRemovedPacket` (:60).
    DoodadsRemoved(SmallVec<[u32; 4]>),
    /// `SCResponseCommonFarmListPacket` (systems/npcs/public_farm.rs): the character's crops on
    /// the public farms, the answer to `CSRequestCommonFarmList`. The doodad object ids are not
    /// in it; a crop gets its object id when it comes into view.
    CommonFarmList(Box<[CommonFarmEntry]>),
    /// `SCDoodadPhaseChangedPacket` (systems/npcs/doodad/packets.rs:53).
    DoodadPhase {
        /// Doodad object id.
        obj: u32,
        /// New phase (`func_group_id`).
        phase: u32,
    },

    // ---- portals (teleport book, recall points) ------------------------------------------
    /// `SCCharacterPortalsPacket` (packets/world.rs:238, writer :248): the teleport book. The
    /// full private list is sent at login when the book is not empty (world/session.rs:710) and
    /// again, in full, by a `CSNotifySubZone` that adds a recall point
    /// (systems/npcs/portal/mod.rs:353); a newly recorded location arrives alone
    /// (portal/mod.rs:486). Merge by [`PortalEntry::id`].
    PortalBook(Box<[PortalEntry]>),
    /// `SCCharacterReturnDistrictsPacket` (packets/world.rs:257, writer :268): the visited recall
    /// districts (always the full list; sent at login when non-empty, world/session.rs:720, and
    /// by `CSNotifySubZone`), plus the character's bound recall point.
    ReturnDistricts {
        /// All visited districts.
        portals: Box<[PortalEntry]>,
        /// Id of the bound return point (0 = none): the id the server compares with the entries'
        /// ids (a bound entry cannot be deleted, systems/npcs/portal/mod.rs:544).
        bound: u32,
    },
    /// `SCPortalInfoSavedPacket` (systems/npcs/portal/packets.rs:287, writer :297): a book entry
    /// was renamed (systems/npcs/portal/mod.rs:508).
    PortalSaved(Box<PortalEntry>),
    /// `SCPortalDeletedPacket` (systems/npcs/portal/packets.rs:270, writer :281): the answer to
    /// `CSDeletePortal`, once per removed entry (portal/mod.rs:576).
    PortalDeleted {
        /// 1 = a recall district, anything else a private book entry (the request's type).
        kind: u8,
        /// Entry id.
        id: i32,
    },
    /// `SCUnitPortalUsedPacket` (systems/npcs/portal/packets.rs:303, writer :313): someone went
    /// through a gate; sent to everyone around it, before the traveller's `TeleportUnit`
    /// (portal/mod.rs:959).
    PortalUsed {
        /// Object id of the gate NPC that was used.
        gate: u32,
    },

    // ---- mates (mounts and pets) -----------------------------------------------------------
    /// `SCMateSpawnedPacket` (systems/vehicles/packets.rs:69, writer :579): a mate was summoned;
    /// its unit state follows.
    MateSpawned(Box<MateInfo>),

    // ---- slaves (boats) --------------------------------------------------------------------
    /// `SCSlaveCreatedPacket` (systems/vehicles/packets.rs:107): a slave was summoned; its
    /// `UnitState` follows.
    SlaveCreated {
        /// Object id of the summoner.
        owner_obj: u32,
        /// Slave timeline id (`tl`).
        tl: u16,
        /// Object id of the slave.
        slave_obj: u32,
        /// The summon effect is skipped.
        hide_effect: bool,
        /// Name of the summoner.
        creator: Box<str>,
    },
    /// `SCMySlavePacket` (systems/vehicles/packets.rs:147): the summoner's own slave record.
    MySlave(Box<MySlaveInfo>),
    /// `SCSlaveBoundPacket` (systems/vehicles/packets.rs:140): a character took the driver's seat
    /// of a slave (sent right after the matching [`WorldEvent::UnitAttached`],
    /// systems/vehicles/slaves.rs:1742).
    SlaveBound {
        /// Character id (not an object id) of the driver.
        master_id: u32,
        /// Object id of the slave.
        slave_obj: u32,
    },
    /// `SCUnitAttachedPacket` (packets/world_objects.rs:224, writer :239): a unit was seated on a
    /// slave or mate (also re-sent when a seated unit comes into view, world/visibility.rs:151).
    UnitAttached {
        /// Object id of the attached unit.
        child: u32,
        /// `AttachPointKind` ([`crate::sc_travel::attach_point`]).
        point: u8,
        /// Object id of the parent (the slave).
        parent: u32,
        /// `AttachUnitReason` ([`crate::sc_travel::attach_reason`]).
        reason: u8,
    },
    /// `SCUnitDetachedPacket` (systems/vehicles/packets.rs:97): a unit left its seat.
    UnitDetached {
        /// Object id of the detached unit.
        obj: u32,
        /// `AttachUnitReason`.
        reason: u8,
    },
    /// `SCSlaveDespawnPacket` (systems/vehicles/packets.rs:133): the slave is going away.
    SlaveDespawned {
        /// Object id of the slave.
        obj: u32,
    },
    /// `SCSlaveRemovedPacket` (systems/vehicles/packets.rs:126): the summoner no longer owns the
    /// slave with timeline id `tl`.
    SlaveRemoved {
        /// Object id of the owner (the slave itself in the rider's escape path).
        obj: u32,
        /// Slave timeline id.
        tl: u16,
    },

    // ---- own data ----------------------------------------------------------------------
    /// `SCCharacterInvenInitPacket` (packets/world.rs:51): container sizes.
    InventoryInit {
        /// Bag slots.
        bag_slots: u32,
        /// Bank slots.
        bank_slots: u32,
    },
    /// `SCCharacterInvenContentsPacket` (packets/world.rs:69, `Item.Write` models/item.rs:1061):
    /// ten slots per packet; empty slots are not listed.
    Inventory {
        /// `SlotType` (2 = bag, 3 = bank).
        slot_type: u8,
        /// Index of this chunk of ten slots.
        chunk: u8,
        /// Occupied slots of the chunk.
        items: Box<[InvItem]>,
    },
    /// `SCItemTaskSuccessPacket` (systems/economy/item_packets.rs:15, tasks
    /// models/item_task.rs:385): every item, money and bank change.
    ItemTask(Box<ItemTaskInfo>),
    /// `SCItemSocketingLunagemResultPacket` (systems/economy/item_effect_packets.rs:91): the end
    /// of a lunagem cast. A failure (`result` 0 with `installed`) took every gem out of the item;
    /// the item task that follows has the item's details.
    LunagemResult {
        /// 1 = the gem went in, 0 = it did not.
        result: u8,
        /// The piece of gear.
        item_id: u64,
        /// The gem's template (the dawnstone's for a removal).
        gem: u32,
        /// A gem was tried (false for the dawnstone).
        installed: bool,
    },
    /// `SCGradeEnchantResultPacket` (systems/economy/item_effect_packets.rs:51): the end of a
    /// regrade (`result`: [`grade_enchant_result`]). A broken item is gone (an item task removes
    /// it), the others carry their new grade.
    GradeEnchantResult {
        /// [`grade_enchant_result`].
        result: u8,
        /// The piece of gear.
        item_id: u64,
        /// Its template.
        template: u32,
        /// The grade before.
        before: u8,
        /// The grade after.
        after: u8,
    },
    /// `SCCharacterLaborPowerChangedPacket` (systems/skills/progress.rs:35).
    LaborChanged {
        /// Labor delta (negative = spent).
        delta: i32,
        /// Actability id that gained points (0 = none).
        actability: i32,
        /// Actability point change.
        point: i32,
        /// Actability step.
        step: u8,
    },
    /// `SCSkillLearnedPacket` / `SCSkillUpgradedPacket` (systems/skills/packets.rs:814). The
    /// skills known at login are in [`UnitInfo::skills`] of the own `UnitState`.
    SkillLearned {
        /// Skill id.
        skill: u32,
        /// Skill level.
        level: u8,
        /// `true` for an upgrade of an already learned skill.
        upgrade: bool,
    },
    /// `SCCooldownsPacket` (systems/skills/packets.rs:441): running cooldowns synced on login.
    Cooldowns {
        /// Skill cooldowns.
        skills: Box<[Cooldown]>,
        /// Shared tag cooldowns.
        tags: Box<[Cooldown]>,
    },
    /// `SCSkillCooldownResetPacket` (systems/skills/packets.rs:499).
    CooldownReset {
        /// Unit object id.
        obj: u32,
        /// Skill id.
        skill: u32,
        /// Shared tag id.
        tag: u32,
        /// The global cooldown was reset.
        gcd: bool,
    },
    /// `SCSkillStartedPacket` (systems/skills/packets.rs:18).
    SkillStarted {
        /// Casting unit.
        caster_obj: u32,
        /// Skill id.
        skill: u32,
        /// Skill instance id (`tl`) that `SkillEnded` refers to.
        tl: u16,
        /// Target object (unit or doodad targets only; 0 otherwise).
        target_obj: u32,
        /// 0 = started, anything else is a `SkillResult` failure.
        result: u8,
    },
    /// `SCSkillEndedPacket` (systems/skills/packets.rs:151).
    SkillEnded {
        /// Skill instance id.
        tl: u16,
    },
    /// `SCSkillStoppedPacket` (systems/skills/packets.rs:167).
    SkillStopped {
        /// Unit object id.
        obj: u32,
        /// Skill id.
        skill: u32,
    },
    /// `SCCastingStoppedPacket` (systems/skills/packets.rs:184).
    CastingStopped {
        /// Skill instance id.
        tl: u16,
        /// Cast duration in ms.
        duration_ms: u32,
    },
    /// `SCBuffCreatedPacket` (systems/skills/packets.rs:201).
    BuffCreated {
        /// Unit that carries the buff.
        obj: u32,
        /// Buff instance index on the unit.
        index: u32,
        /// Buff template id.
        buff_id: u32,
        /// Casting unit/doodad object id.
        caster_obj: u32,
        /// Caster level.
        caster_level: u8,
        /// Skill that created a toggle buff (0 otherwise).
        skill_id: u32,
        /// Charge (negative values are not representable on the wire).
        charge: i32,
        /// Duration in ms (0 = permanent), 10 ms resolution.
        duration_ms: i32,
    },
    /// `SCBuffRemovedPacket` (systems/skills/packets.rs:276).
    BuffRemoved {
        /// Unit that carried the buff.
        obj: u32,
        /// Buff instance index.
        index: u32,
    },

    // ---- chat / errors -----------------------------------------------------------------
    /// `SCChatMessagePacket` (packets/world.rs:698, writer :710). GM senders arrive as
    /// `"GM <name>"`; nothing is stripped here.
    Chat {
        /// Chat type (`-4` whispered, `-3` whisper, `-2` system, `-1` notice, `0` say, `1` shout,
        /// `2` trade, `4` party, `7` guild, ...).
        kind: i16,
        /// Sender object id (0 for system text).
        sender_obj: u32,
        /// Sender character id.
        sender_id: u32,
        /// Sender name.
        name: Box<str>,
        /// Message text.
        msg: Box<str>,
    },
    /// `SCErrorMsgPacket` (packets/world.rs:737).
    ErrorMsg {
        /// `ErrorMessageType` id.
        err: i16,
        /// Second error field: equal to `err` for ordinary messages, a detail for some (the
        /// craft-stop reason for error 148, `ErrCraftStopped`).
        err2: i16,
        /// Type field of the packet.
        kind: u32,
        /// Shown as a notification instead of a dialog.
        notify: bool,
    },

    // ---- loot --------------------------------------------------------------------------
    /// `SCLootableStatePacket` (systems/economy/loot_packets.rs:211).
    LootableState {
        /// Object id of the NPC or doodad.
        owner_obj: u32,
        /// 1 = NPC, 2 = doodad.
        owner_type: u16,
        /// It has loot to take.
        has_loot: bool,
    },
    /// `SCLootBagDataPacket` (systems/economy/loot_packets.rs:75): the open loot window.
    LootBag {
        /// Object id of the loot owner (decoded from the fake item ids; 0 for an empty bag).
        owner_obj: u32,
        /// Loot owner type (1 = NPC, 2 = doodad).
        owner_type: u16,
        /// Items of the bag.
        items: SmallVec<[LootItem; 2]>,
        /// The server offers "loot all".
        loot_all: bool,
    },
    /// `SCLootItemTookPacket` (systems/economy/loot_packets.rs:96).
    LootTook {
        /// Item template id.
        template: u32,
        /// Loot index of the item.
        index: u16,
        /// Stack count taken.
        count: i32,
        /// Loot owner type.
        owner_type: u16,
        /// Loot owner object id.
        owner_obj: u32,
    },
    /// `SCLootItemFailedPacket` (systems/economy/loot_packets.rs:122).
    LootFailed {
        /// `ErrorMessageType` id.
        err: i16,
        /// Loot index of the item.
        index: u16,
        /// Loot owner type.
        owner_type: u16,
        /// Loot owner object id.
        owner_obj: u32,
        /// Item template id.
        template: u32,
    },
}

// ---- dispatch ------------------------------------------------------------------------------

/// Parses one S→C packet body (opcode already stripped; DD04 batches are expanded by the
/// caller) of level 1 (game) or level 2 (proxy). `Ok(None)` when bots do not need the packet or
/// the unit/move kind is not handled; `Err` when the body does not match the writer's layout.
pub fn parse(level: u8, opcode: u16, body: &[u8]) -> Result<Option<WorldEvent>, MarshalError> {
    let Some(read) = reader(level, opcode) else {
        return Ok(None);
    };
    read(&mut PacketStream::from_bytes(body))
}

/// Whether [`parse`] handles this packet, so the caller can skip the work for the others. A
/// handled packet may still parse to `None` for a unit or move kind bots do not track.
pub fn interesting(level: u8, opcode: u16) -> bool {
    reader(level, opcode).is_some()
}

/// The ship companion of [`parse`]: decodes a ship's `SCOneUnitMovementPacket` (move type 3)
/// into [`WorldEvent::ShipMoved`] with its heading, velocity and helm. Every other packet and
/// every other move type is `Ok(None)` (a non-movement packet without reading its body);
/// [`parse`] still returns [`WorldEvent::UnitMoved`] for the same ship packet, so a bot that
/// follows ships calls both.
pub fn parse_ship(level: u8, opcode: u16, body: &[u8]) -> Result<Option<WorldEvent>, MarshalError> {
    if level != LEVEL_GAME || opcode != sc::SCOneUnitMovementPacket {
        return Ok(None);
    }
    sc_travel::read_ship_moved(&mut PacketStream::from_bytes(body))
}

type Reader = fn(&mut PacketStream) -> Res<Option<WorldEvent>>;

fn reader(level: u8, opcode: u16) -> Option<Reader> {
    match level {
        LEVEL_GAME => game_reader(opcode),
        LEVEL_PROXY => proxy_reader(opcode),
        _ => None,
    }
}

fn proxy_reader(opcode: u16) -> Option<Reader> {
    match opcode {
        pp::ChangeStatePacket => Some(read_change_state),
        _ => None,
    }
}

fn game_reader(opcode: u16) -> Option<Reader> {
    Some(match opcode {
        // session
        sc::X2EnterWorldResponsePacket => read_enter_world_response,
        sc::SCCharacterListPacket => read_character_list,
        sc::SCCreateCharacterResponsePacket => read_character_created,
        sc::SCCharacterCreationFailedPacket => read_character_create_failed,
        sc::SCCharacterStatePacket => read_character_state,
        sc::SCPrepareLeaveWorldPacket => read_prepare_leave_world,
        sc::SCLeaveWorldGrantedPacket => read_leave_world_granted,
        sc::SCLeaveWorldCanceledPacket => |_| some(WorldEvent::LeaveWorldCanceled),
        sc::SCLoadInstancePacket => read_load_instance,
        sc::SCProcessingInstancePacket => sc_travel::read_processing_instance,
        sc::SCKickedPacket => read_kicked,
        // units
        sc::SCUnitStatePacket => read_unit_state,
        sc::SCUnitsRemovedPacket => read_units_removed,
        sc::SCOneUnitMovementPacket => read_unit_moved,
        sc::SCTeleportUnitPacket => read_teleport_unit,
        sc::SCUnitPointsPacket => read_unit_points,
        sc::SCUnitDamagedPacket => read_unit_damaged,
        sc::SCUnitDeathPacket => read_unit_death,
        sc::SCCharacterResurrectedPacket => read_resurrected,
        sc::SCNotifyResurrectionPacket => |s| {
            some(WorldEvent::ResurrectOffer {
                caster_obj: read_skill_caster(s)?,
            })
        },
        sc::SCTargetChangedPacket => read_target_changed,
        sc::SCExpChangedPacket => read_exp_changed,
        sc::SCLevelChangedPacket => read_level_changed,
        sc::SCCombatEngagedPacket => |s| some(WorldEvent::CombatEngaged { obj: s.read_bc()? }),
        sc::SCCombatClearedPacket => |s| some(WorldEvent::CombatCleared { obj: s.read_bc()? }),
        sc::SCUnitFactionChangedPacket => read_faction_changed,
        // zone conflicts, pvp
        sc::SCConflictZoneStatePacket => read_conflict_zone_state,
        sc::SCUnitEquipmentsChangedPacket => read_unit_equipments_changed,
        sc::SCUnitPvPPointsChangedPacket => read_pvp_points,
        // tower defence events (rifts)
        sc::SCTowerDefListPacket => sc_event::read_list,
        sc::SCTowerDefStartPacket => sc_event::read_start,
        sc::SCTowerDefEndPacket => sc_event::read_end,
        sc::SCTowerDefWaveStartPacket => sc_event::read_wave,
        // portals
        sc::SCCharacterPortalsPacket => sc_travel::read_portal_book,
        sc::SCCharacterReturnDistrictsPacket => sc_travel::read_return_districts,
        sc::SCPortalInfoSavedPacket => sc_travel::read_portal_saved,
        sc::SCPortalDeletedPacket => sc_travel::read_portal_deleted,
        sc::SCUnitPortalUsedPacket => sc_travel::read_portal_used,
        // slaves
        sc::SCMateSpawnedPacket => sc_travel::read_mate_spawned,
        sc::SCSlaveCreatedPacket => sc_travel::read_slave_created,
        sc::SCMySlavePacket => sc_travel::read_my_slave,
        sc::SCSlaveBoundPacket => sc_travel::read_slave_bound,
        sc::SCUnitAttachedPacket => sc_travel::read_unit_attached,
        sc::SCUnitDetachedPacket => sc_travel::read_unit_detached,
        sc::SCSlaveDespawnPacket => sc_travel::read_slave_despawned,
        sc::SCSlaveRemovedPacket => sc_travel::read_slave_removed,
        // doodads
        sc::SCDoodadCreatedPacket => read_doodad_created,
        sc::SCDoodadsCreatedPacket => read_doodads_created,
        sc::SCDoodadRemovedPacket => read_doodad_removed,
        sc::SCDoodadsRemovedPacket => read_doodads_removed,
        sc::SCDoodadPhaseChangedPacket => read_doodad_phase,
        sc::SCResponseCommonFarmListPacket => read_common_farm_list,
        // own data
        sc::SCCharacterInvenInitPacket => read_inventory_init,
        sc::SCCharacterInvenContentsPacket => read_inventory,
        sc::SCItemTaskSuccessPacket => read_item_task,
        sc::SCItemSocketingLunagemResultPacket => read_lunagem_result,
        sc::SCGradeEnchantResultPacket => read_grade_enchant_result,
        sc::SCCharacterLaborPowerChangedPacket => read_labor_changed,
        sc::SCSkillLearnedPacket => |s| read_skill_learned(s, false),
        sc::SCSkillUpgradedPacket => |s| read_skill_learned(s, true),
        sc::SCCooldownsPacket => read_cooldowns,
        sc::SCSkillCooldownResetPacket => read_cooldown_reset,
        sc::SCSkillStartedPacket => read_skill_started,
        sc::SCSkillEndedPacket => |s| some(WorldEvent::SkillEnded { tl: s.read_u16()? }),
        sc::SCSkillStoppedPacket => read_skill_stopped,
        sc::SCCastingStoppedPacket => read_casting_stopped,
        sc::SCBuffCreatedPacket => read_buff_created,
        sc::SCBuffRemovedPacket => read_buff_removed,
        // chat / errors
        sc::SCChatMessagePacket => read_chat,
        sc::SCErrorMsgPacket => read_error_msg,
        // loot
        sc::SCLootableStatePacket => read_lootable_state,
        sc::SCLootBagDataPacket => read_loot_bag,
        sc::SCLootItemTookPacket => read_loot_took,
        sc::SCLootItemFailedPacket => read_loot_failed,
        _ => return None,
    })
}

pub(crate) fn some(event: WorldEvent) -> Res<Option<WorldEvent>> {
    Ok(Some(event))
}

// ---- yaw helpers ---------------------------------------------------------------------------

/// Radians of a `SCUnitStatePacket` signed-byte yaw (`v / 127` of a turn,
/// `PositionAndRotation::rotation_from_sbytes`, aaemu-game models/transform.rs:147).
pub fn unit_state_yaw_radians(yaw: i8) -> f32 {
    f32::from(yaw) / 127.0 * std::f32::consts::TAU
}

/// Radians of a movement-packet signed-byte direction (`v * 360 / 128` degrees,
/// `convert_direction_to_radian`, aaemu-game models/movement.rs:748).
pub fn movement_direction_radians(direction: i8) -> f32 {
    f32::from(direction) / 128.0 * std::f32::consts::TAU
}

// ---- stream helpers ------------------------------------------------------------------------

pub(crate) fn invalid<T>(msg: impl Into<String>) -> Res<T> {
    Err(MarshalError::Invalid(msg.into()))
}

/// Advances over `n` bytes without copying them.
pub(crate) fn skip(s: &mut PacketStream, n: usize) -> Res<()> {
    if n > s.remaining() {
        return Err(MarshalError::EndOfStream {
            pos: s.pos(),
            wanted: n,
            len: s.len(),
        });
    }
    s.set_pos(s.pos() + n);
    Ok(())
}

/// Rejects a counted list whose elements (at least `min_size` bytes each) cannot fit in the rest
/// of the body, so a corrupt count never drives a large allocation.
pub(crate) fn check_count(s: &PacketStream, count: usize, min_size: usize, what: &str) -> Res<()> {
    if count.saturating_mul(min_size) > s.remaining() {
        return invalid(format!("{what} count {count} exceeds the packet body"));
    }
    Ok(())
}

pub(crate) fn boxed(s: String) -> Box<str> {
    s.into_boxed_str()
}

/// Reads a `u16` length prefix (`write_bytes_sized`) and skips that many bytes.
fn skip_sized(s: &mut PacketStream) -> Res<()> {
    let n = usize::from(s.read_u16()?);
    skip(s, n)
}

/// `UnitCustomModelParams` (models/unit_model.rs:143): a kind byte gates the following blocks.
fn skip_model_params(s: &mut PacketStream) -> Res<()> {
    let kind = s.read_u8()?;
    if kind >= 1 {
        skip(s, 4)?; // hair color
    }
    if kind >= 2 {
        skip(s, 8)?; // skin color, model id
    }
    if kind >= 3 {
        // FaceModel (models/unit_model.rs:83): 20 bytes of movable decal, 4 fixed decals of 8,
        // three map ids, the normal map weight, five colors, then the sized modifier blob.
        skip(s, 20 + 32 + 12 + 4 + 20)?;
        skip_sized(s)?;
    }
    Ok(())
}

// ---- items ---------------------------------------------------------------------------------

/// The fields of an item record bots keep.
struct ItemRec {
    template: u32,
    id: u64,
    grade: u8,
    count: i32,
    /// Lunagems socketed (equipment details only, else 0).
    gems: u8,
}

/// `ItemDetailType.Equipment`: the detail block of gear (models/item.rs:24).
const DETAIL_EQUIPMENT: u8 = 1;
/// Where the seven gem template ids start in the equipment detail body (after the image item,
/// durability, an `i16`, the lunastone, the charge time and the dye: `4 + 1 + 2 + 4 + 8 + 4`,
/// models/item.rs:603 `write_details`).
const GEMS_AT: usize = 23;
/// Gem slots of an equipment detail (`gem_ids: [u32; 7]`).
const GEM_SLOTS: usize = 7;

/// The number of socketed gems in an equipment detail body (the non-zero gem ids); 0 when the
/// body is too short.
fn gems_in(body: &[u8]) -> u8 {
    body.get(GEMS_AT..GEMS_AT + GEM_SLOTS * 4).map_or(0, |b| {
        b.as_chunks::<4>()
            .0
            .iter()
            .filter(|c| c.iter().any(|&x| x != 0))
            .count() as u8
    })
}

/// Reads the detail body of `detail` type and returns the gems of an equipment detail.
fn read_detail_body(s: &mut PacketStream, detail: u8) -> Res<u8> {
    let Some(&len) = DETAIL_BODY_LENS.get(usize::from(detail)) else {
        return invalid(format!("unknown item detail type {detail}"));
    };
    if detail != DETAIL_EQUIPMENT {
        skip(s, len)?;
        return Ok(0);
    }
    let at = s.pos();
    skip(s, len)?;
    Ok(gems_in(&s.as_bytes()[at..at + len]))
}

/// `Item.Write` (models/item.rs:1061) after the template id: id, grade, flags, count, the typed
/// detail block, create time, lifespan, made unit, world id, unsecure and unpack times.
fn read_item_body(s: &mut PacketStream, template: u32) -> Res<ItemRec> {
    let id = s.read_u64()?;
    let grade = s.read_u8()?;
    s.read_u8()?; // flags
    let count = s.read_i32()?;
    let detail = s.read_u8()?;
    let gems = read_detail_body(s, detail)?;
    skip(s, 8 + 4 + 4 + 1 + 8 + 8)?; // create time, lifespan, made unit, world id, unsecure, unpack
    Ok(ItemRec {
        template,
        id,
        grade,
        count,
        gems,
    })
}

/// `Item.Write` of a whole slot: `Ok(None)` for the 4-byte empty form (template 0).
fn read_item(s: &mut PacketStream) -> Res<Option<ItemRec>> {
    let template = s.read_u32()?;
    if template == 0 {
        return Ok(None);
    }
    read_item_body(s, template).map(Some)
}

/// The occupied equipment slots of a character record and the gems in each (same order).
#[derive(Default)]
struct EquipOut {
    slots: Vec<EquipSlot>,
    gems: Vec<u8>,
}

/// The equipment block of `Character.Write` (models/character.rs:449): 28 slots, appearance
/// parts (19..=25) carry the template only.
fn read_character_equipment(s: &mut PacketStream, mut out: Option<&mut EquipOut>) -> Res<()> {
    for slot in 0..EQUIPMENT_SLOTS {
        let template = s.read_u32()?;
        if template == 0 {
            continue;
        }
        let (id, grade, gems) = if BODY_PART_SLOTS.contains(&slot) {
            (0, 0, 0)
        } else {
            let item = read_item_body(s, template)?;
            (item.id, item.grade, item.gems)
        };
        if let Some(out) = out.as_deref_mut() {
            out.slots.push(EquipSlot {
                slot,
                template,
                id,
                grade,
            });
            out.gems.push(gems);
        }
    }
    Ok(())
}

// ---- character records ---------------------------------------------------------------------

/// The fields bots keep of one `Character.Write` record (models/character.rs:449).
struct CharRecord {
    id: u32,
    name: String,
    race: u8,
    gender: u8,
    level: u8,
    hp: i32,
    mp: i32,
    zone: u32,
    faction: u32,
    expedition: u32,
    family: u32,
    abilities: [u8; 3],
    pos: Pos,
    labor: i16,
    money: i64,
    loyalty: i64,
    bank_money: i64,
}

/// A lower bound of one record's size, to reject absurd list counts.
const MIN_CHAR_RECORD: usize = 150;

/// Reads one complete `Character.Write` record. The list has no entry lengths, so every byte
/// must be consumed: stopping after the name would parse the next character from equipment bytes.
fn read_character_record(
    s: &mut PacketStream,
    equipment: Option<&mut EquipOut>,
) -> Res<CharRecord> {
    let id = s.read_u32()?;
    let name = s.read_string()?;
    let race = s.read_u8()?;
    let gender = s.read_u8()?;
    let level = s.read_u8()?;
    let hp = s.read_i32()?;
    let mp = s.read_i32()?;
    let zone = s.read_u32()?;
    let faction = s.read_u32()?;
    s.read_string()?; // faction name
    let expedition = s.read_u32()?;
    let family = s.read_u32()?;
    read_character_equipment(s, equipment)?;
    let abilities = [s.read_u8()?, s.read_u8()?, s.read_u8()?];
    let x = convert_long_to_coord(s.read_i64()?);
    let y = convert_long_to_coord(s.read_i64()?);
    let z = s.read_f32()?;
    skip_model_params(s)?;
    let labor = s.read_i16()?;
    // labor modified (8), dead count (2), dead time (8), rez wait (4), rez time (8), rez penalty
    // (4), last world leave (8)
    skip(s, 8 + 2 + 8 + 4 + 8 + 4 + 8)?;
    let money = s.read_i64()?;
    // money amount (8), crime (2), infamy (4), trials (2), delete request (8), transfer request
    // (8), delete delay (8), consumed labor (4)
    skip(s, 8 + 2 + 4 + 2 + 8 + 8 + 8 + 4)?;
    let loyalty = s.read_i64()?;
    let bank_money = s.read_i64()?;
    // money amount (8), auto AA point (1), prev point (4), point (4), gift (4), updated (8),
    // force name change (1)
    skip(s, 8 + 1 + 4 + 4 + 4 + 8 + 1)?;
    Ok(CharRecord {
        id,
        name,
        race,
        gender,
        level,
        hp,
        mp,
        zone,
        faction,
        expedition,
        family,
        abilities,
        pos: (x, y, z),
        labor,
        money,
        loyalty,
        bank_money,
    })
}

// ---- session readers -----------------------------------------------------------------------

fn read_change_state(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    some(WorldEvent::ChangeState(s.read_i32()?))
}

fn read_enter_world_response(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let reason = s.read_i16()?;
    let gm = s.read_bool()?;
    let token = s.read_u32()?;
    let stream_port = s.read_u16()?;
    some(WorldEvent::EnterWorldResponse {
        reason,
        gm,
        token,
        stream_port,
    })
}

fn read_character_list(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let last = s.read_bool()?;
    let count = usize::from(s.read_u8()?);
    check_count(s, count, MIN_CHAR_RECORD, "character")?;
    let mut chars = SmallVec::new();
    for _ in 0..count {
        let c = read_character_record(s, None)?;
        chars.push(CharSummary {
            id: c.id,
            name: boxed(c.name),
            race: c.race,
            gender: c.gender,
            level: c.level,
        });
    }
    some(WorldEvent::CharacterList { last, chars })
}

fn read_character_created(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    // Only the id and the name: the record behind them is the same as in the list.
    let id = s.read_u32()?;
    let name = boxed(s.read_string()?);
    some(WorldEvent::CharacterCreated { id, name })
}

fn read_character_create_failed(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    some(WorldEvent::CharacterCreateFailed {
        reason: s.read_u8()?,
    })
}

fn read_character_state(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    // world/unit_state.rs:43
    let instance_id = s.read_u32()?;
    skip_sized(s)?; // guid
    skip(s, 4)?; // rwd
    let mut equipment = EquipOut::default();
    let c = read_character_record(s, Some(&mut equipment))?;
    s.read_f32()?; // rot x
    s.read_f32()?; // rot y
    let yaw = s.read_f32()?;
    let exp = s.read_i32()?;
    let recoverable_exp = s.read_i32()?;
    // penaltied exp (4), return district id (4), return district (4), resurrection district (4),
    // ability exp (11 x 4, always zero on the Rust server), unread mail (3 x 4)
    skip(s, 4 + 4 + 4 + 4 + 44 + 12)?;
    let bag_slots = s.read_u8()?;
    let bank_slots = s.read_i16()?;
    some(WorldEvent::CharacterState(Box::new(OwnState {
        id: c.id,
        name: boxed(c.name),
        race: c.race,
        gender: c.gender,
        level: c.level,
        hp: c.hp,
        mp: c.mp,
        zone_id: c.zone,
        faction_id: c.faction,
        expedition_id: c.expedition,
        family_id: c.family,
        abilities: c.abilities,
        pos: c.pos,
        yaw,
        instance_id,
        exp,
        recoverable_exp,
        labor: c.labor,
        money: c.money,
        bank_money: c.bank_money,
        loyalty: c.loyalty,
        bag_slots,
        bank_slots,
        equipment: equipment.slots.into_boxed_slice(),
        equipment_gems: equipment.gems.into_boxed_slice(),
    })))
}

fn read_prepare_leave_world(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let time_ms = s.read_i32()?;
    let target = s.read_u8()?;
    let idle_kick = s.read_bool()?;
    some(WorldEvent::PrepareLeaveWorld {
        time_ms,
        target,
        idle_kick,
    })
}

fn read_leave_world_granted(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    some(WorldEvent::LeaveWorldGranted {
        target: s.read_u8()?,
    })
}

fn read_load_instance(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let instance_id = s.read_u32()?;
    let zone_id = s.read_u32()?;
    let pos = (s.read_f32()?, s.read_f32()?, s.read_f32()?);
    let ang_x = s.read_f32()?;
    let ang_y = s.read_f32()?;
    let yaw = s.read_f32()?;
    some(WorldEvent::LoadInstance {
        instance_id,
        zone_id,
        pos,
        yaw,
        ang_x,
        ang_y,
    })
}

fn read_kicked(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let reason = s.read_u8()?;
    let msg = boxed(s.read_string()?);
    some(WorldEvent::Kicked { reason, msg })
}

// ---- unit readers --------------------------------------------------------------------------

/// How `Inventory_Equip` (world/unit_state.rs:330) lays out the 28 equipment slots.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EquipLayout {
    /// Characters, houses, mates: a full item or `0`, appearance parts as template only.
    Full,
    /// Slaves: a full item or `0` in every slot, including the appearance range.
    Slave,
    /// NPCs: `template, id, grade` or `0`; appearance parts template only; cosplay (27) a full item.
    Npc,
    /// Transfers and shipyards: only the seven appearance templates.
    BodyOnly,
}

fn equip_layout(unit_type: u8) -> EquipLayout {
    match unit_type {
        unit_type::NPC => EquipLayout::Npc,
        unit_type::SLAVE => EquipLayout::Slave,
        unit_type::TRANSFER | unit_type::SHIPYARD => EquipLayout::BodyOnly,
        _ => EquipLayout::Full,
    }
}

/// Reads (and mostly skips) the equipment block of a unit; returns the template in the backpack
/// slot ([`BACKPACK_SLOT`]), 0 when it is empty or the layout has no such slot.
fn read_unit_equipment(s: &mut PacketStream, layout: EquipLayout) -> Res<u32> {
    let mut backpack = 0;
    for slot in 0..EQUIPMENT_SLOTS {
        let body_part = BODY_PART_SLOTS.contains(&slot);
        if body_part && layout != EquipLayout::Slave {
            skip(s, 4)?;
            continue;
        }
        if layout == EquipLayout::BodyOnly {
            if slot == 27 {
                break; // the writer returns before the cosplay slot
            }
            continue;
        }
        let template = s.read_u32()?;
        if template == 0 {
            continue;
        }
        if slot == BACKPACK_SLOT && layout == EquipLayout::Full {
            backpack = template;
        }
        if layout == EquipLayout::Npc && slot != 27 {
            skip(s, 8 + 1)?; // id, grade
        } else {
            read_item_body(s, template)?;
        }
    }
    Ok(backpack)
}

/// `SCUnitStatePacket` (world/unit_state.rs:108 characters, :413 other kinds).
fn read_unit_state(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let obj = s.read_bc()?;
    let name = s.read_string()?;
    let ty = s.read_u8()?;
    let mut tl_id = 0;
    let mut owner_id = 0;
    let id = match ty {
        unit_type::CHARACTER => {
            let id = s.read_u32()?;
            skip(s, 8)?;
            id
        }
        unit_type::NPC => {
            skip(s, 3)?; // object id again
            let template = s.read_u32()?;
            owner_id = s.read_u32()?;
            skip(s, 1)?; // client driven
            template
        }
        unit_type::SLAVE => {
            skip(s, 4)?; // slave db id
            tl_id = s.read_u16()?;
            let template = s.read_u32()?;
            owner_id = s.read_u32()?; // summoner character id
            template
        }
        unit_type::HOUSING => {
            tl_id = s.read_u16()?;
            let template = s.read_u32()?;
            skip(s, 2)?; // build step
            template
        }
        unit_type::TRANSFER => {
            tl_id = s.read_u16()?;
            s.read_u32()?
        }
        unit_type::MATE => {
            tl_id = s.read_u16()?;
            let template = s.read_u32()?;
            owner_id = s.read_u32()?;
            template
        }
        unit_type::SHIPYARD => {
            skip(s, 8)?;
            s.read_u32()?
        }
        // Gimmicks and invalid units are never announced with this packet.
        _ => return Ok(None),
    };
    let owner_name = boxed(s.read_string()?);
    let pos = s.read_position()?;
    skip(s, 4)?; // scale
    let level = s.read_u8()?;
    skip(s, 4)?; // model id
    let backpack = read_unit_equipment(s, equip_layout(ty))?;
    skip_model_params(s)?;
    skip(s, 3)?; // bc(0)
    let hp = s.read_i32()? / POINTS_SCALE;
    let mp = s.read_i32()? / POINTS_SCALE;
    let mut unit = UnitInfo {
        obj,
        unit_type: ty,
        id,
        name: boxed(name),
        owner_name,
        owner_id,
        tl_id,
        pos,
        level,
        hp,
        mp,
        dead: hp <= 0,
        has_loot: false,
        faction_id: 0,
        expedition_id: 0,
        yaw: 0,
        skills: Box::default(),
        buffs: Box::default(),
        backpack: if ty == unit_type::CHARACTER {
            backpack
        } else {
            0
        },
        complete: false,
    };
    // The tail is best effort: a character packet the server truncated, or a layout drift, keeps
    // what was read so far instead of losing the unit.
    unit.complete = read_unit_tail(s, &mut unit).is_ok() && s.remaining() == 0;
    some(WorldEvent::UnitState(Box::new(unit)))
}

/// Everything after hp/mp of `SCUnitStatePacket`: attach points, posture, skills, yaw, factions,
/// flags, the character-only ability/visual block and the three buff lists.
fn read_unit_tail(s: &mut PacketStream, unit: &mut UnitInfo) -> Res<()> {
    let ty = unit.unit_type;
    // AttachPoint1
    match ty {
        unit_type::SLAVE => {
            if s.read_i8()? > -1 {
                skip(s, 3)?; // owner object
            }
        }
        unit_type::TRANSFER => {
            if s.read_u8()? != 255 {
                skip(s, 3)?; // bonding object
            }
        }
        _ => skip(s, 1)?,
    }
    // AttachPoint2: -1 (one byte), a bond doodad (characters) or a bonding object (slaves).
    if s.read_u8()? != 0xFF {
        match ty {
            unit_type::CHARACTER => skip(s, 12)?, // bc owner, kind, space, spot
            unit_type::SLAVE => skip(s, 2)?,      // rest of the bc object id
            _ => {}
        }
    }
    // Unit.ModelPosture (world/unit_state.rs:375)
    let posture = s.read_u8()?;
    unit.has_loot = s.read_bool()?;
    if ty != unit_type::CHARACTER {
        match posture {
            1 => skip(s, 8)?,  // house doors and windows
            4 => skip(s, 5)?,  // actor model state: anim id + flag
            7 => skip(s, 14)?, // farm field state
            8 => skip(s, 8)?,  // turret pitch and yaw
            _ => {}
        }
    }
    skip(s, 1)?; // active weapon
    let skill_count = usize::from(s.read_u8()?);
    check_count(s, skill_count, 5, "skill")?;
    let mut skills = Vec::with_capacity(skill_count);
    for _ in 0..skill_count {
        skills.push((s.read_u32()?, s.read_u8()?));
    }
    unit.skills = skills.into_boxed_slice();
    let passive = usize::try_from(s.read_i32()?).unwrap_or(0);
    check_count(s, passive, 4, "passive buff")?;
    skip(s, passive * 4)?;
    skip(s, 2)?; // roll, pitch
    unit.yaw = s.read_i8()?;
    skip(s, 1)?; // race/gender
    s.read_pisc_w(4)?; // appellations
    let factions = s.read_pisc_w(4)?;
    unit.faction_id = factions[0] as u32;
    unit.expedition_id = factions[1] as u32;
    s.read_pisc_w(2)?; // honor, hostile kills
    skip(s, 2)?; // flags
    if ty == unit_type::CHARACTER {
        skip(s, 10 * 5)?; // abilities: exp + order
        let active = usize::from(s.read_u8()?);
        skip(s, active)?;
        skip(s, 3)?; // bc(0)
        skip(s, 6 + 4)?; // visual option stp + four flags
        skip(s, 4 + 6 * 4)?; // premium, six pStat
    }
    let mut buffs = Vec::new();
    for list in 0..3u8 {
        let count = usize::from(s.read_u8()?);
        for _ in 0..count {
            buffs.push(read_net_buff(s, list)?);
        }
    }
    unit.buffs = buffs.into_boxed_slice();
    Ok(())
}

/// `SkillCaster.Write` (systems/skills/types.rs:105): returns the caster's object id.
fn read_skill_caster(s: &mut PacketStream) -> Res<u32> {
    let kind = s.read_u8()?;
    let obj = s.read_bc()?;
    match kind {
        0 | 1 | 4 => {}
        2 => skip(s, 8 + 4 + 1 + 4)?, // item id, template, type1, type2
        3 => skip(s, 4)?,             // mount skill template
        other => return invalid(format!("SkillCasterType {other} out of range")),
    }
    Ok(obj)
}

/// `Buff.write_net` (models/buff.rs:146), one entry of the unit-state buff lists.
fn read_net_buff(s: &mut PacketStream, list: u8) -> Res<BuffInfo> {
    let index = s.read_u32()?;
    let buff_id = s.read_u32()?;
    read_skill_caster(s)?;
    skip(s, 4 + 1 + 2)?; // caster id, caster level, ability level
    let duration_ms = s.read_i32()?;
    skip(s, 4 + 4 + 4 + 4)?; // elapsed, tick, tick index, stack
    let charge = s.read_i32()?;
    skip(s, 4)?; // cooldown skill
    Ok(BuffInfo {
        list,
        index,
        buff_id,
        duration_ms,
        charge,
    })
}

fn read_units_removed(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let count = usize::from(s.read_u16()?);
    check_count(s, count, 3, "unit")?;
    let mut objs = SmallVec::new();
    for _ in 0..count {
        objs.push(s.read_bc()?);
    }
    some(WorldEvent::UnitsRemoved { objs })
}

fn read_unit_moved(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    // bc object id, move type byte, then the common move header (models/movement.rs:283).
    let obj = s.read_bc()?;
    let kind = s.read_u8()?;
    if !matches!(kind, 1 | 2 | 3 | 5) {
        return Ok(None);
    }
    skip(s, 4)?; // time
    let flags = s.read_u8()?;
    if flags & 0x10 != 0 {
        skip(s, 4 + 1)?; // sc type, phase
    }
    let pos = s.read_position()?;
    let (mut yaw, mut stance) = (0, 0);
    if kind == 1 {
        // UnitMove (models/movement.rs:417): velocity 3 x i16, rotation 3 x i8, delta movement
        // 3 x i8, stance, alertness.
        skip(s, 6 + 2)?;
        yaw = s.read_i8()?;
        skip(s, 3)?;
        stance = s.read_u8()?;
    }
    some(WorldEvent::UnitMoved {
        obj,
        kind,
        pos,
        yaw,
        stance,
    })
}

fn read_teleport_unit(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let reason = s.read_u8()?;
    let error = s.read_i16()?;
    let pos = s.read_position()?;
    some(WorldEvent::TeleportUnit { reason, error, pos })
}

fn read_unit_points(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let obj = s.read_bc()?;
    let hp = s.read_i32()? / POINTS_SCALE;
    let mp = s.read_i32()? / POINTS_SCALE;
    some(WorldEvent::UnitPoints { obj, hp, mp })
}

/// `SCUnitDamagedPacket` (systems/skills/packets.rs:334-389): `CastAction`, `SkillCaster`,
/// 3-byte caster and target object ids, crime state, two PISC groups and the hit tail. The bot
/// only needs who hit whom; the rest is skipped with the widths of the server writer.
fn read_unit_damaged(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    match s.read_u8()? {
        0 => skip(s, 6)?,  // CastSkill: skill id, tl id
        1 => skip(s, 14)?, // CastPlot
        2 => skip(s, 13)?, // CastBuff
        3 => skip(s, 11)?, // CastUnk2
        4 => skip(s, 7)?,  // CastUnk3
        other => return invalid(format!("CastAction {other} out of range")),
    }
    match s.read_u8()? {
        0 | 1 | 4 => skip(s, 3)?, // unit, unk1, doodad object id
        2 => {
            // item: object id then item id, template, type1, type2
            skip(s, 3 + 8 + 4 + 1 + 4)?;
        }
        3 => skip(s, 3 + 4)?, // mount: object id, mount skill template
        other => return invalid(format!("SkillCasterType {other} out of range")),
    }
    let caster = s.read_bc()?;
    let target = s.read_bc()?;
    skip(s, 1 + 3 + 3 + 5)?; // crime state, damage/absorbed/mana PISC groups, hit tail
    some(WorldEvent::UnitDamaged { caster, target })
}

fn read_unit_death(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let obj = s.read_bc()?;
    let reason = s.read_u8()?;
    let respawn_wait_ms = s.read_i32()?;
    let lost_exp = s.read_i32()?;
    skip(s, 1)?; // durability loss rate
    let killer = s.read_bc()?;
    some(WorldEvent::UnitDeath {
        obj,
        killer,
        respawn_wait_ms,
        lost_exp,
        reason,
    })
}

fn read_resurrected(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let obj = s.read_bc()?;
    let pos = s.read_position()?;
    let yaw = s.read_f32()?;
    some(WorldEvent::Resurrected { obj, pos, yaw })
}

fn read_target_changed(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let obj = s.read_bc()?;
    let target = s.read_bc()?;
    some(WorldEvent::TargetChanged { obj, target })
}

fn read_exp_changed(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let obj = s.read_bc()?;
    let delta = s.read_i32()?;
    some(WorldEvent::ExpChanged { obj, delta })
}

fn read_level_changed(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let obj = s.read_bc()?;
    let level = s.read_u8()?;
    some(WorldEvent::LevelChanged { obj, level })
}

fn read_faction_changed(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let obj = s.read_bc()?;
    skip_sized(s)?; // unit name
    let old = s.read_u32()?;
    let faction = s.read_u32()?;
    let temp = s.read_bool()?;
    some(WorldEvent::FactionChanged {
        obj,
        old,
        faction,
        temp,
    })
}

/// The values of the state byte of `SCConflictZoneStatePacket` (`ZoneConflictType`,
/// data/world_data.rs:189). A zone starts at [`TENSION`](conflict_state::TENSION), climbs with
/// the kills of the two factions through [`CONFLICT`](conflict_state::CONFLICT) to
/// [`WAR`](conflict_state::WAR) and ends in [`PEACE`](conflict_state::PEACE).
pub mod conflict_state {
    /// Calm.
    pub const TENSION: u8 = 0;
    /// The first kills.
    pub const DANGER: u8 = 1;
    /// More kills.
    pub const DISPUTE: u8 = 2;
    /// More still.
    pub const UNREST: u8 = 3;
    /// The last step before the fighting.
    pub const CRISIS: u8 = 4;
    /// The factions fight in the zone.
    pub const CONFLICT: u8 = 5;
    /// The zone is at war (the tower is up).
    pub const WAR: u8 = 6;
    /// The loser of a war is protected for a while.
    pub const PEACE: u8 = 7;

    /// Whether the zone is fought over: [`CONFLICT`] or [`WAR`].
    pub fn at_war(state: u8) -> bool {
        state == CONFLICT || state == WAR
    }

    /// The name of `state` for logs; `"unknown"` for a value the server does not use.
    pub fn name(state: u8) -> &'static str {
        match state {
            TENSION => "tension",
            DANGER => "danger",
            DISPUTE => "dispute",
            UNREST => "unrest",
            CRISIS => "crisis",
            CONFLICT => "conflict",
            WAR => "war",
            PEACE => "peace",
            _ => "unknown",
        }
    }
}

/// `SCConflictZoneStatePacket` (packets/world.rs:346): `u16 zone group, u8 state, i64 end time`.
fn read_conflict_zone_state(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let zone_group = s.read_u16()?;
    let state = s.read_u8()?;
    let end_unix_s = s.read_i64()?;
    some(WorldEvent::ConflictZoneState {
        zone_group,
        state,
        end_unix_s,
    })
}

/// `SCUnitEquipmentsChangedPacket` (systems/economy/item_packets.rs:64): `bc obj, u8 count`, then
/// per slot `u8 slot` and an item (`Item.Write`, or the 4-byte empty form).
fn read_unit_equipments_changed(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let obj = s.read_bc()?;
    let count = usize::from(s.read_u8()?);
    // (a slot is at least 5 bytes: the slot and the empty item)
    check_count(s, count, 5, "equipment slot")?;
    let mut slots = SmallVec::new();
    for _ in 0..count {
        let slot = s.read_u8()?;
        let template = read_item(s)?.map_or(0, |i| i.template);
        slots.push((slot, template));
    }
    some(WorldEvent::UnitEquipmentsChanged { obj, slots })
}

/// The `kind` values of `SCUnitPvPPointsChangedPacket` (systems/skills/combat.rs:749).
pub mod pvp_points {
    /// The honor gained in combat.
    pub const HONOR: u8 = 0;
    /// The kills of hostile-faction players.
    pub const HOSTILE_KILLS: u8 = 1;
}

/// `SCUnitPvPPointsChangedPacket` (packets/g2c_misc.rs:104): `bc obj, u8 kind, i32 points`.
fn read_pvp_points(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let obj = s.read_bc()?;
    let kind = s.read_u8()?;
    let points = s.read_i32()?;
    some(WorldEvent::PvpPoints { obj, kind, points })
}

// ---- doodad readers ------------------------------------------------------------------------

/// `Doodad.Write` (models/doodad.rs:247), a fixed 91-byte record.
const DOODAD_RECORD: usize = 91;

fn read_doodad(s: &mut PacketStream) -> Res<DoodadInfo> {
    let obj = s.read_bc()?;
    let template = s.read_u32()?;
    let owner_obj = s.read_bc()?;
    let parent_obj = s.read_bc()?;
    skip(s, 1)?; // attach point
    let pos = s.read_position()?;
    skip(s, 6 + 4)?; // roll, pitch, yaw, scale
    let has_loot = s.read_bool()?;
    let phase = s.read_u32()?;
    let owner_id = s.read_u32()?;
    skip(s, 8 + 4 + 4 + 4 + 8)?; // ucc id, item template, type2, time left, plant time
    let quest_glow = s.read_u32()?;
    skip(s, 4 + 4 + 1 + 4 + 4)?; // family, puzzle group, owner type, owner db id, data
    Ok(DoodadInfo {
        obj,
        template,
        pos,
        owner_obj,
        parent_obj,
        phase,
        owner_id,
        quest_glow,
        has_loot,
    })
}

fn read_doodad_created(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let mut doodads = SmallVec::new();
    doodads.push(read_doodad(s)?);
    some(WorldEvent::DoodadsCreated(doodads))
}

fn read_doodads_created(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let count = usize::from(s.read_u8()?);
    check_count(s, count, DOODAD_RECORD, "doodad")?;
    let mut doodads = SmallVec::new();
    for _ in 0..count {
        doodads.push(read_doodad(s)?);
    }
    some(WorldEvent::DoodadsCreated(doodads))
}

fn read_doodad_removed(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let mut objs = SmallVec::new();
    objs.push(s.read_bc()?);
    some(WorldEvent::DoodadsRemoved(objs))
}

fn read_doodads_removed(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let count = usize::from(s.read_u16()?);
    skip(s, 1)?; // last
    check_count(s, count, 4, "doodad")?;
    let mut objs = SmallVec::new();
    for _ in 0..count {
        objs.push(s.read_bc()?);
        skip(s, 1)?;
    }
    some(WorldEvent::DoodadsRemoved(objs))
}

fn read_doodad_phase(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let obj = s.read_bc()?;
    let phase = s.read_u32()?;
    some(WorldEvent::DoodadPhase { obj, phase })
}

/// One entry of `SCResponseCommonFarmListPacket`: four `u32`, a position (9 bytes), an `i64`.
const FARM_ENTRY: usize = 4 * 4 + 9 + 8;

fn read_common_farm_list(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    skip(s, 4)?; // the count is written twice
    let count = usize::try_from(s.read_i32()?).unwrap_or(0);
    check_count(s, count, FARM_ENTRY, "common farm entry")?;
    let mut list = Vec::with_capacity(count);
    for _ in 0..count {
        let farm_type = s.read_u32()?;
        let template = s.read_u32()?;
        let time_left = s.read_u32()?;
        let phase = s.read_u32()?;
        let pos = s.read_position()?;
        let plant_time = s.read_i64()?;
        list.push(CommonFarmEntry {
            farm_type,
            template,
            time_left,
            phase,
            pos,
            plant_time,
        });
    }
    some(WorldEvent::CommonFarmList(list.into_boxed_slice()))
}

// ---- own data readers ----------------------------------------------------------------------

fn read_inventory_init(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let bag_slots = s.read_u32()?;
    let bank_slots = s.read_u32()?;
    some(WorldEvent::InventoryInit {
        bag_slots,
        bank_slots,
    })
}

/// Items per `SCCharacterInvenContentsPacket` chunk (world/session.rs:386).
const ITEMS_PER_CHUNK: u16 = 10;

fn read_inventory(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let slot_type = s.read_u8()?;
    skip(s, 1)?; // number of chunks
    let chunk = s.read_u8()?;
    let first_slot = u16::from(chunk) * ITEMS_PER_CHUNK;
    let mut items = Vec::new();
    // No item count on the wire: the packet holds items until its end.
    let mut slot = first_slot;
    while s.remaining() > 0 {
        if let Some(item) = read_item(s)? {
            items.push(InvItem {
                slot,
                id: item.id,
                template: item.template,
                grade: item.grade,
                count: item.count,
            });
        }
        slot = slot.saturating_add(1);
    }
    some(WorldEvent::Inventory {
        slot_type,
        chunk,
        items: items.into_boxed_slice(),
    })
}

/// Reads the `(short)128`-prefixed detail block of an item task and skips it.
fn skip_task_details(s: &mut PacketStream) -> Res<()> {
    let len = usize::try_from(s.read_i16()?).unwrap_or(0);
    skip(s, len)
}

/// Reads the `(short)128`-prefixed detail block of an item task (`write_padded_details`,
/// models/item_task.rs:374: the detail type, the details, zero padding): the gems of an equipment
/// detail, `None` for other details.
fn read_task_detail_gems(s: &mut PacketStream) -> Res<Option<u8>> {
    let len = usize::try_from(s.read_i16()?).unwrap_or(0);
    let at = s.pos();
    skip(s, len)?;
    Ok(match s.as_bytes()[at..at + len].split_first() {
        Some((&DETAIL_EQUIPMENT, body)) => Some(gems_in(body)),
        _ => None,
    })
}

fn read_item_change(s: &mut PacketStream) -> Res<ItemChange> {
    let action = s.read_u8()?;
    Ok(match action {
        1 => ItemChange::Money(s.read_i32()?),
        2 => ItemChange::BankMoney(s.read_i32()?),
        3 => ItemChange::GamePoint {
            kind: s.read_u8()?,
            amount: s.read_i32()?,
        },
        16 => ItemChange::AaPoint(s.read_i32()?),
        17 => ItemChange::BankAaPoint(s.read_i32()?),
        18 => ItemChange::AutoUseAaPoint(s.read_u8()?),
        5 | 6 | 15 => {
            // slot type, slot, then `write_task_item_details` (models/item_task.rs:358)
            let slot_type = s.read_u8()?;
            let slot = s.read_u8()?;
            let template = s.read_u32()?;
            let id = s.read_u64()?;
            let grade = s.read_u8()?;
            skip(s, 1)?; // flags
            let count = s.read_i32()?;
            skip_task_details(s)?;
            skip(s, 8 + 4 + 4 + 1 + 8 + 8)?; // create time, lifespan, made unit, world, times
            ItemChange::Added {
                action,
                slot_type,
                slot,
                template,
                id,
                grade,
                count,
            }
        }
        4 => {
            let slot_type = s.read_u8()?;
            let slot = s.read_u8()?;
            let id = s.read_u64()?;
            let delta = s.read_i32()?;
            let template = s.read_u32()?;
            ItemChange::CountUpdate {
                slot_type,
                slot,
                id,
                delta,
                template,
            }
        }
        7 => {
            let slot_type = s.read_u8()?;
            let slot = s.read_u8()?;
            let id = s.read_u64()?;
            let template = s.read_u32()?;
            ItemChange::Removed {
                slot_type,
                slot,
                id,
                template,
            }
        }
        8 => {
            let from_slot_type = s.read_u8()?;
            let from_slot = s.read_u8()?;
            let to_slot_type = s.read_u8()?;
            let to_slot = s.read_u8()?;
            let from_id = s.read_u64()?;
            let to_id = s.read_u64()?;
            skip(s, 4)?; // flags
            ItemChange::Moved {
                from_slot_type,
                from_slot,
                to_slot_type,
                to_slot,
                from_id,
                to_id,
            }
        }
        14 => {
            let slot_type = s.read_u8()?;
            let slot = s.read_u8()?;
            let id = s.read_u64()?;
            let grade = s.read_u8()?;
            ItemChange::GradeChange {
                slot_type,
                slot,
                id,
                grade,
            }
        }
        9 => {
            let slot_type = s.read_u8()?;
            let slot = s.read_u8()?;
            let id = s.read_u64()?;
            let gems = read_task_detail_gems(s)?;
            ItemChange::Updated {
                slot_type,
                slot,
                id,
                gems,
            }
        }
        10 => {
            let slot_type = s.read_u8()?;
            let slot = s.read_u8()?;
            let id = s.read_u64()?;
            let bits = s.read_u8()?;
            skip(s, 8)?;
            ItemChange::FlagsUpdated {
                slot_type,
                slot,
                id,
                bits,
            }
        }
        11 => {
            let slot_type = s.read_u8()?;
            let slot = s.read_u8()?;
            let id = s.read_u64()?;
            let bits = s.read_u8()?;
            skip(s, 3 + 8 + 8)?; // three flags, unsecure and unpack time
            ItemChange::FlagsUpdated {
                slot_type,
                slot,
                id,
                bits,
            }
        }
        12 => ItemChange::RemovedCrafting(s.read_u64()?),
        13 => {
            let slot_type = s.read_u8()?;
            let slot = s.read_u8()?;
            let id = s.read_u64()?;
            ItemChange::RemovedSlot {
                slot_type,
                slot,
                id,
            }
        }
        other => return invalid(format!("unknown item task action {other}")),
    })
}

fn read_lunagem_result(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let result = s.read_u8()?;
    let item_id = s.read_u64()?;
    let gem = s.read_u32()?;
    let installed = s.read_bool()?;
    some(WorldEvent::LunagemResult {
        result,
        item_id,
        gem,
        installed,
    })
}

fn read_grade_enchant_result(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let result = s.read_u8()?;
    let item = read_item(s)?;
    let before = s.read_u8()?;
    let after = s.read_u8()?;
    let (item_id, template) = item.map_or((0, 0), |i| (i.id, i.template));
    some(WorldEvent::GradeEnchantResult {
        result,
        item_id,
        template,
        before,
        after,
    })
}

fn read_item_task(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    some(WorldEvent::ItemTask(Box::new(read_item_task_info(s)?)))
}

/// The item tasks of an `SCItemTaskSuccessPacket` body; `SCTradeMadePacket` starts with the same
/// layout (its `action` byte is the task `reason`).
pub(crate) fn read_item_task_info(s: &mut PacketStream) -> Res<ItemTaskInfo> {
    let reason = s.read_u8()?;
    let count = usize::from(s.read_u8()?);
    check_count(s, count, 1, "item task")?;
    let mut changes = SmallVec::new();
    for _ in 0..count {
        changes.push(read_item_change(s)?);
    }
    let removed = usize::from(s.read_u8()?);
    check_count(s, removed, 8, "forced removal")?;
    let mut force_removed = SmallVec::new();
    for _ in 0..removed {
        force_removed.push(s.read_u64()?);
    }
    Ok(ItemTaskInfo {
        reason,
        changes,
        force_removed,
    })
}

fn read_labor_changed(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let delta = s.read_i32()?;
    let actability = s.read_i32()?;
    let point = s.read_i32()?;
    let step = s.read_u8()?;
    some(WorldEvent::LaborChanged {
        delta,
        actability,
        point,
        step,
    })
}

fn read_skill_learned(s: &mut PacketStream, upgrade: bool) -> Res<Option<WorldEvent>> {
    let skill = s.read_u32()?;
    let level = s.read_u8()?;
    some(WorldEvent::SkillLearned {
        skill,
        level,
        upgrade,
    })
}

fn read_cooldown_list(s: &mut PacketStream) -> Res<Box<[Cooldown]>> {
    let count = usize::try_from(s.read_i32()?).unwrap_or(0);
    check_count(s, count, 12, "cooldown")?;
    let mut list = Vec::with_capacity(count);
    for _ in 0..count {
        list.push(Cooldown {
            id: s.read_u32()?,
            total_ms: s.read_u32()?,
            remaining_ms: s.read_u32()?,
        });
    }
    Ok(list.into_boxed_slice())
}

fn read_cooldowns(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let skills = read_cooldown_list(s)?;
    let tags = read_cooldown_list(s)?;
    some(WorldEvent::Cooldowns { skills, tags })
}

fn read_cooldown_reset(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let obj = s.read_bc()?;
    let skill = s.read_u32()?;
    let tag = s.read_u32()?;
    let gcd = s.read_bool()?;
    some(WorldEvent::CooldownReset {
        obj,
        skill,
        tag,
        gcd,
    })
}

/// `SkillCastTarget.Write` (systems/skills/types.rs:277): returns the target object for unit and
/// doodad targets.
fn read_cast_target(s: &mut PacketStream) -> Res<u32> {
    let kind = s.read_u8()?;
    Ok(match kind {
        0 | 4 => s.read_bc()?,
        1 => {
            skip(s, 8 + 8 + 4 + 4 + 3 + 3)?;
            0
        }
        2 => {
            skip(s, 8 + 8 + 4 + 6 * 4)?;
            0
        }
        3 => {
            skip(s, 3 + 8 + 4 + 1)?;
            0
        }
        5 => {
            skip(s, 8 + 8 + 4 + 4)?;
            0
        }
        other => return invalid(format!("SkillCastTargetType {other} out of range")),
    })
}

/// `SkillObject.Write` (systems/skills/types.rs:456); an unknown flag has no body.
fn skip_skill_object(s: &mut PacketStream) -> Res<()> {
    match s.read_u8()? {
        1 => skip(s, 1 + 4 + 8 + 8 + 4),
        2 => {
            skip(s, 4)?;
            skip_sized(s)
        }
        3 | 6 => skip_sized(s),
        4 => skip(s, 8 + 8 + 4),
        5 => skip(s, 4),
        7 => skip(s, 4 + 8 + 1),
        _ => Ok(()),
    }
}

fn read_skill_started(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let skill = s.read_u32()?;
    let tl = s.read_u16()?;
    let caster_obj = read_skill_caster(s)?;
    let target_obj = read_cast_target(s)?;
    skip_skill_object(s)?;
    skip(s, 2 + 2 + 1)?; // cast times, cast synergy
    let extra_flags = s.read_u8()?;
    // `SetSkillResult` sets bit 1 and the byte only for a non-zero result.
    let result = if extra_flags & 1 != 0 {
        s.read_u8()?
    } else {
        0
    };
    some(WorldEvent::SkillStarted {
        caster_obj,
        skill,
        tl,
        target_obj,
        result,
    })
}

fn read_skill_stopped(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let obj = s.read_bc()?;
    let skill = s.read_u32()?;
    some(WorldEvent::SkillStopped { obj, skill })
}

fn read_casting_stopped(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let tl = s.read_u16()?;
    let duration_ms = s.read_u32()?;
    some(WorldEvent::CastingStopped { tl, duration_ms })
}

fn read_buff_created(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let caster_obj = read_skill_caster(s)?;
    skip(s, 4)?; // caster id
    let obj = s.read_bc()?;
    let index = s.read_u32()?;
    let buff_id = s.read_u32()?;
    let caster_level = s.read_u8()?;
    skip(s, 2)?; // ability level
    let skill_id = s.read_u32()?;
    // Buff.WriteData: pisc(charge, duration / 10, 0, tick / 10)
    let data = s.read_pisc_w(4)?;
    some(WorldEvent::BuffCreated {
        obj,
        index,
        buff_id,
        caster_obj,
        caster_level,
        skill_id,
        charge: data[0] as i32,
        duration_ms: (data[1] as i32).saturating_mul(10),
    })
}

fn read_buff_removed(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let obj = s.read_bc()?;
    let index = s.read_u32()?;
    some(WorldEvent::BuffRemoved { obj, index })
}

// ---- chat / error / loot readers -----------------------------------------------------------

fn read_chat(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let kind = s.read_i16()?;
    skip(s, 2 + 4)?; // sub type, channel faction
    let sender_obj = s.read_bc()?;
    let sender_id = s.read_u32()?;
    skip(s, 1 + 1 + 4)?; // language, race, sender faction
    let name = boxed(s.read_string()?);
    let msg = boxed(s.read_string()?);
    some(WorldEvent::Chat {
        kind,
        sender_obj,
        sender_id,
        name,
        msg,
    })
}

fn read_error_msg(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let err = s.read_i16()?;
    let err2 = s.read_i16()?;
    let kind = s.read_u32()?;
    let notify = s.read_bool()?;
    some(WorldEvent::ErrorMsg {
        err,
        err2,
        kind,
        notify,
    })
}

fn read_lootable_state(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    skip(s, 2)?; // unused item index
    let owner_type = s.read_u16()?;
    let owner_obj = s.read_bc()?;
    skip(s, 1)?;
    let has_loot = s.read_bool()?;
    some(WorldEvent::LootableState {
        owner_obj,
        owner_type,
        has_loot,
    })
}

fn read_loot_bag(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let count = usize::from(s.read_u8()?);
    check_count(s, count, 4, "loot item")?;
    let mut items = SmallVec::new();
    let (mut owner_obj, mut owner_type) = (0, 0);
    for i in 0..count {
        let Some(item) = read_item(s)? else { continue };
        if i == 0 {
            // `(ObjId << 32) + (LootOwnerType << 16) + ItemIndex` (economy/looting.rs:239)
            owner_obj = (item.id >> 32) as u32 & 0x00FF_FFFF;
            owner_type = (item.id >> 16) as u16;
        }
        items.push(LootItem {
            index: item.id as u16,
            template: item.template,
            count: item.count,
        });
    }
    let loot_all = s.read_bool()?;
    some(WorldEvent::LootBag {
        owner_obj,
        owner_type,
        items,
        loot_all,
    })
}

fn read_loot_took(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let template = s.read_u32()?;
    let index = s.read_u16()?;
    let owner_type = s.read_u16()?;
    let owner_obj = s.read_bc()?;
    skip(s, 1)?;
    let count = s.read_i32()?;
    some(WorldEvent::LootTook {
        template,
        index,
        count,
        owner_type,
        owner_obj,
    })
}

fn read_loot_failed(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let err = s.read_i16()?;
    let index = s.read_u16()?;
    let owner_type = s.read_u16()?;
    let owner_obj = s.read_bc()?;
    skip(s, 1)?;
    let template = s.read_u32()?;
    some(WorldEvent::LootFailed {
        err,
        index,
        owner_type,
        owner_obj,
        template,
    })
}
