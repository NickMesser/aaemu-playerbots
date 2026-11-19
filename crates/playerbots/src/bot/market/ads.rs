//! Trade-chat sales: advertisements, buyers and sellers, negotiations (owned by the market
//! feature, goal `market`).
//!
//! [`AdState`] is the bot's memory of what it advertised and who answered. It lives in
//! [`Inner`](super::Inner) (`state.market`) and is driven every brain tick by the market duty
//! (`goals::market::duty`), whatever goal runs:
//!
//! * [`AdState::on_chat`] queues what the bot hears (shouts, trade and group-finding chat, whispers,
//!   see [`hears`]) from the social duty; only lines that talk about buying and selling are kept.
//! * [`AdState::step`] reads the queue and answers: a buyer who replies to the bot's
//!   advertisement gets a quote, a counter-offer, an agreement and the meeting place
//!   ([`talk::Talk`]); the bot answers the advertisements of other bots and of players when it
//!   wants the goods or has them; it posts an advertisement of its own surplus (or of what it
//!   needs) when its turn comes and somebody could read it; limits are per bot
//!   (`ad_interval_secs`) and for the whole process ([`board::Board`]: `max_ads_per_hour`, lines
//!   per minute). Nothing it says is a reply to itself, a command or too long.
//! * An agreed deal ([`Agreed`]) is what the goal ([`crate::bot::goals::market`]) carries out: the
//!   meeting and the trade, which checks the window against the deal
//!   ([`crate::bot::goals::trade::Deal`]).
//!
//! The pure parts: [`grammar`] (what a line says), [`lines`] (what the bots say), [`talk`] (one
//! negotiation), [`price`] (what things cost), [`names`] (item names), [`board`] (what all bots of
//! the process share). Everything here is tested with scripted lines; nothing touches the network.

pub mod board;
pub mod grammar;
pub mod lines;
pub mod names;
pub mod price;
pub mod talk;

use aa_proto::cs::chat_type;
use smallvec::SmallVec;

use self::board::Board;
use self::grammar::Line;
use self::lines::{Decline, Meet, Speech};
use self::names::NameIndex;
use self::price::{PriceSource, Quote};
use self::talk::{Closed, Medium, PlaceQ, Role, Stage, Talk};

/// The chat types the market hears: shout, trade, group finding and whispers (the server's
/// `ChatType`, [`aa_proto::cs::chat_type`]).
pub const HEARD_KINDS: [i16; 4] = [
    chat_type::SHOUT,
    chat_type::TRADE,
    chat_type::GROUP_FIND,
    chat_type::WHISPER,
];

/// Whether the social duty hands lines of chat type `kind` to [`AdState::on_chat`].
pub fn hears(kind: i16) -> bool {
    HEARD_KINDS.contains(&kind)
}

/// Whether a line is worth keeping: it talks about buying and selling.
pub fn worth_hearing(kind: i16, text: &str) -> bool {
    hears(kind) && grammar::parse(text).is_market()
}

/// Lines kept between two ticks.
const INBOX_MAX: usize = 6;
/// Open negotiations at once.
const MAX_TALKS: usize = 3;
/// Speeches waiting to be said.
const MAX_DUE: usize = 4;
/// An advertisement stands this long.
pub const LISTING_MS: u64 = 8 * 60_000;
/// A bot says one line at most this often.
const SAY_GAP_MS: u64 = 2_500;
/// A reply comes after this long (a bot types).
const REPLY_DELAY_MS: (u64, u64) = (1_200, 4_500);
/// Chance that a bot answers an advertisement it has a use for.
const ENGAGE_PERCENT: u8 = 55;
/// Chance that a buyer asks the price instead of taking the goods at the advertised one.
const ASK_FIRST_PERCENT: u8 = 30;
/// The advertisement of one bot is posted at most this much sooner or later than the interval.
const AD_JITTER_PERCENT: u64 = 25;

/// Which side an advertisement is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Goods for sale.
    Sell,
    /// Goods wanted.
    Buy,
}

/// The guild allowance approved for a negotiated purchase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Funding {
    Training(u32),
    FarmLand(u32),
}

/// An advertisement of this bot that is standing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Listing {
    /// Sell or buy.
    pub side: Side,
    /// The item.
    pub template: u32,
    /// Units for sale or wanted.
    pub qty: u32,
    /// Prices per unit.
    pub quote: Quote,
    pub funding: Option<Funding>,
    /// Bot clock it was posted.
    pub posted_ms: u64,
}

/// A deal agreed in chat, waiting for the meeting and the trade.
#[derive(Debug, Clone, PartialEq)]
pub struct Agreed {
    /// Whether the bot sells or buys.
    pub role: Role,
    /// The other character.
    pub peer: Box<str>,
    /// The item.
    pub template: u32,
    /// Units.
    pub qty: u32,
    /// Copper per unit.
    pub unit: u32,
    pub funding: Option<Funding>,
    /// Where to meet.
    pub meet: Meet,
    /// Bot clock of the agreement.
    pub at_ms: u64,
}

impl Agreed {
    /// Copper for all the units.
    pub fn total(&self) -> i64 {
        i64::from(self.unit) * i64::from(self.qty)
    }
}

/// A speech waiting for its time.
#[derive(Debug, Clone, PartialEq)]
pub struct Due {
    /// Bot clock it may be said.
    pub at_ms: u64,
    /// A whisper or the trade channel.
    pub medium: Medium,
    /// The character it is for (empty for an advertisement).
    pub to: Box<str>,
    /// What to say.
    pub speech: Speech,
}

/// A line the bot heard, parsed.
#[derive(Debug, Clone, PartialEq)]
struct Heard {
    kind: i16,
    sender: Box<str>,
    text: Box<str>,
    line: Line,
    /// A whisper that belongs to the market (the social duty leaves it alone).
    claim: bool,
}

/// What a bot has to sell: an item it may sell honestly (the provenance ledger) and how many
/// units fit one trade (the largest stack).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Surplus {
    /// The item.
    pub template: u32,
    /// Units to offer (surplus, at most one stack).
    pub count: u32,
}

/// Something the bot would buy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Need {
    /// The item.
    pub template: u32,
    /// Units it wants.
    pub qty: u32,
    /// Fixed price per unit for a guild order.
    pub order_price: Option<u32>,
    /// Rank allowance supporting this purchase, if one was budgeted.
    pub funding: Option<Funding>,
}

impl Need {
    pub fn quote(self, prices: &dyn PriceSource) -> Option<Quote> {
        match self.order_price {
            Some(price) => Some(Quote {
                ask: price,
                floor: price,
                ceiling: price,
            }),
            None => prices.quote(self.template),
        }
    }
}

/// The limits and switches of one step.
#[derive(Debug, Clone, Copy)]
pub struct Rules {
    /// Advertisements are allowed (`market.enabled` and `chat_ads`).
    pub chat_ads: bool,
    /// Real players (characters that are not one of our bots) may buy.
    pub sell_to_players: bool,
    /// All bots together.
    pub max_ads_per_hour: u32,
    /// One bot.
    pub ad_interval_ms: u64,
}

/// What a step works with: the bot's situation and the rules. See [`AdState::step`].
pub struct Env<'a> {
    /// The bot clock.
    pub now_ms: u64,
    /// Unix time (for the process-wide limits).
    pub unix_ms: u64,
    /// Switches and limits.
    pub rules: Rules,
    /// The bot's own name.
    pub me: &'a str,
    /// Where prices come from.
    pub prices: &'a dyn PriceSource,
    /// Item names to templates.
    pub names: &'a NameIndex,
    /// The English name of an item.
    pub item_name: &'a dyn Fn(u32) -> String,
    /// What the bot may sell.
    pub surplus: &'a [Surplus],
    /// What the bot would buy.
    pub needs: &'a [Need],
    /// Copper carried.
    pub money: i64,
    /// Other bots that read the same trade chat.
    pub audience: usize,
    /// Whether a character is one of our bots.
    pub is_bot: &'a dyn Fn(&str) -> bool,
    /// Whether the seller knows this buyer as a trusted companion.
    pub trusted_companion: &'a dyn Fn(&str) -> bool,
    /// How much of an advertised item the bot wants and what it pays at most per unit, given the
    /// template, the units offered and the asking price.
    pub interest: &'a dyn Fn(u32, u32, u32) -> Option<(u32, u32)>,
    /// The meeting place.
    pub place: &'a mut dyn FnMut(PlaceQ<'_>) -> Option<Meet>,
    /// What all bots share.
    pub board: &'a Board,
    /// Randomness.
    pub rng: &'a mut fastrand::Rng,
}

/// What the bot remembers of its advertisements and buyers. See the module docs.
#[derive(Debug, Default)]
pub struct AdState {
    inbox: SmallVec<[Heard; 4]>,
    listing: Option<Listing>,
    talks: SmallVec<[Talk; 2]>,
    deal: Option<Agreed>,
    due: SmallVec<[Due; 2]>,
    /// Bot clock before which no advertisement is posted.
    next_ad_ms: u64,
    last_say_ms: u64,
    /// Bot clock of the next look at the surroundings ([`AdState::check_due`]).
    check_ms: u64,
    /// Bot clock of the next report of the bot's place to the board ([`AdState::report_due`]).
    report_ms: u64,
    /// Bot clock of the next status line ([`AdState::status_due`]).
    status_ms: u64,
    /// Bot clock of the last whisper the bot sent (0 = none).
    whisper_ms: u64,
    /// Bot clock of the last chat refusal that was reported.
    chat_err_ms: u64,
    /// Advertisements posted.
    pub posted: u32,
    /// Lines about buying and selling heard.
    pub heard: u32,
    /// Negotiations opened (an answer to an advertisement, in either direction).
    pub talks_opened: u32,
    /// Deals agreed.
    pub agreed: u32,
}

impl AdState {
    /// The bot heard a chat line. `kind` is the server's chat type (1 shout, 2 trade, 3 group
    /// finding, -3 whisper), `sender` the speaker's name, `text` the line (at most 200
    /// characters) and `now_ms` the bot clock. Lines that have nothing to do with buying and
    /// selling are dropped; the rest waits for [`AdState::step`].
    pub fn on_chat(&mut self, kind: i16, sender: &str, text: &str, _now_ms: u64) {
        if !hears(kind) || sender.is_empty() {
            return;
        }
        let line = grammar::parse(text);
        if !line.is_market() {
            return;
        }
        let whisper = kind == chat_type::WHISPER;
        let claim = whisper
            && (self.talk_with(sender).is_some()
                || self.listing.is_some()
                || self.deal.as_ref().is_some_and(|d| same(&d.peer, sender)));
        if self.inbox.len() >= INBOX_MAX {
            self.inbox.remove(0);
        }
        self.heard = self.heard.saturating_add(1);
        self.inbox.push(Heard {
            kind,
            sender: sender.into(),
            text: text.into(),
            line,
            claim,
        });
    }

    /// Whether a whisper from `sender` is part of a negotiation, so that the social duty must not
    /// answer it as well.
    pub fn claims_whisper(&self, sender: &str) -> bool {
        self.inbox
            .iter()
            .any(|h| h.claim && h.kind == chat_type::WHISPER && same(&h.sender, sender))
            || self
                .talks
                .iter()
                .any(|t| !t.is_closed() && same(&t.peer, sender))
    }

    /// The negotiation with `peer`, if any.
    pub fn talk_with(&self, peer: &str) -> Option<&Talk> {
        self.talks.iter().find(|t| same(&t.peer, peer))
    }

    /// The advertisement that is standing.
    pub fn listing(&self) -> Option<&Listing> {
        self.listing.as_ref()
    }

    /// The deal that waits for its trade.
    pub fn deal(&self) -> Option<&Agreed> {
        self.deal.as_ref()
    }

    /// Whether a negotiation is open.
    pub fn talking(&self) -> bool {
        self.talks.iter().any(|t| !t.is_closed())
    }

    /// Whether lines wait to be read or said.
    pub fn busy(&self) -> bool {
        !self.inbox.is_empty() || !self.due.is_empty()
    }

    /// The agreed deal is over (`Closed::Done` after the trade, anything else when it fell
    /// through): the talk ends, the units are free again.
    pub fn finish_deal(&mut self, how: Closed) {
        if let Some(d) = self.deal.take() {
            if let Some(t) = self.talks.iter_mut().find(|t| same(&t.peer, &d.peer)) {
                t.close(how);
            }
            if how == Closed::Done {
                if let Some(l) = self.listing.as_mut() {
                    l.qty = l.qty.saturating_sub(d.qty);
                }
                if self.listing.is_some_and(|l| l.qty == 0) {
                    self.listing = None;
                }
            }
        }
        self.talks.retain(|t| !t.is_closed());
    }

    /// Forgets the standing advertisement.
    pub fn withdraw(&mut self) {
        self.listing = None;
    }

    /// Whether the duty should look at its surroundings now: lines wait to be read or said, or
    /// `gap_ms` passed since the last look.
    pub fn check_due(&mut self, now_ms: u64, gap_ms: u64) -> bool {
        if self.busy() || now_ms >= self.check_ms {
            self.check_ms = now_ms + gap_ms;
            true
        } else {
            false
        }
    }

    /// Whether the bot should tell the board where it is (every `gap_ms`).
    pub fn report_due(&mut self, now_ms: u64, gap_ms: u64) -> bool {
        if now_ms >= self.report_ms {
            self.report_ms = now_ms + gap_ms;
            true
        } else {
            false
        }
    }

    /// The bot sent a whisper at `now_ms`.
    pub fn note_whisper(&mut self, now_ms: u64) {
        self.whisper_ms = now_ms.max(1);
    }

    /// Bot clock of the last whisper sent (0 = none).
    pub fn last_whisper_ms(&self) -> u64 {
        self.whisper_ms
    }

    /// The server's refusal of a whisper at `at_ms` was reported.
    pub fn note_chat_error(&mut self, at_ms: u64) {
        self.chat_err_ms = at_ms;
    }

    /// Bot clock of the last refusal that was reported.
    pub fn chat_error_seen_ms(&self) -> u64 {
        self.chat_err_ms
    }

    /// Whether the duty should write its status line now (every `gap_ms`).
    pub fn status_due(&mut self, now_ms: u64, gap_ms: u64) -> bool {
        if now_ms >= self.status_ms {
            self.status_ms = now_ms + gap_ms;
            true
        } else {
            false
        }
    }

    /// Lets the next step post an advertisement without waiting for the interval.
    pub fn post_now(&mut self) {
        self.next_ad_ms = 0;
    }

    /// Says `speech` to the other side of the agreed deal, in the medium the negotiation used.
    pub fn tell_peer(&mut self, speech: Speech, now_ms: u64, delay_ms: u64) {
        let Some(d) = self.deal.as_ref() else { return };
        let medium = self
            .talks
            .iter()
            .find(|t| same(&t.peer, &d.peer))
            .map_or(Medium::Whisper, |t| t.medium);
        let to = d.peer.clone();
        if self.due.len() < MAX_DUE + 2 {
            self.due.push(Due {
                at_ms: now_ms + delay_ms,
                medium,
                to,
                speech,
            });
        }
    }

    /// The speech that is ready to be said at `now_ms` (one line per [`SAY_GAP_MS`]).
    pub fn next_due(&mut self, now_ms: u64) -> Option<Due> {
        if now_ms.saturating_sub(self.last_say_ms) < SAY_GAP_MS {
            return None;
        }
        let i = self.due.iter().position(|d| d.at_ms <= now_ms)?;
        self.last_say_ms = now_ms;
        Some(self.due.remove(i))
    }

    /// Puts a line that was refused (the market spoke too much) back at the end of the queue.
    pub fn postpone(&mut self, mut due: Due, until_ms: u64) {
        due.at_ms = until_ms;
        if self.due.len() < MAX_DUE {
            self.due.push(due);
        }
    }

    /// Reads what was heard, ends what is over and posts an advertisement when it is time.
    pub fn step(&mut self, env: &mut Env<'_>) {
        let now = env.now_ms;
        for t in self.talks.iter_mut() {
            t.expire(now);
        }
        if self
            .deal
            .as_ref()
            .is_some_and(|d| now.saturating_sub(d.at_ms) > talk::AGREED_MS)
        {
            self.finish_deal(Closed::TimedOut);
        }
        self.talks.retain(|t| !t.is_closed());
        if self
            .listing
            .is_some_and(|l| now.saturating_sub(l.posted_ms) > LISTING_MS)
        {
            self.listing = None;
        }
        let heard: SmallVec<[Heard; 4]> = std::mem::take(&mut self.inbox);
        for h in heard {
            self.hear(h, env);
        }
        self.post(env);
    }

    fn say(&mut self, env: &mut Env<'_>, medium: Medium, to: &str, speech: Speech) {
        if self.due.len() >= MAX_DUE {
            return;
        }
        let (lo, hi) = REPLY_DELAY_MS;
        let delay = lo + env.rng.u64(0..hi - lo);
        self.due.push(Due {
            at_ms: env.now_ms + delay,
            medium,
            to: to.into(),
            speech,
        });
    }

    // ---- hearing -------------------------------------------------------------------------

    fn hear(&mut self, h: Heard, env: &mut Env<'_>) {
        let medium = if h.kind == chat_type::WHISPER {
            Medium::Whisper
        } else {
            Medium::Channel
        };
        let from_bot = (env.is_bot)(&h.sender);
        let may = from_bot || env.rules.sell_to_players;
        // a line in the channel that names somebody else is for them
        if addressed_elsewhere(&h, env.me, medium) {
            return;
        }
        // an open talk takes the line, wherever it was said
        if let Some(i) = self.talks.iter().position(|t| same(&t.peer, &h.sender)) {
            self.talk_line(i, &h, medium, may, env);
            return;
        }
        // lines in a channel are for the bot only when they are about its goods or name it
        if medium == Medium::Channel {
            if h.line.sell_ad || h.line.buy_ad {
                self.hear_ad(&h, medium, may, env);
                return;
            }
            let names_me = lines::starts_with_name(&h.text, env.me);
            let about_mine = self.listing.is_some_and(|l| {
                env.names.find(&h.text) == Some(l.template)
                    || grammar::item_hint(
                        &grammar::normalize(&h.text),
                        &(env.item_name)(l.template),
                    )
            });
            if !(names_me || about_mine) {
                return;
            }
        } else if h.line.is_ad() {
            self.hear_ad(&h, medium, may, env);
            return;
        }
        self.hear_reply(&h, medium, may, env);
    }

    /// A reply to the bot's own advertisement (or a whisper about its goods) that starts a talk.
    fn hear_reply(&mut self, h: &Heard, medium: Medium, may: bool, env: &mut Env<'_>) {
        let Some(l) = self.listing else { return };
        if !h.line.is_market() || h.line.refuse {
            return;
        }
        // a line about another item is not an answer to this advertisement
        if let Some(t) = env.names.find(&h.text) {
            if !(env.item_name)(t).eq_ignore_ascii_case(&(env.item_name)(l.template)) {
                return;
            }
        }
        if self.talks.len() >= MAX_TALKS {
            return;
        }
        match l.side {
            Side::Sell => {
                if !may {
                    // a player the bot is not allowed to sell to: a polite no
                    self.say(env, medium, &h.sender, Speech::Decline(Decline::SoldOut));
                    return;
                }
                let free = l.qty;
                let quote = if medium == Medium::Whisper && (env.trusted_companion)(&h.sender) {
                    l.quote.for_companion()
                } else {
                    l.quote
                };
                let mut t = Talk::seller(&h.sender, medium, l.template, free, quote, env.now_ms);
                if self.deal.is_some() {
                    self.say(env, medium, &h.sender, Speech::Decline(Decline::SoldOut));
                    return;
                }
                let reply = t.on_line(&h.line, medium, env.now_ms, &mut *env.place);
                self.keep(t, reply, medium, env);
            }
            Side::Buy => {
                // somebody answers "WTB": what do they have, at what price
                let mut t = Talk::buyer(
                    &h.sender,
                    medium,
                    l.template,
                    l.qty,
                    l.quote.ceiling,
                    env.now_ms,
                );
                t.funding = l.funding;
                let reply = t.on_line(&h.line, medium, env.now_ms, &mut *env.place);
                self.keep(t, reply, medium, env);
            }
        }
    }

    /// The advertisement of somebody else.
    fn hear_ad(&mut self, h: &Heard, medium: Medium, may: bool, env: &mut Env<'_>) {
        // (the bot's own advertisement does not keep it from answering somebody else's: a bot
        // that sells ore may well want hay)
        if !may || self.talks.len() >= MAX_TALKS || self.deal.is_some() {
            return;
        }
        // two bots that sell and want each other's goods would answer both advertisements at the
        // same moment and then wait for each other for good: the one that sorts first leaves the
        // other's selling advertisement to it
        if h.line.sell_ad
            && (env.is_bot)(&h.sender)
            && self.listing.is_some_and(|l| l.side == Side::Sell)
            && env.me.to_lowercase() <= h.sender.to_lowercase()
        {
            return;
        }
        let Some(template) = env.names.find(&h.text) else {
            return;
        };
        let qty = h.line.qty.unwrap_or(1).max(1);
        let Some(price) = h.line.price else { return };
        let unit = if price.per_unit {
            price.copper
        } else {
            price.copper / i64::from(qty)
        };
        let Ok(unit) = u32::try_from(unit.max(1)) else {
            return;
        };
        if env.rng.u8(..100) >= ENGAGE_PERCENT {
            return;
        }
        if h.line.sell_ad {
            // goods for sale: do we want them?
            let Some((want, ceiling)) = (env.interest)(template, qty, unit) else {
                return;
            };
            let mut t = Talk::buyer(
                &h.sender,
                medium,
                template,
                want.min(qty).max(1),
                ceiling,
                env.now_ms,
            );
            t.funding = env
                .needs
                .iter()
                .find(|need| need.template == template)
                .and_then(|need| need.funding);
            let ask_first = env.rng.u8(..100) < ASK_FIRST_PERCENT;
            let speech = t.open_as_buyer(unit, ask_first);
            if speech.is_some() {
                self.keep(t, speech, medium, env);
            }
        } else if h.line.buy_ad {
            // goods wanted: do we have them?
            let Some(have) = env.surplus.iter().find(|s| s.template == template) else {
                return;
            };
            let Some(quote) = env.prices.quote(template) else {
                return;
            };
            if unit < quote.floor {
                return;
            }
            let give = have.count.min(qty).max(1);
            // the buyer's price is the one on the table when it is above the asking price
            let ask = unit.max(quote.ask);
            let q = Quote { ask, ..quote };
            let mut t = Talk::seller(&h.sender, medium, template, give, q, env.now_ms);
            t.qty = give;
            t.stage = Stage::Quoted;
            self.keep(
                t,
                Some(Speech::Offer {
                    template,
                    qty: give,
                    unit: ask,
                }),
                medium,
                env,
            );
        }
    }

    fn talk_line(&mut self, i: usize, h: &Heard, medium: Medium, may: bool, env: &mut Env<'_>) {
        if self.talks[i].role == Role::Seller && !may {
            self.talks[i].close(Closed::Declined);
            self.say(env, medium, &h.sender, Speech::Decline(Decline::SoldOut));
            return;
        }
        // one deal at a time: another buyer after the goods that are promised
        if self.talks[i].role == Role::Seller && self.deal.is_some() && !self.talks[i].is_agreed() {
            self.talks[i].close(Closed::Declined);
            self.say(env, medium, &h.sender, Speech::Decline(Decline::SoldOut));
            return;
        }
        let was_agreed = self.talks[i].is_agreed();
        let reply = {
            let t = &mut self.talks[i];
            t.on_line(&h.line, medium, env.now_ms, &mut *env.place)
        };
        let medium = self.talks[i].medium;
        if let Some(s) = reply {
            self.say(env, medium, &h.sender, s);
        }
        if !was_agreed && self.talks[i].is_agreed() {
            self.make_deal(i, env);
        }
    }

    /// Stores a new talk, says the reply and notes an agreement.
    fn keep(&mut self, t: Talk, reply: Option<Speech>, medium: Medium, env: &mut Env<'_>) {
        if t.is_closed() && reply.is_none() {
            return;
        }
        let peer = t.peer.clone();
        self.talks.push(t);
        self.talks_opened = self.talks_opened.saturating_add(1);
        if let Some(s) = reply {
            self.say(env, medium, &peer, s);
        }
        let i = self.talks.len() - 1;
        if self.talks[i].is_agreed() {
            self.make_deal(i, env);
        }
    }

    fn make_deal(&mut self, i: usize, env: &mut Env<'_>) {
        let t = &self.talks[i];
        let Some(meet) = t.meet else { return };
        if self.deal.is_some() {
            return;
        }
        self.deal = Some(Agreed {
            role: t.role,
            peer: t.peer.clone(),
            template: t.template,
            qty: t.qty,
            unit: t.unit,
            funding: t.funding,
            meet,
            at_ms: env.now_ms,
        });
        self.agreed += 1;
        let (seller, buyer) = match t.role {
            Role::Seller => (env.me, &*t.peer),
            Role::Buyer => (&*t.peer, env.me),
        };
        env.board.note_meet(seller, buyer, meet, env.unix_ms);
    }

    // ---- posting -------------------------------------------------------------------------

    fn post(&mut self, env: &mut Env<'_>) {
        let now = env.now_ms;
        if !env.rules.chat_ads
            || now < self.next_ad_ms
            || self.listing.is_some()
            || self.deal.is_some()
            || self.talking()
            || env.audience == 0
            || !self.due.is_empty()
        {
            return;
        }
        // what to say: goods for sale first, what the bot needs now and then
        let wants_buy = !env.needs.is_empty() && env.money >= 500;
        let sell = self.best_surplus(env);
        let side_sell = match (sell.is_some(), wants_buy) {
            (true, true) => env.rng.u8(..100) < 70,
            (true, false) => true,
            (false, true) => false,
            (false, false) => return,
        };
        let picked = if side_sell {
            sell.map(|(template, qty, quote)| (Side::Sell, template, qty, quote))
        } else {
            self.best_need(env)
                .map(|(template, qty, quote)| (Side::Buy, template, qty, quote))
        };
        let Some((side, template, qty, quote)) = picked else {
            return;
        };
        // (the process-wide limit is taken last, so a bot with nothing to say wastes none)
        if !env.board.take_ad(env.unix_ms, env.rules.max_ads_per_hour) {
            self.next_ad_ms = now + env.rules.ad_interval_ms.max(60_000) / 4;
            return;
        }
        let need = (side == Side::Buy)
            .then(|| env.needs.iter().find(|need| need.template == template))
            .flatten();
        let fixed_price = need.is_some_and(|need| need.order_price.is_some());
        let spread = if fixed_price {
            0
        } else {
            i32::try_from(env.rng.u32(0..21)).unwrap_or(10) - 10
        };
        let q = quote.spread(spread);
        let (quote, price) = match side {
            Side::Sell => (q, q.ask),
            Side::Buy if fixed_price => (q, q.ask),
            // a wanted-ad names what the bot pays; it may go a little over when haggled
            Side::Buy => (
                Quote {
                    ask: q.ask,
                    floor: q.floor.min(q.ask),
                    ceiling: (q.ask * 115 / 100).min(quote.ceiling.max(q.ask)),
                },
                q.ask,
            ),
        };
        let l = Listing {
            side,
            template,
            qty,
            quote,
            funding: need.and_then(|need| need.funding),
            posted_ms: now,
        };
        let speech = match side {
            Side::Sell => Speech::AdSell {
                template,
                qty,
                unit: price,
            },
            Side::Buy => Speech::AdBuy {
                template,
                qty,
                unit: price,
            },
        };
        self.listing = Some(l);
        self.posted += 1;
        let jitter = AD_JITTER_PERCENT;
        let base = env.rules.ad_interval_ms.max(60_000);
        let lo = base * (100 - jitter) / 100;
        let hi = base * (100 + jitter) / 100;
        self.next_ad_ms = now + lo + env.rng.u64(0..hi - lo + 1);
        self.say(env, Medium::Channel, "", speech);
    }

    /// The item the bot can sell for the most: `(template, units, quote)`.
    fn best_surplus(&self, env: &mut Env<'_>) -> Option<(u32, u32, Quote)> {
        env.surplus
            .iter()
            .filter(|s| s.count > 0)
            .filter_map(|s| {
                env.prices
                    .quote(s.template)
                    .map(|q| (s.template, s.count, q))
            })
            .max_by_key(|(_, n, q)| u64::from(*n) * u64::from(q.ask))
    }

    /// What the bot would like to buy: `(template, units, quote)`.
    fn best_need(&self, env: &mut Env<'_>) -> Option<(u32, u32, Quote)> {
        let affordable = |need: &Need| {
            let quote = need.quote(env.prices)?;
            let budget = (env.money / 3 / i64::from(quote.ceiling.max(1))).max(0);
            let qty = need.qty.min(u32::try_from(budget).unwrap_or(u32::MAX));
            (qty > 0).then_some((need.template, qty, quote))
        };
        if let Some(order) = env
            .needs
            .iter()
            .filter(|need| need.order_price.is_some())
            .find_map(affordable)
        {
            return Some(order);
        }
        let index = env.rng.usize(..env.needs.len().max(1));
        env.needs
            .iter()
            .cycle()
            .skip(index)
            .take(env.needs.len())
            .find_map(affordable)
    }
}

/// Whether a line said in a channel is addressed to somebody else by name (`Ann: ok`).
fn addressed_elsewhere(h: &Heard, me: &str, medium: Medium) -> bool {
    if medium == Medium::Whisper {
        return false;
    }
    // "<name>: ..." with a name that is not ours
    let first = h.text.split([':', ',']).next().unwrap_or("");
    // (a word of the trade itself, "ok, ill take them", is no name)
    let looks_like_name = first.split_whitespace().count() == 1
        && first.chars().all(char::is_alphabetic)
        && first.chars().count() >= 2
        && h.text.len() > first.len()
        && h.text[first.len()..].starts_with([':', ','])
        && !grammar::parse(first).is_market();
    looks_like_name && !lines::starts_with_name(&h.text, me)
}

/// Whether two character names are the same one.
fn same(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

#[cfg(test)]
mod tests;
