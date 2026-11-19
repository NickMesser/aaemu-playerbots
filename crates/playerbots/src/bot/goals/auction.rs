//! Auction: list the goods the bot came by honestly at the auction house, now and then buy a
//! bargain, and collect the results by mail.
//!
//! # The visit
//!
//! ```text
//! Plan ─► Walk ─► Find ─► Survey ─► (Price ─► Post)×n ─► Cancel? ─► Browse ─► Buy ─► Verify ─► Linger
//!         to the   the NPC  own lots   search the market,    an overpriced   look at   buyout    own lots   a few
//!         nearest  in view  (client    ask a little under    lot, rarely     the lots  one lot   again      seconds
//!         auctioneer        id)        it, post 48 h
//! ```
//!
//! * **Plan**: the stacks to list ([`pick_stacks`]: whole bag stacks of items the provenance ledger
//!   calls the bot's own, tradable, above a reserve of what it may use itself and what its active
//!   quests still need, worth a trip) and
//!   whether it may buy (never while `bots.gm_bootstrap` is on: the gold would be GM-minted), then
//!   the nearest auctioneer it can walk to ([`aa_world::World::nearest_auctioneer`]).
//! * **Walk / Find**: to within 2.2 m of the NPC. Every request carries the NPC's object id twice;
//!   the server wants an auctioneer within 5 m and answers error 388 otherwise, then the bot
//!   interacts with the NPC and asks again.
//! * **Survey**: a search with the bot's own character id lists its lots. A lot the bot knew that is
//!   gone was sold, expired or cancelled; its result mail is expected (see the detectors).
//! * **Price / Post**: for each stack a search by the exact item name sorted by buyout gives the
//!   market ([`price::quote`]); the ask is [`price::ask`] (a little under the market, never below
//!   what beats the vendor after the 10% tax and the up-front fee), 48 hours. The server answers
//!   `SCAuctionPosted`, error 241 (cannot pay) or, for a refusal it does not report, nothing.
//! * **Browse / Buy**: lots priced far under the vendor price, or items the bot can use at no more
//!   than a shop asks, within a small budget, are bought out. Never a lot of an item that is not
//!   tradable (a quest item, a bound or an unsellable one): no vendor would take it back, so no
//!   price makes it a bargain.
//! * **Linger**: the bot stays a little, like a player reading the list.
//!
//! Results come by mail (types 13 to 16); collecting them is a background duty ([`duty`]) that runs
//! on every brain tick. A visit happens at most every [`VISIT_GAP_MS`].
//!
//! # Detectors
//!
//! [`report`] has the issue reports: a post that took the fee and the item but got no
//! `SCAuctionPosted`, a lot posted and not listed, a search nobody answers, a buyout whose money was
//! taken without an answer or whose item never came, a lot that vanished without a result mail,
//! mails that cannot be collected, and error codes nobody expects.
//!
//! The state machine talks to the bot through the small [`Env`] trait so it can be tested with
//! scripted server events; [`Live`] is the implementation around [`Ctx`].

mod machine;
pub mod report;
pub(crate) mod resale;
#[cfg(test)]
mod tests;

use aa_proto::cs::Out;
use aa_world::Vec3;
use pb_web::Activity;
use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use crate::bot::ctx::Ctx;
use crate::bot::market::auction::price::{self, Ask, Facts};
use crate::bot::market::auction::{AuctionMemory, Fault, Kind, Settled, WinWait};
use crate::bot::market::needs::QuestNeeds;
use crate::bot::motion::{MotionStatus, Pace};
use crate::bot::state::{BotState, Item};
use crate::config::BehaviorCfg;
use crate::issues::IssueReport;
use crate::roster::Temperament;

use super::{FailReason, GoalStatus};

pub(super) use machine::find_npc;
pub use machine::{Run, Spot, Stack};

/// A bot does not visit during its first minutes in the world.
pub const SETTLE_MS: u64 = 4 * 60_000;
/// The least time between two visits of one bot.
pub const VISIT_GAP_MS: u64 = 45 * 60_000;
/// After a visit that did not get anywhere, the next one is this long away.
pub const RETRY_AFTER_FAIL_MS: u64 = 8 * 60_000;
/// Auctioneers further away than this (straight line, metres) are not visited.
pub const MAX_TRIP_M: f32 = 3_500.0;
/// A bot below this many hit points (percent) does not go.
pub const MIN_HP_PCT: u8 = 50;
/// Stacks listed per visit at most.
pub const MAX_LISTINGS: usize = 4;
/// A bot with at least this much copper may look for something to buy.
pub const MIN_BUY_MONEY: i64 = 2_000;
/// The share of the purse a visit may spend, as a divisor (a quarter).
pub const BUY_SHARE: i64 = 4;
/// The most a visit spends (copper, 5 gold).
pub const BUY_CAP: i64 = 50_000;
/// The weight of a visit that only wants to buy.
const BUY_ONLY_WEIGHT: f32 = 0.06;

/// What the state machine needs from the bot. [`Live`] wraps a [`Ctx`]; the tests use a fake.
pub trait Env {
    /// The bot clock in milliseconds.
    fn now_ms(&self) -> u64;
    /// The bot's position.
    fn pos(&self) -> [f32; 3];
    /// What the bot knows (units in view, bag, purse, character id).
    fn state(&self) -> &BotState;
    /// The auction memory.
    fn mem(&mut self) -> &mut AuctionMemory;
    /// What the movement controller is doing.
    fn motion(&self) -> MotionStatus;
    /// Starts a trip to (x, y), stopping `radius` metres short.
    fn travel(&mut self, x: f32, y: f32, radius: f32);
    /// How long (metres) a leg of a walk is.
    fn leg_m(&self) -> f32 {
        280.0
    }
    /// Whether a far trip to (x, y) would go by portal.
    fn portal_trip(&mut self, _x: f32, _y: f32) -> bool {
        false
    }
    /// Whether a trip of `dist` metres would be a GM `/move`.
    fn would_teleport(&self, _dist: f32) -> bool {
        false
    }
    /// Stops walking.
    fn stop(&mut self);
    /// Turns towards (x, y).
    fn face(&mut self, x: f32, y: f32);
    /// Queues a packet.
    fn send(&mut self, out: Out);
    /// Randomness.
    fn rng(&mut self) -> &mut fastrand::Rng;
    /// A notable event for the web UI and the log.
    fn say(&self, text: &str);
    /// Files an issue report.
    fn report(&mut self, report: IssueReport);
    /// The price facts of an item.
    fn facts(&self, template: u32) -> Option<Facts>;
    /// Whether the bot may trade the item at all ([`aa_world::ItemFacts::tradable`]: sellable,
    /// not bound and not a quest item). The bot neither lists nor buys an item that is not, however
    /// cheap the lot is.
    fn tradable(&self, template: u32) -> bool;
    /// The English name of an item.
    fn item_name(&self, template: u32) -> Option<String>;
    /// Whether the bot can use the item itself.
    fn useful(&self, template: u32) -> bool;
    /// The nearest auctioneer the bot can walk to that is not out of favour.
    fn auctioneer(&mut self) -> Option<Spot>;
    /// The stacks to list, best first.
    fn sell_stacks(&mut self, max: usize) -> SmallVec<[Stack; MAX_LISTINGS]>;
    /// How much the bot may spend on a purchase; `None` when it may not buy at all.
    fn buy_budget(&self) -> Option<i64>;
    fn resale_candidate(&self, _lot: &aa_proto::auction::Lot, _budget: i64) -> bool {
        false
    }
    fn note_resale_buy(&mut self, _lot: &aa_proto::auction::Lot, _cost: i64) {}
    fn resale_ask(&self, _stack: Stack) -> Option<Option<Ask>> {
        None
    }
    fn listed_for_resale(&self, _item_id: u64) -> bool {
        false
    }
    fn note_resale_post(&mut self, _stack: Stack, _name: &str, _ask: Ask) {}
    /// The last refusal the server sent at or after bot clock `since_ms`: `(error, reason)`.
    fn error_since(&self, since_ms: u64) -> Option<(i16, u32)>;
}

/// [`Env`] around a tick's [`Ctx`].
pub struct Live<'a, 'c>(pub &'a mut Ctx<'c>);

impl Env for Live<'_, '_> {
    fn now_ms(&self) -> u64 {
        self.0.now_ms
    }

    fn pos(&self) -> [f32; 3] {
        self.0.state.me.pos
    }

    fn state(&self) -> &BotState {
        &*self.0.state
    }

    fn mem(&mut self) -> &mut AuctionMemory {
        &mut self.0.state.market.inner_mut().auction
    }

    fn motion(&self) -> MotionStatus {
        self.0.motion.status()
    }

    fn travel(&mut self, x: f32, y: f32, radius: f32) {
        self.0.travel(x, y, radius, Pace::Run);
    }

    fn leg_m(&self) -> f32 {
        self.0.motion.leg_m()
    }

    fn portal_trip(&mut self, x: f32, y: f32) -> bool {
        crate::bot::travel::has_route(self.0, x, y)
    }

    fn would_teleport(&self, dist: f32) -> bool {
        self.0.would_teleport(dist)
    }

    fn stop(&mut self) {
        self.0.motion.stop();
    }

    fn face(&mut self, x: f32, y: f32) {
        self.0.motion.face(x, y);
    }

    fn send(&mut self, out: Out) {
        self.0.send(out);
    }

    fn rng(&mut self) -> &mut fastrand::Rng {
        &mut *self.0.rng
    }

    fn say(&self, text: &str) {
        self.0.log.say(text);
    }

    fn report(&mut self, report: IssueReport) {
        self.0.report_issue(report);
    }

    fn facts(&self, template: u32) -> Option<Facts> {
        facts_of(self.0.world, template)
    }

    fn tradable(&self, template: u32) -> bool {
        self.0
            .world
            .item_facts(template)
            .is_some_and(aa_world::ItemFacts::tradable)
    }

    fn item_name(&self, template: u32) -> Option<String> {
        self.0.world.item_name(template).map(str::to_owned)
    }

    fn useful(&self, template: u32) -> bool {
        useful_item(self.0, template)
    }

    fn auctioneer(&mut self) -> Option<Spot> {
        let ctx = &*self.0;
        let (now, p) = (ctx.now_ms, ctx.state.me.pos);
        let skipped = |a: &aa_world::Auctioneer| {
            ctx.state
                .market
                .get()
                .is_some_and(|m| m.auction.skipped(machine::spot_key(a.pos.x, a.pos.y), now))
        };
        ctx.world
            .nearest_auctioneer(Vec3::new(p[0], p[1], p[2]), MAX_TRIP_M, skipped)
            .map(|a| Spot {
                template: a.template,
                pos: [a.pos.x, a.pos.y, a.pos.z],
            })
    }

    fn sell_stacks(&mut self, max: usize) -> SmallVec<[Stack; MAX_LISTINGS]> {
        sell_stacks_of(self.0, max)
    }

    fn buy_budget(&self) -> Option<i64> {
        buy_budget_of(self.0)
    }

    fn resale_candidate(&self, lot: &aa_proto::auction::Lot, budget: i64) -> bool {
        resale::candidate(self.0, lot, budget)
    }

    fn note_resale_buy(&mut self, lot: &aa_proto::auction::Lot, cost: i64) {
        resale::bought(self.0, lot, cost);
    }

    fn resale_ask(&self, stack: Stack) -> Option<Option<Ask>> {
        resale::listing_ask(self.0, stack)
    }

    fn listed_for_resale(&self, item_id: u64) -> bool {
        resale::is_listed(self.0, item_id)
    }

    fn note_resale_post(&mut self, stack: Stack, name: &str, ask: Ask) {
        resale::posted(self.0, stack, name, ask);
    }

    fn error_since(&self, since_ms: u64) -> Option<(i16, u32)> {
        self.0.state.econ.error_since(since_ms)
    }
}

/// The price facts of `template`: the vendor's prices.
fn facts_of(world: &aa_world::World, template: u32) -> Option<Facts> {
    world.item_facts(template).map(|f| Facts {
        price: f.price,
        refund: f.refund,
    })
}

/// Whether the bot can use the item itself: it casts a skill when used (a potion, food, a scroll)
/// and the bot's level is near the item's.
fn useful_item(ctx: &Ctx<'_>, template: u32) -> bool {
    ctx.world.item_use_skill(template) != 0
        && ctx
            .world
            .item_facts(template)
            .is_some_and(|f| f.level <= ctx.state.me.level.saturating_add(3))
}

fn buy_budget_of(ctx: &Ctx<'_>) -> Option<i64> {
    if ctx.cfg.bots.gm_bootstrap {
        return None;
    }
    let money = ctx.state.me.money;
    (money >= MIN_BUY_MONEY).then(|| (money / BUY_SHARE).min(BUY_CAP))
}

/// The stacks of the bag worth listing for `ctx`'s bot, see [`pick_stacks`].
fn sell_stacks_of(ctx: &Ctx<'_>, max: usize) -> SmallVec<[Stack; MAX_LISTINGS]> {
    let (now, mem) = (ctx.now_ms, ctx.state.market.get());
    let needs = QuestNeeds::of(ctx.world, &ctx.state.quests);
    let mut picked = pick_stacks(
        ctx.state
            .market
            .sellable_surplus(ctx.world, &ctx.state.bag, &needs),
        &ctx.state.bag,
        |t| facts_of(ctx.world, t),
        |t| mem.is_some_and(|m| m.auction.skipped(t, now) || m.auction.has_lot_of(t)),
        max,
    );
    if let Some(resale) = resale::stack(ctx) {
        picked.retain(|stack| stack.template != resale.template);
        let mut visited = affordable(picked, ctx.state.me.money, |t| facts_of(ctx.world, t));
        visited.insert(0, resale);
        visited.truncate(max);
        return visited;
    }
    affordable(picked, ctx.state.me.money, |t| facts_of(ctx.world, t))
}

/// The listing fee a stack is expected to cost: the fee of the price asked without a market
/// (halfway between what a vendor pays and asks), for the usual duration.
fn estimated_fee(stack: &Stack, facts: Facts) -> i64 {
    let each = (u64::from(facts.price) + u64::from(facts.refund)) / 2;
    let buyout = each.max(u64::from(facts.refund)) * u64::from(stack.count);
    let buyout = i32::try_from(buyout).unwrap_or(i32::MAX);
    i64::from(aa_proto::auction::listing_fee(
        buyout,
        aa_proto::auction::duration::H48,
    ))
}

/// The leading stacks of `stacks` (best first) whose expected listing fees together fit into
/// `money`: the fee is paid in cash when a lot is posted, so a bot that cannot pay it does not
/// walk to the auctioneer for nothing.
pub fn affordable(
    stacks: SmallVec<[Stack; MAX_LISTINGS]>,
    money: i64,
    facts: impl Fn(u32) -> Option<Facts>,
) -> SmallVec<[Stack; MAX_LISTINGS]> {
    let mut left = money.max(0);
    let mut kept = SmallVec::new();
    for s in stacks {
        let fee = facts(s.template).map_or(i64::MAX, |f| estimated_fee(&s, f));
        if fee <= left {
            left -= fee;
            kept.push(s);
        }
    }
    kept
}

/// The stacks to list: of every `(template, surplus)` the biggest whole bag stack that does not
/// exceed the surplus (the server lists a stack whole, so a stack the bot only partly came by
/// honestly is left alone), if a vendor pays at least [`price::MIN_STACK_VALUE`] for it, the
/// template is not `skipped` and the bot has no lot of it up already; the most valuable `max`.
pub fn pick_stacks(
    surplus: impl Iterator<Item = (u32, u32)>,
    bag: &FxHashMap<u16, Item>,
    facts: impl Fn(u32) -> Option<Facts>,
    skipped: impl Fn(u32) -> bool,
    max: usize,
) -> SmallVec<[Stack; MAX_LISTINGS]> {
    let mut found: SmallVec<[(u64, Stack); 8]> = SmallVec::new();
    for (template, left) in surplus {
        if skipped(template) {
            continue;
        }
        let Some(f) = facts(template) else { continue };
        if f.refund == 0 {
            continue;
        }
        let best = bag
            .values()
            .filter(|i| i.template == template && i.count > 0 && i.count as u32 <= left)
            .max_by_key(|i| (i.count, i.id));
        if let Some(i) = best {
            let value = u64::from(f.refund) * i.count as u64;
            if value >= price::MIN_STACK_VALUE {
                found.push((
                    value,
                    Stack {
                        item_id: i.id,
                        template,
                        count: i.count as u32,
                    },
                ));
            }
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.item_id.cmp(&b.1.item_id)));
    found.into_iter().map(|e| e.1).take(max).collect()
}

/// What a vendor would pay for `stacks` (copper).
pub fn stacks_value(stacks: &[Stack], facts: impl Fn(u32) -> Option<Facts>) -> u64 {
    stacks
        .iter()
        .map(|s| facts(s.template).map_or(0, |f| u64::from(f.refund) * u64::from(s.count)))
        .sum()
}

/// Listing goods and buying bargains at the auction house. See the module docs.
#[derive(Debug, Default)]
pub struct Auction {
    run: Option<Box<Run>>,
}

impl Auction {
    /// Called once when the brain picks the goal.
    pub fn start(&mut self, ctx: &mut Ctx<'_>) {
        // (the memory has to exist before any answer arrives)
        ctx.state.market.inner_mut();
        self.run = Some(Box::new(Run::new(ctx.now_ms)));
    }

    /// One tick.
    pub fn tick(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let Some(run) = self.run.as_mut() else {
            return GoalStatus::Failed(FailReason::Other("the visit was not started"));
        };
        let status = run.step(&mut Live(ctx));
        if run.walking() {
            // On the way: the combat controller only fights back against close attackers.
            ctx.combat.note_travelling(ctx.now_ms);
        }
        status
    }

    /// Called when the goal ends, fails or is interrupted: stops walking and sets the pace of the
    /// next visit.
    pub fn stop(&mut self, ctx: &mut Ctx<'_>) {
        if ctx.motion.is_moving() {
            ctx.motion.stop();
        }
        let Some(run) = self.run.take() else { return };
        let now = ctx.now_ms;
        let at_the_house = run.reached_the_house();
        let mem = &mut ctx.state.market.inner_mut().auction;
        if ctx.state.me.dead {
            run.died(mem, now);
        }
        // a visit that reached the auctioneer counts; one that did not (interrupted, blocked)
        // is tried again in a few minutes
        mem.last_visit_end_ms = if at_the_house {
            now.max(1)
        } else {
            (now + RETRY_AFTER_FAIL_MS)
                .saturating_sub(VISIT_GAP_MS)
                .max(1)
        };
    }

    /// Whether the goal drives the combat controller itself (see [`super::Goal::owns_combat`]).
    pub fn owns_combat(&self) -> bool {
        false
    }

    /// The activity the web UI shows while the goal runs.
    pub fn activity(&self, moving: bool, fighting: bool) -> Activity {
        if fighting {
            Activity::Fighting
        } else if moving {
            Activity::Traveling
        } else if self.run.as_ref().is_some_and(|r| r.reached_the_house()) {
            Activity::Trading
        } else {
            Activity::Idle
        }
    }

    /// Appends a one-line description of what the goal is doing to `out`.
    pub fn describe(&self, out: &mut String) {
        match &self.run {
            Some(r) => out.push_str(r.text()),
            None => out.push_str("auction house"),
        }
    }
}

/// The goal's weight before the situation is looked at: 0 (the goal scores itself, see [`score`]).
pub fn base_weight(_behavior: &BehaviorCfg, _temper: &Temperament) -> f32 {
    0.0
}

/// The weight of a visit for a bot that has goods worth `value` copper at a vendor to list.
pub fn weight_for(value: u64, temper: &Temperament) -> f32 {
    let size = (value as f32 / 3_000.0).min(1.0);
    (0.25 + 0.45 * size) * (0.6 + 0.8 * temper.merchant)
}

/// The score of the goal for this tick: 0 unless the market features are on, the bot is settled,
/// healthy and in the main world, its last visit is long enough ago, and there is something to do
/// (goods worth the trip, or gold to spend with `bots.gm_bootstrap` off) and an auctioneer it can
/// walk to. Called whenever the brain scores the goals, so the cheap checks come first.
pub fn score(ctx: &Ctx<'_>, _base: f32) -> f32 {
    let m = &ctx.cfg.market;
    if !(m.enabled && m.auction)
        || ctx.state.place.in_instance()
        || ctx.state.me.dead
        || !ctx.state.me.spawned
        || ctx.now_ms < SETTLE_MS
        || ctx.state.me.hp_pct() < MIN_HP_PCT
    {
        return 0.0;
    }
    if let Some(inner) = ctx.state.market.get() {
        let last = inner.auction.last_visit_end_ms;
        if last != 0 && ctx.now_ms < last + VISIT_GAP_MS {
            return 0.0;
        }
    }
    let stacks = sell_stacks_of(ctx, MAX_LISTINGS);
    let value = stacks_value(&stacks, |t| facts_of(ctx.world, t));
    let weight = if resale::stack(ctx).is_some() {
        0.85
    } else if value >= price::MIN_TOTAL_VALUE {
        weight_for(value, &ctx.persona.temper)
    } else if buy_budget_of(ctx).is_some() {
        BUY_ONLY_WEIGHT
    } else {
        return 0.0;
    };
    let p = ctx.state.me.pos;
    let near = ctx
        .world
        .nearest_auctioneer(Vec3::new(p[0], p[1], p[2]), MAX_TRIP_M, |_| false);
    if near.is_some() {
        weight
    } else {
        0.0
    }
}

/// What one step of the background duty found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DutyOutcome {
    /// A mail was collected.
    pub settled: Option<Settled>,
    /// A mail had to be given up on.
    pub fault: Option<Fault>,
    /// A buyout whose item has not come in time.
    pub late_win: Option<WinWait>,
    /// Lots that vanished and whose result mails are overdue.
    pub overdue: Option<u32>,
}

/// One step of the mail duty on the auction memory: lists the mailbox when asked or when results
/// are due, advances the first mail that needs a request, and returns what the detectors found.
pub fn duty_step(mem: &mut AuctionMemory, now: u64, out: &mut Vec<Out>) -> DutyOutcome {
    let awaiting = mem.awaiting_mail();
    mem.mail.tick(now, awaiting, out);
    DutyOutcome {
        settled: mem.mail.take_settled(),
        fault: mem.mail.take_fault(),
        late_win: mem.take_late_win(now),
        overdue: mem.sale_mail_overdue(now),
    }
}

/// The background duty, run on every brain tick whatever the bot is doing: collects the auction
/// mail (read, take the attachments), lists the mailbox when results are due, and files the
/// reports of the mail detectors.
pub fn duty(ctx: &mut Ctx<'_>) {
    if !(ctx.cfg.market.enabled && ctx.cfg.market.auction) {
        return;
    }
    let now = ctx.now_ms;
    let Some(inner) = ctx.state.market.get_mut() else {
        return;
    };
    let found = duty_step(&mut inner.auction, now, ctx.out);
    let counts = inner.auction.mail.counts;
    if let Some(s) = found.settled {
        if s.kind == Kind::Sold
            && !s
                .sale
                .as_deref()
                .is_some_and(|sale| resale::settle(ctx, sale, i64::from(s.money)))
        {
            use crate::bot::{cmd::GoalKind, guild::treasury};
            treasury::note_receipt(ctx, GoalKind::Auction, s.money.max(0) as u64);
        }
        let coins = crate::bot::goals::trade_run::coins;
        let text = match s.kind {
            Kind::Sold => format!(
                "auction: a lot sold, {} collected (mail {}; {} sold, {} earned in all)",
                coins(i64::from(s.money)),
                s.mail_id,
                counts.sold,
                coins(counts.earned)
            ),
            Kind::Won => format!(
                "auction: the item of a won lot collected (mail {})",
                s.mail_id
            ),
            Kind::Cancelled => "auction: a cancelled lot's item came back".to_owned(),
            Kind::Expired => "auction: an unsold lot's item came back".to_owned(),
            Kind::Refund => format!("auction: outbid, {} refunded", coins(i64::from(s.money))),
        };
        ctx.log.say(text);
    }
    if let Some(f) = found.fault {
        ctx.report_issue(report::mail_fault(f));
    }
    if let Some(w) = found.late_win {
        let name = ctx
            .world
            .item_name(w.template)
            .map_or_else(|| format!("#{}", w.template), str::to_owned);
        ctx.report_issue(report::win_mail_missing(w, &name));
    }
    if let Some(n) = found.overdue {
        ctx.report_issue(report::sale_mail_missing(n));
    }
}

/// Counters of the bot for the log: `visits posted fees sold earned bought spent`.
pub fn summary(mem: &AuctionMemory) -> String {
    let (s, c) = (mem.stats, mem.mail.counts);
    format!(
        "{} visits, {} posted ({} fees), {} sold ({} earned), {} bought ({} spent), {} timeouts",
        s.visits,
        s.posted,
        crate::bot::goals::trade_run::coins(s.fees),
        c.sold,
        crate::bot::goals::trade_run::coins(c.earned),
        s.bought,
        crate::bot::goals::trade_run::coins(s.spent),
        s.timeouts
    )
}
