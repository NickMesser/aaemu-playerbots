//! The player-trade goal: two bots meet, open a trade window, put something up, lock and confirm.
//!
//! # Flow
//!
//! ```text
//!  Prepare ──► Pairing ──(coordinator Pair)──► a new Trade goal with an order
//!     │            └──(unexpected request)──► Await
//!     └──► Meet ──► Invite (initiator: CSCanStartTrade) ──► Window
//!             └───► Await  (responder: CSStartTrade on SCCanStartTrade) ─┘
//!  Window: put up item/money ─► lock when both offers stand ─► OK when both locked ─► SCTradeMade
//! ```
//!
//! * `Prepare` makes sure the bot has something to offer (with `bots.gm_bootstrap` it mints a few
//!   cheap unbound materials with `/item add self`, see [`GOODS`]).
//! * The window part ([`Trade::step`]) is a pure state machine over the [`TradeLog`] (the mirror of
//!   the server's trade packets kept in the bot state) and the bag, so it is unit tested with
//!   synthetic events.
//! * A request nobody asked for (a real player, or a bot that was steered by the operator) is
//!   answered by the brain through [`unsolicited`]: accepted when the bot is idle and its trade
//!   weight is above 0, declined otherwise.
//!
//! # Safety
//!
//! The server lets both sides put up anything and clears the locks whenever an offer changes, so
//! the window part never locks on "the partner put something up". It holds a [`Deal`] (what the
//! bot gives, what it demands) and checks the window against it ([`deal::verify`]) before every
//! lock and every confirmation, again after every unlock, and cancels when the window is not the
//! deal or when the partner changed the offer after the bot locked. A random trade between our own
//! bots has a generous deal (any offer that is not empty), a trade with a player that nobody agreed
//! to in chat must be worth a share of what the bot gives, and a sale agreed in chat
//! (`goals::market`) demands exactly the agreed goods and at least the agreed copper.

mod deal;
mod log;

use std::fmt::Write as _;

use aa_proto::cs::{self, Out};
use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use crate::bot::cmd::{GoalKind, PairKind, PairOrder, PairRole};
use crate::bot::ctx::Ctx;
use crate::bot::market::ledger::bag_count;
use crate::bot::market::needs::QuestNeeds;
use crate::bot::motion::{MotionStatus, Pace};
use crate::bot::state::{dist2d, BotState, Item, StateError, Unit, UnitKind, SLOT_BAG};

use super::{FailReason, GoalStatus};

pub use deal::{verify, Deal, Identity, Refusal, Terms, Verdict};
pub use log::{Offer, Outcome, Put, TradeLog, Window};

/// Unbound, cheap, stackable materials a bot may mint and trade (`bind_id` 1 in compact.sqlite3,
/// no expiry): copper ore, hay bale, blue salt, boar leather, thin cloth scrap, rabbit and fox
/// leather.
pub const GOODS: [u32; 7] = [3411, 3712, 7765, 2151, 7773, 7753, 7749];
/// Items never offered: coins and the tax certificates housing needs.
const KEEP: [u32; 3] = [500, 31891, 31892];
/// Copper a bot needs before it offers money.
const MONEY_MIN: i64 = 1_000;
/// The server refuses a bound item with this `SCErrorMsg` (`TRADE_SOUL_BOUND_ITEM`).
const ERR_SOUL_BOUND: i16 = 251;

/// Within this many metres of the partner the bots trade.
const NEAR_M: f32 = 6.0;
/// The meeting (walking plus waiting) takes at most this long.
const MEET_TIMEOUT_MS: u64 = 180_000;
/// A responder waits this long at the meeting point for the partner.
const WAIT_AT_MEET_MS: u64 = 60_000;
/// A bot without a partner gives up after this long.
const PAIR_WAIT_MS: u64 = 100_000;
/// The coordinator request is repeated this often (its own time-to-live is longer).
const REPAIR_MS: u64 = 25_000;
/// The initiator waits this long for the window after each `CSCanStartTrade` (sent at most twice).
const INVITE_WAIT_MS: u64 = 12_000;
/// A responder waits this long for the partner's request, and for the window after accepting.
const AWAIT_REQUEST_MS: u64 = 90_000;
const ACCEPT_WAIT_MS: u64 = 15_000;
/// A trade window lives at most this long.
const TRADE_MAX_MS: u64 = 60_000;
/// A window of a sale agreed in chat lives this long (a player is slower than a bot).
const DEAL_MAX_MS: u64 = 100_000;
/// A partner of a sale agreed in chat has this long to put up everything that was agreed.
const DEAL_OFFER_WAIT_MS: u64 = 40_000;
/// A partner whose offer is not the deal has this long to put it right before the bot leaves.
const REFUSE_GRACE_MS: u64 = 6_000;
/// The bot locks again after the server undid its lock at most this many times.
const MAX_RELOCKS: u8 = 3;
/// A trade nobody agreed to in chat with a character that is not ours must bring in this many
/// percent of the value (vendor refund prices) of what the bot gives.
const FAIR_PERCENT: i64 = 60;
/// The offers must be quiet this long before the bot locks.
const QUIET_MS: u64 = 700;
/// The bot waits this long for the partner's offer before it walks away from the trade.
const OFFER_WAIT_MS: u64 = 8_000;
/// A lock or OK that was sent is not repeated for this long.
const RESEND_MS: u64 = 2_000;
/// Creating items with `/item add` takes this long to show in the bag.
const MINT_WAIT_MS: u64 = 7_000;
/// How often the initiator re-aims at a partner that is not at the meeting point.
const CHASE_MS: u64 = 3_000;
/// A request older than the server's invitation (60 s) is dropped.
const INVITE_TTL_MS: u64 = 60_000;

/// Item templates that turned out to be bound: the server cancelled a trade right after a put-up
/// with `TRADE_SOUL_BOUND_ITEM`. Shared by all bots (the bind type belongs to the template).
static BOUND: parking_lot::Mutex<Vec<u32>> = parking_lot::Mutex::new(Vec::new());

/// Remembers that `template` cannot be traded.
pub fn learn_bound(template: u32) {
    let mut b = BOUND.lock();
    if b.len() < 64 && !b.contains(&template) {
        b.push(template);
    }
}

/// Whether `template` is known to be bound.
pub fn is_bound(template: u32) -> bool {
    BOUND.lock().contains(&template)
}

/// What the bot puts up.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Plan {
    /// Bag slot of the item.
    pub slot: u8,
    /// Template of the item (0 = no item).
    pub template: u32,
    /// Units of it.
    pub amount: i32,
    /// Copper (0 = none).
    pub money: i32,
}

impl Plan {
    /// Whether the plan offers nothing.
    pub fn is_empty(&self) -> bool {
        self.amount <= 0 && self.money <= 0
    }
}

/// How many units of `template` the bot keeps when it gives items away: `None` when the item
/// must not be given away at all (it is not [`tradable`](aa_world::ItemFacts::tradable): unsellable,
/// bound or a quest item; an unknown template counts as not tradable), else what its active quests
/// still need of it (`needs`). The rule [`choose_offer`] takes.
pub fn keep_of(world: &aa_world::World, needs: &QuestNeeds, template: u32) -> Option<u32> {
    let facts = world.item_facts(template)?;
    facts.tradable().then(|| needs.count(template))
}

/// A sensible offer from the bag and the purse: a few units of a tradable stack and/or a small
/// part of the money. Items of [`GOODS`] come first; with `allow_other`, any other stack of two or
/// more that is not known to be bound comes next (a bot that can mint goods does not gamble on
/// loot that may be bound). Only items `keep` allows are offered (`None`: never, else how many
/// units the bot holds back, see [`keep_of`]), and never the units it holds back. `None` when
/// there is nothing to offer.
pub fn choose_offer(
    bag: &FxHashMap<u16, Item>,
    money: i64,
    allow_other: bool,
    keep: &dyn Fn(u32) -> Option<u32>,
    rng: &mut fastrand::Rng,
) -> Option<Plan> {
    // (slot, stack, units of the item that are free to give)
    let mut good: SmallVec<[(u16, Item, i32); 8]> = SmallVec::new();
    let mut other: SmallVec<[(u16, Item, i32); 8]> = SmallVec::new();
    for (slot, it) in bag {
        if *slot > u16::from(u8::MAX) || it.count <= 0 || KEEP.contains(&it.template) {
            continue;
        }
        if is_bound(it.template) {
            continue;
        }
        let Some(held) = keep(it.template) else {
            continue;
        };
        let free = bag_count(bag, it.template) - i64::from(held);
        let Ok(free) = i32::try_from(free) else {
            continue;
        };
        if free <= 0 {
            continue;
        }
        if GOODS.contains(&it.template) {
            if good.len() < 8 {
                good.push((*slot, *it, free));
            }
        } else if allow_other && it.count >= 2 && other.len() < 8 {
            other.push((*slot, *it, free));
        }
    }
    // the same bag always gives the same candidates in the same order
    good.sort_by_key(|(slot, _, _)| *slot);
    other.sort_by_key(|(slot, _, _)| *slot);
    let pool = if good.is_empty() { &other } else { &good };
    let mut plan = Plan::default();
    if !pool.is_empty() {
        let (slot, it, free) = pool[rng.usize(..pool.len())];
        plan.slot = slot as u8;
        plan.template = it.template;
        plan.amount = rng.i32(1..=it.count.clamp(1, 5).min(free));
    }
    if money >= MONEY_MIN && (plan.amount <= 0 || rng.u8(..100) < 40) {
        let cap = (money / 10).clamp(100, 5_000);
        plan.money = rng.i64(100..=cap) as i32;
    }
    (!plan.is_empty()).then_some(plan)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Make sure there is something to offer.
    Prepare,
    /// Wait for the coordinator to find a partner.
    Pairing,
    /// Walk to the meeting point and the partner.
    Meet,
    /// The initiator asks the partner to trade.
    Invite,
    /// The responder waits for the request and answers it.
    Await,
    /// The window is open.
    Window,
}

/// Something the goal tells the log about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    /// The window opened.
    Opened,
    /// The trade was made; the final window.
    Made(Box<Window>),
    /// The trade was cancelled.
    Canceled {
        /// Server reason.
        reason: i32,
        /// We caused it.
        by_me: bool,
    },
    /// The bot walked away.
    GaveUp(&'static str),
    /// The bot cancelled because the window was not the deal.
    Refused(Refusal),
    /// Item `template` is bound.
    Bound(u32),
}

/// The result of one [`Trade::step`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// What the goal reports.
    pub status: GoalStatus,
    /// What to write to the log.
    pub note: Option<Note>,
}

impl Step {
    fn running() -> Step {
        Step {
            status: GoalStatus::Running,
            note: None,
        }
    }

    fn failed(why: FailReason, note: Option<Note>) -> Step {
        Step {
            status: GoalStatus::Failed(why),
            note,
        }
    }
}

/// What [`Trade::step`] works with: the bot's knowledge, the outbox and randomness.
pub struct Io<'a> {
    /// The bot clock in milliseconds.
    pub now_ms: u64,
    /// The trade mirror.
    pub log: &'a mut TradeLog,
    /// The bag.
    pub bag: &'a FxHashMap<u16, Item>,
    /// Copper carried.
    pub money: i64,
    /// Object id of the partner when it is in view.
    pub peer_obj: Option<u32>,
    /// The pending request (if any) comes from the partner.
    pub incoming_ok: bool,
    /// The server just refused a bound item.
    pub bound_err: bool,
    /// Items other than [`GOODS`] may be offered (the bot cannot mint goods).
    pub allow_other: bool,
    /// Whether the unit of the window is the character the bot agreed to trade with.
    pub identity: Identity,
    /// The partner is one of our bots (or was paired by the coordinator, which only pairs bots).
    pub partner_is_bot: bool,
    /// What the bot keeps of an item template when it gives items away: `None` for an item it
    /// must not give away (not tradable), else how many units its active quests need (see
    /// [`keep_of`]).
    pub keep: &'a dyn Fn(u32) -> Option<u32>,
    /// Copper one unit of an item template is worth (the vendor refund); 0 when unknown.
    pub worth: &'a dyn Fn(u32) -> i64,
    /// Packets to send.
    pub out: &'a mut Vec<Out>,
    /// Randomness.
    pub rng: &'a mut fastrand::Rng,
}

/// Trade with another bot (or answer a request). See the module docs.
#[derive(Debug, Clone)]
pub struct Trade {
    order: Option<PairOrder>,
    role: PairRole,
    phase: Phase,
    started_ms: u64,
    phase_ms: u64,
    /// `TradeLog::opened` when the goal started: older windows are not ours.
    base_seq: u32,
    /// Object id of the partner, 0 while unknown.
    peer: u32,
    peer_name: Option<Box<str>>,
    plan: Plan,
    item_sent: bool,
    money_sent: bool,
    accepted: bool,
    moved: bool,
    /// The bot reached the meeting point (`phase_ms` is then the time of arrival).
    arrived: bool,
    requested: bool,
    minted_gold: bool,
    /// A lock was sent and not yet seen.
    lock_sent: bool,
    asked: u8,
    mint_tries: u8,
    /// Bot clock of the put-ups.
    offered_ms: u64,
    /// Bot clock of the last mint request (Prepare), the last re-aim at the partner (Meet) or the
    /// moment the partner's offer stopped being the deal (Window; 0 = it is, or not yet known).
    aux_ms: u64,
    /// Earliest bot clock for the next lock or OK.
    resend_ms: u64,
    /// What the bot gives and demands; made when the window opens unless the goal was started
    /// with one (a sale agreed in chat).
    deal: Option<Box<Deal>>,
    /// The server confirmed a lock of ours in this window.
    locked_once: bool,
    /// Locks sent after the first one.
    relocks: u8,
}

impl Trade {
    /// A goal for the pairing `order` (`None` when picked by the brain on its own, or started
    /// for an unexpected request).
    pub fn new(order: Option<PairOrder>) -> Trade {
        let role = order.as_ref().map_or(PairRole::Responder, |o| o.role);
        Trade {
            order,
            role,
            phase: Phase::Prepare,
            started_ms: 0,
            phase_ms: 0,
            base_seq: 0,
            peer: 0,
            peer_name: None,
            plan: Plan::default(),
            item_sent: false,
            money_sent: false,
            accepted: false,
            moved: false,
            arrived: false,
            requested: false,
            minted_gold: false,
            lock_sent: false,
            asked: 0,
            mint_tries: 0,
            offered_ms: 0,
            aux_ms: 0,
            resend_ms: 0,
            deal: None,
            locked_once: false,
            relocks: 0,
        }
    }

    /// A goal for a sale agreed in chat: the partner is at hand (`order` names the character), the
    /// bot asks (`role` initiator) or waits for the request (responder), and trades `deal`.
    pub fn for_deal(order: PairOrder, deal: Deal) -> Trade {
        let role = order.role;
        let mut t = Trade::new(Some(order));
        t.role = role;
        t.plan = deal.give;
        t.deal = Some(Box::new(deal));
        t.phase = if role == PairRole::Initiator {
            Phase::Invite
        } else {
            Phase::Await
        };
        t
    }

    /// The deal this goal trades, once it has one.
    pub fn deal(&self) -> Option<&Deal> {
        self.deal.as_deref()
    }

    /// The coordinator's pairing, if any.
    pub fn order(&self) -> Option<&PairOrder> {
        self.order.as_ref()
    }

    /// Called once when the brain picks the goal.
    pub fn start(&mut self, ctx: &mut Ctx<'_>) {
        self.started_ms = ctx.now_ms;
        self.phase_ms = ctx.now_ms;
        self.base_seq = ctx.state.trade.opened;
        ctx.state.trade.outcome = None;
        ctx.state.trade.refused = None;
        if let Some(o) = &self.order {
            self.peer_name = Some(o.partner_name.clone());
            let bot = o.partner_bot;
            set_partner(ctx, Some(bot));
        }
        let asked = ctx.state.trade.incoming.is_some();
        if let (Some(o), Some(_)) = (&self.order, &self.deal) {
            ctx.log
                .say(format!("trade: agreed sale with {}", o.partner_name));
            return;
        }
        ctx.log.say(match (&self.order, self.role) {
            (Some(o), PairRole::Initiator) => format!("trade: going to ask {}", o.partner_name),
            (Some(o), PairRole::Responder) => format!("trade: going to meet {}", o.partner_name),
            (None, _) if asked => "trade: answering a trade request".to_string(),
            (None, _) => "trade: looking for a trading partner".to_string(),
        });
    }

    /// One tick.
    pub fn tick(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let now = ctx.now_ms;
        if matches!(self.phase, Phase::Prepare | Phase::Pairing | Phase::Meet) {
            if let Some(s) = self.early_request(ctx) {
                return s;
            }
        }
        match self.phase {
            Phase::Prepare => self.prepare(ctx),
            Phase::Pairing => {
                if now.saturating_sub(self.started_ms) > PAIR_WAIT_MS {
                    return GoalStatus::Failed(FailReason::NoTarget);
                }
                if !self.requested || now.saturating_sub(self.phase_ms) >= REPAIR_MS {
                    let p = ctx.pos();
                    self.requested =
                        ctx.coord
                            .request_partner(PairKind::Trade, ctx.persona.id, (p[0], p[1]));
                    self.phase_ms = now;
                }
                GoalStatus::Running
            }
            Phase::Meet => self.meet(ctx),
            Phase::Invite | Phase::Await | Phase::Window => self.run_step(ctx),
        }
    }

    /// Called when the goal ends: a window still open is cancelled.
    pub fn stop(&mut self, ctx: &mut Ctx<'_>) {
        if ctx.state.trade.window.is_some() {
            ctx.send(cs::cancel_trade(0));
            ctx.log.say("trade: cancelled");
        }
        if self.requested {
            ctx.coord.cancel(ctx.persona.id);
            self.requested = false;
        }
        if ctx.motion.is_moving() {
            ctx.motion.stop();
        }
        set_partner(ctx, None);
    }

    /// Appends a one-line description of what the goal is doing to `out`.
    pub fn describe(&self, out: &mut String) {
        let who = self
            .peer_name
            .as_deref()
            .or_else(|| self.order.as_ref().map(|o| &*o.partner_name))
            .filter(|n| !n.is_empty())
            .unwrap_or("someone");
        match self.phase {
            Phase::Prepare => out.push_str("getting something to trade"),
            Phase::Pairing => out.push_str("looking for someone to trade with"),
            Phase::Meet => {
                let _ = write!(out, "walking to {who} to trade");
            }
            Phase::Invite => {
                let _ = write!(out, "asking {who} to trade");
            }
            Phase::Await => {
                if self.accepted {
                    let _ = write!(out, "accepted the trade with {who}");
                } else {
                    let _ = write!(out, "waiting for {who}'s trade request");
                }
            }
            Phase::Window => {
                let _ = write!(out, "trading with {who}: offering ");
                if self.plan.amount > 0 {
                    let _ = write!(out, "{} item(s)", self.plan.amount);
                    if self.plan.money > 0 {
                        out.push_str(" + ");
                    }
                }
                if self.plan.money > 0 {
                    let _ = write!(out, "{}c", self.plan.money);
                }
            }
        }
    }

    // ---- before the window ---------------------------------------------------------------

    /// Answers a request that arrived while the bot was still preparing, pairing or walking.
    fn early_request(&mut self, ctx: &mut Ctx<'_>) -> Option<GoalStatus> {
        let (obj, at) = ctx.state.trade.incoming?;
        if ctx.now_ms.saturating_sub(at) > INVITE_TTL_MS {
            ctx.state.trade.incoming = None;
            return None;
        }
        // Nothing to offer yet: keep the request until the bot has been provided for.
        if self.plan.is_empty() {
            return None;
        }
        if !self.requester_ok(ctx, obj) {
            ctx.send(cs::cannot_start_trade(obj, 0));
            ctx.state.trade.incoming = None;
            return None;
        }
        ctx.send(cs::start_trade(obj));
        ctx.state.trade.incoming = None;
        self.adopt_peer(ctx, obj);
        self.accepted = true;
        self.role = PairRole::Responder;
        self.phase = Phase::Await;
        self.phase_ms = ctx.now_ms;
        ctx.motion.stop();
        if self.requested {
            ctx.coord.cancel(ctx.persona.id);
            self.requested = false;
        }
        Some(GoalStatus::Running)
    }

    /// Whether the unit `obj` is the partner of the order (without an order: any unit this bot may
    /// trade with, see [`may_trade_with`]).
    fn requester_ok(&self, ctx: &Ctx<'_>, obj: u32) -> bool {
        let st = &*ctx.state;
        let Some(o) = &self.order else {
            return may_trade_with(ctx, obj);
        };
        match st.units.get(&obj) {
            Some(u) => is_partner(u, o),
            // not in view yet: a bot-to-bot trade, trust the object id the server sent (a sale
            // agreed in chat never trusts a unit it cannot see)
            None => self.peer == obj || (self.peer == 0 && self.deal.is_none()),
        }
    }

    /// Remembers the partner's object id and name and shows the pair in the web UI.
    fn adopt_peer(&mut self, ctx: &mut Ctx<'_>, obj: u32) {
        self.peer = obj;
        if let Some(u) = ctx.state.units.get(&obj) {
            if let Some(n) = &u.name {
                self.peer_name = Some(n.clone());
            }
            let char_id = u.id;
            let bot = match &self.order {
                Some(o) => Some(o.partner_bot),
                None => ctx
                    .hub
                    .slots()
                    .into_iter()
                    .find(|s| s.char_id.load(std::sync::atomic::Ordering::Relaxed) == char_id)
                    .map(|s| s.id),
            };
            set_partner(ctx, bot);
        }
    }

    fn prepare(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let now = ctx.now_ms;
        let needs = QuestNeeds::of(ctx.world, &ctx.state.quests);
        let world = ctx.world;
        match choose_offer(
            &ctx.state.bag,
            ctx.state.me.money,
            !ctx.gm_allowed(),
            &|template| keep_of(world, &needs, template),
            ctx.rng,
        ) {
            Some(plan) => self.plan = plan,
            None => {
                if !ctx.gm_allowed() || self.mint_tries >= 2 {
                    return GoalStatus::Failed(FailReason::Other("nothing to offer"));
                }
                if self.aux_ms == 0 || now.saturating_sub(self.aux_ms) >= MINT_WAIT_MS {
                    self.mint_tries += 1;
                    self.aux_ms = now;
                    let id = GOODS[ctx.rng.usize(..GOODS.len())];
                    let n = ctx.rng.u32(6..=20);
                    ctx.gm(&format!("/item add self {id} {n}"));
                    ctx.log.say(format!(
                        "trade: minted {n} x {}",
                        ctx.world.item_name(id).unwrap_or("goods")
                    ));
                }
                return GoalStatus::Running;
            }
        }
        // a purse with a little money makes some of the trades money trades
        if ctx.gm_allowed()
            && !self.minted_gold
            && ctx.state.me.money < MONEY_MIN
            && ctx.rng.u8(..100) < 40
        {
            self.minted_gold = true;
            ctx.gm("/gold add self 1");
        }
        self.phase_ms = now;
        let incoming = ctx.state.trade.incoming.is_some();
        match (&self.order, incoming) {
            (Some(_), _) => self.phase = Phase::Meet,
            (None, true) => {
                self.phase = Phase::Await;
                self.role = PairRole::Responder;
            }
            (None, false) => {
                self.phase = Phase::Pairing;
                let p = ctx.pos();
                self.requested =
                    ctx.coord
                        .request_partner(PairKind::Trade, ctx.persona.id, (p[0], p[1]));
            }
        }
        GoalStatus::Running
    }

    fn meet(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let now = ctx.now_ms;
        if now.saturating_sub(self.started_ms) > MEET_TIMEOUT_MS {
            return GoalStatus::Failed(FailReason::Timeout);
        }
        let Some(meet) = self.order.as_ref().map(|o| o.meet_at) else {
            return GoalStatus::Failed(FailReason::NoTarget);
        };
        let me = ctx.pos();
        let seen = self.find_peer(ctx.state);
        if let Some((obj, pos)) = seen {
            if dist2d(me, pos) <= NEAR_M {
                ctx.motion.stop();
                ctx.motion.face(pos[0], pos[1]);
                self.adopt_peer(ctx, obj);
                self.phase = if self.role == PairRole::Initiator {
                    Phase::Invite
                } else {
                    Phase::Await
                };
                self.phase_ms = now;
                return GoalStatus::Running;
            }
        }
        if !self.moved {
            self.moved = true;
            ctx.travel(meet.0, meet.1, 3.0, Pace::Run);
            return GoalStatus::Running;
        }
        match ctx.motion.status() {
            MotionStatus::Moving => {}
            MotionStatus::Failed(_) => return GoalStatus::Failed(FailReason::Stuck),
            MotionStatus::Arrived | MotionStatus::Idle => {
                // At the meeting point. A responder whose partner never shows up goes away; the
                // initiator goes the rest of the way to a partner that is not here.
                if !self.arrived {
                    self.arrived = true;
                    self.phase_ms = now;
                } else if self.role == PairRole::Responder
                    && now.saturating_sub(self.phase_ms) > WAIT_AT_MEET_MS
                {
                    return GoalStatus::Failed(FailReason::NoTarget);
                }
                if self.role == PairRole::Initiator && now.saturating_sub(self.aux_ms) >= CHASE_MS {
                    let target = seen
                        .map(|(_, p)| (p[0], p[1]))
                        .or_else(|| self.partner_hub_pos(ctx));
                    if let Some((x, y)) = target {
                        self.aux_ms = now;
                        ctx.travel(x, y, 2.5, Pace::Run);
                    }
                }
            }
        }
        GoalStatus::Running
    }

    /// Where the hub says the partner bot is.
    fn partner_hub_pos(&self, ctx: &Ctx<'_>) -> Option<(f32, f32)> {
        let bot = self.order.as_ref()?.partner_bot;
        let slot = ctx.hub.slot(bot)?;
        let (x, y, online) = slot.with(|g| {
            (
                g.snap.x,
                g.snap.y,
                g.snap.activity != pb_web::Activity::Offline,
            )
        });
        online.then_some((x, y))
    }

    /// The partner in view: by object id when known, else by character id or name.
    fn find_peer(&self, st: &BotState) -> Option<(u32, [f32; 3])> {
        if self.peer != 0 {
            if let Some(u) = st.units.get(&self.peer) {
                return Some((self.peer, u.pos));
            }
        }
        let o = self.order.as_ref()?;
        st.units
            .iter()
            .find(|(_, u)| is_partner(u, o))
            .map(|(obj, u)| (*obj, u.pos))
    }

    // ---- the window ----------------------------------------------------------------------

    fn run_step(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let now = ctx.now_ms;
        let peer_obj = self.find_peer(ctx.state).map(|(o, _)| o);
        let incoming_ok = match ctx.state.trade.incoming {
            Some((obj, at)) => {
                now.saturating_sub(at) <= INVITE_TTL_MS && self.requester_ok(ctx, obj)
            }
            None => false,
        };
        let bound_err = ctx.state.errors.iter().any(|(t, e)| {
            matches!(e, StateError::Msg(ERR_SOUL_BOUND)) && now.saturating_sub(*t) < 5_000
        });
        let allow_other = !ctx.gm_allowed();
        let (identity, partner_is_bot) = self.who_is_in_the_window(ctx);
        let world = ctx.world;
        let needs = QuestNeeds::of(world, &ctx.state.quests);
        let keep = |template: u32| keep_of(world, &needs, template);
        let worth = |template: u32| {
            world
                .item_facts(template)
                .map_or(0, |f| i64::from(f.refund))
        };
        let st = &mut *ctx.state;
        let mut io = Io {
            now_ms: now,
            log: &mut st.trade,
            bag: &st.bag,
            money: st.me.money,
            peer_obj,
            incoming_ok,
            bound_err,
            allow_other,
            identity,
            partner_is_bot,
            keep: &keep,
            worth: &worth,
            out: &mut *ctx.out,
            rng: &mut *ctx.rng,
        };
        let step = self.step(&mut io);
        if self.peer != 0 && self.peer_name.is_none() {
            self.adopt_peer(ctx, self.peer);
        }
        if let Some(note) = step.note {
            self.log_note(ctx, note);
        }
        step.status
    }

    /// Who stands in the open window: whether it is the character this goal agreed to trade with
    /// (by the unit the server named in `SCTradeStarted`), and whether that is one of our bots.
    /// Without a pairing or an agreement the partner is not known to be anybody in particular.
    fn who_is_in_the_window(&self, ctx: &Ctx<'_>) -> (Identity, bool) {
        let Some(w) = ctx.state.trade.window.as_deref() else {
            return (Identity::Unknown, false);
        };
        let unit = ctx
            .state
            .units
            .get(&w.peer_obj)
            .filter(|u| u.kind == UnitKind::Character);
        let identity = match (&self.order, unit) {
            (Some(o), Some(u)) if is_partner(u, o) => Identity::Match,
            (Some(_), Some(_)) => Identity::Mismatch,
            _ => Identity::Unknown,
        };
        // (the coordinator only pairs our own bots; a sale agreed in chat is with whoever
        // answered, so there the unit decides)
        let bot = match (&self.deal, &self.order) {
            (None, Some(_)) => true,
            _ => unit.is_some_and(|u| ctx.hub.is_bot_char(u.id)),
        };
        (identity, bot)
    }

    fn log_note(&mut self, ctx: &mut Ctx<'_>, note: Note) {
        let who = self
            .peer_name
            .as_deref()
            .filter(|n| !n.is_empty())
            .unwrap_or("someone")
            .to_owned();
        match note {
            Note::Opened => ctx.log.say(format!("trade: window open with {who}")),
            Note::Made(w) => {
                let gave = describe_offer(ctx.world, &w.mine, Some(self.plan));
                let got = describe_offer(ctx.world, &w.other, None);
                ctx.log
                    .say(format!("trade made with {who}: gave {gave}; got {got}"));
            }
            Note::Canceled { reason, by_me } => {
                let by = if by_me { "us" } else { "the partner" };
                ctx.log.say(format!(
                    "trade with {who} cancelled by {by} (reason {reason})"
                ));
            }
            Note::GaveUp(why) => ctx.log.say(format!("trade with {who}: {why}")),
            Note::Refused(why) => ctx.log.say(format!(
                "trade with {who} refused: {} (scam guard)",
                why.why()
            )),
            Note::Bound(template) => {
                learn_bound(template);
                ctx.log.say(format!(
                    "trade: {} is bound, not offering it again",
                    ctx.world.item_name(template).unwrap_or("that item")
                ));
            }
        }
    }

    /// One step of the invite / await / window part. Pure: everything it reads and writes is in
    /// `self` and `io`.
    pub fn step(&mut self, io: &mut Io<'_>) -> Step {
        let now = io.now_ms;
        if let Some(outcome) = io.log.outcome.take() {
            // the end of a trade that began before this goal did is none of its business
            let ours = io.log.outcome_seq > self.base_seq;
            match outcome {
                _ if !ours => {}
                Outcome::Made(w) => {
                    return Step {
                        status: GoalStatus::Done,
                        note: Some(Note::Made(w)),
                    }
                }
                Outcome::Canceled {
                    had_window: false, ..
                } => {}
                Outcome::Canceled { reason, by_me, .. } => {
                    let note = if io.bound_err && self.plan.template != 0 && by_me {
                        Note::Bound(self.plan.template)
                    } else {
                        Note::Canceled { reason, by_me }
                    };
                    return Step::failed(FailReason::Other("trade cancelled"), Some(note));
                }
            }
        }
        if self.phase != Phase::Window && io.log.window.is_some() {
            self.phase = Phase::Window;
            self.phase_ms = now;
            return self.open_window(io);
        }
        match self.phase {
            Phase::Invite => self.invite(io),
            Phase::Await => self.await_request(io),
            Phase::Window => self.window(io),
            _ => Step::running(),
        }
    }

    fn invite(&mut self, io: &mut Io<'_>) -> Step {
        let now = io.now_ms;
        if io.log.refused.take().is_some() {
            return Step::failed(
                FailReason::Other("partner declined"),
                Some(Note::GaveUp("the partner declined")),
            );
        }
        if self.asked == 0 || now.saturating_sub(self.phase_ms) >= INVITE_WAIT_MS {
            if self.asked >= 2 {
                return Step::failed(
                    FailReason::Timeout,
                    Some(Note::GaveUp("no answer to the request")),
                );
            }
            let Some(peer) = io.peer_obj else {
                // the partner left the view: wait a little for it to come back
                if now.saturating_sub(self.phase_ms) >= INVITE_WAIT_MS {
                    return Step::failed(FailReason::NoTarget, Some(Note::GaveUp("lost sight")));
                }
                return Step::running();
            };
            io.out.push(cs::can_start_trade(peer));
            self.peer = peer;
            self.asked += 1;
            self.phase_ms = now;
        }
        Step::running()
    }

    fn await_request(&mut self, io: &mut Io<'_>) -> Step {
        let now = io.now_ms;
        if let Some((obj, _)) = io.log.incoming {
            if io.incoming_ok {
                io.out.push(cs::start_trade(obj));
                self.peer = obj;
                self.accepted = true;
                self.phase_ms = now;
            } else {
                io.out.push(cs::cannot_start_trade(obj, 0));
            }
            io.log.incoming = None;
        }
        let limit = if self.accepted {
            ACCEPT_WAIT_MS
        } else {
            AWAIT_REQUEST_MS
        };
        if now.saturating_sub(self.phase_ms) > limit {
            return Step::failed(
                FailReason::Timeout,
                Some(Note::GaveUp("no trade window opened")),
            );
        }
        Step::running()
    }

    fn open_window(&mut self, io: &mut Io<'_>) -> Step {
        let now = io.now_ms;
        self.item_sent = false;
        self.money_sent = false;
        self.resend_ms = 0;
        self.lock_sent = false;
        self.locked_once = false;
        self.relocks = 0;
        self.aux_ms = 0;
        if let Some(deal) = self.deal.as_deref() {
            // a sale agreed in chat: the goods and the copper must still be there
            self.plan = deal.give;
            if !can_give(io.bag, io.money, &deal.give) {
                io.out.push(cs::cancel_trade(0));
                return Step::failed(
                    FailReason::Other("the goods are gone"),
                    Some(Note::GaveUp("the agreed goods are not in the bag any more")),
                );
            }
        } else {
            match choose_offer(io.bag, io.money, io.allow_other, io.keep, io.rng) {
                Some(plan) => self.plan = plan,
                None => {
                    self.plan = Plan::default();
                    io.out.push(cs::cancel_trade(0));
                    return Step::failed(
                        FailReason::Other("nothing to offer"),
                        Some(Note::GaveUp("nothing to offer")),
                    );
                }
            }
            let deal = if io.partner_is_bot {
                Deal::generous(self.plan)
            } else {
                Deal::fair(self.plan, FAIR_PERCENT, io.worth)
            };
            self.deal = Some(Box::new(deal));
        }
        self.offered_ms = now;
        Step {
            status: GoalStatus::Running,
            note: Some(Note::Opened),
        }
    }

    fn cancel(&mut self, io: &mut Io<'_>, why: &'static str, reason: FailReason) -> Step {
        io.out.push(cs::cancel_trade(0));
        Step::failed(reason, Some(Note::GaveUp(why)))
    }

    /// Ends the trade because the window is not the deal.
    fn refuse(&mut self, io: &mut Io<'_>, why: Refusal) -> Step {
        io.out.push(cs::cancel_trade(0));
        Step::failed(FailReason::Other("trade refused"), Some(Note::Refused(why)))
    }

    fn window(&mut self, io: &mut Io<'_>) -> Step {
        let now = io.now_ms;
        let Some(w) = io.log.window.as_deref() else {
            // closed without an answer yet: the outcome is on its way
            if now.saturating_sub(self.phase_ms) > 3_000 {
                return Step::failed(
                    FailReason::Other("trade window closed"),
                    Some(Note::GaveUp("the window closed")),
                );
            }
            return Step::running();
        };
        let strict = self.deal.as_deref().is_some_and(|d| d.strict);
        let max_ms = if strict { DEAL_MAX_MS } else { TRADE_MAX_MS };
        if now.saturating_sub(w.opened_ms) > max_ms {
            return self.cancel(io, "took too long", FailReason::Timeout);
        }
        let (my_lock, other_lock, my_ok) = (w.my_lock, w.other_lock, w.my_ok);
        let changed = w.changed_ms;
        let (own, verdict) = match self.deal.as_deref() {
            Some(d) => (deal::own_offer(d, w), verify(d, w, io.identity, io.worth)),
            None => (deal::Own::Pending, Verdict::Wait),
        };
        // the wrong person, or an offer of ours that is not the deal, ends it before anything is
        // put up
        if let Verdict::Refuse(r @ (Refusal::WrongPartner | Refusal::MineChanged)) = verdict {
            return self.refuse(io, r);
        }

        if !self.item_sent && self.plan.amount > 0 {
            io.out.push(cs::putup_trade_item(
                SLOT_BAG,
                self.plan.slot,
                self.plan.amount,
            ));
            self.item_sent = true;
            self.offered_ms = now;
        }
        if !self.money_sent && self.plan.money > 0 {
            io.out.push(cs::putup_trade_money(self.plan.money));
            self.money_sent = true;
            self.offered_ms = now;
        }
        if own != deal::Own::Standing {
            if now.saturating_sub(self.offered_ms) > OFFER_WAIT_MS {
                return self.cancel(io, "the put-up was not accepted", FailReason::Timeout);
            }
            return Step::running();
        }
        if my_lock {
            // the lock came through (the OK may follow at once)
            self.locked_once = true;
            if self.lock_sent {
                self.lock_sent = false;
                self.resend_ms = 0;
            }
        }
        let quiet = now.saturating_sub(changed) >= QUIET_MS;
        match verdict {
            Verdict::Refuse(why) => {
                // after our lock a partner that changes the offer to something else ends it at
                // once; before it gets a moment to put it right
                if self.locked_once || self.lock_sent {
                    return self.refuse(io, why);
                }
                if self.aux_ms == 0 {
                    self.aux_ms = now.max(1);
                } else if now.saturating_sub(self.aux_ms) > REFUSE_GRACE_MS {
                    return self.refuse(io, why);
                }
                Step::running()
            }
            Verdict::Wait => {
                self.aux_ms = 0;
                if self.locked_once && !my_lock && !self.lock_sent {
                    // the server undid our lock: the partner changed the offer, and it is not
                    // the deal any more
                    return self.cancel(
                        io,
                        "the partner changed the offer after the lock",
                        FailReason::Other("offer changed after the lock"),
                    );
                }
                let patience = if strict {
                    DEAL_OFFER_WAIT_MS
                } else {
                    OFFER_WAIT_MS
                };
                if now.saturating_sub(self.offered_ms.max(changed)) >= patience {
                    let why = if strict {
                        "the partner did not put up what was agreed"
                    } else {
                        "the partner offered nothing"
                    };
                    return self.cancel(io, why, FailReason::NoTarget);
                }
                Step::running()
            }
            Verdict::Ok => {
                self.aux_ms = 0;
                if !my_lock && quiet && now >= self.resend_ms {
                    if !self.lock_sent && self.locked_once {
                        if self.relocks >= MAX_RELOCKS {
                            return self.cancel(
                                io,
                                "the partner keeps changing the offer",
                                FailReason::Other("offer kept changing"),
                            );
                        }
                        self.relocks += 1;
                    }
                    io.out.push(cs::trade_lock(true));
                    self.lock_sent = true;
                    self.resend_ms = now + RESEND_MS;
                }
                if my_lock && other_lock && !my_ok && quiet && now >= self.resend_ms {
                    io.out.push(cs::trade_ok());
                    self.resend_ms = now + RESEND_MS;
                }
                Step::running()
            }
        }
    }
}

/// Whether the bag and the purse still hold what `give` names.
fn can_give(bag: &FxHashMap<u16, Item>, money: i64, give: &Plan) -> bool {
    let item = give.amount <= 0
        || bag
            .get(&u16::from(give.slot))
            .is_some_and(|i| i.template == give.template && i.count >= give.amount);
    item && i64::from(give.money.max(0)) <= money
}

/// Whether the unit is the character `order` names.
fn is_partner(u: &Unit, o: &PairOrder) -> bool {
    u.kind == UnitKind::Character
        && ((o.partner_char != 0 && u.id == o.partner_char)
            || (!o.partner_name.is_empty()
                && u.name
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(&o.partner_name))))
}

/// "5 x Copper Ore + 120 copper" (or "nothing").
fn describe_offer(world: &aa_world::World, offer: &Offer, mine: Option<Plan>) -> String {
    let mut s = String::new();
    for p in &offer.items {
        let template = if p.template != 0 {
            p.template
        } else {
            mine.map_or(0, |m| m.template)
        };
        if !s.is_empty() {
            s.push_str(" + ");
        }
        let name = world.item_name(template).unwrap_or("item");
        let _ = write!(s, "{} x {name}", p.amount);
    }
    if offer.money > 0 {
        if !s.is_empty() {
            s.push_str(" + ");
        }
        let _ = write!(s, "{} copper", offer.money);
    }
    if s.is_empty() {
        s.push_str("nothing");
    }
    s
}

/// Shows `bot` as the trade partner in the web UI (`None` clears it).
fn set_partner(ctx: &Ctx<'_>, bot: Option<u32>) {
    let bot = bot.filter(|b| *b != 0);
    if let Some(slot) = ctx.hub.slot(ctx.persona.id) {
        slot.with(|g| g.snap.partner = bot);
    }
}

/// Whether real players (characters that are not our bots) may trade with this bot: only with
/// `behavior.trade_with_players`, and never while `bots.gm_bootstrap` is on, because what the bot
/// holds may then be GM-made goods and gold that no player should be able to collect.
pub fn players_may_trade(cfg: &crate::config::Config) -> bool {
    cfg.behavior.trade_with_players && !cfg.bots.gm_bootstrap
}

/// Whether the unit `obj` is a character of one of our bots (known by the hub).
fn is_bot_unit(ctx: &Ctx<'_>, obj: u32) -> bool {
    ctx.state
        .units
        .get(&obj)
        .is_some_and(|u| u.kind == UnitKind::Character && ctx.hub.is_bot_char(u.id))
}

/// Whether this bot may trade with the unit `obj` when no pairing order names the partner: another
/// bot of ours, or anybody if [`players_may_trade`]. A unit the bot cannot see is not a known bot.
pub fn may_trade_with(ctx: &Ctx<'_>, obj: u32) -> bool {
    players_may_trade(ctx.cfg) || is_bot_unit(ctx, obj)
}

/// What the brain does with a trade request nobody asked the bot's goals for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    /// Nothing pending (or the running trade goal handles it).
    None,
    /// Start the trade goal, it answers the request.
    Accept,
    /// Tell the requester no.
    Decline,
    /// Drop a request that is too old to answer.
    Expire,
}

/// The decision behind [`unsolicited`]: a request is accepted when the bot is idle (no goal, or
/// walking around or resting), not fighting, alive, and trading has a weight above 0.
pub fn decide(
    running: Option<GoalKind>,
    weight: f32,
    disabled: bool,
    combat_busy: bool,
    dead: bool,
    age_ms: u64,
) -> Reply {
    if running == Some(GoalKind::Trade) {
        return Reply::None;
    }
    if age_ms > INVITE_TTL_MS {
        return Reply::Expire;
    }
    let idle = matches!(
        running,
        None | Some(GoalKind::Wander) | Some(GoalKind::Rest)
    );
    if idle && !combat_busy && !dead && !disabled && weight > 0.0 {
        Reply::Accept
    } else {
        Reply::Decline
    }
}

/// Called by the brain on every tick. Answers a trade request that no trade goal handles and
/// cancels a window nobody tends; returns whether the brain should start the trade goal now.
pub fn unsolicited(
    ctx: &mut Ctx<'_>,
    running: Option<GoalKind>,
    weight: f32,
    disabled: bool,
) -> bool {
    // (the market goal trades through its own trade goal and declines strangers itself)
    if matches!(running, Some(GoalKind::Trade | GoalKind::Market)) {
        return false;
    }
    if ctx.state.trade.window.is_some() {
        // a window without a goal (the goal ended in the middle of it): do not leave it hanging,
        // but ask a few times at a leisurely pace, not once per tick
        if ctx.state.trade.orphan_cancel_due(ctx.now_ms) {
            ctx.send(cs::cancel_trade(0));
        }
        return false;
    }
    ctx.state.trade.orphan_reset();
    let Some((obj, at)) = ctx.state.trade.incoming else {
        return false;
    };
    let allowed = may_trade_with(ctx, obj);
    let age = ctx.now_ms.saturating_sub(at);
    let reply = decide(
        running,
        weight,
        disabled || !allowed,
        ctx.combat.busy(),
        ctx.state.me.dead,
        age,
    );
    match reply {
        Reply::None => false,
        Reply::Expire => {
            ctx.state.trade.incoming = None;
            false
        }
        Reply::Accept => true,
        Reply::Decline => {
            ctx.send(cs::cannot_start_trade(obj, 0));
            ctx.state.trade.incoming = None;
            ctx.log.debug("trade: declined a request (busy)");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aa_proto::sc_activity::{ActivityEvent, ItemBrief};

    fn item(id: u64, template: u32, count: i32) -> Item {
        Item {
            id,
            template,
            count,
        }
    }

    fn bag(items: &[(u16, Item)]) -> FxHashMap<u16, Item> {
        items.iter().copied().collect()
    }

    struct Rig {
        log: TradeLog,
        bag: FxHashMap<u16, Item>,
        money: i64,
        out: Vec<Out>,
        rng: fastrand::Rng,
        peer_obj: Option<u32>,
        incoming_ok: bool,
        bound_err: bool,
        allow_other: bool,
        identity: Identity,
        partner_is_bot: bool,
        /// Templates the bot must not give away (not tradable).
        untradable: Vec<u32>,
        /// Units of a template its quests need.
        held: Vec<(u32, u32)>,
    }

    impl Rig {
        fn new() -> Rig {
            Rig {
                log: TradeLog::default(),
                bag: bag(&[(4, item(900, 3411, 12))]),
                money: 0,
                out: Vec::new(),
                rng: fastrand::Rng::with_seed(5),
                peer_obj: Some(77),
                incoming_ok: true,
                bound_err: false,
                allow_other: true,
                identity: Identity::Match,
                partner_is_bot: true,
                untradable: Vec::new(),
                held: Vec::new(),
            }
        }

        fn step(&mut self, t: &mut Trade, now_ms: u64) -> Step {
            let (untradable, held) = (&self.untradable, &self.held);
            let keep = |template: u32| {
                (!untradable.contains(&template))
                    .then(|| held.iter().find(|h| h.0 == template).map_or(0, |h| h.1))
            };
            let mut io = Io {
                now_ms,
                log: &mut self.log,
                bag: &self.bag,
                money: self.money,
                peer_obj: self.peer_obj,
                incoming_ok: self.incoming_ok,
                bound_err: self.bound_err,
                allow_other: self.allow_other,
                identity: self.identity,
                partner_is_bot: self.partner_is_bot,
                keep: &keep,
                worth: &|template| if template == 3411 { 20 } else { 5 },
                out: &mut self.out,
                rng: &mut self.rng,
            };
            t.step(&mut io)
        }

        fn ev(&mut self, e: ActivityEvent, now_ms: u64) {
            self.log.apply(&e, now_ms);
        }

        /// The packets sent since the last call.
        fn sent(&mut self) -> Vec<Out> {
            std::mem::take(&mut self.out)
        }
    }

    fn order(role: PairRole) -> PairOrder {
        PairOrder {
            kind: PairKind::Trade,
            role,
            partner_bot: 2,
            partner_char: 20,
            partner_name: "Other".into(),
            meet_at: (0.0, 0.0),
        }
    }

    fn op(f: impl FnOnce() -> Out) -> Out {
        f()
    }

    fn at(phase: Phase, role: PairRole) -> Trade {
        let mut t = Trade::new(Some(order(role)));
        t.phase = phase;
        t
    }

    #[test]
    fn the_initiator_asks_once_and_retries_after_the_wait() {
        let mut r = Rig::new();
        let mut t = at(Phase::Invite, PairRole::Initiator);
        assert_eq!(r.step(&mut t, 1000).status, GoalStatus::Running);
        assert_eq!(r.sent(), [op(|| cs::can_start_trade(77))]);
        // no answer yet: nothing new
        r.step(&mut t, 5000);
        assert!(r.sent().is_empty());
        // after the wait, one retry
        r.step(&mut t, 14_000);
        assert_eq!(r.sent(), [op(|| cs::can_start_trade(77))]);
        // and then it gives up
        let s = r.step(&mut t, 27_000);
        assert_eq!(s.status, GoalStatus::Failed(FailReason::Timeout));
    }

    #[test]
    fn a_refusal_ends_the_invitation() {
        let mut r = Rig::new();
        let mut t = at(Phase::Invite, PairRole::Initiator);
        r.step(&mut t, 1000);
        r.sent();
        r.ev(ActivityEvent::CannotStartTrade { obj: 77, reason: 1 }, 1500);
        let s = r.step(&mut t, 2000);
        assert_eq!(
            s.status,
            GoalStatus::Failed(FailReason::Other("partner declined"))
        );
    }

    #[test]
    fn an_initiator_that_cannot_see_the_partner_waits_then_gives_up() {
        let mut r = Rig::new();
        r.peer_obj = None;
        let mut t = at(Phase::Invite, PairRole::Initiator);
        t.phase_ms = 1000;
        assert_eq!(r.step(&mut t, 1500).status, GoalStatus::Running);
        assert!(r.sent().is_empty());
        let s = r.step(&mut t, 14_000);
        assert_eq!(s.status, GoalStatus::Failed(FailReason::NoTarget));
    }

    #[test]
    fn the_responder_accepts_the_partners_request_and_declines_strangers() {
        let mut r = Rig::new();
        let mut t = at(Phase::Await, PairRole::Responder);
        t.phase_ms = 1000;
        r.ev(ActivityEvent::CanStartTrade { requester_obj: 77 }, 2000);
        r.step(&mut t, 2100);
        assert_eq!(r.sent(), [op(|| cs::start_trade(77))]);
        assert!(r.log.incoming.is_none());
        assert!(t.accepted);

        let mut r = Rig::new();
        r.incoming_ok = false;
        let mut t = at(Phase::Await, PairRole::Responder);
        t.phase_ms = 1000;
        r.ev(ActivityEvent::CanStartTrade { requester_obj: 99 }, 2000);
        r.step(&mut t, 2100);
        assert_eq!(r.sent(), [op(|| cs::cannot_start_trade(99, 0))]);
        assert!(!t.accepted);
    }

    #[test]
    fn a_responder_gives_up_when_nothing_happens() {
        let mut r = Rig::new();
        let mut t = at(Phase::Await, PairRole::Responder);
        t.phase_ms = 1000;
        assert_eq!(r.step(&mut t, 50_000).status, GoalStatus::Running);
        let s = r.step(&mut t, 1000 + AWAIT_REQUEST_MS + 1);
        assert_eq!(s.status, GoalStatus::Failed(FailReason::Timeout));
        // after accepting the wait is much shorter
        let mut r = Rig::new();
        let mut t = at(Phase::Await, PairRole::Responder);
        t.phase_ms = 1000;
        r.ev(ActivityEvent::CanStartTrade { requester_obj: 77 }, 1500);
        r.step(&mut t, 2000);
        let s = r.step(&mut t, 2000 + ACCEPT_WAIT_MS + 1);
        assert_eq!(s.status, GoalStatus::Failed(FailReason::Timeout));
    }

    /// Drives a whole trade from the initiator's side with synthetic server events.
    #[test]
    fn a_full_trade_puts_up_locks_confirms_and_finishes() {
        let mut r = Rig::new();
        let mut t = at(Phase::Invite, PairRole::Initiator);
        r.step(&mut t, 1000);
        r.sent();
        r.ev(ActivityEvent::TradeStarted { peer_obj: 77 }, 1200);
        // the window opens: the bot picks its offer and puts it up
        let s = r.step(&mut t, 1300);
        assert_eq!(s.note, Some(Note::Opened));
        assert_eq!(t.phase, Phase::Window);
        assert_eq!(t.plan.template, 3411);
        assert_eq!(t.plan.slot, 4);
        assert!((1..=5).contains(&t.plan.amount));
        r.step(&mut t, 1500);
        let sent = r.sent();
        assert_eq!(
            sent,
            [op(|| cs::putup_trade_item(SLOT_BAG, 4, t.plan.amount))]
        );
        // our put-up is confirmed, the partner's has not arrived: no lock yet
        r.ev(
            ActivityEvent::TradeItemPutup {
                slot_type: 2,
                slot: 4,
                amount: t.plan.amount,
            },
            1600,
        );
        r.step(&mut t, 3000);
        assert!(r.sent().is_empty());
        // the partner's offer arrives; the offers must be quiet for a moment
        r.ev(
            ActivityEvent::OtherTradeItemPutup {
                item: ItemBrief {
                    template_id: 3712,
                    count: 3,
                    ..ItemBrief::default()
                },
            },
            3100,
        );
        r.step(&mut t, 3300);
        assert!(r.sent().is_empty(), "locked before the offers were quiet");
        r.step(&mut t, 3900);
        assert_eq!(r.sent(), [op(|| cs::trade_lock(true))]);
        // the lock is not repeated while the answer is on its way
        r.step(&mut t, 4200);
        assert!(r.sent().is_empty());
        // both locked: confirm
        r.ev(
            ActivityEvent::TradeLockUpdate {
                mine: true,
                other: true,
            },
            4300,
        );
        r.step(&mut t, 4400);
        assert_eq!(r.sent(), [op(cs::trade_ok)]);
        r.ev(
            ActivityEvent::TradeOkUpdate {
                mine: true,
                other: false,
            },
            4500,
        );
        r.step(&mut t, 4600);
        assert!(r.sent().is_empty());
        // the server finishes the trade
        r.ev(
            ActivityEvent::TradeOkUpdate {
                mine: true,
                other: true,
            },
            4700,
        );
        r.ev(
            ActivityEvent::TradeMade {
                action: 0,
                tasks: 2,
                items: Default::default(),
            },
            4800,
        );
        let s = r.step(&mut t, 4900);
        assert_eq!(s.status, GoalStatus::Done);
        let Some(Note::Made(w)) = s.note else {
            panic!("expected Made");
        };
        assert_eq!(w.mine.items[0].amount, t.plan.amount);
        assert_eq!(w.other.items[0].template, 3712);
    }

    #[test]
    fn a_changed_offer_unlocks_and_the_bot_locks_again() {
        let mut r = Rig::new();
        let mut t = at(Phase::Window, PairRole::Initiator);
        r.ev(ActivityEvent::TradeStarted { peer_obj: 77 }, 1000);
        t.phase = Phase::Invite;
        r.step(&mut t, 1100);
        r.step(&mut t, 1200);
        r.sent();
        let amount = t.plan.amount;
        r.ev(
            ActivityEvent::TradeItemPutup {
                slot_type: 2,
                slot: 4,
                amount,
            },
            1300,
        );
        r.ev(ActivityEvent::OtherTradeMoneyPutup { amount: 300 }, 1400);
        r.step(&mut t, 2200);
        assert_eq!(r.sent(), [op(|| cs::trade_lock(true))]);
        r.ev(
            ActivityEvent::TradeLockUpdate {
                mine: true,
                other: false,
            },
            2300,
        );
        // the partner changes its mind: the server unlocks both
        r.ev(ActivityEvent::OtherTradeMoneyPutup { amount: 500 }, 2500);
        r.ev(
            ActivityEvent::TradeLockUpdate {
                mine: false,
                other: false,
            },
            2501,
        );
        r.step(&mut t, 2600);
        assert!(r.sent().is_empty(), "locked again before things settled");
        r.step(&mut t, 4600);
        assert_eq!(r.sent(), [op(|| cs::trade_lock(true))]);
    }

    #[test]
    fn money_only_offers_use_the_money_packet() {
        let mut r = Rig::new();
        r.bag.clear();
        r.money = 5_000;
        let mut t = at(Phase::Invite, PairRole::Initiator);
        r.ev(ActivityEvent::TradeStarted { peer_obj: 77 }, 1000);
        r.step(&mut t, 1100);
        assert_eq!(t.plan.amount, 0);
        assert!((100..=500).contains(&t.plan.money));
        r.step(&mut t, 1200);
        assert_eq!(r.sent(), [op(|| cs::putup_trade_money(t.plan.money))]);
    }

    #[test]
    fn a_window_does_not_open_with_items_the_bot_must_keep() {
        // the only stack is a quest item: the window is cancelled instead of putting it up
        let mut r = Rig::new();
        r.bag = bag(&[(4, item(900, 7716, 12))]);
        r.untradable.push(7716);
        let mut t = at(Phase::Invite, PairRole::Initiator);
        r.ev(ActivityEvent::TradeStarted { peer_obj: 77 }, 1000);
        let s = r.step(&mut t, 1100);
        assert_eq!(
            s.status,
            GoalStatus::Failed(FailReason::Other("nothing to offer"))
        );
        assert_eq!(r.sent(), [op(|| cs::cancel_trade(0))]);
        // the same for goods the bot's quests need in full
        let mut r = Rig::new();
        r.held.push((3411, 12));
        let mut t = at(Phase::Invite, PairRole::Initiator);
        r.ev(ActivityEvent::TradeStarted { peer_obj: 77 }, 1000);
        let s = r.step(&mut t, 1100);
        assert_eq!(
            s.status,
            GoalStatus::Failed(FailReason::Other("nothing to offer"))
        );
    }

    #[test]
    fn nothing_to_offer_cancels_the_window() {
        let mut r = Rig::new();
        r.bag.clear();
        let mut t = at(Phase::Invite, PairRole::Initiator);
        r.ev(ActivityEvent::TradeStarted { peer_obj: 77 }, 1000);
        let s = r.step(&mut t, 1100);
        assert_eq!(
            s.status,
            GoalStatus::Failed(FailReason::Other("nothing to offer"))
        );
        assert_eq!(r.sent(), [op(|| cs::cancel_trade(0))]);
    }

    #[test]
    fn a_partner_that_offers_nothing_is_left_after_the_wait() {
        let mut r = Rig::new();
        let mut t = at(Phase::Invite, PairRole::Initiator);
        r.ev(ActivityEvent::TradeStarted { peer_obj: 77 }, 1000);
        r.step(&mut t, 1100);
        r.step(&mut t, 1200);
        r.sent();
        r.ev(
            ActivityEvent::TradeItemPutup {
                slot_type: 2,
                slot: 4,
                amount: t.plan.amount,
            },
            1300,
        );
        assert_eq!(r.step(&mut t, 5000).status, GoalStatus::Running);
        let s = r.step(&mut t, 1200 + OFFER_WAIT_MS + 500);
        assert_eq!(s.status, GoalStatus::Failed(FailReason::NoTarget));
        assert_eq!(r.sent(), [op(|| cs::cancel_trade(0))]);
    }

    #[test]
    fn a_put_up_the_server_ignores_ends_the_trade() {
        let mut r = Rig::new();
        let mut t = at(Phase::Invite, PairRole::Initiator);
        r.ev(ActivityEvent::TradeStarted { peer_obj: 77 }, 1000);
        r.step(&mut t, 1100);
        r.step(&mut t, 1200);
        r.sent();
        // no SCTradeItemPutup ever comes
        let s = r.step(&mut t, 1200 + OFFER_WAIT_MS + 500);
        assert_eq!(s.status, GoalStatus::Failed(FailReason::Timeout));
        assert_eq!(r.sent(), [op(|| cs::cancel_trade(0))]);
    }

    #[test]
    fn a_cancel_ends_the_goal_and_a_bound_item_is_remembered() {
        let mut r = Rig::new();
        let mut t = at(Phase::Invite, PairRole::Initiator);
        r.ev(ActivityEvent::TradeStarted { peer_obj: 77 }, 1000);
        r.step(&mut t, 1100);
        r.step(&mut t, 1200);
        r.ev(
            ActivityEvent::TradeCanceled {
                reason: 0,
                caused_by_me: true,
            },
            1300,
        );
        r.bound_err = true;
        let s = r.step(&mut t, 1400);
        assert_eq!(s.note, Some(Note::Bound(3411)));
        assert!(matches!(s.status, GoalStatus::Failed(_)));

        // a partner's cancel is just a cancel
        let mut r = Rig::new();
        let mut t = at(Phase::Invite, PairRole::Initiator);
        r.ev(ActivityEvent::TradeStarted { peer_obj: 77 }, 1000);
        r.step(&mut t, 1100);
        r.ev(
            ActivityEvent::TradeCanceled {
                reason: 0,
                caused_by_me: false,
            },
            1300,
        );
        let s = r.step(&mut t, 1400);
        assert_eq!(
            s.note,
            Some(Note::Canceled {
                reason: 0,
                by_me: false
            })
        );
    }

    #[test]
    fn a_stray_cancel_before_the_window_is_ignored() {
        let mut r = Rig::new();
        let mut t = at(Phase::Invite, PairRole::Initiator);
        r.ev(
            ActivityEvent::TradeCanceled {
                reason: 0,
                caused_by_me: true,
            },
            900,
        );
        assert_eq!(r.step(&mut t, 1000).status, GoalStatus::Running);
        assert_eq!(r.sent(), [op(|| cs::can_start_trade(77))]);
    }

    #[test]
    fn the_end_of_an_earlier_trade_is_not_ours() {
        // a goal that started after one window opened ignores that window's cancel
        let mut r = Rig::new();
        r.ev(ActivityEvent::TradeStarted { peer_obj: 5 }, 100);
        let mut t = at(Phase::Invite, PairRole::Initiator);
        t.base_seq = r.log.opened;
        r.ev(
            ActivityEvent::TradeCanceled {
                reason: 0,
                caused_by_me: true,
            },
            200,
        );
        assert_eq!(r.step(&mut t, 300).status, GoalStatus::Running);
        assert!(r.log.outcome.is_none());
        assert_eq!(r.sent(), [op(|| cs::can_start_trade(77))]);
        // but the cancel of its own window counts
        r.ev(ActivityEvent::TradeStarted { peer_obj: 77 }, 400);
        r.ev(
            ActivityEvent::TradeCanceled {
                reason: 0,
                caused_by_me: false,
            },
            500,
        );
        assert!(matches!(r.step(&mut t, 600).status, GoalStatus::Failed(_)));
    }

    #[test]
    fn the_window_has_a_time_limit() {
        let mut r = Rig::new();
        let mut t = at(Phase::Invite, PairRole::Initiator);
        r.ev(ActivityEvent::TradeStarted { peer_obj: 77 }, 1000);
        r.step(&mut t, 1100);
        r.step(&mut t, 1200);
        r.sent();
        r.ev(
            ActivityEvent::TradeItemPutup {
                slot_type: 2,
                slot: 4,
                amount: t.plan.amount,
            },
            1300,
        );
        r.ev(ActivityEvent::OtherTradeMoneyPutup { amount: 100 }, 1400);
        r.ev(
            ActivityEvent::TradeLockUpdate {
                mine: true,
                other: false,
            },
            2000,
        );
        let s = r.step(&mut t, 1000 + TRADE_MAX_MS + 1);
        assert_eq!(s.status, GoalStatus::Failed(FailReason::Timeout));
        assert_eq!(r.sent(), [op(|| cs::cancel_trade(0))]);
    }

    #[test]
    fn offers_prefer_tradable_goods_and_never_exceed_the_stack_or_the_purse() {
        let mut rng = fastrand::Rng::with_seed(1);
        // a bound-looking singleton is never chosen; goods win over other stacks
        let b = bag(&[
            (0, item(1, 5555, 1)),
            (1, item(2, 6666, 30)),
            (2, item(3, 3411, 3)),
        ]);
        for _ in 0..200 {
            let p = choose_offer(&b, 0, true, &any, &mut rng).unwrap();
            assert_eq!((p.slot, p.template), (2, 3411));
            assert!((1..=3).contains(&p.amount));
            assert_eq!(p.money, 0);
        }
        // without goods, a stack of two or more is acceptable
        let b = bag(&[(0, item(1, 5555, 1)), (1, item(2, 6666, 30))]);
        let p = choose_offer(&b, 0, true, &any, &mut rng).unwrap();
        assert_eq!((p.slot, p.template), (1, 6666));
        assert!((1..=5).contains(&p.amount));
        // a bot that can mint goods does not offer unknown stacks
        assert_eq!(choose_offer(&b, 0, false, &any, &mut rng), None);
        // coins and tax certificates are never offered
        let b = bag(&[(0, item(1, 500, 100)), (1, item(2, 31891, 50))]);
        assert_eq!(choose_offer(&b, 0, true, &any, &mut rng), None);
        // money: only with a purse, never more than a tenth (at least 100, at most 5000)
        let empty = FxHashMap::default();
        assert_eq!(choose_offer(&empty, 999, true, &any, &mut rng), None);
        for money in [1_000, 7_777, 1_000_000] {
            let p = choose_offer(&empty, money, true, &any, &mut rng).unwrap();
            assert_eq!(p.amount, 0);
            assert!(i64::from(p.money) <= money / 10 + 100 && p.money >= 100 && p.money <= 5_000);
        }
        // slots above 255 do not fit the packet
        let b = bag(&[(300, item(1, 3411, 5))]);
        assert_eq!(choose_offer(&b, 0, true, &any, &mut rng), None);
    }

    /// The rule of a bot whose items are all tradable and no quest needs any.
    fn any(_: u32) -> Option<u32> {
        Some(0)
    }

    #[test]
    fn untradable_items_are_never_offered() {
        let mut rng = fastrand::Rng::with_seed(1);
        // a quest item among the goods, and unsellable loot among the other stacks
        let b = bag(&[
            (0, item(1, 7716, 30)),
            (1, item(2, 3411, 8)),
            (2, item(3, 8888, 30)),
        ]);
        let rule = |t: u32| (t != 7716 && t != 8888).then_some(0);
        for _ in 0..100 {
            let p = choose_offer(&b, 0, true, &rule, &mut rng).unwrap();
            assert_eq!((p.slot, p.template), (1, 3411));
        }
        // without the goods, nothing of the two is chosen either
        let b = bag(&[(0, item(1, 7716, 30)), (2, item(3, 8888, 30))]);
        assert_eq!(choose_offer(&b, 0, true, &rule, &mut rng), None);
        // a good the rule does not allow is not offered, an unknown template (the rule says
        // `None`) neither
        let b = bag(&[(0, item(1, 3411, 30))]);
        assert_eq!(choose_offer(&b, 0, true, &|_| None, &mut rng), None);
        // money is still offered
        assert!(choose_offer(&b, 5_000, true, &|_| None, &mut rng)
            .is_some_and(|p| p.amount == 0 && p.money > 0));
    }

    #[test]
    fn what_an_active_quest_needs_is_not_offered() {
        let mut rng = fastrand::Rng::with_seed(1);
        // 12 hay bales in two stacks, a quest needs 10: 2 are free, from either stack
        let b = bag(&[(0, item(1, 3712, 7)), (1, item(2, 3712, 5))]);
        let rule = |t: u32| Some(if t == 3712 { 10 } else { 0 });
        for _ in 0..100 {
            let p = choose_offer(&b, 0, true, &rule, &mut rng).unwrap();
            assert_eq!(p.template, 3712);
            assert!((1..=2).contains(&p.amount), "{}", p.amount);
        }
        // all of it needed: nothing to offer
        let all = |t: u32| Some(if t == 3712 { 12 } else { 0 });
        assert_eq!(choose_offer(&b, 0, true, &all, &mut rng), None);
        // a bot whose goods are all needed offers another tradable stack instead
        let b = bag(&[(0, item(1, 3712, 7)), (1, item(2, 6666, 30))]);
        let p = choose_offer(&b, 0, true, &all, &mut rng).unwrap();
        assert_eq!(p.template, 6666);
    }

    #[test]
    fn bound_templates_are_not_offered_again() {
        let mut rng = fastrand::Rng::with_seed(1);
        let b = bag(&[(0, item(1, 424_242, 9))]);
        assert!(choose_offer(&b, 0, true, &any, &mut rng).is_some());
        learn_bound(424_242);
        assert!(is_bound(424_242));
        assert_eq!(choose_offer(&b, 0, true, &any, &mut rng), None);
    }

    #[test]
    fn partners_are_recognised_by_character_id_or_name() {
        let o = order(PairRole::Initiator);
        let unit = |id: u32, name: &str, kind: UnitKind| Unit {
            kind,
            id,
            pos: [0.0; 3],
            hp: 1,
            faction: 0,
            level: 1,
            flags: 0,
            target: 0,
            name: Some(name.into()),
        };
        assert!(is_partner(&unit(20, "x", UnitKind::Character), &o));
        assert!(is_partner(&unit(5, "OTHER", UnitKind::Character), &o));
        assert!(!is_partner(&unit(5, "Else", UnitKind::Character), &o));
        assert!(!is_partner(&unit(20, "Other", UnitKind::Npc), &o));
        let mut nameless = o.clone();
        nameless.partner_char = 0;
        nameless.partner_name = "".into();
        assert!(!is_partner(&unit(0, "", UnitKind::Character), &nameless));
    }

    #[test]
    fn unexpected_requests_are_accepted_only_when_idle_and_wanted() {
        use GoalKind as G;
        let ok = |g: Option<G>, w, d, c, dead, age| decide(g, w, d, c, dead, age);
        assert_eq!(ok(None, 0.2, false, false, false, 100), Reply::Accept);
        assert_eq!(
            ok(Some(G::Wander), 0.2, false, false, false, 100),
            Reply::Accept
        );
        assert_eq!(
            ok(Some(G::Rest), 0.2, false, false, false, 100),
            Reply::Accept
        );
        assert_eq!(
            ok(Some(G::Grind), 0.2, false, false, false, 100),
            Reply::Decline
        );
        assert_eq!(
            ok(Some(G::Duel), 0.2, false, false, false, 100),
            Reply::Decline
        );
        assert_eq!(ok(None, 0.0, false, false, false, 100), Reply::Decline);
        assert_eq!(ok(None, 0.2, true, false, false, 100), Reply::Decline);
        assert_eq!(ok(None, 0.2, false, true, false, 100), Reply::Decline);
        assert_eq!(ok(None, 0.2, false, false, true, 100), Reply::Decline);
        assert_eq!(ok(None, 0.2, false, false, false, 61_000), Reply::Expire);
        // the running trade goal answers for itself
        assert_eq!(
            ok(Some(G::Trade), 0.2, false, false, false, 100),
            Reply::None
        );
    }

    #[test]
    fn real_players_may_trade_only_when_asked_for_and_never_with_gm_goods() {
        use crate::config::Config;
        let mut cfg = Config::default();
        assert!(!players_may_trade(&cfg), "off by default");
        cfg.behavior.trade_with_players = true;
        assert!(players_may_trade(&cfg));
        // what a GM-bootstrapped bot holds may be minted: nothing of it goes to a player
        cfg.bots.gm_bootstrap = true;
        assert!(!players_may_trade(&cfg));
        cfg.behavior.trade_with_players = false;
        assert!(!players_may_trade(&cfg));
        // a request from a player the gate does not allow is declined by the brain
        assert_eq!(
            decide(None, 0.2, true, false, false, 100),
            Reply::Decline,
            "`disabled` is how the brain passes a requester the gate refused"
        );
    }

    #[test]
    fn descriptions_follow_the_phase() {
        let mut t = Trade::new(Some(order(PairRole::Initiator)));
        t.peer_name = Some("Other".into());
        let mut s = String::new();
        t.describe(&mut s);
        assert_eq!(s, "getting something to trade");
        t.phase = Phase::Meet;
        s.clear();
        t.describe(&mut s);
        assert_eq!(s, "walking to Other to trade");
        t.phase = Phase::Window;
        t.plan = Plan {
            slot: 1,
            template: 3411,
            amount: 4,
            money: 250,
        };
        s.clear();
        t.describe(&mut s);
        assert_eq!(s, "trading with Other: offering 4 item(s) + 250c");
    }

    #[test]
    fn a_made_trade_applies_its_inline_item_tasks() {
        use aa_proto::opcodes::sc as op;
        use aa_proto::sc::ServerEvent;
        use aa_proto::sc_world::ItemChange;
        // action, 1 task: money -250, no forced removals, the trailing u32
        let mut body = vec![14u8, 1, 1];
        body.extend((-250i32).to_le_bytes());
        body.push(0);
        body.extend(0u32.to_le_bytes());
        let ev = aa_proto::sc::parse(aa_proto::game::LEVEL_GAME, op::SCTradeMadePacket, &body)
            .unwrap()
            .unwrap();
        let ServerEvent::Activity(ActivityEvent::TradeMade { items, .. }) = &ev else {
            panic!("expected TradeMade");
        };
        assert_eq!(items.changes.as_slice(), [ItemChange::Money(-250)]);
        // a state that applies it loses the money
        let mut st = BotState::new();
        st.me.money = 1000;
        st.apply(&ev);
        assert_eq!(st.me.money, 750);
    }

    // ---- safe trades: a sale agreed in chat ----------------------------------------------

    /// 10 copper ore from bag slot 4 for 3000 copper, the bot waits for the buyer.
    fn sale(role: PairRole) -> Trade {
        let give = Plan {
            slot: 4,
            template: 3411,
            amount: 10,
            money: 0,
        };
        Trade::for_deal(order(role), Deal::agreed(give, &[], 3000))
    }

    /// The window opens and the bot's put-up is confirmed; returns the clock after it.
    fn sale_window(r: &mut Rig, t: &mut Trade) -> u64 {
        r.ev(ActivityEvent::TradeStarted { peer_obj: 77 }, 1000);
        let s = r.step(t, 1100);
        assert_eq!(s.note, Some(Note::Opened));
        r.step(t, 1200);
        assert_eq!(r.sent(), [op(|| cs::putup_trade_item(SLOT_BAG, 4, 10))]);
        r.ev(
            ActivityEvent::TradeItemPutup {
                slot_type: 2,
                slot: 4,
                amount: 10,
            },
            1300,
        );
        1300
    }

    fn locks(sent: &[Out]) -> usize {
        sent.iter().filter(|o| **o == cs::trade_lock(true)).count()
    }

    fn other_item(template: u32, count: i32) -> ActivityEvent {
        ActivityEvent::OtherTradeItemPutup {
            item: ItemBrief {
                template_id: template,
                count,
                ..ItemBrief::default()
            },
        }
    }

    #[test]
    fn a_sale_is_locked_and_confirmed_only_for_the_agreed_money() {
        let mut r = Rig::new();
        let mut t = sale(PairRole::Responder);
        assert_eq!(t.phase, Phase::Await);
        sale_window(&mut r, &mut t);
        r.ev(ActivityEvent::OtherTradeMoneyPutup { amount: 3000 }, 2000);
        r.step(&mut t, 2400);
        assert!(r.sent().is_empty(), "locked before the offers were quiet");
        r.step(&mut t, 2800);
        assert_eq!(r.sent(), [op(|| cs::trade_lock(true))]);
        r.ev(
            ActivityEvent::TradeLockUpdate {
                mine: true,
                other: true,
            },
            2900,
        );
        r.step(&mut t, 3000);
        assert_eq!(r.sent(), [op(cs::trade_ok)]);
        r.ev(
            ActivityEvent::TradeMade {
                action: 0,
                tasks: 2,
                items: Default::default(),
            },
            3200,
        );
        assert_eq!(r.step(&mut t, 3300).status, GoalStatus::Done);
    }

    #[test]
    fn one_copper_for_ten_ore_is_never_locked() {
        let mut r = Rig::new();
        let mut t = sale(PairRole::Responder);
        sale_window(&mut r, &mut t);
        r.ev(ActivityEvent::OtherTradeMoneyPutup { amount: 1 }, 2000);
        let mut sent = Vec::new();
        for now in (2500..40_000).step_by(500) {
            let s = r.step(&mut t, now);
            sent.extend(r.sent());
            assert_eq!(s.status, GoalStatus::Running, "at {now}");
        }
        assert_eq!(locks(&sent), 0, "{sent:?}");
        // the partner never topped it up: the bot walks away
        let s = r.step(&mut t, 2000 + DEAL_OFFER_WAIT_MS + 1000);
        assert_eq!(s.status, GoalStatus::Failed(FailReason::NoTarget));
        assert_eq!(r.sent(), [op(|| cs::cancel_trade(0))]);
        assert_eq!(
            s.note,
            Some(Note::GaveUp("the partner did not put up what was agreed"))
        );
    }

    #[test]
    fn a_partner_that_tops_the_money_up_in_time_gets_the_sale() {
        let mut r = Rig::new();
        let mut t = sale(PairRole::Responder);
        sale_window(&mut r, &mut t);
        r.ev(ActivityEvent::OtherTradeMoneyPutup { amount: 1000 }, 2000);
        r.step(&mut t, 5000);
        assert!(r.sent().is_empty());
        r.ev(ActivityEvent::OtherTradeMoneyPutup { amount: 3000 }, 9000);
        r.step(&mut t, 9300);
        r.step(&mut t, 9900);
        assert_eq!(r.sent(), [op(|| cs::trade_lock(true))]);
    }

    #[test]
    fn a_swapped_item_is_refused_and_never_locked() {
        let mut r = Rig::new();
        let mut t = sale(PairRole::Responder);
        sale_window(&mut r, &mut t);
        // junk instead of the money
        r.ev(other_item(9999, 50), 2000);
        let mut sent = Vec::new();
        for now in (2500..12_000).step_by(500) {
            let s = r.step(&mut t, now);
            sent.extend(r.sent());
            if !matches!(s.status, GoalStatus::Running) {
                assert_eq!(s.note, Some(Note::Refused(Refusal::ExtraItem)));
                assert!(sent.contains(&cs::cancel_trade(0)));
                assert_eq!(locks(&sent), 0);
                return;
            }
        }
        panic!("the bot did not leave: {sent:?}");
    }

    #[test]
    fn the_right_money_and_a_junk_item_gets_nothing_until_the_junk_is_gone() {
        let mut r = Rig::new();
        let mut t = sale(PairRole::Responder);
        sale_window(&mut r, &mut t);
        r.ev(ActivityEvent::OtherTradeMoneyPutup { amount: 3000 }, 2000);
        r.ev(other_item(9999, 1), 2100);
        r.step(&mut t, 3000);
        assert_eq!(locks(&r.sent()), 0);
        // the partner takes the junk down in time: now it is the deal
        r.ev(
            ActivityEvent::OtherTradeItemTookdown {
                item: ItemBrief {
                    template_id: 9999,
                    count: 1,
                    ..ItemBrief::default()
                },
            },
            4000,
        );
        r.step(&mut t, 4200);
        r.step(&mut t, 4900);
        assert_eq!(r.sent(), [op(|| cs::trade_lock(true))]);
    }

    #[test]
    fn an_offer_changed_after_the_lock_ends_the_trade_at_once() {
        let mut r = Rig::new();
        let mut t = sale(PairRole::Responder);
        sale_window(&mut r, &mut t);
        r.ev(ActivityEvent::OtherTradeMoneyPutup { amount: 3000 }, 2000);
        r.step(&mut t, 2800);
        assert_eq!(r.sent(), [op(|| cs::trade_lock(true))]);
        r.ev(
            ActivityEvent::TradeLockUpdate {
                mine: true,
                other: false,
            },
            2900,
        );
        r.step(&mut t, 3000);
        r.sent();
        // the buyer swaps the money for one copper: the server undoes the locks
        r.ev(ActivityEvent::OtherTradeMoneyPutup { amount: 1 }, 3100);
        r.ev(
            ActivityEvent::TradeLockUpdate {
                mine: false,
                other: false,
            },
            3101,
        );
        let s = r.step(&mut t, 3200);
        assert!(matches!(s.status, GoalStatus::Failed(_)), "{s:?}");
        assert_eq!(r.sent(), [op(|| cs::cancel_trade(0))]);
        assert_eq!(
            s.note,
            Some(Note::GaveUp("the partner changed the offer after the lock"))
        );
    }

    #[test]
    fn a_better_offer_after_the_lock_is_locked_again_but_not_forever() {
        let mut r = Rig::new();
        let mut t = sale(PairRole::Responder);
        sale_window(&mut r, &mut t);
        r.ev(ActivityEvent::OtherTradeMoneyPutup { amount: 3000 }, 2000);
        r.step(&mut t, 2800);
        r.sent();
        let mut now = 2900;
        let mut money = 3000;
        for round in 0..=MAX_RELOCKS {
            r.ev(
                ActivityEvent::TradeLockUpdate {
                    mine: true,
                    other: false,
                },
                now,
            );
            r.step(&mut t, now + 100);
            r.sent();
            // the partner adds a little (still the deal): the server undoes the locks
            money += 100;
            now += 200;
            r.ev(ActivityEvent::OtherTradeMoneyPutup { amount: money }, now);
            r.ev(
                ActivityEvent::TradeLockUpdate {
                    mine: false,
                    other: false,
                },
                now + 1,
            );
            r.step(&mut t, now + 100);
            assert!(r.sent().is_empty(), "round {round}: not quiet yet");
            let s = r.step(&mut t, now + 900);
            if round < MAX_RELOCKS {
                assert_eq!(r.sent(), [op(|| cs::trade_lock(true))], "round {round}");
                assert_eq!(s.status, GoalStatus::Running);
                now += 1000;
            } else {
                assert!(matches!(s.status, GoalStatus::Failed(_)), "{s:?}");
                assert_eq!(locks(&r.sent()), 0);
            }
        }
    }

    #[test]
    fn a_window_of_somebody_else_is_cancelled_before_anything_is_put_up() {
        let mut r = Rig::new();
        r.identity = Identity::Mismatch;
        let mut t = sale(PairRole::Responder);
        r.ev(ActivityEvent::TradeStarted { peer_obj: 99 }, 1000);
        r.step(&mut t, 1100);
        let s = r.step(&mut t, 1200);
        assert_eq!(s.note, Some(Note::Refused(Refusal::WrongPartner)));
        assert_eq!(r.sent(), [op(|| cs::cancel_trade(0))], "no put-up first");
        // and a unit the bot cannot see is not the agreed partner either
        let mut r = Rig::new();
        r.identity = Identity::Unknown;
        let mut t = sale(PairRole::Responder);
        r.ev(ActivityEvent::TradeStarted { peer_obj: 99 }, 1000);
        r.step(&mut t, 1100);
        let s = r.step(&mut t, 1200);
        assert_eq!(s.note, Some(Note::Refused(Refusal::WrongPartner)));
    }

    #[test]
    fn a_sale_checks_that_the_goods_are_still_there() {
        let mut r = Rig::new();
        r.bag = bag(&[(4, item(900, 3411, 4))]);
        let mut t = sale(PairRole::Responder);
        r.ev(ActivityEvent::TradeStarted { peer_obj: 77 }, 1000);
        let s = r.step(&mut t, 1100);
        assert!(matches!(s.status, GoalStatus::Failed(_)));
        assert_eq!(r.sent(), [op(|| cs::cancel_trade(0))]);
    }

    fn purchase() -> Trade {
        let give = Plan {
            slot: 0,
            template: 0,
            amount: 0,
            money: 3000,
        };
        Trade::for_deal(
            order(PairRole::Initiator),
            Deal::agreed(give, &[(3411, 10)], 0),
        )
    }

    #[test]
    fn a_buyer_pays_only_for_the_agreed_goods() {
        let mut r = Rig::new();
        r.money = 10_000;
        let mut t = purchase();
        assert_eq!(t.phase, Phase::Invite);
        r.ev(ActivityEvent::TradeStarted { peer_obj: 77 }, 1000);
        r.step(&mut t, 1100);
        r.step(&mut t, 1200);
        assert_eq!(r.sent(), [op(|| cs::putup_trade_money(3000))]);
        r.ev(ActivityEvent::TradeMoneyPutup { amount: 3000 }, 1300);
        // the seller puts up fewer units than agreed: wait
        r.ev(other_item(3411, 4), 2000);
        r.step(&mut t, 3500);
        assert_eq!(locks(&r.sent()), 0);
        // and the rest: it is the deal
        r.ev(other_item(3411, 6), 4000);
        r.step(&mut t, 4200);
        r.step(&mut t, 4800);
        assert_eq!(r.sent(), [op(|| cs::trade_lock(true))]);
    }

    #[test]
    fn a_buyer_does_not_pay_for_the_wrong_goods() {
        let mut r = Rig::new();
        r.money = 10_000;
        let mut t = purchase();
        r.ev(ActivityEvent::TradeStarted { peer_obj: 77 }, 1000);
        r.step(&mut t, 1100);
        r.step(&mut t, 1200);
        r.sent();
        r.ev(ActivityEvent::TradeMoneyPutup { amount: 3000 }, 1300);
        r.ev(other_item(3712, 10), 2000);
        let mut sent = Vec::new();
        let mut outcome = None;
        for now in (2500..12_000).step_by(500) {
            let s = r.step(&mut t, now);
            sent.extend(r.sent());
            if !matches!(s.status, GoalStatus::Running) {
                outcome = Some(s);
                break;
            }
        }
        let s = outcome.expect("the buyer leaves");
        assert_eq!(s.note, Some(Note::Refused(Refusal::ExtraItem)));
        assert_eq!(locks(&sent), 0);
    }

    #[test]
    fn a_random_trade_with_a_player_must_bring_in_a_fair_share() {
        // not a bot of ours: the partner has to put up about as much as the bot gives
        let mut r = Rig::new();
        r.partner_is_bot = false;
        let mut t = at(Phase::Invite, PairRole::Initiator);
        t.order = None;
        r.ev(ActivityEvent::TradeStarted { peer_obj: 77 }, 1000);
        r.step(&mut t, 1100);
        r.step(&mut t, 1200);
        r.sent();
        let amount = t.plan.amount;
        r.ev(
            ActivityEvent::TradeItemPutup {
                slot_type: 2,
                slot: 4,
                amount,
            },
            1300,
        );
        // one copper for up to five ore worth 20 each
        r.ev(ActivityEvent::OtherTradeMoneyPutup { amount: 1 }, 1400);
        let mut sent = Vec::new();
        for now in (2500..7000).step_by(500) {
            r.step(&mut t, now);
            sent.extend(r.sent());
        }
        assert_eq!(locks(&sent), 0, "{sent:?}");
        // a fair amount is accepted
        r.ev(
            ActivityEvent::OtherTradeMoneyPutup {
                amount: 20 * amount,
            },
            7100,
        );
        r.step(&mut t, 7200);
        r.step(&mut t, 8000);
        assert_eq!(r.sent(), [op(|| cs::trade_lock(true))]);
    }

    #[test]
    fn the_goal_stays_small() {
        assert!(
            std::mem::size_of::<Trade>() <= 150,
            "{}",
            std::mem::size_of::<Trade>()
        );
    }
}
