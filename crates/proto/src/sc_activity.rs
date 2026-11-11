// Packet layouts ported from AAEmu-1.2-Rust rust/crates/aaemu-game/src/{systems/quests,
// systems/social, systems/economy, systems/housing}/ and models/{char_data,house,item}.rs
// (GPL-3.0-or-later).
//! Server → client parsers for quests, duels, player trade, teams, housing, mail and specialty
//! trade (see [`crate::sc`]).
//!
//! [`parse`] matches on the opcode first and only then copies the body into a
//! [`PacketStream`], so packets bots do not care about cost one `match`. All packets here travel
//! on the game channel (level 1); other levels are ignored. Every count read from the wire is
//! checked against the bytes left before anything is allocated.
//!
//! Writer citations in the variant docs are `file:line` below `rust/crates/aaemu-game/src/` of
//! AAEmu-1.2-Rust, abbreviated `G/`.
//!
//! The item tasks inside `SCTradeMade` are parsed with the world parser's item task reader
//! ([`ActivityEvent::TradeMade`]`::items`).
//!
//! Not parsed on purpose:
//! - `SCCraftFailed` (0x1bf): defined in G/systems/economy/craft.rs:1050 but never sent, a craft
//!   ends with the skill/item-task packets of the world parser and failures are `SCErrorMsg`;
//! - the specialty sale: `CSSellBackpackGoods` answers with the backpack item task (world
//!   parser, task type `SELL_BACKPACK`), a labor change and the payment mail
//!   ([`ActivityEvent::MailReceived`] with type [`mail_type::SYS_SELL_BACKPACK`]).

use smallvec::SmallVec;

use crate::opcodes::sc;
use crate::pos::convert_long_to_coord;
use crate::sc_world::ItemTaskInfo;
use crate::stream::{MarshalError, PacketStream};

/// Game channel level (`LEVEL_PLAIN`): every packet parsed here is sent on it.
const LEVEL_PLAIN: u8 = 1;

/// Quest status values of [`QuestRow::status`] (G/systems/quests/model.rs:42-50).
pub mod quest_status {
    /// The quest is running.
    pub const PROGRESS: u8 = 1;
    /// The quest failed.
    pub const FAILED: u8 = 2;
    /// All objectives are done, the quest can be turned in.
    pub const READY: u8 = 3;
    /// The quest was dropped.
    pub const DROPPED: u8 = 4;
    /// The quest is completed.
    pub const COMPLETED: u8 = 5;
    /// A daily quest is completed.
    pub const DAILY_COMPLETED: u8 = 6;
}

/// `det` values of [`ActivityEvent::DuelEnded`] (G/systems/social/duel.rs:54-60).
pub mod duel_det {
    /// The receiver lost (not sent by the server today; it uses `WIN` with swapped ids).
    pub const LOSE: u8 = 0;
    /// `winner_*` won against `loser_*` (HP reached 1 or the timer ran out).
    pub const WIN: u8 = 1;
    /// The loser fled beyond the flag border.
    pub const SURRENDER: u8 = 2;
    /// Nobody won.
    pub const DRAW: u8 = 3;
}

/// A few `MailType` values of [`MailHeader::mail_type`] (G/systems/economy/mail.rs:39-84).
pub mod mail_type {
    /// Player mail.
    pub const NORMAL: u8 = 1;
    /// Express player mail.
    pub const EXPRESS: u8 = 2;
    /// Bill mail.
    pub const BILLING: u8 = 6;
    /// Charged (cash on delivery) mail; its body is sent with the notification.
    pub const CHARGED: u8 = 9;
    /// System express mail.
    pub const SYS_EXPRESS: u8 = 18;
    /// The payment of a sold trade pack (`CSSellBackpackGoods`).
    pub const SYS_SELL_BACKPACK: u8 = 19;
    /// House sale payment.
    pub const HOUSING_SALE: u8 = 21;
}

/// One active quest as written by `Quest.Write` (G/models/char_data.rs:458-482).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuestRow {
    /// Quest instance id (`long`).
    pub id: i64,
    /// Quest template (context) id, what the client calls the quest id.
    pub template: u32,
    /// One of [`quest_status`].
    pub status: u8,
    /// Objective counters.
    pub objectives: [i32; 5],
    /// Milliseconds until the time limit, `-1` without one.
    pub left_time_ms: i32,
    /// Current component (`0` without a time limit).
    pub component: u32,
    /// `QuestAcceptorType` (npc, doodad, item, ...).
    pub acceptor_type: u8,
    /// Template id of the acceptor.
    pub acceptor_id: u32,
}

/// The leading fields of `Item.Write` (G/models/item.rs:1061-1082); the class-specific details
/// and the trailing times are skipped.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ItemBrief {
    /// Item template id; `0` is an empty slot (nothing else follows on the wire).
    pub template_id: u32,
    /// Item instance id.
    pub id: u64,
    /// Grade.
    pub grade: u8,
    /// `ItemFlag` bits.
    pub flags: u8,
    /// Stack size.
    pub count: i32,
}

/// A team member as written by `TeamMember.Write` (G/systems/social/packets/team.rs:35-43).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamMemberBrief {
    /// Character id.
    pub char_id: u32,
    /// Character name.
    pub name: Box<str>,
    /// Level.
    pub level: u8,
    /// Team role.
    pub role: u8,
    /// Object id (`0` when offline).
    pub obj_id: u32,
}

/// A team member as written by `TeamMember.WritePerson` (G/systems/social/packets/team.rs:46-66,
/// 50 bytes): where the member is and how it fares. The server sends it for members out of the
/// render distance of each other and whenever a member moves (`SCTeamRemoteMembersEx`), and when a
/// member goes offline (`SCTeamMemberDisconnected`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TeamPerson {
    /// Character id.
    pub char_id: u32,
    /// Zone key of the member's transform (the zone it stands in in the main world, the
    /// dungeon's zone key in an instance: what `SCLoadInstance` carries).
    pub zone_id: u32,
    /// Instance id of the member's world (`0` in the main world).
    pub instance_id: u32,
    /// Level.
    pub level: u8,
    /// Hit points.
    pub hp: i32,
    /// Maximum hit points.
    pub hp_max: i32,
    /// Mana.
    pub mp: i32,
    /// Maximum mana.
    pub mp_max: i32,
    /// Position in the member's own world (instance-local inside an instance).
    pub pos: [f32; 3],
    /// Object id; `0` when the member is offline.
    pub obj_id: u32,
}

/// A house as written by `House.Write` (G/models/house.rs:95-131): the body of `SCMyHouse`,
/// `SCHouseState` and (after the owner id) `SCLoginCharInfoHouse`.
#[derive(Debug, Clone, PartialEq)]
pub struct HouseInfo {
    /// Telescope (`tl`) id the house packets use.
    pub tl: u16,
    /// House id.
    pub id: u32,
    /// Object id.
    pub obj: u32,
    /// House template id (the design).
    pub template: u32,
    /// Current model id.
    pub model: u32,
    /// Co-owner character id.
    pub co_owner: u32,
    /// Owner character id.
    pub owner: u32,
    /// Owner name.
    pub owner_name: Box<str>,
    /// Owner account id.
    pub account: u32,
    /// Permission (private, public, ...).
    pub permission: u8,
    /// Build steps in total (`0` once finished).
    pub all_action: i32,
    /// Build steps done (`0` once finished).
    pub cur_action: i32,
    /// Tax.
    pub tax: i32,
    /// World x.
    pub x: f32,
    /// World y.
    pub y: f32,
    /// World z.
    pub z: f32,
    /// House name.
    pub name: Box<str>,
    /// Whether the owner allows recovery.
    pub allow_recover: bool,
    /// Sell price (`0` when not for sale).
    pub sell_price: u32,
    /// Character id the house is offered to.
    pub sell_to_id: u32,
    /// Name of that character.
    pub sell_to_name: Box<str>,
}

/// A mail header as written by `MailHeader.Write` (G/systems/economy/mail.rs:357-383).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailHeader {
    /// Mail id.
    pub mail_id: i64,
    /// One of [`mail_type`] (the table is longer).
    pub mail_type: u8,
    /// `MailStatus` (0 unread, 1 read, ...).
    pub status: u8,
    /// Title.
    pub title: Box<str>,
    /// Sender name.
    pub sender: Box<str>,
    /// Number of attachments (items; money is in the body).
    pub attachments: u8,
    /// Receiver name.
    pub receiver: Box<str>,
    /// Unix seconds the mail was opened, `0` if never.
    pub open_date: i64,
    /// Whether the mail was returned.
    pub returned: bool,
    /// Extra value (`MailHeader.Extra`).
    pub extra: i64,
}

/// A mail body as written by `MailBody.Write` (G/systems/economy/mail.rs:386-424).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailBodyInfo {
    /// Mail id.
    pub mail_id: i64,
    /// Mail type.
    pub mail_type: u8,
    /// Receiver name.
    pub receiver: Box<str>,
    /// Title.
    pub title: Box<str>,
    /// Text.
    pub text: Box<str>,
    /// Attached money in copper.
    pub money: i32,
    /// Amount to pay for a charged or billing mail.
    pub billing: i32,
    /// Second money amount (`MoneyAmount2`).
    pub money2: i32,
    /// Unix seconds sent.
    pub send_date: i64,
    /// Unix seconds received.
    pub recv_date: i64,
    /// Unix seconds opened.
    pub open_date: i64,
    /// The non-empty attachments, in slot order.
    pub items: Vec<ItemBrief>,
}

/// One taken attachment of [`ActivityEvent::AttachmentTaken`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TakenItem {
    /// Item instance id.
    pub id: u64,
    /// Inventory slot type it went to.
    pub slot_type: u8,
    /// Slot index.
    pub slot: u8,
}

/// A parsed quest, duel, trade, team, housing, mail or specialty packet. Variant docs name the
/// server writer (`G/` = `rust/crates/aaemu-game/src/` of AAEmu-1.2-Rust).
#[derive(Debug, Clone, PartialEq)]
pub enum ActivityEvent {
    // ---- quests -------------------------------------------------------------------------
    /// `SCQuests`: a batch of active quests (the login sends them, 20 per packet): `i32 n`,
    /// then `n` rows. G/packets/world.rs:112-130, rows G/models/char_data.rs:458-482.
    Quests {
        /// The quests in this packet.
        list: Vec<QuestRow>,
    },
    /// `SCCompletedQuests`: `i32 n`, then `n` blocks of `u16 block id` + 64-bit little-endian
    /// mask; bit `i` of block `b` marks quest `b * 64 + i` as completed. `ids` is the expanded
    /// list. G/packets/world.rs:132-154 (bit order G/systems/quests/engine.rs:2286-2325). The
    /// login sends them 200 blocks per packet; later packets add to the earlier ones.
    CompletedQuests {
        /// Completed quest template ids in this packet, ascending.
        ids: Vec<u32>,
    },
    /// `SCQuestContextStarted`: a quest was accepted. `Quest.Write` row, `u32 component`.
    /// G/systems/quests/packets.rs:22-40.
    QuestStarted {
        /// The accepted quest.
        quest: Box<QuestRow>,
        /// First component of the quest.
        component: u32,
    },
    /// `SCQuestContextUpdated`: progress, step or status change (also a drop, status
    /// [`quest_status::DROPPED`]). Quest row, `u32 component`, `4 x i32 p`.
    /// G/systems/quests/packets.rs:42-73.
    QuestUpdated {
        /// The quest after the change.
        quest: Box<QuestRow>,
        /// Component that changed (`0` for a plain counter update).
        component: u32,
        /// Component parameters.
        p: [i32; 4],
    },
    /// `SCQuestContextCompleted`: the quest was turned in. `u32 quest`, 8-byte mask of its
    /// completed block, `u32 component`. G/systems/quests/packets.rs:75-93, sent at
    /// G/systems/quests/engine.rs:616.
    QuestCompleted {
        /// Quest template id.
        quest_id: u32,
        /// Component (always 0 from the server).
        component: u32,
        /// Completed mask of the block `quest_id / 64`.
        block_mask: u64,
    },
    /// `SCQuestContextReset`: a repeatable quest can be taken again (its completed bit was
    /// cleared). `u32 quest`, 8-byte block mask, `u32 block id`. G/systems/quests/packets.rs:95-113,
    /// sent at G/systems/quests/engine.rs:1994.
    QuestReset {
        /// Quest template id.
        quest_id: u32,
        /// Block id (`quest_id / 64`; the writer reuses the component field for it).
        block_id: u32,
        /// Completed mask of the block after the reset.
        block_mask: u64,
    },
    /// `SCQuestContextFailed`: the server refused a quest action. `u32 quest`, `u8 err`
    /// (error codes: G/systems/quests/packets.rs:115-129). G/systems/quests/packets.rs:131-149.
    QuestFailed {
        /// Quest template id.
        quest_id: u32,
        /// Reason byte.
        err: u8,
    },
    /// `SCQuestUnitReqFailed`: a quest start component's unit requirement refused the request.
    /// `u32 quest`, 3-byte `bc` unit object id, a flag byte, then the set parts in order:
    /// `u8 kind` (bit 0x01), `u16 message id` (bit 0x02), `u32 value` (bit 0x04). A zero part is
    /// not written. G/systems/quests/packets.rs:151-206 (the 1.2 client reader `FUN_397b5d00`).
    QuestUnitReqFailed {
        /// Quest template id.
        quest_id: u32,
        /// Object id of the unit the requirement names (the local player for a start request).
        unit_obj: u32,
        /// `unit_reqs.kind_id` (0 when not sent).
        kind: u8,
        /// System message id (0 when not sent).
        message_id: u16,
        /// Requirement value (0 when not sent); for `kind` 0x25 it is the quest that must be in
        /// progress.
        value: u32,
    },
    /// `SCQuestRewardedByMail`: the rewards of these quests came by mail (inventory full).
    /// `u8 n`, `n x u32`. G/systems/quests/packets.rs:151-168.
    QuestRewardedByMail {
        /// Quest template ids.
        quests: SmallVec<[u32; 4]>,
    },
    /// `SCDoodadQuestAccept`: a doodad offers a quest. `bc doodad`, `u32 quest`.
    /// G/systems/quests/packets.rs:170-185.
    DoodadQuestAccept {
        /// Object id of the doodad.
        doodad_obj: u32,
        /// Quest template id.
        quest_id: u32,
    },

    // ---- duel ---------------------------------------------------------------------------
    /// `SCDuelChallenged`: another character challenged us. `u32 challenger character id`.
    /// G/systems/social/packets/duel.rs:9-15.
    DuelChallenged {
        /// Character id of the challenger.
        challenger_char_id: u32,
    },
    /// `SCDuelStartCountdown`: the 3 s countdown started (no body).
    /// G/systems/social/packets/duel.rs:35-38.
    DuelStartCountdown,
    /// `SCDuelStarted`: the duel is on. `bc challenger`, `bc challenged`.
    /// G/systems/social/packets/duel.rs:40-45.
    DuelStarted {
        /// Object id of the challenger.
        challenger_obj: u32,
        /// Object id of the challenged.
        challenged_obj: u32,
    },
    /// `SCDuelEnded`: `u32, u32, bc, bc, u8 det`. The server fills the first slots with the
    /// winner and the second with the loser (the writer calls them challenger/challenged); for
    /// [`duel_det::DRAW`] it sends (peer, receiver) and winner/loser carry no meaning.
    /// G/systems/social/packets/duel.rs:17-33, written at G/systems/social/duel.rs:679-725.
    DuelEnded {
        /// Character id of the winner.
        winner_id: u32,
        /// Character id of the loser.
        loser_id: u32,
        /// Object id of the winner.
        winner_obj: u32,
        /// Object id of the loser.
        loser_obj: u32,
        /// One of [`duel_det`].
        det: u8,
    },
    /// `SCDuelState`: `bc unit`, `bc flag` (`0` = the unit is no longer in a duel).
    /// G/systems/social/packets/duel.rs:47-52.
    DuelState {
        /// Object id of the duelist.
        obj: u32,
        /// Object id of the duel flag, `0` when the duel ended.
        flag_obj: u32,
    },

    // ---- player trade -------------------------------------------------------------------
    /// `SCCanStartTrade`: someone asks us to trade. `bc requester`.
    /// G/systems/economy/trade.rs:931-945.
    CanStartTrade {
        /// Object id of the requester.
        requester_obj: u32,
    },
    /// `SCCannotStartTrade`: `bc obj`, `i32 reason`. G/systems/economy/trade.rs:947-962.
    CannotStartTrade {
        /// Object id of the other character.
        obj: u32,
        /// Reason code.
        reason: i32,
    },
    /// `SCTradeStarted`: the trade window opened. `bc peer`. G/systems/economy/trade.rs:964-978.
    TradeStarted {
        /// Object id of the trade partner.
        peer_obj: u32,
    },
    /// `SCTradeCanceled`: `i32 reason`, `bool caused_by_me`. G/systems/economy/trade.rs:980-995.
    TradeCanceled {
        /// Reason code.
        reason: i32,
        /// Whether we caused the cancel.
        caused_by_me: bool,
    },
    /// `SCTradeItemPutup`: our own put-up was accepted. `u8 slot type`, `u8 slot`, `i32 amount`.
    /// G/systems/economy/trade.rs:997-1015.
    TradeItemPutup {
        /// Slot type of our item.
        slot_type: u8,
        /// Slot of our item.
        slot: u8,
        /// Units offered.
        amount: i32,
    },
    /// `SCOtherTradeItemPutup`: the partner put an item up (an `Item.Write` snapshot with the
    /// offered count). G/systems/economy/trade.rs:1017-1031.
    OtherTradeItemPutup {
        /// The offered item.
        item: ItemBrief,
    },
    /// `SCTradeItemTookdown`: our own take-down. `u8 slot type`, `u8 slot`.
    /// G/systems/economy/trade.rs:1033-1048.
    TradeItemTookdown {
        /// Slot type of our item.
        slot_type: u8,
        /// Slot of our item.
        slot: u8,
    },
    /// `SCOtherTradeItemTookdown`: the partner took an item down (`Item.Write` snapshot).
    /// G/systems/economy/trade.rs:1050-1064.
    OtherTradeItemTookdown {
        /// The removed item.
        item: ItemBrief,
    },
    /// `SCTradeMoneyPutup`: our money offer. `i32 amount`. G/systems/economy/trade.rs:1066-1080.
    TradeMoneyPutup {
        /// Copper offered.
        amount: i32,
    },
    /// `SCOtherTradeMoneyPutup`: the partner's money offer. `i32 amount`.
    /// G/systems/economy/trade.rs:1082-1096.
    OtherTradeMoneyPutup {
        /// Copper offered.
        amount: i32,
    },
    /// `SCTradeLockUpdate`: `bool mine`, `bool other`. G/systems/economy/trade.rs:1098-1113.
    TradeLockUpdate {
        /// Our offer is locked.
        mine: bool,
        /// The partner's offer is locked.
        other: bool,
    },
    /// `SCTradeOkUpdate`: `bool mine`, `bool other`. G/systems/economy/trade.rs:1115-1130.
    TradeOkUpdate {
        /// We confirmed.
        mine: bool,
        /// The partner confirmed.
        other: bool,
    },
    /// `SCTradeMade`: the trade completed. `u8 action`, `u8 n`, `n` item tasks, `u8 force-removed
    /// count`, ids, `u32`. G/systems/economy/trade.rs:1132-1157.
    TradeMade {
        /// Item task type (`TRADE`).
        action: u8,
        /// Number of item tasks in the packet.
        tasks: u8,
        /// The item tasks (the bag and the purse change with the trade), in the layout of
        /// `SCItemTaskSuccess`. Empty when the body after the head does not parse, so the
        /// completion is still reported.
        items: Box<ItemTaskInfo>,
    },

    // ---- team ---------------------------------------------------------------------------
    /// `SCAskToJoinTeam`: an invitation. `u32 team`, `u32 sender`, string name, `bool party`.
    /// G/systems/social/packets/team.rs:106-114.
    AskToJoinTeam {
        /// Team id (`0` for a team that does not exist yet).
        team_id: u32,
        /// Character id of the inviter.
        sender_char_id: u32,
        /// Name of the inviter.
        name: Box<str>,
        /// Party (`true`) or raid (`false`).
        is_party: bool,
    },
    /// `SCJoinedTeam`: we joined a team. `Team.Write` (id, owner, party flag, 10 party counts,
    /// 50 member slots, 12 marks, looting rule) followed by one `TeamMember.Write` per member.
    /// G/systems/social/packets/team.rs:127-135, head G/systems/social/packets/team.rs:82-104.
    JoinedTeam {
        /// Team id.
        team_id: u32,
        /// Character id of the team owner.
        owner_id: u32,
        /// Party (`true`) or raid (`false`).
        is_party: bool,
        /// All members including us.
        members: Box<[TeamMemberBrief]>,
    },
    /// `SCTeamMemberJoined`: `u32 team`, `TeamMember.Write`, `i32 party`.
    /// G/systems/social/packets/team.rs:225-232.
    TeamMemberJoined {
        /// Team id.
        team_id: u32,
        /// The new member.
        member: TeamMemberBrief,
        /// Party index the member was put in.
        party: i32,
    },
    /// `SCTeamMemberLeaved`: `u32 team`, `u32 character`, `bool kicked`.
    /// G/systems/social/packets/team.rs:234-241.
    TeamMemberLeft {
        /// Team id.
        team_id: u32,
        /// Character id of the member that left.
        char_id: u32,
        /// Whether the member was kicked.
        kicked: bool,
    },
    /// `SCLeavedTeam`: we left the team. `u32 team`, `bool kicked`, `bool dismissed`.
    /// G/systems/social/packets/team.rs:137-144.
    LeftTeam {
        /// Team id.
        team_id: u32,
        /// We were kicked.
        kicked: bool,
        /// The team was dismissed.
        dismissed: bool,
    },
    /// `SCTeamDismissed`: `u32 team`. G/systems/social/packets/team.rs:201-206.
    TeamDismissed {
        /// Team id.
        team_id: u32,
    },
    /// `SCRejectedTeam`: our invitation was declined. String name, `bool party`.
    /// G/systems/social/packets/team.rs:169-174.
    RejectedTeam {
        /// Name of the character that declined (or the unknown target).
        name: Box<str>,
        /// Party (`true`) or raid (`false`).
        party: bool,
    },
    /// `SCTeamOwnerChanged`: `u32 team`, `u32 new owner character`.
    /// G/systems/social/packets/team.rs:263-268.
    TeamOwnerChanged {
        /// Team id.
        team_id: u32,
        /// Character id of the new owner.
        owner_id: u32,
    },
    /// `SCTeamRemoteMembersEx` (0xe4): `i32 count` and one `TeamMember.WritePerson` each. Sent
    /// to the others whenever a member moves, and for members out of each other's render distance
    /// on change (500 ms) and at least every 5 s. G/systems/social/packets/team.rs:282-288,
    /// G/systems/social/team.rs:1241-1253 and :1310-1366.
    TeamRemoteMembers {
        /// The members the packet describes.
        members: Box<[TeamPerson]>,
    },
    /// `SCTeamMemberDisconnected` (0xdc): `u32 team`, `u32 character`, `WritePerson`. A member went
    /// offline (an offline owner is replaced first). G/systems/social/packets/team.rs:218-224.
    TeamMemberDisconnected {
        /// Team id.
        team_id: u32,
        /// Character id of the member that went offline.
        char_id: u32,
        /// The member's last state (object id `0`).
        person: Box<TeamPerson>,
    },
    /// `SCTeamMemberRoleChanged` (0xde): `u32 team`, `u32 character`, `u8 role` (1 tank,
    /// 2 healer, 3 attacker). G/systems/social/packets/team.rs:255-262.
    TeamMemberRoleChanged {
        /// Team id.
        team_id: u32,
        /// Character id.
        char_id: u32,
        /// The new role.
        role: u8,
    },
    /// `SCRefreshTeamMember` (0xe3): `u32 team`, `u32 character`, `bc object id`: the object id of
    /// a member that logged in or joined. G/systems/social/packets/team.rs:161-168.
    TeamRefreshMember {
        /// Team id.
        team_id: u32,
        /// Character id.
        char_id: u32,
        /// The member's object id.
        obj_id: u32,
    },
    /// `SCTeamLootingRuleChanged` (0xe2): `u32 team`, `LootingRule.Write` (`u8 method`,
    /// `u8 minimum grade`, `u32 loot master`, `bool roll for bind-on-pickup`), `u8 change flags`.
    /// G/systems/social/packets/team.rs:209-216.
    TeamLootingRuleChanged {
        /// Team id.
        team_id: u32,
        /// Loot method.
        method: u8,
        /// Minimum grade that is rolled for.
        min_grade: u8,
        /// Character id of the loot master.
        master: u32,
        /// Bind-on-pickup items are rolled for.
        roll_bop: bool,
        /// What the change touched.
        flags: u8,
    },

    // ---- housing ------------------------------------------------------------------------
    /// `SCMyHouse`: a house of ours (sent at login and after building or buying one): the
    /// `House.Write` body. G/systems/housing/packets.rs:177-178, G/models/house.rs:95-131.
    MyHouse(Box<HouseInfo>),
    /// `SCLoginCharInfoHouse`: `u32 owner character`, then the `House.Write` body (character
    /// list). G/systems/housing/packets.rs:179-180, G/systems/housing/ops.rs:55-64.
    LoginCharInfoHouse {
        /// Owner character id.
        owner_id: u32,
        /// The house.
        house: Box<HouseInfo>,
    },
    /// `SCMyHouseRemoved`: one of our houses is gone (demolished or sold). `u16 tl`.
    /// G/systems/housing/packets.rs:171-175.
    MyHouseRemoved {
        /// Telescope id of the house.
        tl: u16,
    },
    /// `SCHouseDemolished`: a house in view was demolished. `u16 tl`.
    /// G/systems/housing/packets.rs:74-78.
    HouseDemolished {
        /// Telescope id of the house.
        tl: u16,
    },
    /// `SCConstructHouseTax`: the answer to `CSConstructHouseTax`.
    /// `u32 design, i32 heavy, i32 normal, bool is_heavy, i32 base, i32 deposit, i32 total`.
    /// G/systems/housing/packets.rs:35-54, sent at G/systems/housing/ops.rs:106.
    ConstructHouseTax {
        /// House design (template) id.
        design: u32,
        /// Heavy-tax houses the account owns.
        heavy_count: i32,
        /// Normal-tax houses the account owns.
        normal_count: i32,
        /// Whether the design is a heavy-tax house.
        heavy: bool,
        /// Base tax.
        base: i32,
        /// Deposit tax.
        deposit: i32,
        /// Total to pay (copper).
        total: i32,
    },
    /// `SCHouseBuildProgress`: `u16 tl, u32 model, i32 all steps, i32 current step`.
    /// G/systems/housing/packets.rs:63-72, sent at G/systems/housing/ops.rs:1727.
    HouseBuildProgress {
        /// Telescope id of the house.
        tl: u16,
        /// Current model id.
        model: u32,
        /// Build steps in total.
        all: i32,
        /// Build steps done.
        cur: i32,
    },
    /// `SCHouseTaxInfo`: `u16 tl, i32 dominion rate, i32 deposit, i32 total, i64 due (unix s),
    /// bool already paid, i32 weeks without pay, bool heavy`.
    /// G/systems/housing/packets.rs:139-161, sent at G/systems/housing/ops.rs:149.
    HouseTaxInfo {
        /// Telescope id of the house.
        tl: u16,
        /// Dominion tax rate.
        dominion_tax_rate: i32,
        /// Deposit amount (`money_amount`).
        deposit: i32,
        /// Total tax to pay (`money_amount2`).
        total: i32,
        /// Tax due date (unix seconds).
        due: i64,
        /// The tax of this period is paid.
        already_paid: bool,
        /// Weeks without payment (`-1` when paid).
        weeks_without_pay: i32,
        /// Whether it is a heavy-tax house.
        heavy: bool,
    },

    // ---- mail ---------------------------------------------------------------------------
    /// `SCCountUnreadMail`: the unread mail counters (login). `4 x i32`: sent, received,
    /// MIA received, commercial received. G/packets/world.rs:179-193 (`CountUnreadMail`
    /// G/models/char_data.rs:552).
    CountUnreadMail {
        /// `[sent, received, mia_received, commercial_received]`.
        unread: [i32; 4],
    },
    /// `SCGotMail`: a new mail arrived (also the payment mail of a sold trade pack). Header,
    /// 4 unread counters, `bool has body`, optional body (only charged mails), `bool cancel`.
    /// G/systems/economy/mail_packets.rs:95-118.
    MailReceived {
        /// The header.
        mail: Box<MailHeader>,
        /// `[sent, received, mia_received, commercial_received]` unread counters.
        unread: [i32; 4],
        /// The mail is withdrawn again.
        is_cancel: bool,
        /// The body, present for charged mails.
        body: Option<Box<MailBodyInfo>>,
    },
    /// `SCMailList`: `bool is_sent`, `i32 n`, `n` headers (a page of the mailbox).
    /// G/systems/economy/mail_packets.rs:120-139.
    MailList {
        /// Sent (`true`) or received (`false`) mails.
        is_sent: bool,
        /// The headers of this page.
        mails: Vec<MailHeader>,
    },
    /// `SCMailListEnd`: the mailbox listing is complete. `i32 headers`, `i32 bodies`.
    /// G/systems/economy/mail_packets.rs:141-156.
    MailListEnd {
        /// Total headers sent.
        total_headers: i32,
        /// Total bodies sent.
        total_bodies: i32,
    },
    /// `SCMailBody`: the answer to `CSReadMail`. `bool prepare`, `bool is_sent`, `MailBody.Write`,
    /// `bool open date modified`, 4 unread counters. G/systems/economy/mail_packets.rs:158-179.
    MailBody {
        /// The mail is only being prepared (not opened yet).
        is_prepare: bool,
        /// Sent (`true`) or received (`false`) mail.
        is_sent: bool,
        /// The body with money and attachments.
        body: Box<MailBodyInfo>,
        /// The open date changed with this read.
        open_date_modified: bool,
        /// Unread counters after the read.
        unread: [i32; 4],
    },
    /// `SCMailSent`: our mail was accepted. Header, then `(u8 slot type, u8 slot)` per attached
    /// item up to the end of the packet. G/systems/economy/mail_packets.rs:75-93.
    MailSent {
        /// The sent mail.
        mail: Box<MailHeader>,
        /// `(slot type, slot)` of the attached items.
        items: Vec<(u8, u8)>,
    },
    /// `SCMailFailed`: sending failed. `u8 err`, `(u8 slot type, u8 slot)` per item, `bool money`
    /// (the items fill the packet up to the last byte). G/systems/economy/mail_packets.rs:53-73.
    MailFailed {
        /// Error code.
        err: u8,
        /// `(slot type, slot)` of the items that failed.
        items: Vec<(u8, u8)>,
        /// The money failed too.
        money: bool,
    },
    /// `SCAttachmentTaken`: the answer to a take-attachment request. `i64 mail`, `bool money`,
    /// `bool aa point`, `bool sequentially`, `u8 n`, 10 fixed `(u64 item, u8 slot type, u8 slot)`
    /// entries of which the first `n` are valid. G/systems/economy/mail_packets.rs:206-238.
    AttachmentTaken {
        /// Mail id.
        mail_id: i64,
        /// The money was taken.
        money: bool,
        /// The AA points were taken.
        aa_point: bool,
        /// Taken one by one (`CSTakeAttachmentSequentially`).
        take_sequentially: bool,
        /// The items that moved to the inventory.
        items: Box<[TakenItem]>,
    },
    /// `SCChargeMoneyPaid`: a charged mail was paid. `i64 mail`.
    /// G/systems/economy/mail_packets.rs:240-254.
    ChargeMoneyPaid {
        /// Mail id.
        mail_id: i64,
    },
    /// `SCMailDeleted`: `bool is_sent`, `i64 mail`, `bool counters modified`, 4 unread counters.
    /// G/systems/economy/mail_packets.rs:256-276.
    MailDeleted {
        /// Sent (`true`) or received (`false`) mail.
        is_sent: bool,
        /// Mail id.
        mail_id: i64,
        /// Whether `unread` changed.
        unread_modified: bool,
        /// Unread counters after the delete.
        unread: [i32; 4],
    },
    /// `SCMailStatusUpdated`: `bool is_sent`, `i64 mail`, `u8 status` (1 = read).
    /// G/systems/economy/mail_packets.rs:296-314.
    MailStatusUpdated {
        /// Sent (`true`) or received (`false`) mail.
        is_sent: bool,
        /// Mail id.
        mail_id: i64,
        /// New `MailStatus`.
        status: u8,
    },

    // ---- specialty trade ----------------------------------------------------------------
    /// `SCSpecialtyRatio`: the price ratio at the current spot (percent). `i32 ratio`.
    /// G/systems/economy/specialty.rs:572-586.
    SpecialtyRatio {
        /// Price ratio in percent.
        ratio: i32,
    },
    /// `SCSpecialtyCurrent`: current rates of a route. `i32 n, u16 from zone group, u16 to zone
    /// group`, `n x (u32 item, u32 rate)`. G/systems/economy/specialty.rs:588-606.
    SpecialtyCurrent {
        /// Origin zone group.
        from_zone_group: u16,
        /// Destination zone group.
        to_zone_group: u16,
        /// `(item template id, rate)` pairs.
        rates: Box<[(u32, u32)]>,
    },
    // ---- auction house -------------------------------------------------------------------
    /// An answer of the auction house (`SCAuctionPosted`, `SCAuctionSearched`, `SCAuctionBid`,
    /// `SCAuctionCanceled`, `SCAuctionLowestPrice`); see [`crate::auction`].
    Auction(Box<crate::auction::AuctionEvent>),
    // ---- guilds --------------------------------------------------------------------------
    /// A packet about guilds (expeditions): the member list, an invitation, a join or leave, a
    /// role change; see [`crate::guild`].
    Guild(Box<crate::guild::GuildEvent>),
}

type Reader = fn(&mut PacketStream) -> Result<ActivityEvent, MarshalError>;

/// Parses one S→C game packet body (opcode already stripped). `Ok(None)` when the packet is not
/// one of this module's or is on another channel level; `Err` when a packet of this module does
/// not match its layout.
pub fn parse(level: u8, opcode: u16, body: &[u8]) -> Result<Option<ActivityEvent>, MarshalError> {
    use ActivityEvent as E;
    if level != LEVEL_PLAIN {
        return Ok(None);
    }
    let read: Reader = match opcode {
        // quests
        sc::SCQuestsPacket => quests,
        sc::SCCompletedQuestsPacket => completed_quests,
        sc::SCQuestContextStartedPacket => |s| {
            Ok(E::QuestStarted {
                quest: Box::new(read_quest_row(s)?),
                component: s.read_u32()?,
            })
        },
        sc::SCQuestContextUpdatedPacket => |s| {
            let quest = Box::new(read_quest_row(s)?);
            let component = s.read_u32()?;
            let p = [s.read_i32()?, s.read_i32()?, s.read_i32()?, s.read_i32()?];
            Ok(E::QuestUpdated {
                quest,
                component,
                p,
            })
        },
        sc::SCQuestContextCompletedPacket => |s| {
            Ok(E::QuestCompleted {
                quest_id: s.read_u32()?,
                block_mask: s.read_u64()?,
                component: s.read_u32()?,
            })
        },
        sc::SCQuestContextResetPacket => |s| {
            Ok(E::QuestReset {
                quest_id: s.read_u32()?,
                block_mask: s.read_u64()?,
                block_id: s.read_u32()?,
            })
        },
        sc::SCQuestContextFailedPacket => |s| {
            Ok(E::QuestFailed {
                quest_id: s.read_u32()?,
                err: s.read_u8()?,
            })
        },
        sc::SCQuestUnitReqFailedPacket => |s| {
            let quest_id = s.read_u32()?;
            let unit_obj = s.read_bc()?;
            let flags = s.read_u8()?;
            let kind = if flags & 0x01 != 0 { s.read_u8()? } else { 0 };
            let message_id = if flags & 0x02 != 0 { s.read_u16()? } else { 0 };
            let value = if flags & 0x04 != 0 { s.read_u32()? } else { 0 };
            Ok(E::QuestUnitReqFailed {
                quest_id,
                unit_obj,
                kind,
                message_id,
                value,
            })
        },
        sc::SCQuestRewardedByMailPacket => |s| {
            let n = usize::from(s.read_u8()?);
            check_count(s, n, 4)?;
            let mut quests = SmallVec::new();
            for _ in 0..n {
                quests.push(s.read_u32()?);
            }
            Ok(E::QuestRewardedByMail { quests })
        },
        sc::SCDoodadQuestAcceptPacket => |s| {
            Ok(E::DoodadQuestAccept {
                doodad_obj: s.read_bc()?,
                quest_id: s.read_u32()?,
            })
        },
        // duel
        sc::SCDuelChallengedPacket => |s| {
            Ok(E::DuelChallenged {
                challenger_char_id: s.read_u32()?,
            })
        },
        sc::SCDuelStartCountdownPacket => |_| Ok(E::DuelStartCountdown),
        sc::SCDuelStartedPacket => |s| {
            Ok(E::DuelStarted {
                challenger_obj: s.read_bc()?,
                challenged_obj: s.read_bc()?,
            })
        },
        sc::SCDuelEndedPacket => |s| {
            Ok(E::DuelEnded {
                winner_id: s.read_u32()?,
                loser_id: s.read_u32()?,
                winner_obj: s.read_bc()?,
                loser_obj: s.read_bc()?,
                det: s.read_u8()?,
            })
        },
        sc::SCDuelStatePacket => |s| {
            Ok(E::DuelState {
                obj: s.read_bc()?,
                flag_obj: s.read_bc()?,
            })
        },
        // trade
        sc::SCCanStartTradePacket => |s| {
            Ok(E::CanStartTrade {
                requester_obj: s.read_bc()?,
            })
        },
        sc::SCCannotStartTradePacket => |s| {
            Ok(E::CannotStartTrade {
                obj: s.read_bc()?,
                reason: s.read_i32()?,
            })
        },
        sc::SCTradeStartedPacket => |s| {
            Ok(E::TradeStarted {
                peer_obj: s.read_bc()?,
            })
        },
        sc::SCTradeCanceledPacket => |s| {
            Ok(E::TradeCanceled {
                reason: s.read_i32()?,
                caused_by_me: s.read_bool()?,
            })
        },
        sc::SCTradeItemPutupPacket => |s| {
            Ok(E::TradeItemPutup {
                slot_type: s.read_u8()?,
                slot: s.read_u8()?,
                amount: s.read_i32()?,
            })
        },
        sc::SCOtherTradeItemPutupPacket => |s| {
            Ok(E::OtherTradeItemPutup {
                item: read_item(s)?,
            })
        },
        sc::SCTradeItemTookdownPacket => |s| {
            Ok(E::TradeItemTookdown {
                slot_type: s.read_u8()?,
                slot: s.read_u8()?,
            })
        },
        sc::SCOtherTradeItemTookdownPacket => |s| {
            Ok(E::OtherTradeItemTookdown {
                item: read_item(s)?,
            })
        },
        sc::SCTradeMoneyPutupPacket => |s| {
            Ok(E::TradeMoneyPutup {
                amount: s.read_i32()?,
            })
        },
        sc::SCOtherTradeMoneyPutupPacket => |s| {
            Ok(E::OtherTradeMoneyPutup {
                amount: s.read_i32()?,
            })
        },
        sc::SCTradeLockUpdatePacket => |s| {
            Ok(E::TradeLockUpdate {
                mine: s.read_bool()?,
                other: s.read_bool()?,
            })
        },
        sc::SCTradeOkUpdatePacket => |s| {
            Ok(E::TradeOkUpdate {
                mine: s.read_bool()?,
                other: s.read_bool()?,
            })
        },
        sc::SCTradeMadePacket => |s| {
            let start = s.pos();
            let action = s.read_u8()?;
            let tasks = s.read_u8()?;
            s.set_pos(start);
            let items = crate::sc_world::read_item_task_info(s).unwrap_or_default();
            Ok(E::TradeMade {
                action,
                tasks,
                items: Box::new(items),
            })
        },
        // team
        sc::SCAskToJoinTeamPacket => |s| {
            Ok(E::AskToJoinTeam {
                team_id: s.read_u32()?,
                sender_char_id: s.read_u32()?,
                name: read_str(s)?,
                is_party: s.read_bool()?,
            })
        },
        sc::SCJoinedTeamPacket => joined_team,
        sc::SCTeamMemberJoinedPacket => |s| {
            Ok(E::TeamMemberJoined {
                team_id: s.read_u32()?,
                member: read_team_member(s)?,
                party: s.read_i32()?,
            })
        },
        sc::SCTeamMemberLeavedPacket => |s| {
            Ok(E::TeamMemberLeft {
                team_id: s.read_u32()?,
                char_id: s.read_u32()?,
                kicked: s.read_bool()?,
            })
        },
        sc::SCLeavedTeamPacket => |s| {
            Ok(E::LeftTeam {
                team_id: s.read_u32()?,
                kicked: s.read_bool()?,
                dismissed: s.read_bool()?,
            })
        },
        sc::SCTeamDismissedPacket => |s| {
            Ok(E::TeamDismissed {
                team_id: s.read_u32()?,
            })
        },
        sc::SCRejectedTeamPacket => |s| {
            Ok(E::RejectedTeam {
                name: read_str(s)?,
                party: s.read_bool()?,
            })
        },
        sc::SCTeamOwnerChangedPacket => |s| {
            Ok(E::TeamOwnerChanged {
                team_id: s.read_u32()?,
                owner_id: s.read_u32()?,
            })
        },
        sc::SCTeamRemoteMembersExPacket => team_remote_members,
        sc::SCTeamMemberDisconnectedPacket => |s| {
            Ok(E::TeamMemberDisconnected {
                team_id: s.read_u32()?,
                char_id: s.read_u32()?,
                person: Box::new(read_team_person(s)?),
            })
        },
        sc::SCTeamMemberRoleChangedPacket => |s| {
            Ok(E::TeamMemberRoleChanged {
                team_id: s.read_u32()?,
                char_id: s.read_u32()?,
                role: s.read_u8()?,
            })
        },
        sc::SCRefreshTeamMemberPacket => |s| {
            Ok(E::TeamRefreshMember {
                team_id: s.read_u32()?,
                char_id: s.read_u32()?,
                obj_id: s.read_bc()?,
            })
        },
        sc::SCTeamLootingRuleChangedPacket => |s| {
            Ok(E::TeamLootingRuleChanged {
                team_id: s.read_u32()?,
                method: s.read_u8()?,
                min_grade: s.read_u8()?,
                master: s.read_u32()?,
                roll_bop: s.read_bool()?,
                flags: s.read_u8()?,
            })
        },
        // housing
        sc::SCMyHousePacket => |s| Ok(E::MyHouse(Box::new(HouseInfo::read(s)?))),
        sc::SCLoginCharInfoHousePacket => |s| {
            Ok(E::LoginCharInfoHouse {
                owner_id: s.read_u32()?,
                house: Box::new(HouseInfo::read(s)?),
            })
        },
        sc::SCMyHouseRemovedPacket => |s| Ok(E::MyHouseRemoved { tl: s.read_u16()? }),
        sc::SCHouseDemolishedPacket => |s| Ok(E::HouseDemolished { tl: s.read_u16()? }),
        sc::SCConstructHouseTaxPacket => |s| {
            Ok(E::ConstructHouseTax {
                design: s.read_u32()?,
                heavy_count: s.read_i32()?,
                normal_count: s.read_i32()?,
                heavy: s.read_bool()?,
                base: s.read_i32()?,
                deposit: s.read_i32()?,
                total: s.read_i32()?,
            })
        },
        sc::SCHouseBuildProgressPacket => |s| {
            Ok(E::HouseBuildProgress {
                tl: s.read_u16()?,
                model: s.read_u32()?,
                all: s.read_i32()?,
                cur: s.read_i32()?,
            })
        },
        sc::SCHouseTaxInfoPacket => |s| {
            Ok(E::HouseTaxInfo {
                tl: s.read_u16()?,
                dominion_tax_rate: s.read_i32()?,
                deposit: s.read_i32()?,
                total: s.read_i32()?,
                due: s.read_i64()?,
                already_paid: s.read_bool()?,
                weeks_without_pay: s.read_i32()?,
                heavy: s.read_bool()?,
            })
        },
        // mail
        sc::SCCountUnreadMailPacket => |s| {
            Ok(E::CountUnreadMail {
                unread: read_unread(s)?,
            })
        },
        sc::SCGotMailPacket => mail_received,
        sc::SCMailListPacket => mail_list,
        sc::SCMailListEndPacket => |s| {
            Ok(E::MailListEnd {
                total_headers: s.read_i32()?,
                total_bodies: s.read_i32()?,
            })
        },
        sc::SCMailBodyPacket => |s| {
            Ok(E::MailBody {
                is_prepare: s.read_bool()?,
                is_sent: s.read_bool()?,
                body: Box::new(read_mail_body(s)?),
                open_date_modified: s.read_bool()?,
                unread: read_unread(s)?,
            })
        },
        sc::SCMailSentPacket => |s| {
            let mail = Box::new(read_mail_header(s)?);
            let items = read_slot_pairs(s, s.remaining() / 2)?;
            Ok(E::MailSent { mail, items })
        },
        sc::SCMailFailedPacket => |s| {
            let err = s.read_u8()?;
            // The items fill the packet up to the trailing money flag.
            let items = read_slot_pairs(s, s.remaining().saturating_sub(1) / 2)?;
            Ok(E::MailFailed {
                err,
                items,
                money: s.read_bool()?,
            })
        },
        sc::SCAttachmentTakenPacket => attachment_taken,
        sc::SCChargeMoneyPaidPacket => |s| {
            Ok(E::ChargeMoneyPaid {
                mail_id: s.read_i64()?,
            })
        },
        sc::SCMailDeletedPacket => |s| {
            Ok(E::MailDeleted {
                is_sent: s.read_bool()?,
                mail_id: s.read_i64()?,
                unread_modified: s.read_bool()?,
                unread: read_unread(s)?,
            })
        },
        sc::SCMailStatusUpdatedPacket => |s| {
            Ok(E::MailStatusUpdated {
                is_sent: s.read_bool()?,
                mail_id: s.read_i64()?,
                status: s.read_u8()?,
            })
        },
        // specialty
        sc::SCSpecialtyRatioPacket => |s| {
            Ok(E::SpecialtyRatio {
                ratio: s.read_i32()?,
            })
        },
        sc::SCSpecialtyCurrentPacket => specialty_current,
        // the auction house answers live in `crate::auction`, the guild packets in `crate::guild`
        _ => {
            if let Some(a) = crate::auction::parse(level, opcode, body)? {
                return Ok(Some(E::Auction(Box::new(a))));
            }
            return Ok(crate::guild::parse(level, opcode, body)?.map(|g| E::Guild(Box::new(g))));
        }
    };
    read(&mut PacketStream::from_bytes(body)).map(Some)
}

// ---- helpers --------------------------------------------------------------------------------

/// Fails when `n` elements of at least `min_len` bytes cannot fit into the bytes left, so a
/// corrupt count never drives an allocation or a long loop.
fn check_count(s: &PacketStream, n: usize, min_len: usize) -> Result<(), MarshalError> {
    match n.checked_mul(min_len) {
        Some(need) if need <= s.remaining() => Ok(()),
        _ => Err(MarshalError::EndOfStream {
            pos: s.pos(),
            wanted: n.saturating_mul(min_len),
            len: s.len(),
        }),
    }
}

/// Reads an `i32` element count and validates it with [`check_count`].
fn read_count(s: &mut PacketStream, min_len: usize) -> Result<usize, MarshalError> {
    let n = s.read_i32()?;
    let n = usize::try_from(n).map_err(|_| MarshalError::Invalid(format!("negative count {n}")))?;
    check_count(s, n, min_len)?;
    Ok(n)
}

/// Skips `n` bytes.
fn skip(s: &mut PacketStream, n: usize) -> Result<(), MarshalError> {
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

/// Reads a string into a boxed `str`.
fn read_str(s: &mut PacketStream) -> Result<Box<str>, MarshalError> {
    Ok(s.read_string()?.into_boxed_str())
}

/// The four `CountUnreadMail` counters (G/models/char_data.rs:552).
fn read_unread(s: &mut PacketStream) -> Result<[i32; 4], MarshalError> {
    Ok([s.read_i32()?, s.read_i32()?, s.read_i32()?, s.read_i32()?])
}

/// Reads `n` `(slot type, slot)` byte pairs.
fn read_slot_pairs(s: &mut PacketStream, n: usize) -> Result<Vec<(u8, u8)>, MarshalError> {
    check_count(s, n, 2)?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push((s.read_u8()?, s.read_u8()?));
    }
    Ok(out)
}

/// Detail body length per `ItemDetailType` byte, as the 1.2 client's item detail reader consumes
/// them (G/models/item.rs:155-167); unknown types read no body.
const DETAIL_BODY_LENS: [usize; 10] = [0, 55, 29, 6, 9, 24, 16, 16, 8, 4];

/// Bytes of `Item.Write` after the detail body: create time (8), lifespan (4), made unit (4),
/// world (1), unsecure time (8), unpack time (8).
const ITEM_TAIL_LEN: usize = 33;

/// `Item.Write` (G/models/item.rs:1061-1082): `u32 template`, and, unless the template is 0,
/// `u64 id, u8 grade, u8 flags, i32 count, u8 detail type`, the detail body and the tail.
pub(crate) fn read_item(s: &mut PacketStream) -> Result<ItemBrief, MarshalError> {
    let template_id = s.read_u32()?;
    if template_id == 0 {
        return Ok(ItemBrief::default());
    }
    let item = ItemBrief {
        template_id,
        id: s.read_u64()?,
        grade: s.read_u8()?,
        flags: s.read_u8()?,
        count: s.read_i32()?,
    };
    let detail_type = usize::from(s.read_u8()?);
    skip(s, DETAIL_BODY_LENS.get(detail_type).copied().unwrap_or(0))?;
    skip(s, ITEM_TAIL_LEN)?;
    Ok(item)
}

// ---- quests ---------------------------------------------------------------------------------

/// Size of one `Quest.Write` row.
const QUEST_ROW_LEN: usize = 76;

/// `Quest.Write` (G/models/char_data.rs:458-482).
fn read_quest_row(s: &mut PacketStream) -> Result<QuestRow, MarshalError> {
    let id = s.read_i64()?;
    let template = s.read_u32()?;
    let status = s.read_u8()?;
    let mut objectives = [0; 5];
    for o in &mut objectives {
        *o = s.read_i32()?;
    }
    s.read_bool()?; // isCheckSet
    s.read_bc()?; // obj id
    s.read_u32()?; // type id
    s.read_bc()?; // obj id
    s.read_bc()?; // obj id
    let left_time_ms = s.read_i32()?;
    let component = s.read_u32()?;
    s.read_i64()?; // doodad id
    s.read_i64()?; // accept time
    let acceptor_type = s.read_u8()?;
    let acceptor_id = s.read_u32()?;
    Ok(QuestRow {
        id,
        template,
        status,
        objectives,
        left_time_ms,
        component,
        acceptor_type,
        acceptor_id,
    })
}

fn quests(s: &mut PacketStream) -> Result<ActivityEvent, MarshalError> {
    let n = read_count(s, QUEST_ROW_LEN)?;
    let mut list = Vec::with_capacity(n);
    for _ in 0..n {
        list.push(read_quest_row(s)?);
    }
    Ok(ActivityEvent::Quests { list })
}

fn completed_quests(s: &mut PacketStream) -> Result<ActivityEvent, MarshalError> {
    let n = read_count(s, 10)?;
    let mut ids = Vec::new();
    for _ in 0..n {
        let block = u32::from(s.read_u16()?);
        let mut mask = s.read_u64()?;
        while mask != 0 {
            ids.push(block * 64 + mask.trailing_zeros());
            mask &= mask - 1;
        }
    }
    Ok(ActivityEvent::CompletedQuests { ids })
}

// ---- team -----------------------------------------------------------------------------------

/// Member slots of `Team.Write` (the server's `Team::members` has 50).
const TEAM_SLOTS: usize = 50;

/// `TeamMember.Write` (G/systems/social/packets/team.rs:35-43).
fn read_team_member(s: &mut PacketStream) -> Result<TeamMemberBrief, MarshalError> {
    let char_id = s.read_u32()?;
    let name = read_str(s)?;
    s.read_u8()?; // race
    s.read_u8()?; // gender
    let level = s.read_u8()?;
    let role = s.read_u8()?;
    let obj_id = s.read_bc()?;
    Ok(TeamMemberBrief {
        char_id,
        name,
        level,
        role,
        obj_id,
    })
}

/// `Team.Write` head (G/systems/social/packets/team.rs:82-104) and the members after it.
fn joined_team(s: &mut PacketStream) -> Result<ActivityEvent, MarshalError> {
    let team_id = s.read_u32()?;
    let owner_id = s.read_u32()?;
    let is_party = s.read_bool()?;
    skip(s, 10)?; // party counts
    skip(s, TEAM_SLOTS * 5)?; // (u32 character id, bool online) per slot
    for _ in 0..12 {
        // marks: kind 1 carries a character id, kind 2 an object id
        match s.read_u8()? {
            1 => skip(s, 4)?,
            2 => skip(s, 3)?,
            _ => {}
        }
    }
    skip(s, 7)?; // looting rule: u8 method, u8 grade, u32 master, bool roll
    let mut members = Vec::new();
    while s.remaining() > 0 && members.len() < TEAM_SLOTS {
        members.push(read_team_member(s)?);
    }
    Ok(ActivityEvent::JoinedTeam {
        team_id,
        owner_id,
        is_party,
        members: members.into_boxed_slice(),
    })
}

/// Bytes of one `TeamMember.WritePerson` (G/systems/social/packets/team.rs:46-66).
const TEAM_PERSON_BYTES: usize = 50;

/// `TeamMember.WritePerson`: character id, zone, instance, level, hit points and mana (current and
/// maximum), the 9-byte position, the object id (`0` offline), then a flag byte, the three
/// abilities and five reserved bytes that bots do not use.
fn read_team_person(s: &mut PacketStream) -> Result<TeamPerson, MarshalError> {
    let char_id = s.read_u32()?;
    let zone_id = s.read_u32()?;
    let instance_id = s.read_u32()?;
    let level = s.read_u8()?;
    let hp = s.read_i32()?;
    let hp_max = s.read_i32()?;
    let mp = s.read_i32()?;
    let mp_max = s.read_i32()?;
    let (x, y, z) = s.read_position()?;
    let obj_id = s.read_bc()?;
    skip(s, 9)?;
    Ok(TeamPerson {
        char_id,
        zone_id,
        instance_id,
        level,
        hp,
        hp_max,
        mp,
        mp_max,
        pos: [x, y, z],
        obj_id,
    })
}

/// `SCTeamRemoteMembersEx`: `i32 count` and the members (the count is checked against the bytes
/// left before anything is allocated).
fn team_remote_members(s: &mut PacketStream) -> Result<ActivityEvent, MarshalError> {
    let count = usize::try_from(s.read_i32()?).unwrap_or(0);
    if count > s.remaining() / TEAM_PERSON_BYTES {
        return Err(MarshalError::EndOfStream {
            pos: s.pos(),
            wanted: count.saturating_mul(TEAM_PERSON_BYTES),
            len: s.len(),
        });
    }
    let mut members = Vec::with_capacity(count);
    for _ in 0..count {
        members.push(read_team_person(s)?);
    }
    Ok(ActivityEvent::TeamRemoteMembers {
        members: members.into_boxed_slice(),
    })
}

// ---- housing --------------------------------------------------------------------------------

impl HouseInfo {
    /// Reads a `House.Write` body (G/models/house.rs:95-131). Public because `SCHouseState`
    /// (a house entering view) carries the same body.
    pub fn read(s: &mut PacketStream) -> Result<Self, MarshalError> {
        Ok(Self {
            tl: s.read_u16()?,
            id: s.read_u32()?,
            obj: s.read_bc()?,
            template: s.read_u32()?,
            model: s.read_u32()?,
            co_owner: s.read_u32()?,
            owner: s.read_u32()?,
            owner_name: read_str(s)?,
            account: s.read_u32()?,
            permission: s.read_u8()?,
            all_action: s.read_i32()?,
            cur_action: s.read_i32()?,
            tax: s.read_i32()?,
            x: convert_long_to_coord(s.read_i64()?),
            y: convert_long_to_coord(s.read_i64()?),
            z: s.read_f32()?,
            name: read_str(s)?,
            allow_recover: s.read_bool()?,
            sell_price: s.read_u32()?,
            sell_to_id: s.read_u32()?,
            sell_to_name: read_str(s)?,
        })
    }
}

// ---- mail -----------------------------------------------------------------------------------

/// Smallest `MailHeader.Write` (empty strings): 8+1+1+2+2+1+2+8+1+8.
const MAIL_HEADER_MIN_LEN: usize = 34;

/// Attachment slots of a mail body (G/systems/economy/mail.rs:146).
const MAX_MAIL_ATTACHMENTS: usize = 10;

/// `MailHeader.Write` (G/systems/economy/mail.rs:370-383).
fn read_mail_header(s: &mut PacketStream) -> Result<MailHeader, MarshalError> {
    Ok(MailHeader {
        mail_id: s.read_i64()?,
        mail_type: s.read_u8()?,
        status: s.read_u8()?,
        title: read_str(s)?,
        sender: read_str(s)?,
        attachments: s.read_u8()?,
        receiver: read_str(s)?,
        open_date: s.read_i64()?,
        returned: s.read_bool()?,
        extra: s.read_i64()?,
    })
}

/// `MailBody.Write` (G/systems/economy/mail.rs:402-424): the fixed part, then ten item slots
/// (`Item.Write`, or an `i32 0` for an empty one).
fn read_mail_body(s: &mut PacketStream) -> Result<MailBodyInfo, MarshalError> {
    let mut body = MailBodyInfo {
        mail_id: s.read_i64()?,
        mail_type: s.read_u8()?,
        receiver: read_str(s)?,
        title: read_str(s)?,
        text: read_str(s)?,
        money: s.read_i32()?,
        billing: s.read_i32()?,
        money2: s.read_i32()?,
        send_date: s.read_i64()?,
        recv_date: s.read_i64()?,
        open_date: s.read_i64()?,
        items: Vec::new(),
    };
    for _ in 0..MAX_MAIL_ATTACHMENTS {
        let item = read_item(s)?;
        if item.template_id != 0 {
            body.items.push(item);
        }
    }
    Ok(body)
}

fn mail_received(s: &mut PacketStream) -> Result<ActivityEvent, MarshalError> {
    let mail = Box::new(read_mail_header(s)?);
    let unread = read_unread(s)?;
    let body = if s.read_bool()? {
        Some(Box::new(read_mail_body(s)?))
    } else {
        None
    };
    Ok(ActivityEvent::MailReceived {
        mail,
        unread,
        is_cancel: s.read_bool()?,
        body,
    })
}

fn mail_list(s: &mut PacketStream) -> Result<ActivityEvent, MarshalError> {
    let is_sent = s.read_bool()?;
    let n = read_count(s, MAIL_HEADER_MIN_LEN)?;
    let mut mails = Vec::with_capacity(n);
    for _ in 0..n {
        mails.push(read_mail_header(s)?);
    }
    Ok(ActivityEvent::MailList { is_sent, mails })
}

fn attachment_taken(s: &mut PacketStream) -> Result<ActivityEvent, MarshalError> {
    let mail_id = s.read_i64()?;
    let money = s.read_bool()?;
    let aa_point = s.read_bool()?;
    let take_sequentially = s.read_bool()?;
    let n = usize::from(s.read_u8()?).min(MAX_MAIL_ATTACHMENTS);
    let mut items = Vec::with_capacity(n);
    for i in 0..MAX_MAIL_ATTACHMENTS {
        let item = TakenItem {
            id: s.read_u64()?,
            slot_type: s.read_u8()?,
            slot: s.read_u8()?,
        };
        if i < n {
            items.push(item);
        }
    }
    Ok(ActivityEvent::AttachmentTaken {
        mail_id,
        money,
        aa_point,
        take_sequentially,
        items: items.into_boxed_slice(),
    })
}

// ---- specialty ------------------------------------------------------------------------------

fn specialty_current(s: &mut PacketStream) -> Result<ActivityEvent, MarshalError> {
    let n = s.read_i32()?;
    let from_zone_group = s.read_u16()?;
    let to_zone_group = s.read_u16()?;
    let n = usize::try_from(n).map_err(|_| MarshalError::Invalid(format!("negative count {n}")))?;
    check_count(s, n, 8)?;
    let mut rates = Vec::with_capacity(n);
    for _ in 0..n {
        rates.push((s.read_u32()?, s.read_u32()?));
    }
    Ok(ActivityEvent::SpecialtyCurrent {
        from_zone_group,
        to_zone_group,
        rates: rates.into_boxed_slice(),
    })
}
