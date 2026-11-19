//! The state machine of one visit to the auction house. See the parent module for the picture.

use aa_proto::auction::{self as wire, duration, sort_kind, sort_order, Lot, Search, PAGE_SIZE};
use aa_proto::cs;
use smallvec::SmallVec;

use super::report;
use super::{Env, MAX_LISTINGS};
use crate::bot::goals::{FailReason, GoalStatus};
use crate::bot::market::auction::price::{self, Ask, Facts, Skip, Why};
use crate::bot::market::auction::{AuctionMemory, OwnLot, Page};
use crate::bot::motion::MotionStatus;
use crate::bot::state::{dist2d, BotState, UnitKind};

/// The whole visit must end within this long.
const VISIT_TIMEOUT_MS: u64 = 25 * 60_000;
/// The bot stands this far from the auctioneer (the server accepts 5 m).
const STOP_M: f32 = 2.2;
/// Closer, after the server said the bot was too far.
const CLOSE_M: f32 = 1.0;
/// Requests are made from within this distance of the NPC.
const REACH_M: f32 = 4.0;
/// A walk counts as arrived this much beyond its stop radius.
const ARRIVE_SLACK_M: f32 = 0.7;
/// A spawn and the NPC seen for it may differ by this much.
const SPAWN_MATCH_M: f32 = 20.0;
/// Failed trips in a row after which the walk is given up.
const MAX_TRIP_FAILS: u8 = 3;
/// How long to wait for the NPC to show up once the bot stands at its spawn.
const FIND_WAIT_MS: u64 = 8_000;
/// How long a request is waited for.
const ANSWER_WAIT_MS: u64 = 6_000;
/// Searches of one request are tried this often.
const MAX_SEARCH_TRIES: u8 = 2;
/// Pages of the own lots looked at.
const SURVEY_PAGES: i32 = 4;
/// Pages of the market looked at per item.
const PRICE_PAGES: i32 = 2;
/// Pages of the general list looked at when browsing.
const BROWSE_PAGES: u8 = 2;
/// Lots bought per visit at most.
const MAX_BUYS: u8 = 2;
/// Silent post refusals after which the bot stops listing for the visit.
const MAX_SILENT_POSTS: u8 = 2;
/// An item whose market is below the vendor price is left alone this long.
const UNPROFITABLE_MS: u64 = 60 * 60_000;
/// An item the server refused silently is left alone this long.
const REFUSED_MS: u64 = 2 * 60 * 60_000;
/// An auctioneer that cannot be reached or does not show is left alone this long.
const BAD_SPOT_MS: u64 = 30 * 60_000;
/// A lot of the bot must be this old before it is looked at for a cancel.
const CANCEL_AFTER_MS: u64 = 90 * 60_000;
/// The chance that an old lot is looked at for a cancel.
const CANCEL_CHANCE: f32 = 0.35;
/// A lot is cancelled when the market asks less than this percentage of its price.
const CANCEL_BELOW_PCT: u64 = 80;
/// The bot stays this long after its business, in milliseconds.
const LINGER_MS: (u64, u64) = (15_000, 55_000);
/// Own lots kept from a survey (for cancels).
const MAX_OWN_LOTS: usize = 24;
/// The error ids that are explained by the rules.
const ERR_CANNOT_PUT_UP: i16 = wire::ERR_CANNOT_PUT_UP;
const ERR_TOO_FAR: i16 = wire::ERR_AUCTIONEER_TOO_FAR;

/// A key for an auctioneer spawn in the memory's skip list: its position in 8 m cells, with the top
/// bit set so it never meets an item template.
pub fn spot_key(x: f32, y: f32) -> u32 {
    0x8000_0000 | (((x / 8.0) as i32 as u32 & 0x7fff) << 16) | ((y / 8.0) as i32 as u32 & 0xffff)
}

/// An auctioneer spawn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spot {
    /// NPC template.
    pub template: u32,
    /// Spawn position.
    pub pos: [f32; 3],
}

/// A bag stack to list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stack {
    /// Item instance id.
    pub item_id: u64,
    /// Item template.
    pub template: u32,
    /// Stack size.
    pub count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Phase {
    /// Choose the stacks and the auctioneer.
    #[default]
    Plan,
    /// Walk to the auctioneer.
    Walk,
    /// Find the NPC in view.
    Find,
    /// Search the bot's own lots.
    Survey,
    /// Take the next stack.
    Next,
    /// Search the market for the current item.
    Pricing,
    /// Wait for the answer to a post.
    Posting,
    /// Wait for the answer to a cancel.
    Cancelling,
    /// Look at the lots on offer.
    Browse,
    /// Wait for the answer to a buyout.
    Buying,
    /// Stay a little.
    Linger,
}

/// What a walking tick came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Walk {
    Going,
    There,
    Stuck,
}

/// What waiting for a search page came to.
enum Waited {
    Pending,
    Got(Box<Page>),
    Refused(i16),
    Silent,
}

/// The stack being priced or posted (or the old lot being looked at).
#[derive(Debug)]
struct Cur {
    stack: Stack,
    name: String,
    facts: Facts,
    ask: Option<Ask>,
    /// The market quote the ask is based on, for the log.
    quote: Option<price::Quote>,
    /// Set when the item is one of the bot's own lots and the search is to see whether it is
    /// overpriced.
    reprice: Option<Box<Lot>>,
}

/// One visit. Boxed in [`Auction`](super::Auction): the goal is one pointer when idle.
#[derive(Debug)]
pub struct Run {
    phase: Phase,
    end_ms: u64,
    /// When the request of the phase went out (0 = none yet).
    sent_ms: u64,
    /// The phase was just entered (its first request is not a retry).
    fresh: bool,
    tries: u8,
    fails: u8,
    spot: Spot,
    /// Object id of the auctioneer (0 = not seen yet) and where it stands.
    obj: u32,
    npc_pos: [f32; 3],
    /// Stand closer than usual (the last request was out of range).
    close: bool,
    /// The bot interacted with the NPC after an out-of-range answer.
    approached: bool,
    /// The phase to go back to after walking up to the NPC again.
    resume: Phase,
    /// The bot stood at the auctioneer.
    arrived: bool,
    sell: SmallVec<[Stack; MAX_LISTINGS]>,
    cur: Option<Cur>,
    /// The search page being asked for.
    page: i32,
    /// Pages left to browse.
    pages_left: u8,
    /// Per-item buyouts collected for the current item.
    units: Vec<u32>,
    /// Lots of the current item seen while pricing it (for bargains).
    seen: Vec<Lot>,
    /// The bot's own lots found by the survey.
    own_lots: Vec<Lot>,
    /// Lots posted in this visit.
    posted_ids: SmallVec<[(u64, u32); 4]>,
    /// The survey is the check after the postings.
    verify: bool,
    bargain: Option<(Why, Lot)>,
    best: Option<(Why, Lot)>,
    /// The lot being bought, or cancelled.
    lot: Option<Lot>,
    buying_for_resale: bool,
    relist_after_buy: bool,
    budget: i64,
    bought: u8,
    posted: u8,
    silent_posts: u8,
    cancel_done: bool,
    money_before: i64,
    linger_until: u64,
    text: Box<str>,
}

impl Run {
    /// A fresh visit starting at bot clock `now_ms`.
    pub fn new(now_ms: u64) -> Run {
        Run {
            phase: Phase::Plan,
            end_ms: now_ms + VISIT_TIMEOUT_MS,
            sent_ms: 0,
            fresh: true,
            tries: 0,
            fails: 0,
            spot: Spot {
                template: 0,
                pos: [0.0; 3],
            },
            obj: 0,
            npc_pos: [0.0; 3],
            close: false,
            approached: false,
            resume: Phase::Survey,
            arrived: false,
            sell: SmallVec::new(),
            cur: None,
            page: 0,
            pages_left: 0,
            units: Vec::new(),
            seen: Vec::new(),
            own_lots: Vec::new(),
            posted_ids: SmallVec::new(),
            verify: false,
            bargain: None,
            best: None,
            lot: None,
            buying_for_resale: false,
            relist_after_buy: false,
            budget: 0,
            bought: 0,
            posted: 0,
            silent_posts: 0,
            cancel_done: false,
            money_before: 0,
            linger_until: 0,
            text: "Auction: planning the visit".into(),
        }
    }

    /// Whether the bot is on its way to the auctioneer.
    pub fn walking(&self) -> bool {
        self.phase == Phase::Walk
    }

    /// Whether the bot stood at the auctioneer during this visit.
    pub fn reached_the_house(&self) -> bool {
        self.arrived
    }

    /// The one-line description for the web UI.
    pub fn text(&self) -> &str {
        &self.text
    }

    fn fail(&mut self, env: &mut dyn Env, why: FailReason, text: &'static str) -> GoalStatus {
        env.say(&format!("auction: the visit failed, {text}"));
        let why = match why {
            FailReason::Other(_) => FailReason::Other(text),
            other => other,
        };
        GoalStatus::Failed(why)
    }

    fn enter(&mut self, env: &dyn Env, phase: Phase, text: impl Into<Box<str>>) {
        let _ = env;
        self.phase = phase;
        self.sent_ms = 0;
        self.tries = 0;
        self.fresh = true;
        self.text = text.into();
    }

    /// One step of the visit against `env`.
    pub fn step(&mut self, env: &mut dyn Env) -> GoalStatus {
        if env.now_ms() >= self.end_ms {
            return self.fail(env, FailReason::Timeout, "it took too long");
        }
        match self.phase {
            Phase::Plan => self.do_plan(env),
            Phase::Walk => self.do_walk(env),
            Phase::Find => self.do_find(env),
            Phase::Survey => self.do_survey(env),
            Phase::Next => self.do_next(env),
            Phase::Pricing => self.do_pricing(env),
            Phase::Posting => self.do_posting(env),
            Phase::Cancelling => self.do_cancelling(env),
            Phase::Browse => self.do_browse(env),
            Phase::Buying => self.do_buying(env),
            Phase::Linger => self.do_linger(env),
        }
    }

    // ---- plan, walk, find -----------------------------------------------------------------

    fn do_plan(&mut self, env: &mut dyn Env) -> GoalStatus {
        let sell = env.sell_stacks(MAX_LISTINGS);
        let budget = env.buy_budget();
        if sell.is_empty() && budget.is_none() {
            return self.fail(
                env,
                FailReason::NoTarget,
                "nothing to sell and nothing to buy with",
            );
        }
        let Some(spot) = env.auctioneer() else {
            return self.fail(env, FailReason::NoTarget, "no auctioneer within reach");
        };
        self.spot = spot;
        self.sell = sell;
        self.budget = budget.unwrap_or(0);
        let dist = dist2d(env.pos(), spot.pos);
        env.say(&format!(
            "auction: going to the auctioneer ({}), {} stack(s) to list{}",
            length(dist),
            self.sell.len(),
            if self.budget > 0 {
                ", with gold to spend"
            } else {
                ""
            }
        ));
        self.resume = Phase::Survey;
        self.enter(
            env,
            Phase::Walk,
            format!("Auction: walking to the auctioneer ({})", length(dist)),
        );
        GoalStatus::Running
    }

    fn dest(&self) -> [f32; 2] {
        if self.obj != 0 {
            [self.npc_pos[0], self.npc_pos[1]]
        } else {
            [self.spot.pos[0], self.spot.pos[1]]
        }
    }

    fn do_walk(&mut self, env: &mut dyn Env) -> GoalStatus {
        let radius = if self.close { CLOSE_M } else { STOP_M };
        match self.walk(env, self.dest(), radius) {
            Walk::Going => GoalStatus::Running,
            Walk::Stuck => {
                self.skip_spot(env);
                self.fail(env, FailReason::Stuck, "could not get to the auctioneer")
            }
            Walk::There => {
                self.enter(env, Phase::Find, "Auction: looking for the auctioneer");
                GoalStatus::Running
            }
        }
    }

    fn do_find(&mut self, env: &mut dyn Env) -> GoalStatus {
        let found = find_npc(env.state(), self.spot, env.pos());
        let Some((obj, pos)) = found else {
            let now = env.now_ms();
            if self.sent_ms == 0 {
                self.sent_ms = now;
            } else if now.saturating_sub(self.sent_ms) > FIND_WAIT_MS {
                let name = format!("#{}", self.spot.template);
                env.report(report::auctioneer_missing(&name, self.spot.template));
                self.skip_spot(env);
                return self.fail(env, FailReason::NoTarget, "the auctioneer is not there");
            }
            return GoalStatus::Running;
        };
        self.obj = obj;
        self.npc_pos = pos;
        let radius = if self.close { CLOSE_M } else { STOP_M };
        if dist2d(env.pos(), pos) > REACH_M.max(radius + ARRIVE_SLACK_M) {
            self.enter(env, Phase::Walk, "Auction: walking up to the auctioneer");
            return GoalStatus::Running;
        }
        env.stop();
        env.face(pos[0], pos[1]);
        if !self.arrived {
            env.mem().stats.visits += 1;
        }
        self.arrived = true;
        let to = self.resume;
        let text = headline(to);
        self.enter(env, to, text);
        GoalStatus::Running
    }

    fn skip_spot(&self, env: &mut dyn Env) {
        let now = env.now_ms();
        let key = spot_key(self.spot.pos[0], self.spot.pos[1]);
        env.mem().skip(key, now + BAD_SPOT_MS, now);
    }

    /// The goal was dropped because the bot died: when that happened on the way to the
    /// auctioneer, the way is not safe and the auctioneer is left alone for a while (a bot that
    /// walks into the same death every few minutes helps nobody).
    pub fn died(&self, mem: &mut AuctionMemory, now: u64) {
        if self.phase == Phase::Walk {
            let key = spot_key(self.spot.pos[0], self.spot.pos[1]);
            mem.skip(key, now + BAD_SPOT_MS, now);
        }
    }

    /// One walking tick towards `dest`, stopping `radius` metres short.
    fn walk(&mut self, env: &mut dyn Env, dest: [f32; 2], radius: f32) -> Walk {
        let me = env.pos();
        let d = dist2d(me, [dest[0], dest[1], 0.0]);
        if d <= radius + ARRIVE_SLACK_M {
            env.stop();
            self.fails = 0;
            return Walk::There;
        }
        match env.motion() {
            MotionStatus::Moving => return Walk::Going,
            MotionStatus::Failed(_) => {
                self.fails += 1;
                if self.fails >= MAX_TRIP_FAILS {
                    return Walk::Stuck;
                }
            }
            MotionStatus::Idle | MotionStatus::Arrived => {}
        }
        // A new leg: the whole way when it is short, a portal or a `/move`, else a stretch of it.
        let leg = env.leg_m();
        if d <= leg + 20.0 || env.would_teleport(d) || env.portal_trip(dest[0], dest[1]) {
            env.travel(dest[0], dest[1], radius);
        } else {
            let k = leg / d;
            env.travel(
                me[0] + (dest[0] - me[0]) * k,
                me[1] + (dest[1] - me[1]) * k,
                1.0,
            );
        }
        Walk::Going
    }

    // ---- requests -------------------------------------------------------------------------

    /// Sends `out` as the request of this phase.
    fn request(&mut self, env: &mut dyn Env, out: cs::Out) {
        env.mem().clear_inbox();
        env.send(out);
        self.sent_ms = env.now_ms();
        self.tries += 1;
    }

    fn me(env: &dyn Env) -> (u32, String) {
        let me = &env.state().me;
        (me.char_id, me.name.to_string())
    }

    fn search(&mut self, env: &mut dyn Env, q: &Search<'_>) {
        let out = wire::search(self.obj, self.obj, q);
        self.request(env, out);
    }

    /// Waits for the search page requested at `sent_ms`.
    fn wait_page(&mut self, env: &mut dyn Env) -> Waited {
        if let Some(a) = env.mem().inbox.page.take() {
            if a.at_ms >= self.sent_ms {
                return Waited::Got(a.what);
            }
        }
        if let Some((err, _)) = env.error_since(self.sent_ms) {
            return Waited::Refused(err);
        }
        if env.now_ms().saturating_sub(self.sent_ms) > ANSWER_WAIT_MS {
            return Waited::Silent;
        }
        Waited::Pending
    }

    /// The auctioneer said "too far": interact with it, go closer, and come back to `resume`.
    /// `false` when that was tried already (the refusal is then reported).
    fn out_of_range(&mut self, env: &mut dyn Env, resume: Phase) -> bool {
        if self.approached {
            env.report(report::refused_in_range(self.spot.template));
            self.skip_spot(env);
            return false;
        }
        self.approached = true;
        self.close = true;
        env.mem().stats.refused += 1;
        env.send(cs::interact_npc(self.obj, true));
        self.resume = resume;
        self.enter(env, Phase::Walk, "Auction: stepping up to the auctioneer");
        true
    }

    /// An error nobody expects: filed once, the visit goes on without the request.
    fn unexpected(&mut self, env: &mut dyn Env, code: i16, request: &str) {
        env.mem().stats.refused += 1;
        env.report(report::unexpected_error(code, request));
        env.say(&format!(
            "auction: the {request} was refused with error {code}"
        ));
    }

    // ---- survey ---------------------------------------------------------------------------

    fn do_survey(&mut self, env: &mut dyn Env) -> GoalStatus {
        let (me, _) = Self::me(env);
        if me == 0 {
            return self.fail(env, FailReason::Other(""), "the character id is not known");
        }
        if self.sent_ms == 0 {
            if self.fresh {
                self.fresh = false;
                self.page = 0;
                self.own_lots.clear();
                if !self.verify {
                    env.mem().begin_survey();
                    env.mem().mail.want_list();
                }
            }
            let q = Search {
                client_id: me,
                page: self.page,
                ..Search::default()
            };
            self.search(env, &q);
            return GoalStatus::Running;
        }
        match self.wait_page(env) {
            Waited::Pending => GoalStatus::Running,
            Waited::Got(page) => self.on_survey_page(env, me, &page),
            Waited::Refused(ERR_TOO_FAR) => {
                if self.out_of_range(env, Phase::Survey) {
                    GoalStatus::Running
                } else {
                    self.fail(env, FailReason::Stuck, "the auctioneer is out of reach")
                }
            }
            Waited::Refused(code) => {
                self.unexpected(env, code, "search");
                self.fail(
                    env,
                    FailReason::Other(""),
                    "the auction house refused the search",
                )
            }
            Waited::Silent => {
                env.mem().stats.timeouts += 1;
                if self.tries < MAX_SEARCH_TRIES {
                    self.sent_ms = 0;
                    return GoalStatus::Running;
                }
                env.report(report::search_unanswered("own lots"));
                self.skip_spot(env);
                self.fail(
                    env,
                    FailReason::Timeout,
                    "the auction house does not answer",
                )
            }
        }
    }

    fn on_survey_page(&mut self, env: &mut dyn Env, me: u32, page: &Page) -> GoalStatus {
        let now = env.now_ms();
        for lot in page.lots.iter().filter(|l| l.client_id == me) {
            env.mem().survey_saw(lot, now);
            if self.own_lots.len() < MAX_OWN_LOTS {
                self.own_lots.push(lot.clone());
            }
        }
        if page.lots.len() >= PAGE_SIZE && self.page + 1 < SURVEY_PAGES {
            self.page += 1;
            self.sent_ms = 0;
            self.tries = 0;
            return GoalStatus::Running;
        }
        if self.verify {
            // the lots posted in this visit must be listed
            let posted = std::mem::take(&mut self.posted_ids);
            for (id, template) in posted {
                if !self.own_lots.iter().any(|l| l.id == id) {
                    let name = env
                        .item_name(template)
                        .unwrap_or_else(|| format!("#{template}"));
                    env.report(report::lot_not_listed(&name, template));
                    env.mem().forget_own(id);
                }
            }
            self.enter(env, Phase::Linger, "Auction: looking around");
            return GoalStatus::Running;
        }
        let gone = env.mem().end_survey();
        for g in &gone {
            let name = env
                .item_name(g.template)
                .unwrap_or_else(|| format!("#{}", g.template));
            env.say(&format!(
                "auction: the lot of {name} x{} is gone from the house (its mail is due)",
                g.count
            ));
        }
        self.enter(env, Phase::Next, "Auction: choosing what to list");
        GoalStatus::Running
    }

    // ---- listing --------------------------------------------------------------------------

    fn do_next(&mut self, env: &mut dyn Env) -> GoalStatus {
        let now = env.now_ms();
        while !self.sell.is_empty() {
            let stack = self.sell.remove(0);
            let template = stack.template;
            if env.mem().skipped(template, now) || env.mem().has_lot_of(template) {
                continue;
            }
            // (the stacks come from the sellable surplus, which holds tradable items only)
            if !env.tradable(template) {
                continue;
            }
            let (Some(facts), Some(name)) = (env.facts(template), env.item_name(template)) else {
                continue;
            };
            let cur = Cur {
                stack,
                name,
                facts,
                ask: None,
                quote: None,
                reprice: None,
            };
            if let Some(note) = env.mem().price(template, now).copied() {
                // looked up recently: no need to search again
                self.cur = Some(cur);
                return self.decide(env, note.quote);
            }
            self.units.clear();
            self.seen.clear();
            let text = format!("Auction: looking up the price of {}", cur.name);
            self.cur = Some(cur);
            self.page = 0;
            self.enter(env, Phase::Pricing, text);
            return GoalStatus::Running;
        }
        self.after_listing(env)
    }

    /// The listing is over: an old lot may be looked at for a cancel, then the bot browses.
    fn after_listing(&mut self, env: &mut dyn Env) -> GoalStatus {
        if !self.cancel_done {
            self.cancel_done = true;
            if let Some(lot) = self.pick_old_lot(env) {
                let template = lot.template;
                let (Some(facts), Some(name)) = (env.facts(template), env.item_name(template))
                else {
                    return self.start_browse(env);
                };
                self.units.clear();
                self.seen.clear();
                self.page = 0;
                self.cur = Some(Cur {
                    stack: Stack {
                        item_id: lot.item_id,
                        template,
                        count: lot.count.max(0) as u32,
                    },
                    name: name.clone(),
                    facts,
                    ask: None,
                    quote: None,
                    reprice: Some(Box::new(lot)),
                });
                self.enter(
                    env,
                    Phase::Pricing,
                    format!("Auction: checking the price of my {name}"),
                );
                return GoalStatus::Running;
            }
        }
        self.start_browse(env)
    }

    /// An own lot old enough, without a bid, that the bot decides to look at.
    fn pick_old_lot(&mut self, env: &mut dyn Env) -> Option<Lot> {
        let now = env.now_ms();
        let known: SmallVec<[OwnLot; 8]> = env.mem().own().iter().copied().collect();
        let lot = self.own_lots.iter().find(|l| {
            !l.has_bid()
                && !env.listed_for_resale(l.item_id)
                && known
                    .iter()
                    .any(|k| k.id == l.id && now.saturating_sub(k.posted_ms) >= CANCEL_AFTER_MS)
        })?;
        let chance = env.rng().f32();
        (chance < CANCEL_CHANCE).then(|| lot.clone())
    }

    fn do_pricing(&mut self, env: &mut dyn Env) -> GoalStatus {
        let Some(cur) = self.cur.as_ref() else {
            return self.fail(env, FailReason::Other(""), "no item to price");
        };
        let (template, name) = (cur.stack.template, cur.name.clone());
        let (me, _) = Self::me(env);
        if self.sent_ms == 0 {
            let q = Search {
                keyword: &name,
                exact: true,
                page: self.page,
                sort: [sort_kind::DIRECT_PRICE, sort_order::ASC],
                ..Search::default()
            };
            self.search(env, &q);
            return GoalStatus::Running;
        }
        match self.wait_page(env) {
            Waited::Pending => GoalStatus::Running,
            Waited::Got(page) => {
                self.units
                    .extend(price::market_units(&page.lots, template, 0, me));
                for l in page.lots.iter().filter(|l| l.template == template) {
                    if self.seen.len() < 2 * PAGE_SIZE {
                        self.seen.push(l.clone());
                    }
                }
                if page.lots.len() >= PAGE_SIZE && self.page + 1 < PRICE_PAGES {
                    self.page += 1;
                    self.sent_ms = 0;
                    self.tries = 0;
                    return GoalStatus::Running;
                }
                self.finish_pricing(env, me)
            }
            Waited::Refused(ERR_TOO_FAR) => {
                // the stack goes back, the price is asked for again after the step up
                if let Some(c) = self.cur.take() {
                    if c.reprice.is_none() {
                        self.sell.insert(0, c.stack);
                    }
                }
                if self.out_of_range(env, Phase::Next) {
                    GoalStatus::Running
                } else {
                    self.fail(env, FailReason::Stuck, "the auctioneer is out of reach")
                }
            }
            Waited::Refused(code) => {
                self.unexpected(env, code, "search");
                self.cur = None;
                self.enter(env, Phase::Next, "Auction: choosing what to list");
                GoalStatus::Running
            }
            Waited::Silent => {
                env.mem().stats.timeouts += 1;
                if self.tries < MAX_SEARCH_TRIES {
                    self.sent_ms = 0;
                    return GoalStatus::Running;
                }
                env.report(report::search_unanswered("market"));
                self.skip_spot(env);
                self.fail(
                    env,
                    FailReason::Timeout,
                    "the auction house does not answer",
                )
            }
        }
    }

    fn finish_pricing(&mut self, env: &mut dyn Env, me: u32) -> GoalStatus {
        let now = env.now_ms();
        let Some(mut cur) = self.cur.take() else {
            return self.fail(env, FailReason::Other(""), "no item to price");
        };
        let quote = price::quote(&mut self.units);
        env.mem().note_price(cur.stack.template, quote, now);
        // a lot far under what a vendor pays is a find
        if self.bargain.is_none()
            && self.budget > 0
            && self.bought < MAX_BUYS
            && env.tradable(cur.stack.template)
        {
            {
                let budget = self.budget;
                let money = env.state().me.money;
                let useful = env.useful(cur.stack.template);
                self.bargain = self
                    .seen
                    .iter()
                    .filter_map(|l| {
                        buy_reason(env, l, cur.facts, useful, me, money, budget).map(|w| (w, l))
                    })
                    .min_by_key(|(w, l)| (*w as u8, l.buyout))
                    .map(|(w, l)| (w, l.clone()));
            }
        }
        cur.quote = quote;
        if let Some(lot) = cur.reprice.take() {
            return self.finish_reprice(env, &cur, *lot, quote);
        }
        self.cur = Some(cur);
        self.decide(env, quote)
    }

    /// The bot's own lot was compared with the market: cancel it when it asks far more.
    fn finish_reprice(
        &mut self,
        env: &mut dyn Env,
        cur: &Cur,
        lot: Lot,
        quote: Option<price::Quote>,
    ) -> GoalStatus {
        let own_unit = lot.buyout_unit().map_or(0, u64::from);
        let overpriced = quote
            .is_some_and(|q| own_unit > 0 && u64::from(q.unit) * 100 < own_unit * CANCEL_BELOW_PCT);
        if !overpriced {
            env.say(&format!(
                "auction: my {} lot is priced fine ({} each), leaving it",
                cur.name, own_unit
            ));
            return self.start_browse(env);
        }
        env.say(&format!(
            "auction: my {} lot asks {} each, the market {}: cancelling it",
            cur.name,
            own_unit,
            quote.map_or(0, |q| q.unit)
        ));
        let out = wire::cancel(self.obj, self.obj, &lot);
        self.request(env, out);
        let text = format!("Auction: cancelling my {} lot", cur.name);
        self.lot = Some(lot);
        self.phase = Phase::Cancelling;
        self.text = text.into();
        GoalStatus::Running
    }

    fn do_cancelling(&mut self, env: &mut dyn Env) -> GoalStatus {
        let Some(lot) = self.lot.clone() else {
            return self.start_browse(env);
        };
        if let Some(a) = env.mem().inbox.canceled.take() {
            if a.at_ms >= self.sent_ms {
                env.mem().own_cancelled(a.what);
                env.say("auction: lot cancelled, the item comes back by mail");
                self.lot = None;
                return self.start_browse(env);
            }
        }
        if let Some((err, _)) = env.error_since(self.sent_ms) {
            if err != ERR_TOO_FAR {
                self.unexpected(env, err, "cancel");
            }
            self.lot = None;
            return self.start_browse(env);
        }
        if env.now_ms().saturating_sub(self.sent_ms) > ANSWER_WAIT_MS {
            // (the server ignores a cancel of a lot with a bid, or somebody else's)
            env.mem().stats.timeouts += 1;
            env.say(&format!(
                "auction: the cancel of lot {} got no answer",
                lot.id
            ));
            self.lot = None;
            return self.start_browse(env);
        }
        GoalStatus::Running
    }

    /// The ask for the current stack, given the market `quote`: posts it, or leaves the item alone.
    fn decide(&mut self, env: &mut dyn Env, quote: Option<price::Quote>) -> GoalStatus {
        let now = env.now_ms();
        let Some(cur) = self.cur.as_mut() else {
            return self.fail(env, FailReason::Other(""), "no item to list");
        };
        let pct = env.rng().u32(95..=99);
        let (template, count) = (cur.stack.template, cur.stack.count);
        cur.quote = quote;
        let ask = match env.resale_ask(cur.stack) {
            Some(ask) => ask.ok_or(Skip::BelowVendor),
            None => price::ask(cur.facts, count, quote, pct, duration::H48),
        };
        let name = cur.name.clone();
        let ask = match ask {
            Ok(a) => a,
            Err(skip) => {
                env.mem().skip(template, now + UNPROFITABLE_MS, now);
                env.say(&format!(
                    "auction: not listing {name} x{count}: {}",
                    match skip {
                        Skip::BelowVendor => "a vendor pays more than the market after fee and tax",
                        Skip::NoValue => "it has no sane price",
                    }
                ));
                self.cur = None;
                self.enter(env, Phase::Next, "Auction: choosing what to list");
                return GoalStatus::Running;
            }
        };
        let money = env.state().me.money;
        if money < i64::from(ask.fee) {
            env.say(&format!(
                "auction: cannot pay the {} fee for {name}, listing stops",
                ask.fee
            ));
            self.cur = None;
            self.sell.clear();
            self.enter(env, Phase::Next, "Auction: choosing what to list");
            return GoalStatus::Running;
        }
        let item_id = cur.stack.item_id;
        cur.ask = Some(ask);
        self.money_before = money;
        let out = wire::post(
            self.obj,
            self.obj,
            item_id,
            ask.start,
            ask.buyout,
            duration::H48,
        );
        self.request(env, out);
        self.phase = Phase::Posting;
        self.text = format!("Auction: posting {name} x{count}").into();
        GoalStatus::Running
    }

    fn do_posting(&mut self, env: &mut dyn Env) -> GoalStatus {
        let now = env.now_ms();
        let Some(cur) = self.cur.as_ref() else {
            return self.fail(env, FailReason::Other(""), "no item to list");
        };
        let (stack, name, ask, quote, facts) =
            (cur.stack, cur.name.clone(), cur.ask, cur.quote, cur.facts);
        let Some(ask) = ask else {
            return self.fail(env, FailReason::Other(""), "no price to post");
        };
        if let Some(a) = env.mem().inbox.posted.take() {
            if a.at_ms >= self.sent_ms
                && (a.what.item_id == stack.item_id || a.what.template == stack.template)
            {
                let lot = a.what;
                {
                    let mem = env.mem();
                    mem.stats.posted += 1;
                    mem.stats.fees += i64::from(ask.fee);
                    mem.last_post_ms = now;
                    mem.note_own(OwnLot {
                        id: lot.id,
                        template: stack.template,
                        count: lot.count,
                        buyout: lot.buyout,
                        posted_ms: now,
                        posted_here: true,
                        seen: true,
                    });
                }
                self.posted += 1;
                self.posted_ids.push((lot.id, stack.template));
                if lot.item_id == stack.item_id && lot.buyout == ask.buyout {
                    env.note_resale_post(stack, &name, ask);
                }
                let vendor = u64::from(facts.refund) * u64::from(stack.count);
                env.say(&format!(
                    "auction: posted {name} x{} for {} (start {}, fee {}, net {}, vendor {vendor}; market {})",
                    stack.count,
                    ask.buyout,
                    ask.start,
                    ask.fee,
                    ask.net,
                    match quote {
                        Some(q) => format!("{} each over {} lot(s)", q.unit, q.lots),
                        None => "none".to_owned(),
                    }
                ));
                self.cur = None;
                self.enter(env, Phase::Next, "Auction: choosing what to list");
                return GoalStatus::Running;
            }
        }
        if let Some((err, _)) = env.error_since(self.sent_ms) {
            return self.post_refused(env, err, stack, &name);
        }
        if now.saturating_sub(self.sent_ms) <= ANSWER_WAIT_MS {
            return GoalStatus::Running;
        }
        // No answer. Did the post happen anyway?
        env.mem().stats.timeouts += 1;
        let st = env.state();
        let item_gone = !st.bag.values().any(|i| i.id == stack.item_id);
        let fee_paid = self.money_before - st.me.money >= i64::from(ask.fee);
        if item_gone && fee_paid {
            env.report(report::post_without_answer(&name, stack.template, ask.fee));
            env.say(&format!(
                "auction: {name} was taken and the fee charged, but no lot came back"
            ));
            env.mem().skip(stack.template, now + REFUSED_MS, now);
        } else {
            self.silent_posts += 1;
            env.report(report::silent_post(stack.template));
            env.say(&format!(
                "auction: the post of {name} got no answer and changed nothing"
            ));
            env.mem().skip(stack.template, now + REFUSED_MS, now);
            if self.silent_posts >= MAX_SILENT_POSTS {
                self.sell.clear();
            }
        }
        self.cur = None;
        self.enter(env, Phase::Next, "Auction: choosing what to list");
        GoalStatus::Running
    }

    fn post_refused(
        &mut self,
        env: &mut dyn Env,
        err: i16,
        stack: Stack,
        name: &str,
    ) -> GoalStatus {
        let now = env.now_ms();
        match err {
            ERR_TOO_FAR => {
                // back it goes, to be priced (from memory) and posted after the step up
                self.cur = None;
                self.sell.insert(0, stack);
                if self.out_of_range(env, Phase::Next) {
                    GoalStatus::Running
                } else {
                    self.fail(env, FailReason::Stuck, "the auctioneer is out of reach")
                }
            }
            ERR_CANNOT_PUT_UP => {
                // the fee cannot be paid, or the item cannot be put up: nothing more is listed
                env.mem().stats.refused += 1;
                env.say(&format!(
                    "auction: the server will not take {name} (error {err}: cannot pay the fee or put it up)"
                ));
                env.mem().skip(stack.template, now + REFUSED_MS, now);
                self.cur = None;
                self.sell.clear();
                self.enter(env, Phase::Next, "Auction: choosing what to list");
                GoalStatus::Running
            }
            other => {
                self.unexpected(env, other, "post");
                env.mem().skip(stack.template, now + REFUSED_MS, now);
                self.cur = None;
                self.enter(env, Phase::Next, "Auction: choosing what to list");
                GoalStatus::Running
            }
        }
    }

    // ---- browsing and buying ----------------------------------------------------------------

    fn start_browse(&mut self, env: &mut dyn Env) -> GoalStatus {
        self.cur = None;
        if self.budget <= 0 || self.bought >= MAX_BUYS {
            return self.start_verify(env);
        }
        if let Some((why, lot)) = self.bargain.take() {
            return self.buy(env, why, lot);
        }
        self.pages_left = BROWSE_PAGES;
        // the server sorts by buyout, so page 0 holds the cheapest lots; a later page is asked for
        // only after a full one (the server warns about a page past the end)
        self.page = 0;
        self.best = None;
        self.enter(env, Phase::Browse, "Auction: looking at the lots on offer");
        GoalStatus::Running
    }

    fn do_browse(&mut self, env: &mut dyn Env) -> GoalStatus {
        let (me, _) = Self::me(env);
        if self.sent_ms == 0 {
            let q = Search {
                page: self.page,
                ..Search::default()
            };
            self.search(env, &q);
            return GoalStatus::Running;
        }
        match self.wait_page(env) {
            Waited::Pending => GoalStatus::Running,
            Waited::Got(page) => {
                self.look_at(env, me, &page.lots);
                self.pages_left = self.pages_left.saturating_sub(1);
                if page.lots.len() >= PAGE_SIZE && self.pages_left > 0 {
                    self.page += 1;
                    self.sent_ms = 0;
                    self.tries = 0;
                    return GoalStatus::Running;
                }
                match self.best.take() {
                    Some((why, lot)) => self.buy(env, why, lot),
                    None => {
                        env.say("auction: nothing worth buying on the first pages");
                        self.start_verify(env)
                    }
                }
            }
            Waited::Refused(ERR_TOO_FAR) => {
                if self.out_of_range(env, Phase::Browse) {
                    GoalStatus::Running
                } else {
                    self.fail(env, FailReason::Stuck, "the auctioneer is out of reach")
                }
            }
            Waited::Refused(code) => {
                self.unexpected(env, code, "search");
                self.start_verify(env)
            }
            Waited::Silent => {
                env.mem().stats.timeouts += 1;
                if self.tries < MAX_SEARCH_TRIES {
                    self.sent_ms = 0;
                    return GoalStatus::Running;
                }
                self.start_verify(env)
            }
        }
    }

    /// Notes the best lot of `lots` the bot would buy.
    fn look_at(&mut self, env: &mut dyn Env, me: u32, lots: &[Lot]) {
        let money = env.state().me.money;
        for lot in lots {
            if !env.tradable(lot.template) {
                continue;
            }
            let Some(facts) = env.facts(lot.template) else {
                continue;
            };
            let useful = env.useful(lot.template);
            let Some(why) = buy_reason(env, lot, facts, useful, me, money, self.budget) else {
                continue;
            };
            let better = match &self.best {
                None => true,
                Some((w, b)) => (why as u8, lot.buyout) < (*w as u8, b.buyout),
            };
            if better {
                self.best = Some((why, lot.clone()));
            }
        }
    }

    fn buy(&mut self, env: &mut dyn Env, why: Why, lot: Lot) -> GoalStatus {
        if why == Why::Resale && !env.resale_candidate(&lot, self.budget) {
            return self.start_verify(env);
        }
        self.buying_for_resale = why == Why::Resale;
        let (me, name) = Self::me(env);
        let item = env
            .item_name(lot.template)
            .unwrap_or_else(|| format!("#{}", lot.template));
        env.say(&format!(
            "auction: buying out {item} x{} for {} ({})",
            lot.count,
            lot.buyout,
            match why {
                Why::Bargain => "a bargain",
                Why::Resale => "guild resale",
                Why::Useful => "useful, no dearer than a shop",
            }
        ));
        self.money_before = env.state().me.money;
        let out = wire::bid(self.obj, self.obj, &lot, me, &name, lot.buyout);
        self.request(env, out);
        self.phase = Phase::Buying;
        self.text = format!("Auction: buying {item}").into();
        self.lot = Some(lot);
        GoalStatus::Running
    }

    fn do_buying(&mut self, env: &mut dyn Env) -> GoalStatus {
        let now = env.now_ms();
        let Some(lot) = self.lot.clone() else {
            return self.start_verify(env);
        };
        if let Some(a) = env.mem().inbox.bid.take() {
            if a.at_ms >= self.sent_ms && a.what.lot_id == lot.id {
                let done = a.what;
                {
                    let mem = env.mem();
                    mem.stats.bought += 1;
                    mem.stats.spent += i64::from(done.money);
                    if done.buyout {
                        mem.expect_win(done.lot_id, done.template, now);
                    }
                }
                self.bought += 1;
                self.budget -= i64::from(done.money);
                if self.buying_for_resale && done.buyout {
                    env.note_resale_buy(&lot, i64::from(done.money));
                    self.relist_after_buy = true;
                }
                let name = env
                    .item_name(done.template)
                    .unwrap_or_else(|| format!("#{}", done.template));
                env.say(&format!(
                    "auction: bought out {name} x{} for {} (the item comes by mail)",
                    lot.count, done.money
                ));
                self.lot = None;
                return self.after_buy(env);
            }
        }
        if let Some((err, _)) = env.error_since(self.sent_ms) {
            if err != ERR_TOO_FAR {
                self.unexpected(env, err, "buyout");
            }
            self.lot = None;
            return self.start_verify(env);
        }
        if now.saturating_sub(self.sent_ms) <= ANSWER_WAIT_MS {
            return GoalStatus::Running;
        }
        env.mem().stats.timeouts += 1;
        let paid = self.money_before - env.state().me.money;
        let item = env
            .item_name(lot.template)
            .unwrap_or_else(|| format!("#{}", lot.template));
        if paid >= i64::from(lot.buyout) {
            env.report(report::bid_without_answer(&item, lot.template, lot.buyout));
            env.mem().expect_win(lot.id, lot.template, now);
            if self.buying_for_resale {
                env.note_resale_buy(&lot, i64::from(lot.buyout));
                self.relist_after_buy = true;
            }
            env.say(&format!(
                "auction: the money for {item} was taken but no answer came"
            ));
        } else {
            env.say(&format!(
                "auction: the buyout of {item} got no answer and cost nothing"
            ));
        }
        self.lot = None;
        self.start_verify(env)
    }

    fn after_buy(&mut self, env: &mut dyn Env) -> GoalStatus {
        if self.bought < MAX_BUYS && self.budget > 0 && env.rng().f32() < 0.4 {
            // one more look
            self.pages_left = BROWSE_PAGES;
            self.page = 0;
            self.best = None;
            self.enter(env, Phase::Browse, "Auction: looking at the lots on offer");
            return GoalStatus::Running;
        }
        self.start_verify(env)
    }

    // ---- the end ----------------------------------------------------------------------------

    fn start_verify(&mut self, env: &mut dyn Env) -> GoalStatus {
        if self.posted > 0 {
            self.verify = true;
            self.page = 0;
            self.enter(
                env,
                Phase::Survey,
                "Auction: checking that my lots are listed",
            );
        } else {
            self.begin_linger(env);
        }
        GoalStatus::Running
    }

    fn begin_linger(&mut self, env: &mut dyn Env) {
        let (lo, hi) = LINGER_MS;
        let span = env.rng().u64(lo..hi);
        self.linger_until = env.now_ms() + span;
        self.enter(env, Phase::Linger, "Auction: looking around");
    }

    fn do_linger(&mut self, env: &mut dyn Env) -> GoalStatus {
        if self.linger_until == 0 {
            self.begin_linger(env);
        }
        if env.now_ms() < self.linger_until {
            if self.relist_after_buy && usize::from(self.posted) < MAX_LISTINGS {
                if let Some(stack) = env.ready_resale() {
                    self.relist_after_buy = false;
                    self.budget = 0;
                    self.sell.push(stack);
                    self.enter(env, Phase::Next, "Auction: listing the purchased resale");
                }
            }
            return GoalStatus::Running;
        }
        let total = super::summary(env.mem());
        env.say(&format!(
            "auction: visit done, {} posted, {} bought (all visits: {total})",
            self.posted, self.bought
        ));
        GoalStatus::Done
    }
}

fn buy_reason(
    env: &dyn Env,
    lot: &Lot,
    facts: Facts,
    useful: bool,
    me: u32,
    money: i64,
    budget: i64,
) -> Option<Why> {
    if env.resale_candidate(lot, budget) {
        Some(Why::Resale)
    } else {
        price::worth_buying(lot, facts, useful, me, money, budget)
    }
}

fn headline(phase: Phase) -> &'static str {
    match phase {
        Phase::Survey => "Auction: checking my lots",
        Phase::Next | Phase::Pricing => "Auction: pricing what I sell",
        Phase::Browse => "Auction: looking at the lots on offer",
        _ => "Auction: at the auctioneer",
    }
}

/// The auctioneer NPC in view that belongs to the planned spawn: `(object id, position)`.
pub(in crate::bot::goals) fn find_npc(
    st: &BotState,
    spot: Spot,
    from: [f32; 3],
) -> Option<(u32, [f32; 3])> {
    st.units
        .iter()
        .filter(|(_, u)| u.kind == UnitKind::Npc && u.id == spot.template && !u.dead())
        .map(|(obj, u)| (*obj, u.pos, dist2d(u.pos, spot.pos)))
        .filter(|c| c.2 <= SPAWN_MATCH_M)
        .min_by(|a, b| {
            dist2d(a.1, from)
                .total_cmp(&dist2d(b.1, from))
                .then(a.0.cmp(&b.0))
        })
        .map(|c| (c.0, c.1))
}

/// "640 m" or "1.2 km".
fn length(m: f32) -> String {
    if m >= 1000.0 {
        format!("{:.1} km", m / 1000.0)
    } else {
        format!("{m:.0} m")
    }
}
