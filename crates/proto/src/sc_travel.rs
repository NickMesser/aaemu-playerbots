// Packet layouts ported from the S->C writers of AAEmu-1.2-Rust rust/crates/aaemu-game/src
// (systems/npcs/portal, systems/instances, systems/vehicles, models/movement.rs), GPL-3.0-or-later.
//! Server → client parsers for travel: the teleport book and recall points, instance dungeons and
//! boats (see [`crate::sc_world::WorldEvent`], whose variants carry the payload types below).
//!
//! The readers return [`WorldEvent`] values and are wired into the dispatcher of
//! [`crate::sc_world`]; this module only holds the payload types, the small wire constants bots
//! need and the readers. Writer citations are `file:line` below `rust/crates/aaemu-game/src/` of
//! AAEmu-1.2-Rust, abbreviated `G/`.
//!
//! # Ship movement
//!
//! A ship's `SCOneUnitMovementPacket` (move type 3) keeps parsing to
//! [`WorldEvent::UnitMoved`] with `kind == 3` so existing position tracking is unchanged;
//! [`crate::sc::parse_ship`] decodes the same packet into [`WorldEvent::ShipMoved`] with the
//! heading, velocity, throttle and steering. A bot that follows ships calls both on the packet.
//!
//! Not parsed: `SCUnitMovementsPacket` (0x06b). The server only ever writes it in its own unit
//! tests (G/models/movement.rs:726), and it omits the per-entry move type byte (nothing
//! identifies a ship in it), so ships cannot be told apart in a batch.

use crate::pos::convert_long_to_coord;
use crate::sc_world::{boxed, check_count, invalid, skip, some, Pos, Res, WorldEvent};
use crate::stream::PacketStream;

/// `AttachPointKind` values bots use (G/systems/vehicles/data.rs:19).
pub mod attach_point {
    /// Not attached.
    pub const NONE: u8 = 0;
    /// The driver's seat (`CSBindSlave` always asks for it).
    pub const DRIVER: u8 = 1;
    /// The first passenger seat (skill 13863 "Take the Passenger Seat").
    pub const PASSENGER0: u8 = 2;
    /// The second passenger seat.
    pub const PASSENGER1: u8 = 3;
}

/// `AttachUnitReason` values the server sends with `SCUnitAttached` / `SCUnitDetached`
/// (G/systems/vehicles/slaves.rs:53).
pub mod attach_reason {
    /// No reason (the slave went away while the player was seated, slaves.rs:4743).
    pub const NONE: u8 = 0;
    /// The reason of an unbind answering `CSDiscardSlave` (G/systems/vehicles/packets.rs:344).
    pub const SLAVE_BINDING: u8 = 5;
    /// The reason of a bind answering `CSBindSlave` (G/systems/vehicles/slaves.rs:2039).
    pub const NEW_MASTER: u8 = 6;
    /// Boarding a transfer.
    pub const BOARD_TRANSFER: u8 = 7;
}

/// `ErrorMessageType` ids (the `err` of [`WorldEvent::ErrorMsg`]) the travel code answers with.
pub mod error_message {
    /// A slave cannot be bound (dead, no such seat, too far, a manned cannon; G/systems/vehicles/mod.rs:271).
    pub const SLAVE_CANNOT_BIND: i16 = 96;
    /// The seat is taken or the slave carries the owner's mark (G/systems/vehicles/mod.rs:275).
    pub const SLAVE_ALREADY_HAS_MASTER: i16 = 97;
    /// The summon search found no room for the ship (G/systems/vehicles/mod.rs:279).
    pub const SLAVE_SPAWN_SHIP_NEED_MORE_SPACE: i16 = 100;
    /// The summon item is locked (G/systems/vehicles/mod.rs:280).
    pub const SLAVE_SPAWN_ITEM_LOCKED: i16 = 101;
    /// Unbind first (G/systems/vehicles/mod.rs:283).
    pub const SLAVE_UNBIND_FIRST: i16 = 102;
    /// Too far from the gate / slave (G/systems/vehicles/mod.rs:277).
    pub const TOO_FAR_AWAY: i16 = 154;
    /// The destination zone is closed (G/systems/economy/error_message.rs:289).
    pub const CLOSED_ZONE: i16 = 287;
    /// No server instance resources are free (G/systems/instances/worlds.rs:63).
    pub const NO_SERVER_INSTANCE_RESOURCE: i16 = 253;
    /// Try the instance again later (G/systems/instances/worlds.rs:64).
    pub const TRY_LATER_INSTANCE: i16 = 254;
    /// Prohibited inside an instance (G/systems/instances/worlds.rs:66).
    pub const PROHIBITED_IN_INSTANCE: i16 = 258;
    /// The instance entry limit was reached (G/systems/instances/worlds.rs:68).
    pub const INSTANCE_VISIT_LIMIT: i16 = 453;
    /// The character's level does not fit the instance (G/systems/instances/worlds.rs:69).
    pub const INSTANCE_LEVEL: i16 = 470;
    /// The instance quota is used up (G/systems/instances/worlds.rs:70).
    pub const INSTANCE_QUOTA: i16 = 471;
    /// Leave the party before entering alone (G/systems/instances/worlds.rs:71).
    pub const INSTANCE_LEAVE_PARTY: i16 = 472;
    /// A required entry item is missing; the item id follows in the message type
    /// (G/systems/instances/worlds.rs:73).
    pub const ENTER_INST_REQ_ITEM: i16 = 632;
    /// The instance needs a party (G/systems/instances/worlds.rs:74).
    pub const NEED_PARTY: i16 = 671;
    /// Somebody else's gate with the safe-portal flag (G/systems/npcs/portal/mod.rs:51).
    pub const NOT_MY_PORTAL: i16 = 609;
    /// A portal cannot be used with a backpack (G/systems/npcs/portal/mod.rs:52).
    pub const CANNOT_USE_PORTAL_WITH_BACKPACK: i16 = 610;
    /// A portal cannot be used while on trial (G/systems/npcs/portal/mod.rs:53).
    pub const CANNOT_USE_PORTAL_IN_TRIAL: i16 = 611;
}

/// `MoveTypeEnum::Ship` (G/models/movement.rs:36-43): the move type byte of a ship's
/// `SCOneUnitMovement`.
pub const MOVE_TYPE_SHIP: u8 = 3;

/// `MoveTypeFlags.HAS_SC_TYPE_AND_PHASE` (G/models/movement.rs:72): `u32 sc type, u8 phase`
/// follow the flags byte.
const FLAG_SC_TYPE_AND_PHASE: u8 = 0x10;

/// Metres per second per velocity short of ship and vehicle moves (`30 / 32767`,
/// G/models/movement.rs:25 `VELOCITY_MPS_PER_SHORT`).
pub const SHIP_MPS_PER_SHORT: f32 = 30.0 / 32767.0;

/// Quaternion component per rotation short: the server's decoder
/// (`get_slave_rotation_in_degrees`, G/models/movement.rs:791) scales by this constant and its
/// encoder divides by it (G/systems/ai/physics/replication.rs:186).
const QUATERNION_PER_SHORT: f32 = 0.000_030_52;

/// Zone origin cells are 1024 m wide (`PortalWire` offsets, G/systems/npcs/portal/mod.rs:330-349).
const ZONE_CELL_METRES: f32 = 1024.0;

// ---- payloads ------------------------------------------------------------------------------

/// One teleport book or recall entry (`PortalWire`, G/packets/world.rs:216; `Portal.Write`).
#[derive(Debug, Clone, PartialEq)]
pub struct PortalEntry {
    /// Book entry id; what `CSDeletePortal` and the open-gate skill object name.
    pub id: u32,
    /// Name given by the player (private entries) or the recall point's name.
    pub name: Box<str>,
    /// Zone key of the saved spot (0 = unknown).
    pub zone_id: u32,
    /// World X minus `zone origin x * 1024` (G/systems/npcs/portal/mod.rs:330-349): metres from
    /// the west edge of the zone's origin cell. See [`PortalEntry::world_xy`].
    pub off_x: f32,
    /// World Y minus `zone origin y * 1024`.
    pub off_y: f32,
    /// Height of the saved spot.
    pub z: f32,
    /// Rotation about Z as stored in the book (degrees; the server converts it with `deg_to_rad`
    /// when a gate is used, G/systems/npcs/portal/mod.rs:974).
    pub z_rot: f32,
}

impl PortalEntry {
    /// World `(x, y)` of the entry, given the zone's origin cell `(origin_x, origin_y)` (the
    /// server's `xml_world_zones()[zone_id].origin_x / origin_y`; the wire carries only the
    /// offsets, G/systems/npcs/portal/mod.rs:330-349). With `zone_id == 0` the offsets already are
    /// world coordinates (pass `(0.0, 0.0)`).
    pub fn world_xy(&self, origin_x: f32, origin_y: f32) -> (f32, f32) {
        (
            self.off_x + origin_x * ZONE_CELL_METRES,
            self.off_y + origin_y * ZONE_CELL_METRES,
        )
    }
}

/// `SCMySlavePacket` (G/systems/vehicles/packets.rs:147): the summoner's own view of a freshly
/// created or hidden-away slave.
#[derive(Debug, Clone, PartialEq)]
pub struct MySlaveInfo {
    /// Slave object id.
    pub obj: u32,
    /// Slave timeline id (`tl`); what `CSBindSlave` / `CSDiscardSlave` / `CSDestroySlave` name.
    pub tl: u16,
    /// Slave name.
    pub name: Box<str>,
    /// Slave template id.
    pub template: u32,
    /// Hit points as sent. Unlike `SCUnitState` the server writes the real value here, not the
    /// value times 100 (G/systems/vehicles/slaves.rs:4457-4468 passes `unit.hp`).
    pub hp: i32,
    /// Maximum hit points as sent (real value).
    pub max_hp: i32,
    /// World position.
    pub pos: Pos,
}

/// `SCMateSpawnedPacket` (G/systems/vehicles/packets.rs:69, writer :579-592): the owner's
/// client creates its record of a summoned mate from it. The mate's unit state follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MateInfo {
    /// Mate timeline id (`tl`); what `CSMountMate` / `CSUnMountMate` / `CSRemoveMate` name.
    pub tl: u16,
    /// Mate database id (`mates.id`).
    pub id: u32,
    /// The summon item instance id.
    pub item_id: u64,
    /// The mate's user state (1 = the server's default on spawn).
    pub user_state: u8,
    /// Mate experience.
    pub experience: i32,
    /// Mate mileage.
    pub mileage: i32,
    /// Spawn delay in milliseconds (0 on a summon).
    pub spawn_delay_ms: u32,
    /// The mount-bar skill slots: exactly ten `u32`s on the wire, zero padded (the 1.2 data has
    /// at most nine).
    pub skills: Box<[u32]>,
}

/// A ship's movement update (`SCOneUnitMovementPacket`, move type 3: `ShipMove`,
/// G/models/movement.rs:552, built by G/systems/ai/physics/replication.rs:198).
///
/// Axes are the game's: x east, y north, z up. `yaw` is in the bot convention of
/// [`crate::cs::MoveUnit`]: **0 = bow toward +Y, positive turns counter-clockwise** (so +π/2 has
/// the bow toward −X).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShipMotion {
    /// Ship (slave) object id.
    pub obj: u32,
    /// Movement timestamp: milliseconds since the ship spawned.
    pub time: u32,
    /// World position.
    pub pos: Pos,
    /// Velocity in m/s (`short * 30 / 32767`).
    pub velocity: [f32; 3],
    /// Horizontal speed in m/s.
    pub speed: f32,
    /// Heading in radians, see the type docs.
    pub yaw: f32,
    /// Angular velocity `(x, y, z)` as sent (rad/s, game axes; z turns the heading, positive
    /// z = counter-clockwise).
    pub ang_vel: [f32; 3],
    /// Applied (smoothed) steering, -127..=127. Positive turns to starboard, i.e. lowers `yaw`:
    /// the server gives a positive request the angular velocity `-rot_speed` about the hull's up
    /// axis and the bow heading is that axis' rotation (G/systems/ai/physics/ship.rs:239 `bow`,
    /// :982 test `steering_builds_a_capped_yaw_rate`). Derived from the physics code, not
    /// measured on a live ship.
    pub steering: i8,
    /// Applied (smoothed) throttle, -127..=127 (positive = ahead, ship.rs:952
    /// `throttle_accelerates_along_the_bow_and_coasts_down`).
    pub throttle: i8,
    /// Zone id the ship is in (16 bits of the zone key).
    pub zone: u16,
    /// The hull is aground (`ShipMove::stuck`; the server currently always sends `false`).
    pub stuck: bool,
}

impl ShipMotion {
    /// Speed along the bow in m/s: positive when moving ahead, negative when going astern.
    pub fn forward_speed(&self) -> f32 {
        // Heading vector of yaw (0 = +Y, counter-clockwise): (-sin yaw, cos yaw).
        -self.velocity[0] * self.yaw.sin() + self.velocity[1] * self.yaw.cos()
    }
}

// ---- readers: portals ----------------------------------------------------------------------

/// `PortalWire.write` (G/packets/world.rs:216-236): `u32 id, str name, u32 zone, f32 x, f32 y, f32 z,
/// f32 z rot`.
fn read_portal_entry(s: &mut PacketStream) -> Res<PortalEntry> {
    Ok(PortalEntry {
        id: s.read_u32()?,
        name: boxed(s.read_string()?),
        zone_id: s.read_u32()?,
        off_x: s.read_f32()?,
        off_y: s.read_f32()?,
        z: s.read_f32()?,
        z_rot: s.read_f32()?,
    })
}

/// Smallest `PortalWire` on the wire: id, empty name (length prefix), zone, four floats.
const PORTAL_ENTRY_MIN: usize = 4 + 2 + 4 + 16;

/// `i32 count` + that many entries (G/packets/world.rs:248 and :268).
fn read_portal_list(s: &mut PacketStream) -> Res<Box<[PortalEntry]>> {
    let count = s.read_i32()?;
    let Ok(count) = usize::try_from(count) else {
        return invalid(format!("portal count {count} is negative"));
    };
    check_count(s, count, PORTAL_ENTRY_MIN, "portal")?;
    let mut list = Vec::with_capacity(count);
    for _ in 0..count {
        list.push(read_portal_entry(s)?);
    }
    Ok(list.into_boxed_slice())
}

/// `SCCharacterPortalsPacket` (G/packets/world.rs:238, writer :248).
pub(crate) fn read_portal_book(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    some(WorldEvent::PortalBook(read_portal_list(s)?))
}

/// `SCCharacterReturnDistrictsPacket` (G/packets/world.rs:257, writer :268).
pub(crate) fn read_return_districts(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let portals = read_portal_list(s)?;
    let bound = s.read_u32()?;
    some(WorldEvent::ReturnDistricts { portals, bound })
}

/// `SCPortalInfoSavedPacket` (G/systems/npcs/portal/packets.rs:287, writer :297).
pub(crate) fn read_portal_saved(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    some(WorldEvent::PortalSaved(Box::new(read_portal_entry(s)?)))
}

/// `SCPortalDeletedPacket` (G/systems/npcs/portal/packets.rs:270, writer :281).
pub(crate) fn read_portal_deleted(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let kind = s.read_u8()?;
    let id = s.read_i32()?;
    some(WorldEvent::PortalDeleted { kind, id })
}

/// `SCUnitPortalUsedPacket` (G/systems/npcs/portal/packets.rs:303, writer :313).
pub(crate) fn read_portal_used(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    some(WorldEvent::PortalUsed { gate: s.read_bc()? })
}

// ---- readers: instances --------------------------------------------------------------------

/// `SCProcessingInstancePacket` (G/systems/instances/packets.rs:387, writer :397).
pub(crate) fn read_processing_instance(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    some(WorldEvent::ProcessingInstance {
        zone_id: s.read_i32()?,
    })
}

// ---- readers: boats ------------------------------------------------------------------------

/// `SCMateSpawnedPacket` (G/systems/vehicles/packets.rs:69, writer :579): `u16 tl, u32 id,
/// u64 item id, u8 user state, i32 experience, i32 mileage, u32 spawn delay`, then ten `u32`
/// skill slots (the writer pads `10 - skills` zeros, G/systems/vehicles/packets.rs:90-92).
pub(crate) fn read_mate_spawned(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let tl = s.read_u16()?;
    let id = s.read_u32()?;
    let item_id = s.read_u64()?;
    let user_state = s.read_u8()?;
    let experience = s.read_i32()?;
    let mileage = s.read_i32()?;
    let spawn_delay_ms = s.read_u32()?;
    let mut skills = Vec::with_capacity(MATE_SKILL_SLOTS);
    for _ in 0..MATE_SKILL_SLOTS {
        skills.push(s.read_u32()?);
    }
    some(WorldEvent::MateSpawned(Box::new(MateInfo {
        tl,
        id,
        item_id,
        user_state,
        experience,
        mileage,
        spawn_delay_ms,
        skills: skills.into_boxed_slice(),
    })))
}

/// Mount-bar skill slots of `SCMateSpawnedPacket` (`packets.rs:586`: always ten).
pub const MATE_SKILL_SLOTS: usize = 10;

/// `SCSlaveCreatedPacket` (G/systems/vehicles/packets.rs:107).
pub(crate) fn read_slave_created(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let owner_obj = s.read_bc()?;
    let tl = s.read_u16()?;
    let slave_obj = s.read_bc()?;
    let hide_effect = s.read_bool()?;
    skip(s, 8)?; // unk id
    let creator = boxed(s.read_string()?);
    some(WorldEvent::SlaveCreated {
        owner_obj,
        tl,
        slave_obj,
        hide_effect,
        creator,
    })
}

/// `SCMySlavePacket` (G/systems/vehicles/packets.rs:147): `fx64` coordinates, `f32` height.
pub(crate) fn read_my_slave(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let obj = s.read_bc()?;
    let tl = s.read_u16()?;
    let name = boxed(s.read_string()?);
    let template = s.read_u32()?;
    let hp = s.read_i32()?;
    let max_hp = s.read_i32()?;
    let x = convert_long_to_coord(s.read_i64()?);
    let y = convert_long_to_coord(s.read_i64()?);
    let z = s.read_f32()?;
    some(WorldEvent::MySlave(Box::new(MySlaveInfo {
        obj,
        tl,
        name,
        template,
        hp,
        max_hp,
        pos: (x, y, z),
    })))
}

/// `SCSlaveBoundPacket` (G/systems/vehicles/packets.rs:140): a character took the driver's seat.
pub(crate) fn read_slave_bound(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let master_id = s.read_u32()?;
    let slave_obj = s.read_bc()?;
    some(WorldEvent::SlaveBound {
        master_id,
        slave_obj,
    })
}

/// `SCUnitAttachedPacket` (G/packets/world_objects.rs:224, writer :239): note the wire order,
/// `bc child, u8 point, bc parent, u8 reason`.
pub(crate) fn read_unit_attached(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let child = s.read_bc()?;
    let point = s.read_u8()?;
    let parent = s.read_bc()?;
    let reason = s.read_u8()?;
    some(WorldEvent::UnitAttached {
        child,
        point,
        parent,
        reason,
    })
}

/// `SCUnitDetachedPacket` (G/systems/vehicles/packets.rs:97).
pub(crate) fn read_unit_detached(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let obj = s.read_bc()?;
    let reason = s.read_u8()?;
    some(WorldEvent::UnitDetached { obj, reason })
}

/// `SCSlaveDespawnPacket` (G/systems/vehicles/packets.rs:133).
pub(crate) fn read_slave_despawned(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    some(WorldEvent::SlaveDespawned { obj: s.read_bc()? })
}

/// `SCSlaveRemovedPacket` (G/systems/vehicles/packets.rs:126). The id is the owner's object id
/// in most paths and the slave's own in the rider's escape (G/systems/vehicles/slaves.rs:2364).
pub(crate) fn read_slave_removed(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let obj = s.read_bc()?;
    let tl = s.read_u16()?;
    some(WorldEvent::SlaveRemoved { obj, tl })
}

/// A ship move of `SCOneUnitMovementPacket` (`ShipMove::write`, G/models/movement.rs:589):
/// `bc obj, u8 3, u32 time, u8 flags, [u32 sc type, u8 phase], position(9), i16 vel x3,
/// i16 rotation x3, f32 angular velocity x3, i8 steering, i8 throttle, u16 zone, u8 stuck`.
/// Any other move type is `Ok(None)`.
pub(crate) fn read_ship_moved(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let obj = s.read_bc()?;
    if s.read_u8()? != MOVE_TYPE_SHIP {
        return Ok(None);
    }
    let time = s.read_u32()?;
    let flags = s.read_u8()?;
    if flags & FLAG_SC_TYPE_AND_PHASE != 0 {
        skip(s, 4 + 1)?;
    }
    let pos = s.read_position()?;
    let mut v = [0.0f32; 3];
    for c in &mut v {
        *c = f32::from(s.read_i16()?) * SHIP_MPS_PER_SHORT;
    }
    let mut q = [0.0f32; 3];
    for c in &mut q {
        *c = f32::from(s.read_i16()?) * QUATERNION_PER_SHORT;
    }
    let ang_vel = [s.read_f32()?, s.read_f32()?, s.read_f32()?];
    let steering = s.read_i8()?;
    let throttle = s.read_i8()?;
    let zone = s.read_u16()?;
    let stuck = s.read_bool()?;
    let yaw = quaternion_yaw(q);
    some(WorldEvent::ShipMoved(Box::new(ShipMotion {
        obj,
        time,
        pos,
        velocity: v,
        speed: (v[0] * v[0] + v[1] * v[1]).sqrt(),
        yaw,
        ang_vel,
        steering,
        throttle,
        zone,
        stuck,
    })))
}

/// Heading (radians, 0 = bow toward +Y, counter-clockwise positive) of the wire quaternion
/// `(x, y, z)` whose scalar `w` is the positive square root of the rest (the server flips the
/// quaternion so that `w >= 0`, G/systems/ai/physics/replication.rs:177). It is the yaw term of
/// the server's `GetYawPitchRollFromQuat` (G/models/movement.rs:772), in `f64`, without the
/// receiver cutoff that zeroes `w` near a half turn (movement.rs:795-796: `norm < 0.9975`), which
/// would skew the heading by up to 6 degrees.
fn quaternion_yaw(q: [f32; 3]) -> f32 {
    let [x, y, z] = q.map(f64::from);
    let w = (1.0 - x * x - y * y - z * z).max(0.0).sqrt();
    (2.0 * (w * z + x * y)).atan2(1.0 - 2.0 * (y * y + z * z)) as f32
}
