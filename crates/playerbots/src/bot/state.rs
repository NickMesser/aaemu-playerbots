//! What one bot knows: itself, the units and doodads the server shows it, its bag, quests,
//! cooldowns and buffs.
//!
//! [`BotState::apply`] is the only writer: every parsed [`ServerEvent`] updates the state, and
//! the few events that need an answer (a server teleport, a death, a kick) leave a note in
//! [`BotState::reactions`] for the bot task to act on. The state never touches the network and
//! never reads a clock (the bot task sets [`BotState::now_ms`] first), so it is deterministic and
//! testable.
//!
//! Memory is bounded: units are capped at [`MAX_UNITS`], doodads at [`MAX_DOODADS`] (and the
//! bot task drops doodads that do nothing before they get here), the small lists are
//! [`SmallVec`]s with hard limits, strings are kept only where something reads them (character
//! names).

use aa_proto::sc::ServerEvent;
use aa_proto::sc_activity::{quest_status, ActivityEvent, QuestRow};
use aa_proto::sc_world::{unit_type, EquipSlot, ItemChange, ItemTaskInfo, UnitInfo, WorldEvent};
use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use super::goals::trade::Outcome;
use super::goals::trade_run::Ledger;
use super::place::Place;
use super::travel::TravelState;
use super::vehicle::VehicleState;
use super::watch::classify;
use super::watch::{Finding, Watch};
use super::zones::ZoneStates;

/// Most units kept in the view.
pub const MAX_UNITS: usize = 1024;
/// Most doodads kept in the view.
pub const MAX_DOODADS: usize = 2048;
/// `SlotType` of the equipment container in item tasks.
pub const SLOT_EQUIPMENT: u8 = 1;
/// `SlotType` of the bag.
pub const SLOT_BAG: u8 = 2;

/// Chat lines kept between two brain ticks (system lines, whispers and local chat).
pub const INBOX_MAX: usize = 16;

/// How long a character found dead at login waits before it asks for the temple resurrection
/// (the server sent no respawn timer for it).
pub const DEAD_AT_LOGIN_WAIT_MS: u64 = 1_500;

/// Unit flag: the unit is dead.
pub const U_DEAD: u8 = 1;
/// Unit flag: the corpse still has loot.
pub const U_LOOT: u8 = 2;
/// Unit flag: the unit belongs to a player (summon, pet).
pub const U_OWNED: u8 = 4;
/// Unit flag: the backpack slot of the character is occupied (a carried trade pack, a glider);
/// the template is in [`PvpMem`] (`BotState::pack_of`).
pub const U_PACK: u8 = 8;
/// Unit flag: the first hit on this NPC was the bot's (the server's tag, and the quest credit,
/// belongs to it and its party in looting range).
pub const U_TAG_ME: u8 = 16;
/// Unit flag: the first hit on this NPC was a party member's.
pub const U_TAG_PARTY: u8 = 32;
/// Unit flag: the first hit on this NPC was another character's: the server credits that tag,
/// not the bot that lands the killing blow.
pub const U_TAG_OTHER: u8 = 64;
/// Mask of the three tag flags.
pub const U_TAG_MASK: u8 = U_TAG_ME | U_TAG_PARTY | U_TAG_OTHER;

/// Who the bot saw hit an NPC first (the server's tag).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagOwner {
    /// The bot's own hit.
    Me,
    /// A member of the bot's party.
    Party,
    /// Another character.
    Other,
}

/// What kind of unit the server shows. Houses, transfers, mates and shipyards are not tracked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitKind {
    /// A player character (a bot or a real player).
    Character,
    /// An NPC: monster, quest giver, merchant.
    Npc,
    /// A vehicle (cart, boat).
    Slave,
}

impl UnitKind {
    /// The kind of a `BaseUnitType` wire value, `None` for the kinds bots ignore.
    pub fn from_wire(t: u8) -> Option<UnitKind> {
        match t {
            unit_type::CHARACTER => Some(UnitKind::Character),
            unit_type::NPC => Some(UnitKind::Npc),
            unit_type::SLAVE => Some(UnitKind::Slave),
            _ => None,
        }
    }
}

/// A unit in view.
#[derive(Debug, Clone, PartialEq)]
pub struct Unit {
    /// What it is.
    pub kind: UnitKind,
    /// Character id (characters) or template id (NPCs, vehicles).
    pub id: u32,
    /// Position.
    pub pos: [f32; 3],
    /// Hit points (real value); only moves with the server's updates.
    pub hp: i32,
    /// Faction id; 0 when the server did not send one (use the NPC template's).
    pub faction: u32,
    /// Level.
    pub level: u8,
    /// `U_DEAD | U_LOOT | U_OWNED | U_PACK | U_TAG_*`.
    pub flags: u8,
    /// Object id of the unit's current target (`SCTargetChangedPacket`), 0 when none/unknown.
    /// For an NPC this is who it is fighting.
    pub target: u32,
    /// Name, characters only.
    pub name: Option<Box<str>>,
}

impl Unit {
    /// Whether the unit is dead.
    pub fn dead(&self) -> bool {
        self.flags & U_DEAD != 0
    }

    /// Whether the corpse still has loot.
    pub fn has_loot(&self) -> bool {
        self.flags & U_LOOT != 0
    }

    /// Whether a player owns the unit.
    pub fn owned(&self) -> bool {
        self.flags & U_OWNED != 0
    }

    /// Whether the character's backpack slot is occupied (see [`U_PACK`]).
    pub fn carries_pack(&self) -> bool {
        self.flags & U_PACK != 0
    }

    /// Who the bot saw hit this NPC first, `None` while it has seen no hit.
    pub fn tag(&self) -> Option<TagOwner> {
        match self.flags & U_TAG_MASK {
            U_TAG_ME => Some(TagOwner::Me),
            U_TAG_PARTY => Some(TagOwner::Party),
            U_TAG_OTHER => Some(TagOwner::Other),
            _ => None,
        }
    }

    /// Whether another character (not the bot, not its party) holds the tag.
    pub fn tagged_by_other(&self) -> bool {
        self.tag() == Some(TagOwner::Other)
    }

    /// Horizontal distance to `p`.
    pub fn dist2d(&self, p: [f32; 3]) -> f32 {
        dist2d(self.pos, p)
    }
}

/// A doodad in view (only doodads that can be used, looted or crafted at).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Doodad {
    /// Doodad template.
    pub template: u32,
    /// Position.
    pub pos: [f32; 3],
    /// Current phase (function group).
    pub phase: u32,
}

/// An item in the bag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Item {
    /// Item instance id.
    pub id: u64,
    /// Item template.
    pub template: u32,
    /// Stack size.
    pub count: i32,
}

/// An item on its way from one slot to another ([`BotState::move_item`]).
#[derive(Debug, Clone, Copy)]
struct Held {
    id: u64,
    template: u32,
    count: i32,
    grade: u8,
}

/// One of the bot's houses.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HouseBrief {
    /// Transport id the house packets use.
    pub tl: u16,
    /// House id.
    pub id: u32,
    /// Design (template).
    pub template: u32,
    /// Position.
    pub pos: [f32; 3],
    /// Build steps left (0 = finished).
    pub build_left: i32,
}

/// The bot itself.
#[derive(Debug, Clone, Default)]
pub struct Me {
    /// Character id.
    pub char_id: u32,
    /// Object id in the world, 0 until the own unit state arrived.
    pub obj: u32,
    /// Character name.
    pub name: Box<str>,
    /// Race id.
    pub race: u8,
    /// Gender id.
    pub gender: u8,
    /// Level.
    pub level: u8,
    /// Experience.
    pub exp: i32,
    /// Hit points.
    pub hp: i32,
    /// Mana.
    pub mp: i32,
    /// Highest hit points seen (the wire has no maximum).
    pub hp_max: i32,
    /// Highest mana seen.
    pub mp_max: i32,
    /// Position (the motion controller owns it while in the world).
    pub pos: [f32; 3],
    /// Facing in radians (0 = +Y, counter-clockwise).
    pub yaw: f32,
    /// Copper.
    pub money: i64,
    /// Labor points.
    pub labor: i16,
    /// Faction id.
    pub faction: u32,
    /// The three active abilities (11 = none).
    pub abilities: [u8; 3],
    /// Zone key from the server.
    pub zone_id: u32,
    /// Dead and waiting to be resurrected.
    pub dead: bool,
    /// Bot clock (ms) at which a resurrection may be requested.
    pub respawn_at_ms: u64,
    /// The own unit state arrived: the character is in the world.
    pub spawned: bool,
}

impl Me {
    /// Hit points in percent of the highest value seen. 0 without hit points (dead, or nothing
    /// known yet: a character that logs in dead never saw a maximum); 100 when the bot has hit
    /// points but no maximum is known.
    pub fn hp_pct(&self) -> u8 {
        if self.hp <= 0 {
            0
        } else {
            pct(self.hp, self.hp_max)
        }
    }

    /// Mana in percent of the highest value seen; 100 when unknown (many builds have no mana).
    pub fn mp_pct(&self) -> u8 {
        pct(self.mp, self.mp_max)
    }
}

fn pct(v: i32, max: i32) -> u8 {
    if max <= 0 {
        100
    } else {
        (i64::from(v.max(0)) * 100 / i64::from(max)).clamp(0, 100) as u8
    }
}

/// The quests of the bot.
#[derive(Debug, Clone, Default)]
pub struct QuestLog {
    /// Quests in progress (status in-progress, ready or failed).
    pub active: Vec<QuestRow>,
    /// Completed quest ids, ascending.
    pub completed: Vec<u32>,
}

impl QuestLog {
    /// The active row of quest `id`.
    pub fn row(&self, id: u32) -> Option<&QuestRow> {
        self.active.iter().find(|q| q.template == id)
    }

    /// Whether quest `id` is completed.
    pub fn is_done(&self, id: u32) -> bool {
        self.completed.binary_search(&id).is_ok()
    }

    fn mark_done(&mut self, id: u32) {
        if let Err(at) = self.completed.binary_search(&id) {
            self.completed.insert(at, id);
        }
    }

    fn mark_undone(&mut self, id: u32) {
        if let Ok(at) = self.completed.binary_search(&id) {
            self.completed.remove(at);
        }
    }

    fn upsert(&mut self, row: QuestRow) {
        match quest_status_class(row.status) {
            StatusClass::Gone => {
                self.active.retain(|q| q.template != row.template);
                if matches!(
                    row.status,
                    quest_status::COMPLETED | quest_status::DAILY_COMPLETED
                ) {
                    self.mark_done(row.template);
                }
            }
            StatusClass::Active => {
                match self.active.iter().position(|q| q.template == row.template) {
                    Some(i) => self.active[i] = row,
                    None if self.active.len() < 64 => self.active.push(row),
                    None => {}
                }
            }
        }
    }
}

enum StatusClass {
    Active,
    Gone,
}

fn quest_status_class(status: u8) -> StatusClass {
    match status {
        quest_status::DROPPED | quest_status::COMPLETED | quest_status::DAILY_COMPLETED => {
            StatusClass::Gone
        }
        _ => StatusClass::Active,
    }
}

/// Something the server refused or reported, newest last in [`BotState::errors`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateError {
    /// `SCErrorMsg` with the `ErrorMessageType` id.
    Msg(i16),
    /// `SCQuestContextFailed`.
    Quest {
        /// Quest template.
        quest: u32,
        /// Reason byte.
        err: u8,
    },
    /// `SCLootItemFailed`.
    Loot(i16),
    /// A skill start the server refused (`SkillStarted.result != 0`).
    Skill {
        /// Skill id.
        skill: u32,
        /// `SkillResult` byte.
        result: u8,
    },
}

/// A chat line addressed to or heard by the bot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatIn {
    /// Chat type (0 say, -3 whisper, -2 system, -1 notice, 1 shout, ...).
    pub kind: i16,
    /// Sender object id (0 for system text).
    pub sender_obj: u32,
    /// Sender name.
    pub name: Box<str>,
    /// Text, cut to 200 characters.
    pub text: Box<str>,
    /// Bot clock (ms) when it arrived.
    pub at_ms: u64,
}

/// Things [`BotState::apply`] noticed that the bot task must answer on the wire.
#[derive(Debug, Clone, Default)]
pub struct Reactions {
    /// The server moved the bot here (`SCTeleportUnit`): answer `CSTeleportEnded`.
    pub teleport: Option<[f32; 3]>,
    /// `SCLoadInstance`: answer `CSInstanceLoaded`.
    pub load_instance: bool,
    /// The `SCLoadInstance` moved the bot to another world ([`Place::on_load`]): the entity view
    /// was dropped, and the bot task drops the combat target and loot job that belonged to it.
    pub world_changed: bool,
    /// The bot was resurrected here.
    pub resurrected: Option<[f32; 3]>,
    /// The bot died.
    pub died: bool,
    /// The bot died with a trade pack in the back slot: it is a doodad at the death spot now.
    pub pack_dropped: bool,
    /// `SCKicked` with its reason and message.
    pub kicked: Option<(u8, Box<str>)>,
    /// `SCLeaveWorldGranted`: the server lets the bot log out.
    pub leave_granted: bool,
}

/// Everything one bot knows. See the module docs.
#[derive(Debug, Default)]
pub struct BotState {
    /// The bot itself.
    pub me: Me,
    /// Units in view by object id.
    pub units: FxHashMap<u32, Unit>,
    /// Interesting doodads in view by object id.
    pub doodads: FxHashMap<u32, Doodad>,
    /// Bag content by slot.
    pub bag: FxHashMap<u16, Item>,
    /// Equipped items.
    pub equipment: Vec<EquipSlot>,
    /// Quests.
    pub quests: QuestLog,
    /// Learned skills `(skill, level)`.
    pub skills: Vec<(u32, u8)>,
    /// Skill cooldowns `(skill, ready at bot clock ms)`.
    pub cooldowns: SmallVec<[(u32, u64); 8]>,
    /// Own buffs `(instance index, buff id)`.
    pub buffs: SmallVec<[(u32, u32); 8]>,
    /// Own houses.
    pub houses: SmallVec<[HouseBrief; 1]>,
    /// Units that target the bot `(object id, bot clock ms)`.
    pub threats: SmallVec<[(u32, u64); 4]>,
    /// Units the bot killed `(object id, bot clock ms)`, newest last.
    pub kills: SmallVec<[(u32, u64); 4]>,
    /// Bot clock (ms) of the last hit point loss.
    pub last_damage_ms: u64,
    /// Recent errors, newest last (at most 8).
    pub errors: SmallVec<[(u64, StateError); 8]>,
    /// Chat the bot heard, newest last (at most [`INBOX_MAX`]).
    pub inbox: SmallVec<[ChatIn; 2]>,
    /// The bot's clock in milliseconds, set by the bot task before it applies events.
    pub now_ms: u64,
    /// Notes for the bot task, see [`Reactions`].
    pub reactions: Reactions,
    /// Events dropped because a collection was full.
    pub dropped: u32,
    /// The trade window and unanswered trade requests (the trade goal and the brain read it).
    pub trade: super::goals::trade::TradeLog,
    /// Housing notes: the tax quote, object ids of the bot's houses, the map marker.
    pub house_log: super::goals::housing::HouseLog,
    /// Economy notes the trade-run goal reads: refusals, craft casts, payment mails, bad routes.
    pub econ: Ledger,
    /// What the quest goal remembers between its runs (failed and quarantined quests).
    pub quest_mem: Option<Box<super::goals::quest::Memory>>,
    /// Monsters the bot could not reach and leaves alone (grind and quest goals), allocated at
    /// the first one.
    pub avoid: Option<Box<super::avoid::Avoid>>,
    /// The detectors that tell the team about server problems (packet trail, requests waiting for
    /// an answer, findings), see [`super::watch`]. One allocation per bot.
    pub watch: Box<Watch>,
    /// Which world the bot is in (main world or an instance), kept by `SCProcessingInstance` and
    /// `SCLoadInstance`.
    pub place: Place,
    /// What the bot knows about portals; gets the portal events (owned by the portals feature).
    pub travel: TravelState,
    /// What the bot knows about its boat; gets the slave events (owned by the sailing feature).
    pub vehicle: VehicleState,
    /// The team the bot is in and its dungeon memory (owned by the dungeons feature); made on
    /// the first team event or dungeon goal.
    pub party: Option<Box<super::party::PartyMem>>,
    /// What the bot may sell and what the market features remember (the provenance ledger, see
    /// [`super::market`]); allocated when the bot first gains something it may sell.
    pub market: super::market::MarketState,
    /// The conflict state of the zone groups the server announced (one pointer until the first
    /// announcement), see [`BotState::zone_state`] and [`BotState::at_war`].
    pub zone_states: ZoneStates,
    /// What the bot knows about its guild (expedition) and what the guild duty plans (one pointer
    /// until the first guild packet or duty tick), see [`super::guild`].
    pub guild: super::guild::GuildState,
    /// What the bot remembers of fights with characters: the carriers in view, the characters it
    /// shuns, the run in progress, its kills (owned by the pvp feature, see [`super::pvp`]);
    /// allocated by the first carrier in view or fight.
    pub pvp: Option<Box<super::pvp::PvpMem>>,
    /// What the bot knows of its gear beyond the equipment list (bag grades, gems), the equip
    /// duty's refusals and the gear goal's notes (owned by the gear feature, see
    /// [`super::gear`]); allocated at log-in or by the first gear fact.
    pub gear: Option<Box<super::gear::GearMem>>,
}

/// The module that follows an event besides the general bookkeeping of
/// [`BotState::apply`]: the routing table of the travel features.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Portal events, followed by [`TravelState::on_event`].
    Travel,
    /// Slave and ship events, followed by [`VehicleState::on_event`].
    Vehicle,
    /// Instance events, followed by [`Place`] (`SCProcessingInstance`, `SCLoadInstance`).
    Place,
    /// Everything else: only the general bookkeeping.
    Other,
}

/// Where `ev` is routed. Listed variant by variant, so a new travel event has to be placed here.
pub fn route(ev: &WorldEvent) -> Route {
    match ev {
        WorldEvent::PortalBook(_)
        | WorldEvent::ReturnDistricts { .. }
        | WorldEvent::PortalSaved(_)
        | WorldEvent::PortalDeleted { .. }
        | WorldEvent::PortalUsed { .. } => Route::Travel,
        // (the travel state keeps the last skill answer of the bot: the portal skills and the
        // mount / glider casts the mount feature checks)
        WorldEvent::SkillStarted { .. } => Route::Travel,
        WorldEvent::MateSpawned(_) => Route::Travel,
        WorldEvent::SlaveCreated { .. }
        | WorldEvent::MySlave(_)
        | WorldEvent::SlaveBound { .. }
        | WorldEvent::UnitAttached { .. }
        | WorldEvent::UnitDetached { .. }
        | WorldEvent::SlaveDespawned { .. }
        | WorldEvent::SlaveRemoved { .. }
        | WorldEvent::ShipMoved(_) => Route::Vehicle,
        WorldEvent::ProcessingInstance { .. } | WorldEvent::LoadInstance { .. } => Route::Place,
        _ => Route::Other,
    }
}

impl BotState {
    /// A fresh state.
    pub fn new() -> BotState {
        BotState::default()
    }

    /// Forgets everything about the world (a new connection starts from scratch) and frees the
    /// memory of the collections.
    pub fn reset(&mut self) {
        // (the sailing cooldown outlives a connection: a bot that keeps losing its link does not
        // get a trip every time it is back)
        let trips = self.vehicle.trip_log();
        // (and so does the gear memory: item ids outlive a connection, and the counts of what the
        // bot bought and minted keep a reconnecting bot from fetching it all again)
        let gear = self.gear.take().map(|mut g| {
            g.after_reset();
            g
        });
        // (a pack dropped at a death spot is a doodad in the world, not a connection fact: the
        // bot still has to go back for it after a reconnect)
        let lost_pack = self.econ.lost_pack();
        *self = BotState::default();
        self.vehicle.restore_trip_log(trips);
        self.gear = gear;
        if let Some(lost) = lost_pack {
            self.econ.restore_lost_pack(lost);
        }
    }

    /// The gear memory, allocated on first use.
    pub fn gear_mut(&mut self) -> &mut super::gear::GearMem {
        self.gear.get_or_insert_with(Box::default)
    }

    // ---- queries ------------------------------------------------------------------------

    /// The conflict state of zone group `zone_group` (one of
    /// [`conflict_state`](aa_proto::sc_world::conflict_state)) and the Unix second it ends;
    /// `None` when the server has not announced the zone group.
    pub fn zone_state(&self, zone_group: u16) -> Option<(u8, i64)> {
        self.zone_states.get(zone_group)
    }

    /// Whether zone group `zone_group` is fought over (conflict or war). `false` when the server
    /// has not announced it.
    pub fn at_war(&self, zone_group: u16) -> bool {
        self.zone_states.at_war(zone_group)
    }

    /// The unit with object id `obj`.
    pub fn unit(&self, obj: u32) -> Option<&Unit> {
        self.units.get(&obj)
    }

    /// Whether `char_id` is a member of the bot's party.
    pub fn is_party_char(&self, char_id: u32) -> bool {
        self.party
            .as_ref()
            .is_some_and(|p| p.team.in_team() && p.team.has(char_id))
    }

    /// Whether `obj` is a character of the bot's party (in view).
    pub fn is_party_obj(&self, obj: u32) -> bool {
        self.units
            .get(&obj)
            .is_some_and(|u| u.kind == UnitKind::Character && self.is_party_char(u.id))
    }

    /// Whether another character (not the bot, not its party) holds the tag of `obj`, or the
    /// NPC is fighting one: a monster the bot should not expect quest credit for.
    pub fn taken_by_other(&self, obj: u32) -> bool {
        let Some(u) = self.units.get(&obj) else {
            return false;
        };
        u.tagged_by_other()
            || (u.target != 0 && u.target != self.me.obj && !self.is_party_obj(u.target))
    }

    /// The template of the item in the backpack slot of character `obj` (a carried trade pack);
    /// 0 when the slot is empty or the character is not in view.
    pub fn pack_of(&self, obj: u32) -> u32 {
        self.pvp.as_ref().map_or(0, |p| p.pack_of(obj))
    }

    /// The pvp memory, created on first use.
    pub fn pvp_mut(&mut self) -> &mut super::pvp::PvpMem {
        self.pvp.get_or_insert_with(Default::default)
    }

    /// The bag count of item `template` (all stacks).
    pub fn item_count(&self, template: u32) -> i64 {
        self.bag
            .values()
            .filter(|i| i.template == template && i.count > 0)
            .map(|i| i64::from(i.count))
            .sum()
    }

    /// The bag slot holding `template`. A stack with no items left (a consumed item whose removal
    /// the bot has not seen, or never will) is not an item.
    pub fn find_item(&self, template: u32) -> Option<(u16, &Item)> {
        self.find_item_where(template, |_| true)
    }

    /// The lowest bag slot holding a stack of `template` that `keep` accepts.
    pub fn find_item_where(
        &self,
        template: u32,
        mut keep: impl FnMut(&Item) -> bool,
    ) -> Option<(u16, &Item)> {
        self.bag
            .iter()
            .filter(|(_, i)| i.template == template && i.count > 0 && keep(i))
            .min_by_key(|(slot, _)| **slot)
            .map(|(slot, item)| (*slot, item))
    }

    /// The bot's skill level, `None` when it has not learned `skill`.
    pub fn skill_level(&self, skill: u32) -> Option<u8> {
        self.skills.iter().find(|s| s.0 == skill).map(|s| s.1)
    }

    /// Whether `skill` is off cooldown at the bot clock `now_ms`.
    pub fn cooldown_ready(&self, skill: u32, now_ms: u64) -> bool {
        self.cooldowns
            .iter()
            .find(|c| c.0 == skill)
            .is_none_or(|c| now_ms >= c.1)
    }

    /// Marks `skill` ready at `ready_at_ms`.
    pub fn set_cooldown(&mut self, skill: u32, ready_at_ms: u64) {
        let now = self.now_ms;
        if let Some(c) = self.cooldowns.iter_mut().find(|c| c.0 == skill) {
            c.1 = ready_at_ms;
            return;
        }
        if self.cooldowns.len() >= 16 {
            self.cooldowns.retain(|c| c.1 > now);
        }
        if self.cooldowns.len() < 16 {
            self.cooldowns.push((skill, ready_at_ms));
        }
    }

    /// Whether a unit that is not a bot of ours is a character within `radius` metres of the bot.
    pub fn any_character_within(&self, radius: f32, mut is_ours: impl FnMut(u32) -> bool) -> bool {
        self.units.values().any(|u| {
            u.kind == UnitKind::Character && !is_ours(u.id) && u.dist2d(self.me.pos) <= radius
        })
    }

    /// Keeps a chat line for the social duty, which files the inbox at every brain tick. At
    /// [`INBOX_MAX`] lines the oldest ordinary chat line goes first, so a burst of system lines
    /// (the answer to a GM command such as `/build` is one line per step) and whispers survive
    /// the local chatter around them.
    fn push_inbox(&mut self, line: ChatIn) {
        if self.inbox.len() >= INBOX_MAX {
            let victim = self
                .inbox
                .iter()
                .position(|l| !matches!(l.kind, -3..=-1))
                .unwrap_or(0);
            self.inbox.remove(victim);
        }
        self.inbox.push(line);
    }

    /// The memory of unreachable monsters, created on first use.
    pub fn avoid_mut(&mut self) -> &mut super::avoid::Avoid {
        self.avoid.get_or_insert_with(Default::default)
    }

    /// Whether the bot leaves monster `obj` of NPC `template` alone at the bot clock `now_ms`.
    pub fn avoids(&self, obj: u32, template: u32, now_ms: u64) -> bool {
        self.avoid
            .as_ref()
            .is_some_and(|a| a.skips(obj, template, now_ms))
    }

    /// Records a refused action.
    pub fn push_error(&mut self, e: StateError) {
        if self.errors.len() >= 8 {
            self.errors.remove(0);
        }
        self.errors.push((self.now_ms, e));
    }

    /// Whether the last `window_ms` contained a hit point loss.
    pub fn damaged_within(&self, window_ms: u64) -> bool {
        self.last_damage_ms != 0 && self.now_ms.saturating_sub(self.last_damage_ms) <= window_ms
    }

    // ---- events -------------------------------------------------------------------------

    /// Updates the state from one server event.
    pub fn apply(&mut self, ev: &ServerEvent) {
        self.econ.observe(ev, self.now_ms, self.me.obj);
        self.watch.observe(ev, self.me.obj, self.now_ms);
        self.market.on_server_event(ev, self.now_ms);
        match ev {
            ServerEvent::World(w) => self.apply_world(w),
            ServerEvent::Activity(a) => self.apply_activity(a),
        }
    }

    fn apply_world(&mut self, ev: &WorldEvent) {
        match route(ev) {
            Route::Travel => return self.travel.on_event(ev, self.me.obj, self.now_ms),
            Route::Vehicle => {
                self.vehicle.on_event(ev, self.me.obj);
                self.travel.on_vehicle_event(ev, self.me.obj, self.now_ms);
                return;
            }
            Route::Place | Route::Other => {}
        }
        match ev {
            WorldEvent::CharacterState(own) => {
                let me = &mut self.me;
                me.char_id = own.id;
                me.name = own.name.clone();
                me.race = own.race;
                me.gender = own.gender;
                me.level = own.level;
                me.hp = own.hp;
                me.mp = own.mp;
                me.hp_max = me.hp_max.max(own.hp);
                me.mp_max = me.mp_max.max(own.mp);
                me.zone_id = own.zone_id;
                me.faction = own.faction_id;
                me.abilities = own.abilities;
                me.pos = [own.pos.0, own.pos.1, own.pos.2];
                me.yaw = own.yaw;
                me.exp = own.exp;
                me.labor = own.labor;
                me.money = own.money;
                self.guild.on_own_guild(own.expedition_id, self.now_ms);
                self.equipment = own.equipment.to_vec();
                self.gear_mut().on_login(own);
                // Saved with no hit points: the server sends no death event for it.
                if own.hp <= 0 {
                    self.watch.death.found_dead_at_login(self.now_ms);
                    self.found_dead(DEAD_AT_LOGIN_WAIT_MS);
                }
            }
            WorldEvent::UnitState(info) => self.apply_unit_state(info),
            WorldEvent::UnitsRemoved { objs } => {
                for o in objs {
                    self.units.remove(o);
                    self.threats.retain(|t| t.0 != *o);
                    if let Some(p) = self.pvp.as_mut() {
                        p.forget(*o);
                    }
                }
            }
            WorldEvent::UnitEquipmentsChanged { obj, slots } => {
                // (the own equipment follows the item tasks)
                if *obj != self.me.obj {
                    for &(slot, template) in slots.iter() {
                        if slot == aa_proto::sc_world::BACKPACK_SLOT {
                            self.note_pack(*obj, template);
                        }
                    }
                }
            }
            WorldEvent::PvpPoints { obj, kind, points } => {
                if *obj == self.me.obj && self.me.obj != 0 {
                    let mem = self.pvp_mut();
                    match *kind {
                        aa_proto::sc_world::pvp_points::HONOR => mem.honor = *points,
                        aa_proto::sc_world::pvp_points::HOSTILE_KILLS => {
                            mem.hostile_kills = *points
                        }
                        _ => {}
                    }
                }
            }
            WorldEvent::UnitMoved { obj, pos, .. } => {
                self.travel.note_mate_moved(*obj, [pos.0, pos.1, pos.2]);
                if let Some(u) = self.units.get_mut(obj) {
                    u.pos = [pos.0, pos.1, pos.2];
                }
            }
            WorldEvent::TeleportUnit { pos, .. } => {
                self.me.pos = [pos.0, pos.1, pos.2];
                self.reactions.teleport = Some(self.me.pos);
            }
            WorldEvent::LoadInstance {
                instance_id,
                zone_id,
                pos,
                yaw,
                ..
            } => {
                self.me.pos = [pos.0, pos.1, pos.2];
                self.me.yaw = *yaw;
                self.reactions.load_instance = true;
                if self.place.on_load(*instance_id, *zone_id, self.now_ms) {
                    self.reactions.world_changed = true;
                    self.clear_view();
                }
            }
            WorldEvent::ProcessingInstance { .. } => self.place.on_processing(self.now_ms),
            WorldEvent::UnitPoints { obj, hp, mp } => {
                if *obj == self.me.obj && self.me.obj != 0 {
                    if *hp < self.me.hp {
                        self.last_damage_ms = self.now_ms.max(1);
                    }
                    self.me.hp = *hp;
                    self.me.mp = *mp;
                    self.me.hp_max = self.me.hp_max.max(*hp);
                    self.me.mp_max = self.me.mp_max.max(*mp);
                    if *hp <= 0 && !self.me.dead {
                        // (the server says SCUnitDeath right after; if it does not, that is a finding)
                        self.watch.death.points_zero(self.now_ms);
                        // The server sends the zero points BEFORE the death packet, and
                        // `check_alive` marks the bot dead on them: the death packet then finds
                        // it dead already, so the dropped pack is noted here, once per death.
                        self.note_pack_drop();
                    }
                    self.check_alive();
                } else if let Some(u) = self.units.get_mut(obj) {
                    u.hp = *hp;
                    if *hp <= 0 {
                        u.flags |= U_DEAD;
                    }
                }
            }
            WorldEvent::UnitDamaged { caster, target } => {
                // The first hit the bot sees is the server's tag: the tag decides quest credit,
                // so a killing blow on a monster somebody else tagged is not counted as one.
                let bit = if *caster == self.me.obj && self.me.obj != 0 {
                    U_TAG_ME
                } else if self.is_party_obj(*caster) {
                    U_TAG_PARTY
                } else {
                    // (a character the bot cannot classify, or one out of view: not its tag)
                    U_TAG_OTHER
                };
                if let Some(u) = self.units.get_mut(target) {
                    if u.kind == UnitKind::Npc && u.tag().is_none() {
                        u.flags |= bit;
                    }
                }
            }
            WorldEvent::UnitDeath {
                obj,
                killer,
                respawn_wait_ms,
                ..
            } => self.apply_death(*obj, *killer, *respawn_wait_ms),
            WorldEvent::Resurrected { obj, pos, yaw } => {
                if *obj == self.me.obj && self.me.obj != 0 {
                    self.watch.death.alive();
                    self.me.dead = false;
                    self.me.pos = [pos.0, pos.1, pos.2];
                    self.me.yaw = *yaw;
                    self.me.hp = self.me.hp.max(1);
                    self.reactions.resurrected = Some(self.me.pos);
                } else if let Some(u) = self.units.get_mut(obj) {
                    u.flags &= !U_DEAD;
                    u.pos = [pos.0, pos.1, pos.2];
                }
            }
            WorldEvent::TargetChanged { obj, target } => {
                if let Some(u) = self.units.get_mut(obj) {
                    u.target = *target;
                }
                if let Some(mem) = self.party.as_mut() {
                    mem.team
                        .on_target(*obj, *target, self.me.obj, &self.units, self.now_ms);
                }
                if *obj == self.me.obj {
                    return;
                }
                if *target == self.me.obj && self.me.obj != 0 {
                    let now = self.now_ms;
                    match self.threats.iter_mut().find(|t| t.0 == *obj) {
                        Some(t) => t.1 = now,
                        None => {
                            if self.threats.len() >= 4 {
                                self.threats.remove(0);
                            }
                            self.threats.push((*obj, now));
                        }
                    }
                } else {
                    self.threats.retain(|t| t.0 != *obj);
                }
            }
            WorldEvent::ExpChanged { obj, delta } => {
                if *obj == self.me.obj {
                    self.me.exp = self.me.exp.saturating_add(*delta);
                }
            }
            WorldEvent::LevelChanged { obj, level } => {
                if *obj == self.me.obj && self.me.obj != 0 {
                    self.me.level = *level;
                } else if let Some(u) = self.units.get_mut(obj) {
                    u.level = *level;
                }
            }
            WorldEvent::FactionChanged { obj, faction, .. } => {
                if *obj == self.me.obj && self.me.obj != 0 {
                    self.me.faction = *faction;
                } else if let Some(u) = self.units.get_mut(obj) {
                    u.faction = *faction;
                }
            }
            WorldEvent::ConflictZoneState {
                zone_group,
                state,
                end_unix_s,
            } => {
                if !self.zone_states.set(*zone_group, *state, *end_unix_s) {
                    self.dropped = self.dropped.saturating_add(1);
                }
            }
            WorldEvent::DoodadsCreated(list) => {
                for d in list {
                    if self.doodads.len() >= MAX_DOODADS && !self.doodads.contains_key(&d.obj) {
                        self.dropped = self.dropped.saturating_add(1);
                        continue;
                    }
                    self.doodads.insert(
                        d.obj,
                        Doodad {
                            template: d.template,
                            pos: [d.pos.0, d.pos.1, d.pos.2],
                            phase: d.phase,
                        },
                    );
                }
            }
            WorldEvent::DoodadsRemoved(objs) => {
                for o in objs {
                    self.doodads.remove(o);
                }
            }
            WorldEvent::DoodadPhase { obj, phase } => {
                if let Some(d) = self.doodads.get_mut(obj) {
                    d.phase = *phase;
                }
            }
            WorldEvent::InventoryInit { .. } => {}
            WorldEvent::Inventory {
                slot_type,
                chunk,
                items,
            } => {
                if *slot_type == SLOT_BAG {
                    let lo = u16::from(*chunk) * 10;
                    self.bag.retain(|slot, _| !(lo..lo + 10).contains(slot));
                    for it in items.iter().filter(|it| it.count > 0) {
                        self.bag.insert(
                            it.slot,
                            Item {
                                id: it.id,
                                template: it.template,
                                count: it.count,
                            },
                        );
                    }
                    self.market.on_bag_reloaded(&self.bag);
                    if self.gear.is_some() || items.iter().any(|it| it.grade != 0) {
                        self.gear_mut().on_bag(items);
                    }
                }
            }
            WorldEvent::ItemTask(task) => self.apply_item_task(task),
            WorldEvent::LunagemResult {
                result, item_id, ..
            } => {
                let now = self.now_ms;
                self.gear_mut().on_lunagem(now, *result, *item_id);
            }
            WorldEvent::GradeEnchantResult {
                result,
                item_id,
                before,
                after,
                ..
            } => {
                let now = self.now_ms;
                self.gear_mut()
                    .on_grade(now, *result, *item_id, *before, *after);
            }
            WorldEvent::LaborChanged { delta, .. } => {
                let v = i32::from(self.me.labor).saturating_add(*delta);
                self.me.labor = v.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
            }
            WorldEvent::SkillLearned { skill, level, .. } => {
                match self.skills.iter_mut().find(|s| s.0 == *skill) {
                    Some(s) => s.1 = *level,
                    None => self.skills.push((*skill, *level)),
                }
            }
            WorldEvent::Cooldowns { skills, .. } => {
                let now = self.now_ms;
                for c in skills.iter() {
                    self.set_cooldown(c.id, now + u64::from(c.remaining_ms));
                }
            }
            WorldEvent::CooldownReset { obj, skill, .. } => {
                if *obj == self.me.obj {
                    self.cooldowns.retain(|c| c.0 != *skill);
                }
            }
            WorldEvent::BuffCreated {
                obj,
                index,
                buff_id,
                ..
            } => {
                if *obj == self.me.obj && self.me.obj != 0 && self.buffs.len() < 32 {
                    self.buffs.retain(|b| b.0 != *index);
                    self.buffs.push((*index, *buff_id));
                }
            }
            WorldEvent::BuffRemoved { obj, index } => {
                if *obj == self.me.obj {
                    self.buffs.retain(|b| b.0 != *index);
                }
            }
            WorldEvent::Chat {
                kind,
                sender_obj,
                name,
                msg,
                ..
            } => {
                if *sender_obj != self.me.obj || *sender_obj == 0 {
                    self.push_inbox(ChatIn {
                        kind: *kind,
                        sender_obj: *sender_obj,
                        name: name.clone(),
                        text: msg.chars().take(200).collect::<String>().into_boxed_str(),
                        at_ms: self.now_ms,
                    });
                }
            }
            WorldEvent::ErrorMsg { err, .. } => self.push_error(StateError::Msg(*err)),
            WorldEvent::LootableState {
                owner_obj,
                owner_type,
                has_loot,
            } => {
                if *owner_type == 1 {
                    if let Some(u) = self.units.get_mut(owner_obj) {
                        if *has_loot {
                            u.flags |= U_LOOT;
                        } else {
                            u.flags &= !U_LOOT;
                        }
                    }
                }
            }
            WorldEvent::LootFailed { err, .. } => self.push_error(StateError::Loot(*err)),
            WorldEvent::Kicked { reason, msg } => {
                self.reactions.kicked = Some((*reason, msg.clone()));
            }
            WorldEvent::LeaveWorldGranted { .. } => self.reactions.leave_granted = true,
            // Session steps, combat flow and loot windows are handled by the session code, the
            // combat controller and the goals that watch them.
            _ => {}
        }
    }

    /// Drops the picture of the world the bot left: the server shows the new one afresh once the
    /// load is answered, and nothing the old one held (units, doodads, who targets the bot, whom
    /// it killed or could not reach) is valid in it. The combat target is the bot task's to drop
    /// ([`Reactions::world_changed`]).
    fn clear_view(&mut self) {
        self.units = FxHashMap::default();
        self.doodads = FxHashMap::default();
        self.threats.clear();
        self.kills.clear();
        self.avoid = None;
        if let Some(mem) = self.party.as_mut() {
            mem.team.member_targets.clear();
            mem.team.attackers.clear();
        }
        if let Some(p) = self.pvp.as_mut() {
            p.clear_view();
        }
    }

    /// Notes that character `obj`'s backpack slot holds `template` (0 = nothing now).
    fn note_pack(&mut self, obj: u32, template: u32) {
        let Some(u) = self.units.get_mut(&obj) else {
            return;
        };
        if u.kind != UnitKind::Character {
            return;
        }
        if template != 0 {
            u.flags |= U_PACK;
        } else {
            u.flags &= !U_PACK;
        }
        if template != 0 || self.pvp.is_some() {
            self.pvp_mut().set_pack(obj, template);
        }
    }

    fn apply_unit_state(&mut self, info: &UnitInfo) {
        let is_me = info.unit_type == unit_type::CHARACTER
            && self.me.char_id != 0
            && info.id == self.me.char_id;
        if is_me {
            let first_spawn = !self.me.spawned;
            let me = &mut self.me;
            me.obj = info.obj;
            me.level = info.level;
            me.hp = info.hp;
            me.mp = info.mp;
            me.hp_max = me.hp_max.max(info.hp);
            me.mp_max = me.mp_max.max(info.mp);
            if info.complete {
                me.faction = info.faction_id;
            }
            if !me.spawned {
                me.pos = [info.pos.0, info.pos.1, info.pos.2];
                me.spawned = true;
            }
            if info.complete {
                self.skills = info.skills.to_vec();
                self.buffs.clear();
                for b in info.buffs.iter().take(32) {
                    self.buffs.push((b.index, b.buff_id));
                }
            }
            if info.dead {
                if first_spawn {
                    self.watch.death.found_dead_at_login(self.now_ms);
                }
                self.found_dead(DEAD_AT_LOGIN_WAIT_MS);
            } else {
                self.check_alive();
            }
            return;
        }
        if info.unit_type == unit_type::MATE {
            self.travel.note_mate_state(
                info.obj,
                info.tl_id,
                info.owner_id,
                self.me.char_id,
                [info.pos.0, info.pos.1, info.pos.2],
            );
        }
        let Some(kind) = UnitKind::from_wire(info.unit_type) else {
            return;
        };
        if self.units.len() >= MAX_UNITS && !self.units.contains_key(&info.obj) {
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        let mut flags = 0;
        if info.dead {
            flags |= U_DEAD;
        }
        if info.has_loot {
            flags |= U_LOOT;
        }
        if info.owner_id != 0 {
            flags |= U_OWNED;
        }
        if kind == UnitKind::Npc && info.id == aa_world::travel::portal::PORTAL_ENTRANCE_NPC {
            // an open portal's entrance: the portal travel remembers whose gate it is
            self.travel.note_gate(
                info.obj,
                info.owner_id,
                info.owner_id != 0 && info.owner_id == self.me.char_id,
                [info.pos.0, info.pos.1, info.pos.2],
                self.now_ms,
            );
        }
        if kind == UnitKind::Character && info.backpack != 0 {
            flags |= U_PACK;
        }
        self.units.insert(
            info.obj,
            Unit {
                kind,
                id: info.id,
                pos: [info.pos.0, info.pos.1, info.pos.2],
                hp: info.hp,
                faction: info.faction_id,
                level: info.level,
                flags,
                target: 0,
                name: (kind == UnitKind::Character).then(|| info.name.clone()),
            },
        );
        if kind == UnitKind::Character && (info.backpack != 0 || self.pvp.is_some()) {
            // (the server repeats the whole state of a unit that comes into view again)
            self.pvp_mut().set_pack(info.obj, info.backpack);
        }
    }

    /// The own hit points say the bot is dead although no death event told so (a character that
    /// logged in dead, a lost packet): start the normal death flow. Hit points again after a
    /// wrong guess (or a resurrection whose notice came late or never) clear it, and the bot
    /// goes on as after any resurrection (it rests first).
    fn check_alive(&mut self) {
        if self.me.hp <= 0 {
            self.found_dead(DEAD_AT_LOGIN_WAIT_MS);
        } else if self.me.dead {
            self.watch.death.alive();
            self.me.dead = false;
            self.reactions.resurrected = Some(self.me.pos);
        }
    }

    /// Marks the bot dead without a death event: like `apply_death` for the bot, but the first
    /// timer stands when the death is already known.
    fn found_dead(&mut self, wait_ms: u64) {
        if self.me.dead {
            return;
        }
        self.me.dead = true;
        self.me.hp = 0;
        self.me.respawn_at_ms = self.now_ms + wait_ms.max(1000);
        self.reactions.died = true;
        self.threats.clear();
        // the requests of a dead bot are moot
        self.watch.tracker.clear();
    }

    /// The server drops the equipped trade pack as a doodad at the death spot right after the
    /// death packet (`Character.DropTradePackToFloor`), before the item task removes it from the
    /// back slot: remember where to look for it after the resurrection. Called once per death by
    /// whichever packet shows the death first.
    fn note_pack_drop(&mut self) {
        let backpack = self
            .equipment
            .iter()
            .find(|e| e.slot == aa_proto::sc_world::BACKPACK_SLOT)
            .map_or(0, |e| e.template);
        if backpack != 0 {
            self.econ.note_death_with_pack(
                backpack,
                self.me.pos,
                self.place.instance_id(),
                self.now_ms,
            );
            self.reactions.pack_dropped = true;
        }
    }

    fn apply_death(&mut self, obj: u32, killer: u32, respawn_wait_ms: i32) {
        if obj == self.me.obj && self.me.obj != 0 {
            let was_dead = self.me.dead;
            self.me.dead = true;
            self.me.hp = 0;
            let wait = u64::try_from(respawn_wait_ms).unwrap_or(0).max(1000);
            self.me.respawn_at_ms = self.now_ms + wait;
            self.reactions.died = true;
            self.threats.clear();
            self.watch.death.death_packet();
            self.watch.tracker.clear();
            // (a death the zero points did not announce first; otherwise the pack is noted there)
            if !was_dead {
                self.note_pack_drop();
            }
            if self
                .units
                .get(&killer)
                .is_some_and(|u| u.kind == UnitKind::Character)
            {
                let now = self.now_ms;
                let mem = self.pvp_mut();
                mem.killed_by = Some((killer, now));
                mem.deaths += 1;
            }
            return;
        }
        let mut was_character = false;
        if let Some(u) = self.units.get_mut(&obj) {
            u.flags |= U_DEAD;
            u.hp = 0;
            was_character = u.kind == UnitKind::Character;
            // (a dead carrier drops its pack: it is a doodad on the ground now)
            u.flags &= !U_PACK;
        }
        if was_character {
            if let Some(p) = self.pvp.as_mut() {
                p.set_pack(obj, 0);
            }
        }
        self.threats.retain(|t| t.0 != obj);
        if killer == self.me.obj && self.me.obj != 0 {
            if self.kills.len() >= 4 {
                self.kills.remove(0);
            }
            self.kills.push((obj, self.now_ms));
            if was_character {
                self.pvp_mut().kills += 1;
            }
        }
    }

    fn apply_item_task(&mut self, task: &ItemTaskInfo) {
        let touched = super::market::ledger::Touched::before(task, &self.bag);
        for change in &task.changes {
            self.apply_item_change(change);
            self.gear_note(change);
        }
        if !task.force_removed.is_empty() {
            self.bag
                .retain(|_, item| !task.force_removed.contains(&item.id));
        }
        self.market.on_item_task(task.reason, &touched, &self.bag);
    }

    /// Tells the gear memory about an item change that carries a gear fact (a grade, gems).
    fn gear_note(&mut self, change: &ItemChange) {
        let fact = match change {
            ItemChange::Added { grade, .. } => *grade != 0,
            ItemChange::GradeChange { .. } | ItemChange::Updated { gems: Some(_), .. } => true,
            _ => false,
        };
        if fact || self.gear.is_some() {
            self.gear_mut().on_change(change);
        }
    }

    fn apply_item_change(&mut self, change: &ItemChange) {
        match *change {
            ItemChange::Money(delta) => self.me.money += i64::from(delta),
            ItemChange::Added {
                slot_type,
                slot,
                template,
                id,
                grade,
                count,
                ..
            } => match slot_type {
                SLOT_BAG => {
                    // an empty stack is no item: it would hide what the slot held before
                    self.bag_put(u16::from(slot), id, template, count);
                }
                SLOT_EQUIPMENT => {
                    self.equipment.retain(|e| e.slot != slot);
                    self.equipment.push(EquipSlot {
                        slot,
                        template,
                        id,
                        grade,
                    });
                }
                _ => {}
            },
            ItemChange::GradeChange {
                slot_type: SLOT_EQUIPMENT,
                id,
                grade,
                ..
            } => {
                if let Some(e) = self.equipment.iter_mut().find(|e| e.id == id) {
                    e.grade = grade;
                }
            }
            ItemChange::CountUpdate {
                slot_type,
                slot,
                delta,
                id,
                template,
            } if slot_type == SLOT_BAG => {
                // The stack is found by its instance id first: the slot number alone can be
                // stale after a move the bot did not see.
                let key = self
                    .bag
                    .iter()
                    .find(|(_, i)| i.id == id && id != 0)
                    .map(|(k, _)| *k)
                    .unwrap_or_else(|| u16::from(slot));
                match self.bag.get_mut(&key) {
                    Some(item) => {
                        item.count = item.count.saturating_add(delta);
                        if item.count <= 0 {
                            self.bag.remove(&key);
                        }
                    }
                    None if delta > 0 => {
                        self.bag_put(key, id, template, delta);
                    }
                    None => {}
                }
            }
            ItemChange::Removed {
                slot_type,
                slot,
                id,
                ..
            }
            | ItemChange::RemovedSlot {
                slot_type,
                slot,
                id,
            } => match slot_type {
                SLOT_BAG => self.bag_take(u16::from(slot), id),
                SLOT_EQUIPMENT => self.equipment.retain(|e| e.slot != slot),
                _ => {}
            },
            ItemChange::Moved {
                from_slot_type,
                from_slot,
                to_slot_type,
                to_slot,
                from_id,
                to_id,
            } => self.move_item(
                from_slot_type,
                from_slot,
                to_slot_type,
                to_slot,
                from_id,
                to_id,
            ),
            _ => {}
        }
    }

    /// Puts a stack into a bag slot; a stack without items empties the slot instead.
    fn bag_put(&mut self, slot: u16, id: u64, template: u32, count: i32) {
        if count > 0 {
            self.bag.insert(
                slot,
                Item {
                    id,
                    template,
                    count,
                },
            );
        } else {
            self.bag.remove(&slot);
        }
    }

    /// The server took item `id` out of bag slot `slot`: the stack with that id leaves wherever
    /// the bot has it (its idea of the slot can be stale), else the slot is cleared.
    fn bag_take(&mut self, slot: u16, id: u64) {
        let before = self.bag.len();
        if id != 0 {
            self.bag.retain(|_, i| i.id != id);
        }
        if self.bag.len() == before {
            self.bag.remove(&slot);
        }
    }

    /// Moves and swaps inside the bag and between the bag and the equipment (the grade goes with
    /// the piece). A move to or from another container only keeps the bag consistent.
    fn move_item(
        &mut self,
        from_type: u8,
        from: u8,
        to_type: u8,
        to: u8,
        from_id: u64,
        to_id: u64,
    ) {
        if from_type == SLOT_BAG && to_type == SLOT_BAG {
            let a = self.bag.remove(&u16::from(from));
            let b = self.bag.remove(&u16::from(to));
            if let Some(a) = a.filter(|a| a.id == from_id || from_id == 0) {
                self.bag.insert(u16::from(to), a);
            }
            if let Some(b) = b.filter(|b| to_id != 0 && b.id == to_id) {
                self.bag.insert(u16::from(from), b);
            }
            return;
        }
        // (an equip into an empty slot comes as a move from the empty slot, id 0, to the bag item)
        let a = self.take_held(from_type, from, from_id);
        let b = self.take_held(to_type, to, to_id);
        if let Some(a) = a {
            self.put_held(to_type, to, a);
        }
        if let Some(b) = b {
            self.put_held(from_type, from, b);
        }
    }

    /// Empties slot `slot` of container `kind` and returns what it held when that is item `id`
    /// (in the bag also any item when `id` is 0): a stale item the server does not name is
    /// dropped.
    fn take_held(&mut self, kind: u8, slot: u8, id: u64) -> Option<Held> {
        match kind {
            SLOT_BAG => {
                let it = self.bag.remove(&u16::from(slot))?;
                if it.id != id && id != 0 {
                    return None;
                }
                let grade = self.gear.as_deref().map_or(0, |g| g.grade_of(it.id));
                Some(Held {
                    id: it.id,
                    template: it.template,
                    count: it.count,
                    grade,
                })
            }
            SLOT_EQUIPMENT => {
                let i = self.equipment.iter().position(|e| e.slot == slot)?;
                let e = self.equipment.remove(i);
                (e.id == id && id != 0).then_some(Held {
                    id: e.id,
                    template: e.template,
                    count: 1,
                    grade: e.grade,
                })
            }
            _ => None,
        }
    }

    /// Puts `h` into slot `slot` of container `kind`.
    fn put_held(&mut self, kind: u8, slot: u8, h: Held) {
        match kind {
            SLOT_BAG => {
                self.bag.insert(
                    u16::from(slot),
                    Item {
                        id: h.id,
                        template: h.template,
                        count: h.count,
                    },
                );
                if h.grade != 0 {
                    self.gear_mut().note_grade(h.id, h.grade);
                }
            }
            SLOT_EQUIPMENT => {
                self.equipment.retain(|e| e.slot != slot);
                self.equipment.push(EquipSlot {
                    slot,
                    template: h.template,
                    id: h.id,
                    grade: h.grade,
                });
            }
            _ => {}
        }
    }

    fn apply_activity(&mut self, ev: &ActivityEvent) {
        self.trade.apply(ev, self.now_ms);
        self.house_log.apply(ev, self.now_ms, self.me.char_id);
        super::party::observe(&mut self.party, ev, self.now_ms);
        if let ActivityEvent::Guild(g) = ev {
            self.guild.on_event(g, self.me.char_id, self.now_ms);
        }
        match ev {
            ActivityEvent::Quests { list } => {
                for row in list {
                    self.quests.upsert(*row);
                }
            }
            ActivityEvent::CompletedQuests { ids } => {
                for id in ids {
                    self.quests.mark_done(*id);
                }
            }
            ActivityEvent::QuestStarted { quest, .. } => self.quests.upsert(**quest),
            ActivityEvent::QuestUpdated { quest, .. } => self.quests.upsert(**quest),
            ActivityEvent::QuestCompleted { quest_id, .. } => {
                self.quests.active.retain(|q| q.template != *quest_id);
                self.quests.mark_done(*quest_id);
            }
            ActivityEvent::QuestReset { quest_id, .. } => self.quests.mark_undone(*quest_id),
            // the bag and the purse follow a finished trade
            ActivityEvent::TradeMade { items, .. } => {
                self.audit_trade(items);
                self.apply_item_task(items);
            }
            ActivityEvent::QuestFailed { quest_id, err } => self.push_error(StateError::Quest {
                quest: *quest_id,
                err: *err,
            }),
            ActivityEvent::QuestUnitReqFailed {
                quest_id,
                kind,
                value,
                ..
            } => {
                // (kept in the quest memory, not the error ring: the ring is inline per bot and
                // a wider error entry would cost it 64 bytes)
                let now = self.now_ms;
                self.quest_mem.get_or_insert_with(Default::default).unit_req =
                    Some((*quest_id, *kind, *value, now));
            }
            ActivityEvent::MyHouse(h) => self.upsert_house(h),
            ActivityEvent::LoginCharInfoHouse { owner_id, house } => {
                if *owner_id == self.me.char_id && self.me.char_id != 0 {
                    self.upsert_house(house);
                }
            }
            ActivityEvent::MyHouseRemoved { tl } => self.houses.retain(|h| h.tl != *tl),
            ActivityEvent::HouseBuildProgress { tl, all, cur, .. } => {
                if let Some(h) = self.houses.iter_mut().find(|h| h.tl == *tl) {
                    h.build_left = (*all - *cur).max(0);
                }
            }
            _ => {}
        }
    }

    /// Compares a finished trade with the two offers, before its item task changes the bag: a
    /// purse or a bag that moved by something else than both sides put up is a finding.
    fn audit_trade(&mut self, task: &ItemTaskInfo) {
        let Some(Outcome::Made(w)) = &self.trade.outcome else {
            return;
        };
        if task.changes.is_empty() {
            // the item tasks did not parse: nothing to compare
            return;
        }
        let got = classify::tally(task, &self.bag);
        let mine_items: i64 = w.mine.items.iter().map(|p| i64::from(p.amount)).sum();
        let mut other: SmallVec<[(u32, i64); 4]> = SmallVec::new();
        for p in &w.other.items {
            match other.iter_mut().find(|o| o.0 == p.template) {
                Some(o) => o.1 += i64::from(p.amount),
                None => other.push((p.template, i64::from(p.amount))),
            }
        }
        if let Some(fault) = classify::trade_fault(
            i64::from(w.mine.money),
            i64::from(w.other.money),
            mine_items,
            &other,
            &got,
        ) {
            self.watch.push(Finding::Trade(fault));
        }
    }

    fn upsert_house(&mut self, h: &aa_proto::sc_activity::HouseInfo) {
        let brief = HouseBrief {
            tl: h.tl,
            id: h.id,
            template: h.template,
            pos: [h.x, h.y, h.z],
            build_left: (h.all_action - h.cur_action).max(0),
        };
        match self.houses.iter_mut().find(|x| x.tl == h.tl) {
            Some(x) => *x = brief,
            None => {
                if self.houses.len() < 4 {
                    self.houses.push(brief);
                }
            }
        }
    }
}

/// Horizontal distance between two positions.
pub fn dist2d(a: [f32; 3], b: [f32; 3]) -> f32 {
    let (dx, dy) = (a[0] - b[0], a[1] - b[1]);
    (dx * dx + dy * dy).sqrt()
}

/// Full distance between two positions.
pub fn dist3d(a: [f32; 3], b: [f32; 3]) -> f32 {
    let dz = a[2] - b[2];
    (dist2d(a, b).powi(2) + dz * dz).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aa_proto::sc_world::{BuffInfo, DoodadInfo, InvItem, ItemTaskInfo, OwnState};

    fn own_state() -> OwnState {
        OwnState {
            id: 500,
            name: "Aria".into(),
            race: 1,
            gender: 2,
            level: 3,
            hp: 1000,
            mp: 400,
            zone_id: 7,
            faction_id: 101,
            expedition_id: 0,
            family_id: 0,
            abilities: [1, 11, 11],
            pos: (15000.0, 15100.0, 126.0),
            yaw: 1.0,
            instance_id: 1,
            exp: 50,
            recoverable_exp: 0,
            labor: 50,
            money: 20_000,
            bank_money: 0,
            loyalty: 0,
            bag_slots: 50,
            bank_slots: 0,
            equipment: Box::new([EquipSlot {
                slot: 15,
                template: 9,
                id: 99,
                grade: 1,
            }]),
            equipment_gems: Box::new([2]),
        }
    }

    fn unit_info(obj: u32, ty: u8, id: u32) -> UnitInfo {
        UnitInfo {
            obj,
            unit_type: ty,
            id,
            name: "N".into(),
            owner_name: "".into(),
            owner_id: 0,
            tl_id: 0,
            pos: (15010.0, 15100.0, 126.0),
            level: 5,
            hp: 300,
            mp: 0,
            dead: false,
            has_loot: false,
            faction_id: 115,
            expedition_id: 0,
            yaw: 0,
            skills: Box::new([(18132, 1)]),
            buffs: Box::new([BuffInfo {
                list: 0,
                index: 4,
                buff_id: 77,
                duration_ms: 0,
                charge: 0,
            }]),
            backpack: 0,
            complete: true,
        }
    }

    fn world(w: WorldEvent) -> ServerEvent {
        ServerEvent::World(w)
    }

    fn in_world() -> BotState {
        let mut s = BotState::new();
        s.apply(&world(WorldEvent::CharacterState(Box::new(own_state()))));
        s.apply(&world(WorldEvent::UnitState(Box::new(unit_info(
            0x400,
            unit_type::CHARACTER,
            500,
        )))));
        s
    }

    #[test]
    fn the_first_hit_seen_decides_the_tag_and_a_target_marks_a_taken_monster() {
        let mut s = in_world();
        s.apply(&world(WorldEvent::UnitState(Box::new(unit_info(
            0x700,
            unit_type::NPC,
            731,
        )))));
        // the bot hits first: its tag
        s.apply(&world(WorldEvent::UnitDamaged {
            caster: 0x400,
            target: 0x700,
        }));
        assert_eq!(s.unit(0x700).unwrap().tag(), Some(TagOwner::Me));
        assert!(!s.taken_by_other(0x700));
        // a later hit by somebody else does not take it back
        s.apply(&world(WorldEvent::UnitDamaged {
            caster: 0x999,
            target: 0x700,
        }));
        assert_eq!(s.unit(0x700).unwrap().tag(), Some(TagOwner::Me));

        // another monster is hit by a stranger first: no credit for the bot
        s.apply(&world(WorldEvent::UnitState(Box::new(unit_info(
            0x701,
            unit_type::NPC,
            732,
        )))));
        s.apply(&world(WorldEvent::UnitDamaged {
            caster: 0x999,
            target: 0x701,
        }));
        assert_eq!(s.unit(0x701).unwrap().tag(), Some(TagOwner::Other));
        assert!(s.taken_by_other(0x701));

        // a monster that fights another character is taken too, one that fights the bot is not
        s.apply(&world(WorldEvent::TargetChanged {
            obj: 0x701,
            target: 0x999,
        }));
        assert_eq!(s.unit(0x701).unwrap().target, 0x999);
        s.apply(&world(WorldEvent::TargetChanged {
            obj: 0x700,
            target: 0x400,
        }));
        assert!(!s.taken_by_other(0x700));
    }

    #[test]
    fn zone_conflict_packets_fill_the_zone_table() {
        use aa_proto::sc_world::conflict_state as cs;
        let zone = |zone_group, state, end_unix_s| {
            world(WorldEvent::ConflictZoneState {
                zone_group,
                state,
                end_unix_s,
            })
        };
        let mut s = BotState::new();
        assert_eq!(s.zone_state(7), None);
        assert!(!s.at_war(7));
        // one packet per zone group at world entry ...
        s.apply(&zone(7, cs::TENSION, 1_700_000_000));
        s.apply(&zone(8, cs::CONFLICT, 1_700_000_500));
        s.apply(&zone(9, cs::PEACE, 0));
        assert_eq!(s.zone_state(7), Some((cs::TENSION, 1_700_000_000)));
        assert_eq!(s.zone_state(9), Some((cs::PEACE, 0)));
        assert!(!s.at_war(7) && s.at_war(8) && !s.at_war(9));
        // ... and one on each change
        s.apply(&zone(7, cs::WAR, 1_700_003_600));
        s.apply(&zone(8, cs::PEACE, 1_700_007_200));
        assert_eq!(s.zone_state(7), Some((cs::WAR, 1_700_003_600)));
        assert!(s.at_war(7) && !s.at_war(8));
        assert_eq!(s.zone_state(8), Some((cs::PEACE, 1_700_007_200)));
        assert_eq!(s.zone_states.len(), 3);
        assert_eq!(s.dropped, 0);
        // the table is bounded; what does not fit is counted like any other dropped event
        for g in 100..(100 + crate::bot::zones::MAX_ZONE_STATES as u16) {
            s.apply(&zone(g, cs::TENSION, 0));
        }
        assert_eq!(s.zone_states.len(), crate::bot::zones::MAX_ZONE_STATES);
        assert!(s.dropped > 0);
        // a new connection starts from scratch
        s.reset();
        assert_eq!(s.zone_state(7), None);
        assert!(s.zone_states.is_empty());
    }

    #[test]
    fn character_state_and_own_unit_state_fill_the_self_info() {
        let s = in_world();
        assert_eq!(s.me.char_id, 500);
        assert_eq!(&*s.me.name, "Aria");
        assert_eq!(s.me.obj, 0x400);
        assert!(s.me.spawned);
        assert_eq!((s.me.hp, s.me.hp_max, s.me.mp_max), (300, 1000, 400));
        assert_eq!(s.me.hp_pct(), 30);
        assert_eq!(s.me.money, 20_000);
        assert_eq!(s.equipment.len(), 1);
        assert_eq!(s.skills, vec![(18132, 1)]);
        assert_eq!(s.buffs.as_slice(), &[(4, 77)]);
        // the own unit is not an entity of the view
        assert!(s.units.is_empty());
        // a second own unit state does not move the bot
        let mut again = unit_info(0x400, unit_type::CHARACTER, 500);
        again.pos = (1.0, 2.0, 3.0);
        let mut s2 = s;
        s2.apply(&world(WorldEvent::UnitState(Box::new(again))));
        assert_eq!(s2.me.pos, [15010.0, 15100.0, 126.0]);
    }

    #[test]
    fn units_come_and_go_and_track_moves_points_and_death() {
        let mut s = in_world();
        s.apply(&world(WorldEvent::UnitState(Box::new(unit_info(
            0x401,
            unit_type::NPC,
            3475,
        )))));
        s.apply(&world(WorldEvent::UnitState(Box::new(unit_info(
            0x402,
            unit_type::CHARACTER,
            900,
        )))));
        // houses are not tracked
        s.apply(&world(WorldEvent::UnitState(Box::new(unit_info(
            0x403,
            unit_type::HOUSING,
            1,
        )))));
        assert_eq!(s.units.len(), 2);
        assert_eq!(s.unit(0x401).unwrap().kind, UnitKind::Npc);
        assert!(s.unit(0x401).unwrap().name.is_none());
        assert_eq!(s.unit(0x402).unwrap().name.as_deref(), Some("N"));
        assert_eq!(s.unit(0x401).unwrap().faction, 115);

        s.apply(&world(WorldEvent::UnitMoved {
            obj: 0x401,
            kind: 1,
            pos: (1.0, 2.0, 3.0),
            yaw: 0,
            stance: 0,
        }));
        assert_eq!(s.unit(0x401).unwrap().pos, [1.0, 2.0, 3.0]);

        s.now_ms = 10_000;
        s.apply(&world(WorldEvent::UnitDeath {
            obj: 0x401,
            killer: 0x400,
            respawn_wait_ms: 0,
            lost_exp: 0,
            reason: 0,
        }));
        assert!(s.unit(0x401).unwrap().dead());
        assert_eq!(s.kills.as_slice(), &[(0x401, 10_000)]);
        s.apply(&world(WorldEvent::LootableState {
            owner_obj: 0x401,
            owner_type: 1,
            has_loot: true,
        }));
        assert!(s.unit(0x401).unwrap().has_loot());

        s.apply(&world(WorldEvent::UnitsRemoved {
            objs: [0x401u32, 0x999].into_iter().collect(),
        }));
        assert!(s.unit(0x401).is_none());
        assert_eq!(s.units.len(), 1);
    }

    #[test]
    fn threats_follow_target_changes_and_damage_is_noted() {
        let mut s = in_world();
        s.now_ms = 5000;
        s.apply(&world(WorldEvent::TargetChanged {
            obj: 0x401,
            target: 0x400,
        }));
        assert_eq!(s.threats.as_slice(), &[(0x401, 5000)]);
        s.apply(&world(WorldEvent::TargetChanged {
            obj: 0x401,
            target: 0x777,
        }));
        assert!(s.threats.is_empty());

        s.now_ms = 6000;
        s.apply(&world(WorldEvent::UnitPoints {
            obj: 0x400,
            hp: 200,
            mp: 390,
        }));
        assert_eq!((s.me.hp, s.me.mp), (200, 390));
        assert!(s.damaged_within(1000));
        s.now_ms = 9000;
        assert!(!s.damaged_within(1000));
        // healing is not damage
        s.apply(&world(WorldEvent::UnitPoints {
            obj: 0x400,
            hp: 900,
            mp: 390,
        }));
        assert_eq!(s.last_damage_ms, 6000);
    }

    #[test]
    fn own_death_and_resurrection_leave_reactions() {
        let mut s = in_world();
        s.now_ms = 1000;
        s.apply(&world(WorldEvent::UnitDeath {
            obj: 0x400,
            killer: 0x401,
            respawn_wait_ms: 5000,
            lost_exp: 10,
            reason: 1,
        }));
        assert!(s.me.dead);
        assert_eq!(s.me.respawn_at_ms, 6000);
        assert!(std::mem::take(&mut s.reactions.died));
        s.apply(&world(WorldEvent::Resurrected {
            obj: 0x400,
            pos: (1.0, 2.0, 3.0),
            yaw: 0.5,
        }));
        assert!(!s.me.dead);
        assert_eq!(s.reactions.resurrected, Some([1.0, 2.0, 3.0]));
        assert_eq!(s.me.pos, [1.0, 2.0, 3.0]);
        assert!(s.me.hp >= 1);
        // a death without a wait still waits a second
        s.apply(&world(WorldEvent::UnitDeath {
            obj: 0x400,
            killer: 0,
            respawn_wait_ms: 0,
            lost_exp: 0,
            reason: 0,
        }));
        assert_eq!(s.me.respawn_at_ms, 2000);
    }

    #[test]
    fn dying_with_a_pack_remembers_the_spot_to_look_for_it() {
        let mut s = in_world();
        s.now_ms = 5_000;
        s.me.pos = [100.0, 200.0, 300.0];
        s.equipment.push(EquipSlot {
            slot: aa_proto::sc_world::BACKPACK_SLOT,
            template: 31_855,
            id: 77,
            grade: 0,
        });
        s.apply(&world(WorldEvent::UnitDeath {
            obj: 0x400,
            killer: 0,
            respawn_wait_ms: 5_000,
            lost_exp: 0,
            reason: 0,
        }));
        let lost = s.econ.lost_pack().expect("the dropped pack is noted");
        assert_eq!(
            (lost.item, lost.pos, lost.inst),
            (31_855, [100.0, 200.0, 300.0], 0)
        );
        assert!(std::mem::take(&mut s.reactions.pack_dropped));
        // a second death packet does not move the spot or reset the timer
        s.now_ms = 9_000;
        s.apply(&world(WorldEvent::UnitDeath {
            obj: 0x400,
            killer: 0,
            respawn_wait_ms: 5_000,
            lost_exp: 0,
            reason: 0,
        }));
        assert_eq!(s.econ.lost_pack(), Some(lost));
        assert!(!std::mem::take(&mut s.reactions.pack_dropped));
        // a reconnect keeps the note: the doodad is still in the world
        s.reset();
        assert_eq!(s.econ.lost_pack(), Some(lost));
        // a death without a pack leaves no note
        let mut s = in_world();
        s.apply(&world(WorldEvent::UnitDeath {
            obj: 0x400,
            killer: 0,
            respawn_wait_ms: 5_000,
            lost_exp: 0,
            reason: 0,
        }));
        assert!(s.econ.lost_pack().is_none());
        assert!(!std::mem::take(&mut s.reactions.pack_dropped));
    }

    #[test]
    fn zero_points_before_the_death_packet_still_note_the_dropped_pack() {
        // The server's order on a lethal hit: `SCUnitPoints` with 0 hit points, then
        // `SCUnitDeath` (`reduce_current_hp` broadcasts the points, then runs `do_die`). The
        // points mark the bot dead, so the death packet finds it dead already.
        let mut s = in_world();
        s.now_ms = 5_000;
        s.me.pos = [100.0, 200.0, 300.0];
        s.equipment.push(EquipSlot {
            slot: aa_proto::sc_world::BACKPACK_SLOT,
            template: 31_855,
            id: 77,
            grade: 0,
        });
        s.apply(&world(WorldEvent::UnitPoints {
            obj: 0x400,
            hp: 0,
            mp: 0,
        }));
        assert!(s.me.dead);
        let lost = s.econ.lost_pack().expect("the dropped pack is noted");
        assert_eq!(
            (lost.item, lost.pos, lost.inst, lost.at_ms),
            (31_855, [100.0, 200.0, 300.0], 0, 5_000)
        );
        assert!(std::mem::take(&mut s.reactions.pack_dropped));
        // the death packet that follows neither moves the note nor reports the drop again
        s.now_ms = 5_020;
        s.apply(&world(WorldEvent::UnitDeath {
            obj: 0x400,
            killer: 0,
            respawn_wait_ms: 5_000,
            lost_exp: 0,
            reason: 0,
        }));
        assert_eq!(s.econ.lost_pack(), Some(lost));
        assert!(!std::mem::take(&mut s.reactions.pack_dropped));
        // a bot that logs in dead (hit points 0 before any other word) drops nothing
        let mut s = in_world();
        s.equipment.push(EquipSlot {
            slot: aa_proto::sc_world::BACKPACK_SLOT,
            template: 31_855,
            id: 77,
            grade: 0,
        });
        s.me.hp = 0;
        s.check_alive();
        assert!(s.me.dead);
        assert!(s.econ.lost_pack().is_none());
        // and the points of a bot that is dead already are not a second death
        s.apply(&world(WorldEvent::UnitPoints {
            obj: 0x400,
            hp: 0,
            mp: 0,
        }));
        assert!(s.econ.lost_pack().is_none());
    }

    #[test]
    fn a_character_saved_dead_is_dead_at_login() {
        // the server sends no death event for it: only hp 0 in CharacterState and UnitState
        let mut s = BotState::new();
        s.now_ms = 2000;
        let mut own = own_state();
        own.hp = 0;
        s.apply(&world(WorldEvent::CharacterState(Box::new(own))));
        assert!(s.me.dead, "dead from the character state");
        assert_eq!(s.me.respawn_at_ms, 2000 + DEAD_AT_LOGIN_WAIT_MS);
        assert!(std::mem::take(&mut s.reactions.died));
        assert_eq!(s.me.hp_pct(), 0, "no maximum was ever seen, still not 100");
        let mut me = unit_info(0x400, unit_type::CHARACTER, 500);
        me.hp = 0;
        me.dead = true;
        s.apply(&world(WorldEvent::UnitState(Box::new(me))));
        assert!(s.me.dead);
        assert_eq!(s.me.hp_pct(), 0);
        assert_eq!(
            s.me.respawn_at_ms,
            2000 + DEAD_AT_LOGIN_WAIT_MS,
            "timer kept"
        );
        // the resurrection brings it back like after any death
        s.apply(&world(WorldEvent::Resurrected {
            obj: 0x400,
            pos: (1.0, 2.0, 3.0),
            yaw: 0.0,
        }));
        assert!(!s.me.dead);
        assert!(s.me.hp_pct() > 0);
    }

    #[test]
    fn own_dead_unit_state_alone_marks_the_bot_dead_and_hit_points_revive_it() {
        let mut s = in_world();
        let mut me = unit_info(0x400, unit_type::CHARACTER, 500);
        me.hp = 0;
        me.dead = true;
        s.apply(&world(WorldEvent::UnitState(Box::new(me))));
        assert!(s.me.dead);
        assert!(std::mem::take(&mut s.reactions.died));
        // a death event that follows brings the server's own timer
        s.now_ms = 100;
        s.apply(&world(WorldEvent::UnitDeath {
            obj: 0x400,
            killer: 0,
            respawn_wait_ms: 9000,
            lost_exp: 0,
            reason: 0,
        }));
        assert_eq!(s.me.respawn_at_ms, 9100);
        // hit points again (a wrong guess, a late resurrection notice) clear it
        s.apply(&world(WorldEvent::UnitPoints {
            obj: 0x400,
            hp: 500,
            mp: 100,
        }));
        assert!(!s.me.dead);
        // zero hit points without a death event start the death flow too
        s.apply(&world(WorldEvent::UnitPoints {
            obj: 0x400,
            hp: 0,
            mp: 100,
        }));
        assert!(s.me.dead);
    }

    #[test]
    fn hp_percent_is_never_100_without_hit_points() {
        let mut me = Me::default();
        assert_eq!(me.hp_pct(), 0, "nothing known yet");
        me.hp = 0;
        me.hp_max = 1000;
        assert_eq!(me.hp_pct(), 0);
        me.hp = 5;
        assert_eq!(me.hp_pct(), 0, "rounded down, but not 100");
        me.hp = 1000;
        assert_eq!(me.hp_pct(), 100);
        me.hp_max = 0;
        assert_eq!(me.hp_pct(), 100, "hit points without a known maximum");
        // mana without a maximum stays 100: many builds have none
        assert_eq!(me.mp_pct(), 100);
    }

    fn travel_events() -> Vec<WorldEvent> {
        use aa_proto::sc_travel::{MateInfo, PortalEntry};
        let entry = PortalEntry {
            id: 4,
            name: "Home".into(),
            zone_id: 7,
            off_x: 1.0,
            off_y: 2.0,
            z: 100.0,
            z_rot: 0.0,
        };
        vec![
            WorldEvent::PortalBook(Box::new([entry.clone()])),
            WorldEvent::ReturnDistricts {
                portals: Box::new([entry.clone()]),
                bound: 4,
            },
            WorldEvent::PortalSaved(Box::new(entry)),
            WorldEvent::PortalDeleted { kind: 0, id: 4 },
            WorldEvent::PortalUsed { gate: 0x500 },
            WorldEvent::MateSpawned(Box::new(MateInfo {
                tl: 5,
                id: 9,
                item_id: 77,
                user_state: 1,
                experience: 0,
                mileage: 0,
                spawn_delay_ms: 0,
                skills: Box::default(),
            })),
            WorldEvent::SkillStarted {
                caster_obj: 0x400,
                skill: 10602,
                tl: 1,
                target_obj: 0x400,
                result: 0,
            },
        ]
    }

    fn vehicle_events() -> Vec<WorldEvent> {
        use aa_proto::sc_travel::{MySlaveInfo, ShipMotion};
        vec![
            WorldEvent::SlaveCreated {
                owner_obj: 0x400,
                tl: 3,
                slave_obj: 0x600,
                hide_effect: false,
                creator: "Aria".into(),
            },
            WorldEvent::MySlave(Box::new(MySlaveInfo {
                obj: 0x600,
                tl: 3,
                name: "Boat".into(),
                template: 9,
                hp: 10,
                max_hp: 10,
                pos: (1.0, 2.0, 3.0),
            })),
            WorldEvent::SlaveBound {
                master_id: 500,
                slave_obj: 0x600,
            },
            WorldEvent::UnitAttached {
                child: 0x400,
                point: 1,
                parent: 0x600,
                reason: 6,
            },
            WorldEvent::UnitDetached {
                obj: 0x400,
                reason: 5,
            },
            WorldEvent::SlaveDespawned { obj: 0x600 },
            WorldEvent::SlaveRemoved { obj: 0x400, tl: 3 },
            WorldEvent::ShipMoved(Box::new(ShipMotion {
                obj: 0x600,
                time: 1,
                pos: (1.0, 2.0, 3.0),
                velocity: [0.0; 3],
                speed: 0.0,
                yaw: 0.0,
                ang_vel: [0.0; 3],
                steering: 0,
                throttle: 0,
                zone: 1,
                stuck: false,
            })),
        ]
    }

    #[test]
    fn travel_events_are_routed_to_their_features() {
        for ev in travel_events() {
            assert_eq!(route(&ev), Route::Travel, "{ev:?}");
        }
        for ev in vehicle_events() {
            assert_eq!(route(&ev), Route::Vehicle, "{ev:?}");
        }
        for ev in [
            WorldEvent::ProcessingInstance { zone_id: 3 },
            load_instance(1, 2, (0.0, 0.0, 0.0)),
        ] {
            assert_eq!(route(&ev), Route::Place, "{ev:?}");
        }
        // a ship's position event is the ordinary mover event, its helm is the vehicle's
        let moved = WorldEvent::UnitMoved {
            obj: 0x600,
            kind: 3,
            pos: (0.0, 0.0, 0.0),
            yaw: 0,
            stance: 0,
        };
        assert_eq!(route(&moved), Route::Other);
        assert_eq!(route(&WorldEvent::LeaveWorldCanceled), Route::Other);
    }

    #[test]
    fn forwarded_events_change_nothing_else_yet() {
        let mut s = in_world();
        s.apply(&world(WorldEvent::UnitState(Box::new(unit_info(
            0x401,
            unit_type::NPC,
            3475,
        )))));
        let (pos, units, reactions) = (s.me.pos, s.units.len(), format!("{:?}", s.reactions));
        for ev in travel_events().into_iter().chain(vehicle_events()) {
            s.apply(&world(ev));
        }
        assert_eq!((s.me.pos, s.units.len()), (pos, units));
        assert_eq!(format!("{:?}", s.reactions), reactions);
        assert!(!s.place.in_instance() && !s.place.loading());
    }

    fn load_instance(instance_id: u32, zone_id: u32, pos: (f32, f32, f32)) -> WorldEvent {
        WorldEvent::LoadInstance {
            instance_id,
            zone_id,
            pos,
            yaw: 1.5,
            ang_x: 0.0,
            ang_y: 0.0,
        }
    }

    /// A state with a monster in view, a doodad, a threat, a kill and an unreachable monster.
    fn busy_state() -> BotState {
        let mut s = in_world();
        s.apply(&world(WorldEvent::UnitState(Box::new(unit_info(
            0x401,
            unit_type::NPC,
            3475,
        )))));
        s.apply(&world(WorldEvent::DoodadsCreated(
            [DoodadInfo {
                obj: 0x800,
                template: 7,
                pos: (1.0, 2.0, 3.0),
                owner_obj: 0,
                parent_obj: 0,
                phase: 1,
                owner_id: 0,
                quest_glow: 0,
                has_loot: false,
            }]
            .into_iter()
            .collect(),
        )));
        s.threats.push((0x401, 5));
        s.kills.push((0x402, 5));
        s.avoid_mut().ignore_obj(0x401, 100_000);
        assert!(!s.units.is_empty() && !s.doodads.is_empty() && s.avoid.is_some());
        s
    }

    #[test]
    fn a_dungeon_entry_moves_the_place_and_drops_the_old_view() {
        let mut s = busy_state();
        s.now_ms = 7_000;
        s.apply(&world(WorldEvent::ProcessingInstance { zone_id: 301 }));
        assert!(s.place.loading() && !s.place.in_instance());
        assert_eq!(s.place.loading_since_ms(), Some(7_000));
        // the old world is still the picture until the load arrives
        assert!(!s.units.is_empty());

        s.now_ms = 9_000;
        s.apply(&world(load_instance(12, 301, (50.0, 60.0, 70.0))));
        assert!(s.place.in_instance() && s.place.loading());
        assert_eq!((s.place.instance_id(), s.place.zone_key()), (12, 301));
        assert_eq!(s.me.pos, [50.0, 60.0, 70.0]);
        assert!(s.reactions.load_instance && s.reactions.world_changed);
        assert!(s.units.is_empty() && s.doodads.is_empty());
        assert!(s.threats.is_empty() && s.kills.is_empty() && s.avoid.is_none());
        // what the server shows afterwards is the new world
        s.apply(&world(WorldEvent::UnitState(Box::new(unit_info(
            0x700,
            unit_type::NPC,
            9,
        )))));
        assert_eq!(s.units.len(), 1);
        // the bot task answers the load and says so
        s.place.loaded();
        assert!(s.place.in_instance() && !s.place.loading());

        // and back out
        s.reactions = Reactions::default();
        s.apply(&world(load_instance(0, 5, (1.0, 2.0, 3.0))));
        assert!(!s.place.in_instance() && s.reactions.world_changed);
        assert!(s.units.is_empty());
    }

    #[test]
    fn a_load_of_the_world_the_bot_is_in_keeps_its_view() {
        let mut s = busy_state();
        // a teleport the server announces as a load, inside the main world
        s.apply(&world(load_instance(0, 5, (1.0, 2.0, 3.0))));
        assert!(s.reactions.load_instance, "the load is still answered");
        assert!(!s.reactions.world_changed);
        assert!(!s.units.is_empty() && !s.doodads.is_empty() && s.avoid.is_some());
        assert!(!s.place.in_instance() && s.place.loading());
    }

    #[test]
    fn a_new_session_starts_in_the_main_world() {
        let mut s = busy_state();
        s.apply(&world(load_instance(12, 301, (50.0, 60.0, 70.0))));
        assert!(s.place.in_instance());
        s.reset();
        assert!(!s.place.in_instance() && !s.place.loading());
    }

    #[test]
    fn teleport_and_kick_are_noted() {
        let mut s = in_world();
        s.apply(&world(WorldEvent::TeleportUnit {
            reason: 0,
            error: 0,
            pos: (10.0, 20.0, 30.0),
        }));
        assert_eq!(s.me.pos, [10.0, 20.0, 30.0]);
        assert_eq!(s.reactions.teleport, Some([10.0, 20.0, 30.0]));
        s.apply(&world(WorldEvent::Kicked {
            reason: 3,
            msg: "bye".into(),
        }));
        assert_eq!(s.reactions.kicked.as_ref().map(|k| k.0), Some(3));
    }

    #[test]
    fn inventory_chunks_and_item_tasks() {
        let mut s = in_world();
        s.apply(&world(WorldEvent::Inventory {
            slot_type: SLOT_BAG,
            chunk: 0,
            items: Box::new([
                InvItem {
                    slot: 0,
                    id: 11,
                    template: 4045,
                    grade: 0,
                    count: 3,
                },
                InvItem {
                    slot: 7,
                    id: 12,
                    template: 417,
                    grade: 0,
                    count: 1,
                },
            ]),
        }));
        s.apply(&world(WorldEvent::Inventory {
            slot_type: SLOT_BAG,
            chunk: 1,
            items: Box::new([InvItem {
                slot: 12,
                id: 13,
                template: 4045,
                grade: 0,
                count: 2,
            }]),
        }));
        // a bank chunk is not the bag
        s.apply(&world(WorldEvent::Inventory {
            slot_type: 3,
            chunk: 0,
            items: Box::new([InvItem {
                slot: 1,
                id: 14,
                template: 1,
                grade: 0,
                count: 1,
            }]),
        }));
        assert_eq!(s.bag.len(), 3);
        assert_eq!(s.item_count(4045), 5);
        assert_eq!(s.find_item(4045).unwrap().0, 0);

        let task = |changes: Vec<ItemChange>| {
            world(WorldEvent::ItemTask(Box::new(ItemTaskInfo {
                reason: 0,
                changes: changes.into_iter().collect(),
                force_removed: Default::default(),
            })))
        };
        s.apply(&task(vec![
            ItemChange::Money(-5000),
            ItemChange::CountUpdate {
                slot_type: SLOT_BAG,
                slot: 0,
                id: 11,
                delta: -3,
                template: 4045,
            },
            ItemChange::Added {
                action: 5,
                slot_type: SLOT_BAG,
                slot: 20,
                template: 555,
                id: 99,
                grade: 0,
                count: 4,
            },
            ItemChange::Removed {
                slot_type: SLOT_BAG,
                slot: 7,
                id: 12,
                template: 417,
            },
        ]));
        assert_eq!(s.me.money, 15_000);
        assert!(!s.bag.contains_key(&0), "an emptied stack leaves the bag");
        assert!(!s.bag.contains_key(&7));
        assert_eq!(s.item_count(555), 4);

        s.apply(&task(vec![ItemChange::Moved {
            from_slot_type: SLOT_BAG,
            from_slot: 20,
            to_slot_type: SLOT_BAG,
            to_slot: 12,
            from_id: 99,
            to_id: 13,
        }]));
        assert_eq!(s.bag.get(&12).map(|i| i.id), Some(99));
        assert_eq!(s.bag.get(&20).map(|i| i.id), Some(13));
    }

    #[test]
    fn quests_are_tracked_through_their_life() {
        let mut s = in_world();
        let row = |template: u32, status: u8| QuestRow {
            id: i64::from(template),
            template,
            status,
            objectives: [0; 5],
            left_time_ms: -1,
            component: 0,
            acceptor_type: 0,
            acceptor_id: 0,
        };
        let act = ServerEvent::Activity;
        s.apply(&act(ActivityEvent::Quests {
            list: vec![
                row(10, quest_status::PROGRESS),
                row(11, quest_status::READY),
            ],
        }));
        s.apply(&act(ActivityEvent::CompletedQuests { ids: vec![3, 5, 9] }));
        assert_eq!(s.quests.active.len(), 2);
        assert!(s.quests.is_done(5) && !s.quests.is_done(6));

        s.apply(&act(ActivityEvent::QuestStarted {
            quest: Box::new(row(12, quest_status::PROGRESS)),
            component: 1,
        }));
        s.apply(&act(ActivityEvent::QuestUpdated {
            quest: Box::new(row(12, quest_status::READY)),
            component: 2,
            p: [0; 4],
        }));
        assert_eq!(s.quests.row(12).unwrap().status, quest_status::READY);
        assert_eq!(s.quests.active.len(), 3);

        s.apply(&act(ActivityEvent::QuestCompleted {
            quest_id: 12,
            component: 0,
            block_mask: 0,
        }));
        assert!(s.quests.row(12).is_none() && s.quests.is_done(12));
        s.apply(&act(ActivityEvent::QuestUpdated {
            quest: Box::new(row(10, quest_status::DROPPED)),
            component: 0,
            p: [0; 4],
        }));
        assert!(s.quests.row(10).is_none() && !s.quests.is_done(10));
        s.apply(&act(ActivityEvent::QuestReset {
            quest_id: 12,
            block_id: 0,
            block_mask: 0,
        }));
        assert!(!s.quests.is_done(12));
        s.apply(&act(ActivityEvent::QuestFailed {
            quest_id: 11,
            err: 4,
        }));
        assert_eq!(
            s.errors.last().map(|e| e.1),
            Some(StateError::Quest { quest: 11, err: 4 })
        );
    }

    #[test]
    fn doodads_are_bounded_and_phase_updates() {
        let mut s = in_world();
        let d = |obj: u32| DoodadInfo {
            obj,
            template: 7,
            pos: (1.0, 2.0, 3.0),
            owner_obj: 0,
            parent_obj: 0,
            phase: 1,
            owner_id: 0,
            quest_glow: 0,
            has_loot: false,
        };
        s.apply(&world(WorldEvent::DoodadsCreated(
            [d(1), d(2)].into_iter().collect(),
        )));
        s.apply(&world(WorldEvent::DoodadPhase { obj: 2, phase: 9 }));
        assert_eq!(s.doodads[&2].phase, 9);
        s.apply(&world(WorldEvent::DoodadsRemoved(
            [1u32].into_iter().collect(),
        )));
        assert_eq!(s.doodads.len(), 1);
        for i in 0..(MAX_DOODADS as u32 + 10) {
            s.apply(&world(WorldEvent::DoodadsCreated(
                [d(1000 + i)].into_iter().collect(),
            )));
        }
        assert_eq!(s.doodads.len(), MAX_DOODADS);
        assert!(s.dropped > 0);
    }

    #[test]
    fn cooldowns_chat_and_errors_are_bounded() {
        let mut s = in_world();
        s.now_ms = 100;
        s.set_cooldown(2, 500);
        assert!(!s.cooldown_ready(2, 400));
        assert!(s.cooldown_ready(2, 500));
        assert!(s.cooldown_ready(99, 0));
        s.apply(&world(WorldEvent::CooldownReset {
            obj: 0x400,
            skill: 2,
            tag: 0,
            gcd: false,
        }));
        assert!(s.cooldown_ready(2, 0));
        for k in 0..40u32 {
            s.set_cooldown(1000 + k, 10_000);
        }
        assert!(s.cooldowns.len() <= 16);

        for i in 0..10 {
            s.apply(&world(WorldEvent::Chat {
                kind: 0,
                sender_obj: 0x500 + i,
                sender_id: 1,
                name: "Bob".into(),
                msg: "hi".repeat(300).into(),
            }));
            s.apply(&world(WorldEvent::ErrorMsg {
                err: i as i16,
                err2: i as i16,
                kind: 0,
                notify: false,
            }));
        }
        assert_eq!(s.inbox.len(), 10);
        assert_eq!(s.inbox[0].text.chars().count(), 200);
        assert_eq!(s.errors.len(), 8);
        assert_eq!(s.errors.last().map(|e| e.1), Some(StateError::Msg(9)));
        // own chat echoes are not "heard"
        let before = s.inbox.len();
        s.apply(&world(WorldEvent::Chat {
            kind: 0,
            sender_obj: 0x400,
            sender_id: 500,
            name: "Aria".into(),
            msg: "me".into(),
        }));
        assert_eq!(s.inbox.len(), before);
    }

    fn chat(kind: i16, n: u32) -> WorldEvent {
        WorldEvent::Chat {
            kind,
            sender_obj: if kind < 0 { 0 } else { 0x500 + n },
            sender_id: 0,
            name: format!("N{n}").into(),
            msg: format!("line {n}").into(),
        }
    }

    #[test]
    fn a_burst_of_system_lines_survives_until_the_social_duty_files_it() {
        let mut s = in_world();
        // a GM /build answers with one line per step: more than the old four-line inbox held
        for n in 0..INBOX_MAX as u32 {
            s.apply(&world(chat(-2, n)));
        }
        assert_eq!(s.inbox.len(), INBOX_MAX);
        assert_eq!(&*s.inbox[0].text, "line 0", "nothing was lost");
        assert_eq!(&*s.inbox[INBOX_MAX - 1].text, "line 15");
    }

    #[test]
    fn a_full_inbox_drops_local_chat_before_system_lines_and_whispers() {
        let mut s = in_world();
        s.apply(&world(chat(-3, 100))); // a whisper
        s.apply(&world(chat(-2, 101))); // a system line
        for n in 0..(INBOX_MAX as u32 + 6) {
            s.apply(&world(chat(0, n))); // local chatter
        }
        assert_eq!(s.inbox.len(), INBOX_MAX);
        let kinds: Vec<i16> = s.inbox.iter().map(|l| l.kind).collect();
        assert_eq!(
            &kinds[..2],
            &[-3, -2],
            "the whisper and the system line stay"
        );
        // the oldest local lines went, the newest stayed
        assert_eq!(&*s.inbox.last().unwrap().text, "line 21");
        assert!(!s.inbox.iter().any(|l| &*l.text == "line 0"));
        // when nothing but protected lines is left to drop, the oldest of them goes
        for n in 0..(INBOX_MAX as u32 + 2) {
            s.apply(&world(chat(-2, 200 + n)));
        }
        assert_eq!(s.inbox.len(), INBOX_MAX);
        assert_eq!(&*s.inbox.last().unwrap().text, "line 217");
    }

    #[test]
    fn empty_stacks_are_not_items() {
        let mut s = in_world();
        s.apply(&world(WorldEvent::Inventory {
            slot_type: SLOT_BAG,
            chunk: 0,
            items: Box::new([
                InvItem {
                    slot: 0,
                    id: 11,
                    template: 16401,
                    grade: 0,
                    count: 0,
                },
                InvItem {
                    slot: 3,
                    id: 12,
                    template: 16401,
                    grade: 0,
                    count: 2,
                },
            ]),
        }));
        assert_eq!(s.bag.len(), 1, "the empty stack was not stored");
        assert_eq!(s.find_item(16401).map(|x| (x.0, x.1.id)), Some((3, 12)));
        // a stack that is in the bag with nothing in it (a state older code could reach) is still
        // not found, and not counted
        s.bag.insert(
            0,
            Item {
                id: 13,
                template: 777,
                count: 0,
            },
        );
        assert!(s.find_item(777).is_none());
        assert_eq!(s.item_count(777), 0);
        // an item that arrives empty clears the slot instead of hiding what was there
        s.apply(&world(WorldEvent::ItemTask(Box::new(ItemTaskInfo {
            reason: 0,
            changes: [ItemChange::Added {
                action: 5,
                slot_type: SLOT_BAG,
                slot: 3,
                template: 16401,
                id: 14,
                grade: 0,
                count: 0,
            }]
            .into_iter()
            .collect(),
            force_removed: Default::default(),
        }))));
        assert!(s.find_item(16401).is_none());
        assert!(!s.bag.contains_key(&3));
        // a filter on top: the lowest slot that passes
        for (slot, id) in [(5u16, 20u64), (6, 21)] {
            s.bag.insert(
                slot,
                Item {
                    id,
                    template: 9,
                    count: 1,
                },
            );
        }
        assert_eq!(s.find_item_where(9, |i| i.id != 20).map(|x| x.0), Some(6));
    }

    #[test]
    fn a_consumed_item_leaves_the_bag_by_its_id_even_when_the_slot_is_stale() {
        let task = |changes: Vec<ItemChange>| {
            world(WorldEvent::ItemTask(Box::new(ItemTaskInfo {
                reason: 0,
                changes: changes.into_iter().collect(),
                force_removed: Default::default(),
            })))
        };
        for stale_slot in [false, true] {
            let mut s = in_world();
            s.bag.insert(
                4,
                Item {
                    id: 700,
                    template: 16401,
                    count: 1,
                },
            );
            s.bag.insert(
                9,
                Item {
                    id: 701,
                    template: 55,
                    count: 3,
                },
            );
            // the server says item 700 left slot 4 (or, with a stale picture, slot 11)
            let slot = if stale_slot { 11 } else { 4 };
            s.apply(&task(vec![ItemChange::RemovedSlot {
                slot_type: SLOT_BAG,
                slot,
                id: 700,
            }]));
            assert!(s.find_item(16401).is_none(), "stale_slot={stale_slot}");
            assert_eq!(s.item_count(55), 3, "another item stays");
        }
        // a removal of an id the bot never saw still clears the named slot
        let mut s = in_world();
        s.bag.insert(
            4,
            Item {
                id: 700,
                template: 16401,
                count: 1,
            },
        );
        s.apply(&task(vec![ItemChange::Removed {
            slot_type: SLOT_BAG,
            slot: 4,
            id: 999,
            template: 16401,
        }]));
        assert!(s.find_item(16401).is_none());
    }

    #[test]
    fn a_count_update_finds_its_stack_by_id_and_removes_an_emptied_one() {
        let mut s = in_world();
        s.bag.insert(
            8,
            Item {
                id: 40,
                template: 16401,
                count: 3,
            },
        );
        let update = |slot: u8, delta: i32| {
            world(WorldEvent::ItemTask(Box::new(ItemTaskInfo {
                reason: 0,
                changes: [ItemChange::CountUpdate {
                    slot_type: SLOT_BAG,
                    slot,
                    id: 40,
                    delta,
                    template: 16401,
                }]
                .into_iter()
                .collect(),
                force_removed: Default::default(),
            })))
        };
        // the packet names slot 2, the bot has the stack in slot 8: the id wins
        s.apply(&update(2, -2));
        assert_eq!(s.item_count(16401), 1);
        assert_eq!(s.bag.len(), 1);
        s.apply(&update(2, -1));
        assert!(s.find_item(16401).is_none(), "used up");
        assert!(s.bag.is_empty());
    }

    #[test]
    fn hit_points_alone_start_the_death_flow_and_end_it() {
        let mut s = in_world();
        s.now_ms = 10_000;
        s.apply(&world(WorldEvent::UnitPoints {
            obj: 0x400,
            hp: 0,
            mp: 50,
        }));
        // no SCUnitDeath came: the bot is dead all the same and asks for the resurrection
        assert!(s.me.dead);
        assert!(std::mem::take(&mut s.reactions.died));
        assert_eq!(s.me.respawn_at_ms, 10_000 + DEAD_AT_LOGIN_WAIT_MS);
        assert_eq!(s.me.hp_pct(), 0);
        // the resurrection notice never comes, the hit points do: alive, and it rests as after
        // any resurrection
        s.now_ms = 20_000;
        s.apply(&world(WorldEvent::UnitPoints {
            obj: 0x400,
            hp: 100,
            mp: 50,
        }));
        assert!(!s.me.dead);
        assert!(s.reactions.resurrected.is_some());
        assert!(s.me.hp_pct() > 0);
        // a second report of other hit points changes nothing
        s.reactions.resurrected = None;
        s.apply(&world(WorldEvent::UnitPoints {
            obj: 0x400,
            hp: 120,
            mp: 50,
        }));
        assert!(s.reactions.resurrected.is_none());
        assert!(!s.reactions.died);
    }

    #[test]
    fn zero_points_without_a_death_packet_are_a_finding_and_the_packet_clears_it() {
        use crate::bot::watch::classify::{Death, DEATH_PACKET_GRACE_MS};
        use crate::bot::watch::{Pending, Req};
        // the points reach 0, the server never says the bot died
        let mut s = in_world();
        s.now_ms = 10_000;
        s.apply(&world(WorldEvent::UnitPoints {
            obj: 0x400,
            hp: 0,
            mp: 50,
        }));
        assert_eq!(s.watch.death.points_zero_ms, 10_000);
        assert_eq!(
            s.watch.death.check(10_500, s.me.dead, s.me.respawn_at_ms),
            None
        );
        let late = 10_000 + DEATH_PACKET_GRACE_MS + 1;
        assert_eq!(
            s.watch.death.check(late, s.me.dead, s.me.respawn_at_ms),
            Some(Death::NoDeathPacket)
        );
        assert_eq!(
            s.watch
                .death
                .check(late + 1_000, s.me.dead, s.me.respawn_at_ms),
            None,
            "once per episode"
        );

        // the packet follows right away: nothing to report, and the requests of a dead bot go
        let mut s = in_world();
        s.watch.tracker.expect(Pending {
            req: Req::SkillCast,
            id: 1,
            aux: 0,
            sent_ms: 9_000,
            wait_ms: 6_000,
            pos: [0.0; 2],
        });
        s.now_ms = 10_000;
        s.apply(&world(WorldEvent::UnitPoints {
            obj: 0x400,
            hp: 0,
            mp: 50,
        }));
        s.now_ms = 10_100;
        s.apply(&world(WorldEvent::UnitDeath {
            obj: 0x400,
            killer: 0x999,
            respawn_wait_ms: 5_000,
            lost_exp: 0,
            reason: 0,
        }));
        assert_eq!(s.watch.death.points_zero_ms, 0);
        assert_eq!(
            s.watch.death.check(61_000, s.me.dead, s.me.respawn_at_ms),
            Some(Death::NeverResurrected),
            "the bot died properly but was never revived, a different finding"
        );
        assert_eq!(s.watch.tracker.len(), 0, "the dead bot waits for nothing");
    }

    #[test]
    fn a_bot_found_dead_at_login_is_noted_and_a_revival_forgets_it() {
        use crate::bot::watch::{Pending, Req};
        let mut s = BotState::new();
        s.now_ms = 3_000;
        let mut own = own_state();
        own.hp = 0;
        s.apply(&world(WorldEvent::CharacterState(Box::new(own))));
        assert!(s.me.dead);
        assert_eq!(s.watch.death.login_dead_ms, 3_000);
        s.watch.tracker.expect(Pending {
            req: Req::QuestAccept,
            id: 4,
            aux: 0,
            sent_ms: 3_000,
            wait_ms: 6_000,
            pos: [0.0; 2],
        });
        // alive again: the episode is over
        s.now_ms = 9_000;
        s.apply(&world(WorldEvent::UnitPoints {
            obj: 0x400,
            hp: 500,
            mp: 50,
        }));
        s.apply(&world(WorldEvent::UnitState(Box::new(unit_info(
            0x400,
            unit_type::CHARACTER,
            500,
        )))));
        assert!(!s.me.dead);
        assert_eq!(s.watch.death.login_dead_ms, 0);
    }

    #[test]
    fn a_finished_trade_is_audited_against_both_offers() {
        use crate::bot::goals::trade::{Put, Window};
        use crate::bot::watch::classify::TradeFault;
        let task = |changes: Vec<ItemChange>| ActivityEvent::TradeMade {
            action: 0,
            tasks: changes.len() as u8,
            items: Box::new(ItemTaskInfo {
                reason: 0,
                changes: changes.into_iter().collect(),
                force_removed: Default::default(),
            }),
        };
        // the bot puts up 1 gold, the partner 3 items of template 55; both arrive correctly
        let window = || {
            let mut w = Window::new(0x777, 0);
            w.mine.money = 10_000;
            w.other.items.push(Put {
                slot: 0,
                template: 55,
                amount: 3,
            });
            Box::new(w)
        };
        let good = vec![
            ItemChange::Money(-10_000),
            ItemChange::Added {
                action: 5,
                slot_type: SLOT_BAG,
                slot: 4,
                template: 55,
                id: 90,
                grade: 0,
                count: 3,
            },
        ];
        let mut s = in_world();
        s.trade.window = Some(window());
        s.apply(&ServerEvent::Activity(task(good.clone())));
        assert!(!s.watch.has_findings(), "a right trade is not a finding");

        // the items never come
        let mut s = in_world();
        s.trade.window = Some(window());
        s.apply(&ServerEvent::Activity(task(vec![ItemChange::Money(
            -10_000,
        )])));
        let found = s.watch.take_findings();
        assert_eq!(
            found.as_slice(),
            &[Finding::Trade(TradeFault::ItemsIn {
                template: 55,
                expected: 3,
                actual: 0
            })]
        );

        // the purse moved by another amount
        let mut s = in_world();
        s.trade.window = Some(window());
        let mut wrong = good;
        wrong[0] = ItemChange::Money(-9_000);
        s.apply(&ServerEvent::Activity(task(wrong)));
        let found = s.watch.take_findings();
        assert!(matches!(
            found.as_slice(),
            [Finding::Trade(TradeFault::Money {
                expected: -10_000,
                actual: -9_000
            })]
        ));

        // an item task that did not parse is nothing to compare, and neither is a trade the
        // bot saw no window for
        let mut s = in_world();
        s.trade.window = Some(window());
        s.apply(&ServerEvent::Activity(task(Vec::new())));
        let mut s2 = in_world();
        s2.apply(&ServerEvent::Activity(task(vec![ItemChange::Money(5)])));
        assert!(!s.watch.has_findings() && !s2.watch.has_findings());
    }

    #[test]
    fn item_tasks_feed_the_provenance_ledger() {
        use crate::bot::market::ledger::reason;
        let task = |reason: u8, changes: Vec<ItemChange>| {
            world(WorldEvent::ItemTask(Box::new(ItemTaskInfo {
                reason,
                changes: changes.into_iter().collect(),
                force_removed: Default::default(),
            })))
        };
        let added = |slot: u8, id: u64, count: i32| ItemChange::Added {
            action: 5,
            slot_type: SLOT_BAG,
            slot,
            template: 3411,
            id,
            grade: 0,
            count,
        };
        let mut s = in_world();
        assert!(s.market.get().is_none(), "no memory before a gain");
        // looted ore is the bot's own, GM-made ore (a second stack) is not
        s.apply(&task(reason::LOOT, vec![added(0, 50, 12)]));
        s.apply(&task(reason::GM, vec![added(1, 51, 100)]));
        assert_eq!(s.item_count(3411), 112);
        assert_eq!(s.market.clean_count(3411), 12);
        // gathering more merges into the first stack
        s.apply(&task(
            reason::SKILL_EFFECT_GAIN_ITEM,
            vec![ItemChange::CountUpdate {
                slot_type: SLOT_BAG,
                slot: 0,
                id: 50,
                delta: 3,
                template: 3411,
            }],
        ));
        assert_eq!(s.market.clean_count(3411), 15);
        // whatever leaves the bag leaves the ledger, however it leaves
        s.apply(&task(
            reason::STORE_SELL,
            vec![ItemChange::CountUpdate {
                slot_type: SLOT_BAG,
                slot: 1,
                id: 51,
                delta: -90,
                template: 3411,
            }],
        ));
        assert_eq!(s.item_count(3411), 25);
        assert_eq!(s.market.clean_count(3411), 0, "the honest part goes first");
        // a stack that is removed whole
        s.apply(&task(reason::QUEST_COMPLETE, vec![added(2, 52, 4)]));
        assert_eq!(s.market.clean_count(3411), 4);
        s.apply(&task(
            reason::MAIL,
            vec![ItemChange::Removed {
                slot_type: SLOT_BAG,
                slot: 2,
                id: 52,
                template: 3411,
            }],
        ));
        assert_eq!(s.market.clean_count(3411), 0);
        // a bag that the server resends is the truth
        s.apply(&task(reason::LOOT, vec![added(3, 53, 6)]));
        assert_eq!(s.market.clean_count(3411), 6);
        s.apply(&world(WorldEvent::Inventory {
            slot_type: SLOT_BAG,
            chunk: 0,
            items: Box::new([InvItem {
                slot: 3,
                id: 53,
                template: 3411,
                grade: 0,
                count: 2,
            }]),
        }));
        assert_eq!(s.market.clean_count(3411), 2);
        // and a new connection forgets all of it
        s.reset();
        assert!(s.market.get().is_none());
    }

    #[test]
    fn reset_frees_everything() {
        let mut s = in_world();
        for i in 0..200u32 {
            s.apply(&world(WorldEvent::UnitState(Box::new(unit_info(
                0x1000 + i,
                unit_type::NPC,
                1,
            )))));
        }
        assert_eq!(s.units.len(), 200);
        s.reset();
        assert!(s.units.is_empty() && s.units.capacity() == 0);
        assert_eq!(s.me.obj, 0);
    }
}
