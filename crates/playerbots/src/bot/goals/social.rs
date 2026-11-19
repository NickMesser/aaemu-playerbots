//! Social behavior: chat.
//!
//! Not a goal the brain chooses but a duty that runs on every brain tick, whatever the bot is
//! doing ([`Social::tick`], called by the [`Brain`](crate::bot::brain::Brain)).
//!
//! # What it does
//!
//! * **Routes the chat inbox.** Whispers and local lines go to the reply logic. System (`-2`) and
//!   notice (`-1`) lines, for example the answer to a GM `/build`, are *not* thrown away: they go
//!   into a small per-bot ring that goals read with [`system_lines`]. Everything else is dropped.
//! * **Answers players.** A whisper to the bot is always a candidate. A local line is a candidate
//!   when it names the bot, or comes from a real player (a character that is not one of ours)
//!   within `social.reply_radius_m` and a dice roll (`social.reply_chance`, scaled by the bot's
//!   chattiness) says yes; of all the bots that hear such a line only the first to claim it
//!   answers. Whispers are answered with a whisper, local lines in local chat.
//! * **Replies** come from the LLM when it is on ([`reply`]: a task off the bot's task, polled on
//!   the tick) and from a small table of persona-flavoured lines ([`canned`]) otherwise or when
//!   the LLM is busy; a jokey bot quips back at small talk now and then ([`humor`]), a crude one
//!   answers coarsely.
//! * **Personalities** ([`style`]): the bot's `chatty`, `joker` and `crude` traits scale how often
//!   it answers what it overhears, how often it speaks unasked, how long it waits between lines,
//!   how many of its lines are jokes and how coarse it is, under the operator's
//!   `[personality] profanity` cap. A chatterbox takes a larger share of the limits below, a
//!   quiet bot hardly speaks.
//! * **Remarks** on what happened to the bot, which the chat notices by itself ([`Tally`]): a
//!   level up, a death or a gank, a quest turned in, a pack sold, a harvest, a house finished, a
//!   bag full of loot; and after the goals the brain reports ([`ChatEvent`]): a duel (won or lost)
//!   and a trade. Bot-to-bot: a hello now and then to a bot standing close by.
//! * **Ambient lines** in local chat while a real player is near (small talk and jokes,
//!   [`lines`], [`humor`]; the LLM may write some of them with `[personality] llm_chatter`), and
//!   from the chattier bots now and then a line on the shout, trade or group-finding channel
//!   (never an advertisement).
//!
//! # Limits that keep it quiet
//!
//! Per bot: a gap between replies, a burst cap, a cap per conversation ([`memory`]), a gap
//! between two unasked lines and at most 1 to 5 unasked lines in 10 minutes, by chattiness;
//! across bots: replies, chatter and ambient lines per minute, channel lines per hour, LLM-written
//! ambient lines per minute, and one answer per overheard line ([`gate::ChatGate`] in the hub).
//! The LLM has its own limits (requests per minute, tokens per day) in `pb-llm`. Unasked lines
//! need a real player within 150 m unless `social.chatter_needs_player` is off.
//!
//! # The language filter
//!
//! Every line, whoever wrote it, passes [`pb_llm::persona::sanitize_chat`] at the bot's level
//! when it is scheduled and once more just before it goes on the wire: commands, links and
//! AI talk are refused, slurs, hate, sexual words, insults and threats too
//! ([`pb_llm::persona::blocked`], also in [`wire_text`] for the lines of the other features), and
//! swearing above the level is softened or the line is dropped. **A line that starts with `/` is
//! never sent** ([`say_packet`], [`whisper_packet`], [`channel_packet`]).
//!
//! The core is [`Social::step`] and [`Social::observe`], free of the tick context so they can be
//! tested with plain values; [`Social::tick`] adapts a [`Ctx`] to them.

pub mod canned;
pub mod gate;
pub mod humor;
pub mod lines;
pub mod memory;
pub mod reply;
pub mod ring;
pub mod style;

#[cfg(test)]
mod tests;

use std::hash::{Hash, Hasher};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use aa_proto::cs::{self, Out};
use aa_world::World;
use pb_llm::persona::Profanity;
use rustc_hash::{FxHashMap, FxHasher};
use smallvec::SmallVec;
use tokio::sync::oneshot;
use tokio::sync::oneshot::error::TryRecvError;

use self::canned::{Intent, Recent};
use self::lines::{Chatter, Slots};
use self::memory::Memory;
use self::reply::LlmReply;
pub use self::ring::{SlotSocial, SysLine};
use self::style::{Style, UNASKED_MAX, UNASKED_SPAN_MS};
use crate::bot::ctx::{Ctx, Persona};
use crate::bot::goals::duel::Outcome;
use crate::bot::state::{ChatIn, Me, Unit, UnitKind};
use crate::config::{race_name, PersonalityCfg, SocialCfg};
use crate::hub::{unix_ms, Hub};

/// Pause between two replies of a bot, in milliseconds (random in the range; scaled by the
/// chattiness, see [`Style::reply_gap`]).
const REPLY_GAP_MS: (u64, u64) = (8_000, 14_000);
/// Replies one bot may send within [`BURST_SPAN_MS`].
const BURST: usize = 4;
/// The window of [`BURST`].
const BURST_SPAN_MS: u64 = 120_000;
/// Think time before a canned reply is sent.
const CANNED_DELAY_MS: (u64, u64) = (1_500, 4_000);
/// Extra think time after an LLM reply arrived.
const LLM_DELAY_MS: (u64, u64) = (200, 1_000);
/// An LLM request that has not answered after this long is forgotten.
const PENDING_TTL_MS: u64 = 25_000;
/// After a failed LLM request the bot answers from the table for this long.
const LLM_BACKOFF_MS: u64 = 30_000;
/// A duel or trade is worth a remark for this long.
const EVENT_TTL_MS: u64 = 25_000;
/// Pause between two looks for a bot to greet (and for an ambient or a channel line).
const SCAN_MS: (u64, u64) = (15_000, 30_000);
/// Pause between two greetings of a bot.
const HELLO_GAP_MS: (u64, u64) = (8 * 60_000, 20 * 60_000);
/// A bot this close (metres) gets a hello.
const HELLO_RADIUS: f32 = 12.0;
/// Longest character name a whisper may target.
const MAX_TARGET_CHARS: usize = 32;
/// A duel's outcome colours the remark at the end of the duel goal for this long.
const DUEL_MEMORY_MS: u64 = 5 * 60_000;
/// A death is worth a remark at the resurrection when it was this recent.
const DEATH_MEMORY_MS: u64 = 5 * 60_000;
/// After the spawn the counters settle this long before a change is worth a remark (the quest
/// log and the houses arrive after it).
const SETTLE_MS: u64 = 30_000;
/// The bag is counted this often for the loot remark.
const BAG_SAMPLE_MS: u64 = 60_000;
/// Items gained between two counts that make a haul worth a remark.
const LOOT_JUMP: u32 = 12;
/// LLM requests for ambient lines per minute, all bots together.
const LLM_CHATTER_PER_MIN: u32 = 2;
/// Share of the ambient lines the LLM is asked to write when it is on and has room.
const LLM_AMBIENT_SHARE: f32 = 0.5;

// ---- the send path ---------------------------------------------------------------------------

/// The line as it may go on the wire: trimmed, one line, not empty, at most 200 characters,
/// never command-like (see [`cs::plain_chat_text`]) and never a slur, hate, sexual word, insult or
/// threat ([`pb_llm::persona::blocked`]). `None` means "do not send".
pub fn wire_text(text: &str) -> Option<&str> {
    cs::plain_chat_text(text).filter(|t| !pb_llm::persona::blocked(t))
}

/// The packet for a local chat line, or `None` when the line must not be sent (empty, too long,
/// control characters, a leading `/`, or a blocked word). Every local line a bot says goes through
/// here.
pub fn say_packet(text: &str) -> Option<Out> {
    Some(cs::chat(cs::chat_type::SAY, "", wire_text(text)?))
}

/// The packet for a whisper to `to`, or `None` (same rules as [`say_packet`], and `to` must be a
/// plain character name). A leading "GM " of a staff member's display name is removed.
pub fn whisper_packet(to: &str, text: &str) -> Option<Out> {
    let to = whisper_target(to)?;
    Some(cs::chat(cs::chat_type::WHISPER, to, wire_text(text)?))
}

/// The packet for a line on a public channel (shout, trade or group finding, see
/// [`lines::CHANNELS`]), or `None` (same rules as [`say_packet`]; any other channel is refused).
pub fn channel_packet(kind: i16, text: &str) -> Option<Out> {
    if !lines::CHANNELS.contains(&kind) {
        return None;
    }
    Some(cs::chat(kind, "", wire_text(text)?))
}

/// The character name behind a sender name: "GM Nick" is the character "Nick".
pub fn strip_gm(name: &str) -> &str {
    let name = name.trim();
    match name.get(..3) {
        Some(prefix) if prefix.eq_ignore_ascii_case("gm ") => name[3..].trim(),
        _ => name,
    }
}

fn whisper_target(to: &str) -> Option<&str> {
    let to = strip_gm(to);
    let ok = !to.is_empty()
        && to.chars().count() <= MAX_TARGET_CHARS
        && to
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-');
    ok.then_some(to)
}

/// A reply or remark that passes the language filter at `level` (commands, links, AI talk and
/// blocked words refused, swearing above `level` softened) and the wire check, or `None`.
fn finalize(text: &str, level: Profanity) -> Option<String> {
    let t = pb_llm::persona::sanitize_chat(text, level)?;
    wire_text(&t).map(str::to_string)
}

/// [`finalize`], counting what the filter did.
fn filtered(text: &str, level: Profanity, stats: &gate::ChatStats) -> Option<String> {
    match finalize(text, level) {
        Some(t) => {
            if pb_llm::persona::profanity_of(text) > level {
                stats.softened.fetch_add(1, Ordering::Relaxed);
            }
            Some(t)
        }
        None => {
            stats.refused.fetch_add(1, Ordering::Relaxed);
            None
        }
    }
}

// ---- what goals may read ---------------------------------------------------------------------

/// The system and notice lines the bot received at or after `since_ms` (the bot clock, see
/// [`Ctx::now_ms`]), oldest first. A goal that sends a GM command remembers `ctx.now_ms` and reads
/// the server's answer with this on later ticks.
pub fn system_lines(ctx: &Ctx<'_>, since_ms: u64) -> Vec<SysLine> {
    ctx.log.with_slot(|g| g.social.system_since(since_ms))
}

// ---- state and plumbing ----------------------------------------------------------------------

/// What the chat logic needs from the process: who is one of our bots, the LLM, the global gate.
pub trait Services {
    /// Whether character `id` is one of our bots.
    fn is_bot_char(&self, id: u32) -> bool;
    /// Whether a character called `name` is one of our bots.
    fn is_bot_name(&self, name: &str) -> bool;
    /// The LLM add-on, if on.
    fn llm(&self) -> Option<Arc<pb_llm::Llm>>;
    /// The process-wide chat limits and counters.
    fn gate(&self) -> &gate::ChatGate;
    /// A structured memory for a private reply to this character.
    fn companion_memory(&self, _bot: u32, _character: u32) -> Option<String> {
        None
    }
}

impl Services for Hub {
    fn is_bot_char(&self, id: u32) -> bool {
        Hub::is_bot_char(self, id)
    }

    fn is_bot_name(&self, name: &str) -> bool {
        self.chat.names.contains(name)
    }

    fn llm(&self) -> Option<Arc<pb_llm::Llm>> {
        Hub::llm(self).cloned()
    }

    fn gate(&self) -> &gate::ChatGate {
        &self.chat.gate
    }

    fn companion_memory(&self, bot: u32, character: u32) -> Option<String> {
        self.journal.read(bot, |j| {
            let record = j.relationships.get(character)?;
            let activity = record.preference.map_or("unspecified", |p| p.label());
            let mut memory = format!(
                "You have partied together {} times. Your trust in this player is {} out of 100. Their stated preferred activity is {}. Use this to suggest an activity when relevant.",
                record.shared_parties, record.trust, activity
            );
            if record.remembers_bloodlust() {
                memory.push_str(" You saw this player enable Bloodlust while in your party.");
            }
            if record.remembers_theft() {
                memory.push_str(" You saw fresh theft footprints left by this player while in your party.");
            }
            Some(memory)
        })
    }
}

/// Facts about the bot for flavour and for the LLM prompt.
#[derive(Debug, Clone, Copy)]
pub struct Facts<'a> {
    /// Character name.
    pub name: &'a str,
    /// Race name.
    pub race: &'a str,
    /// Class build name.
    pub class: &'a str,
    /// Dominant temperament trait (`quester`, `duelist`, ...).
    pub dominant: &'static str,
    /// Name of the zone the bot is in.
    pub zone: Option<&'a str>,
}

impl Facts<'_> {
    /// The persona the LLM plays (an ordinary, clean-mouthed style).
    pub fn to_persona(&self) -> pb_llm::Persona {
        pb_llm::Persona {
            name: self.name.to_string(),
            race: self.race.to_string(),
            class: self.class.to_string(),
            temperament: temperament_text(self.dominant).to_string(),
            home_zone: self.zone.unwrap_or_default().to_string(),
            style: pb_llm::persona::ChatStyle::default(),
        }
    }

    /// The persona the LLM plays, talking in `style` (the bot's personality, see
    /// [`Style::llm`]).
    pub fn persona_with(&self, style: pb_llm::persona::ChatStyle) -> pb_llm::Persona {
        pb_llm::Persona {
            style,
            ..self.to_persona()
        }
    }
}

/// The facts about the bot of this tick.
pub fn facts<'a>(ctx: &'a Ctx<'_>) -> Facts<'a> {
    facts_of(ctx.persona, &ctx.state.me, ctx.world)
}

/// [`facts`] from the parts, so a caller can keep other fields of the context borrowed.
fn facts_of<'a>(persona: &'a Persona, me: &Me, world: &'a World) -> Facts<'a> {
    Facts {
        name: &persona.name,
        race: race_name(me.race),
        class: &persona.archetype.name,
        dominant: persona.temper.dominant(),
        zone: world
            .zone_key_at(me.pos[0], me.pos[1])
            .and_then(|k| world.zone_name(k)),
    }
}

/// The temperament trait as a few words of character.
pub fn temperament_text(dominant: &str) -> &'static str {
    match dominant {
        "quester" => "cheerful quester, always chasing the next quest",
        "duelist" => "competitive and proud, loves a good duel",
        "trader" => "chatty haggler who loves a good deal",
        "merchant" => "practical trade-pack runner, business first",
        "homebody" => "laid-back homebody who likes a quiet corner",
        _ => "friendly and easygoing",
    }
}

/// Everything one step looks at.
pub struct Inputs<'a> {
    /// Roster id and the party roster, used to resolve a companion's private message.
    pub bot_id: u32,
    pub team: Option<&'a crate::bot::party::TeamLog>,
    /// The bot clock in milliseconds.
    pub now_ms: u64,
    /// Wall-clock instant, for the process-wide limits.
    pub now: Instant,
    /// In the world and alive.
    pub active: bool,
    /// A real player is within the watch radius.
    pub real_near: bool,
    /// Where the bot is.
    pub pos: [f32; 3],
    /// Units in view.
    pub units: &'a FxHashMap<u32, Unit>,
    /// The `[social]` config.
    pub cfg: &'a SocialCfg,
    /// The `[personality]` config.
    pub pcfg: &'a PersonalityCfg,
    /// How the bot talks (its personality).
    pub style: Style,
    /// Who the bot is.
    pub facts: Facts<'a>,
    /// A recent server quote while standing at an auctioneer.
    auction_quote: Option<AuctionRemark<'a>>,
    /// Randomness.
    pub rng: &'a mut fastrand::Rng,
}

#[derive(Clone, Copy)]
struct AuctionRemark<'a> {
    item: &'a str,
    copper: u32,
    seen_ms: u64,
}

fn auction_remark<'a>(ctx: &Ctx<'a>, last_spoken: u64) -> Option<AuctionRemark<'a>> {
    if !ctx.cfg.behavior.settlement_visits || ctx.state.place.in_instance() || !ctx.real_player_near
    {
        return None;
    }
    let market = ctx.state.market.get()?;
    let observed = market
        .auction
        .prices()
        .iter()
        .filter_map(|entry| {
            let quote = entry.quote?;
            if entry.at_ms <= last_spoken || ctx.now_ms.saturating_sub(entry.at_ms) > 3 * 60_000 {
                return None;
            }
            Some(AuctionRemark {
                item: ctx.world.item_name(entry.template)?,
                copper: quote.lowest,
                seen_ms: entry.at_ms,
            })
        })
        .max_by_key(|quote| quote.seen_ms)?;
    let [x, y, z] = ctx.pos();
    ctx.world
        .nearest_auctioneer(aa_world::Vec3::new(x, y, z), 15.0, |_| false)?;
    Some(observed)
}

/// A line to put on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Local chat.
    Say(String),
    /// A whisper to the named character.
    Whisper {
        /// Character name.
        to: String,
        /// The line.
        text: String,
    },
    /// A line on a public channel.
    Channel {
        /// The chat type, one of [`lines::CHANNELS`].
        kind: i16,
        /// The line.
        text: String,
    },
}

/// Something worth a remark. The brain notes the ends of duels and trades; the chat notices the
/// rest by itself ([`Social::observe`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatEvent {
    /// A duel ended; the partner's name if known.
    DuelDone(Option<Box<str>>),
    /// A trade was completed.
    TradeDone(Option<Box<str>>),
    /// The bot reached this level.
    LevelUp(u8),
    /// The bot was resurrected after a death to a monster (or a fall).
    Died,
    /// The bot was resurrected after a character killed it.
    Ganked,
    /// A quest was turned in.
    QuestDone,
    /// A trade pack was sold.
    PackSold,
    /// A crop was harvested.
    Harvest,
    /// A house was finished.
    HouseDone,
    /// The bag filled up with loot.
    Loot,
}

impl ChatEvent {
    /// Which event wins when two come together (a quest turned in that gives a level).
    fn rank(&self) -> u8 {
        match self {
            ChatEvent::LevelUp(_) => 9,
            ChatEvent::HouseDone => 8,
            ChatEvent::Ganked => 7,
            ChatEvent::Died => 6,
            ChatEvent::PackSold => 5,
            ChatEvent::DuelDone(_) => 4,
            ChatEvent::TradeDone(_) => 3,
            ChatEvent::Harvest => 2,
            ChatEvent::QuestDone => 1,
            ChatEvent::Loot => 0,
        }
    }

    /// The chance of a remark before the chattiness scales it ([`Style::remark_chance`]).
    fn base_chance(&self) -> f32 {
        match self {
            ChatEvent::LevelUp(_) | ChatEvent::HouseDone => 0.9,
            ChatEvent::DuelDone(_) | ChatEvent::TradeDone(_) | ChatEvent::Ganked => 0.7,
            ChatEvent::PackSold => 0.6,
            ChatEvent::Died => 0.5,
            ChatEvent::Harvest => 0.35,
            ChatEvent::Loot => 0.3,
            ChatEvent::QuestDone => 0.25,
        }
    }
}

/// What the chat watches to notice what happened to the bot: a few counters of its state, read
/// on every tick ([`Tally::of`]) and compared by [`Social::observe`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    /// In the world.
    pub spawned: bool,
    /// Dead.
    pub dead: bool,
    /// Level.
    pub level: u8,
    /// Quests completed.
    pub quests_done: u32,
    /// Trade packs sold (this process).
    pub packs_sold: u32,
    /// Houses finished.
    pub houses_done: u8,
    /// Houses still being built.
    pub houses_building: u8,
    /// Crops harvested (this session).
    pub harvested: u32,
    /// Deaths to a character (this process).
    pub pvp_deaths: u32,
    /// The last duel end the server reported and the bot clock it arrived at.
    pub duel: Option<(Outcome, u64)>,
    /// Items in the bag, counted once a minute (`None` between two counts).
    pub bag_units: Option<u32>,
}

impl Tally {
    /// What the bot's state says this tick; the bag is counted only when `count_bag`.
    pub fn of(ctx: &Ctx<'_>, count_bag: bool) -> Tally {
        let st = &*ctx.state;
        Tally {
            spawned: st.me.spawned,
            dead: st.me.dead,
            level: st.me.level,
            quests_done: st.quests.completed.len() as u32,
            packs_sold: st.econ.packs_sold,
            houses_done: st.houses.iter().filter(|h| h.build_left <= 0).count() as u8,
            houses_building: st.houses.iter().filter(|h| h.build_left > 0).count() as u8,
            harvested: st.market.farm().map_or(0, |f| f.harvested),
            pvp_deaths: st.pvp.as_ref().map_or(0, |p| p.deaths),
            duel: ctx.combat.duel.peek_end().map(|e| (e.outcome, e.at_ms)),
            bag_units: count_bag.then(|| {
                st.bag
                    .values()
                    .map(|i| u32::try_from(i.count).unwrap_or(0))
                    .sum()
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sent {
    Llm,
    Canned,
    Chatter,
    Ambient,
    LlmAmbient,
    Channel,
}

/// A line waiting for its moment to be sent.
#[derive(Debug)]
struct Due {
    at_ms: u64,
    action: Action,
    how: Sent,
    /// The conversation partner (for the memory), empty for chatter.
    who: Box<str>,
    /// The key of the template, for the hub's memory of what was said lately (0: none).
    told: u16,
    /// A joke or a quip.
    joke: bool,
}

/// What an LLM request is for.
#[derive(Debug)]
enum Ask {
    /// A reply to a player.
    Reply(Plan),
    /// An ambient line; the canned one is the fallback.
    Ambient(Box<Due>),
}

/// A line being asked of the LLM.
#[derive(Debug)]
struct Pending {
    rx: oneshot::Receiver<LlmReply>,
    ask: Ask,
    since_ms: u64,
}

/// Who to answer, where, and what they said.
#[derive(Debug, Clone)]
struct Plan {
    who: String,
    character: Option<u32>,
    whisper: bool,
    addressed: bool,
    text: String,
    intent: Intent,
}

/// The chat state of one bot. See the module docs.
#[derive(Debug, Default)]
pub struct Social {
    next_reply_ms: u64,
    next_chatter_ms: u64,
    next_scan_ms: u64,
    next_hello_ms: u64,
    next_ambient_ms: u64,
    last_auction_remark_ms: u64,
    next_channel_ms: u64,
    next_bag_ms: u64,
    llm_off_until_ms: u64,
    burst: gate::RateWindow<BURST>,
    /// Unasked lines of the last [`UNASKED_SPAN_MS`].
    unasked: gate::RateWindow<UNASKED_MAX>,
    recent: Recent,
    /// The ambient, joke and channel templates used lately.
    jokes: Recent<8>,
    pending: Option<Pending>,
    due: Option<Due>,
    event: Option<(ChatEvent, u64)>,
    mem: Memory,
    /// The counters of the last tick.
    seen: Tally,
    /// When the counters were first read after the spawn (0: not yet).
    primed_ms: u64,
    /// The death waiting for the resurrection: by a character, and when.
    died: Option<(bool, u64)>,
    /// The outcome of the last duel and when it was seen.
    last_duel: Option<(Outcome, u64)>,
    /// The bag count of the last sample.
    bag_mark: Option<u32>,
}

fn rand_range(rng: &mut fastrand::Rng, (lo, hi): (u64, u64)) -> u64 {
    if hi <= lo {
        lo
    } else {
        lo + rng.u64(0..hi - lo)
    }
}

/// `mean` times a random factor in `lo..hi`.
fn spread(rng: &mut fastrand::Rng, mean: u64, lo: f32, hi: f32) -> u64 {
    (mean as f32 * (lo + (hi - lo) * rng.f32())) as u64
}

impl Social {
    /// A bot that just started: the first look for a bot to greet comes a little later.
    pub fn new(rng: &mut fastrand::Rng) -> Social {
        Social {
            next_scan_ms: rand_range(rng, SCAN_MS),
            ..Social::default()
        }
    }

    /// Something happened that the bot may remark on. A fresh event of a higher rank is not
    /// replaced by a lesser one.
    pub fn note(&mut self, event: ChatEvent, now_ms: u64) {
        if let Some((old, at)) = &self.event {
            if now_ms.saturating_sub(*at) <= EVENT_TTL_MS && old.rank() > event.rank() {
                return;
            }
        }
        self.event = Some((event, now_ms));
    }

    /// Whether a reply is on its way (asked of the LLM or waiting to be sent).
    pub fn busy(&self) -> bool {
        self.pending.is_some() || self.due.is_some()
    }

    fn has_work(&self, now_ms: u64) -> bool {
        self.busy() || self.event.is_some() || now_ms >= self.next_scan_ms
    }

    /// Notices what happened since the last tick (`t` is what the state says now) and notes it
    /// for a remark: a level up, a death (told at the resurrection), a quest, a pack, a harvest, a
    /// house, a haul of loot; and remembers a duel's outcome for the remark at the duel's end.
    /// Counters that jump by more than two are a reload (a reconnect), not an achievement.
    pub fn observe(&mut self, t: &Tally, now_ms: u64) {
        let prev = std::mem::replace(&mut self.seen, *t);
        if !t.spawned {
            // (a reconnect: what was waiting for the old session is forgotten)
            self.primed_ms = 0;
            self.died = None;
            return;
        }
        if self.primed_ms == 0 {
            self.primed_ms = now_ms.max(1);
            self.bag_mark = t.bag_units;
            return;
        }
        if t.duel != prev.duel {
            if let Some((outcome, _)) = t.duel {
                self.last_duel = Some((outcome, now_ms));
            }
        }
        if t.dead && !prev.dead {
            self.died = Some((t.pvp_deaths > prev.pvp_deaths, now_ms));
        }
        if !t.dead && prev.dead {
            if let Some((ganked, at)) = self.died.take() {
                if now_ms.saturating_sub(at) <= DEATH_MEMORY_MS {
                    let ev = if ganked {
                        ChatEvent::Ganked
                    } else {
                        ChatEvent::Died
                    };
                    self.note(ev, now_ms);
                }
            }
        }
        if t.dead {
            return;
        }
        if prev.level > 0 && t.level > prev.level && t.level - prev.level <= 3 {
            self.note(ChatEvent::LevelUp(t.level), now_ms);
        }
        if let Some(units) = t.bag_units {
            let haul = self.bag_mark.is_some_and(|m| units >= m + LOOT_JUMP);
            self.bag_mark = Some(units);
            if haul && now_ms.saturating_sub(self.primed_ms) >= SETTLE_MS {
                self.note(ChatEvent::Loot, now_ms);
            }
        }
        if now_ms.saturating_sub(self.primed_ms) < SETTLE_MS {
            return;
        }
        let small = |a: u32, b: u32| b > a && b - a <= 2;
        if small(prev.quests_done, t.quests_done) {
            self.note(ChatEvent::QuestDone, now_ms);
        }
        if small(prev.packs_sold, t.packs_sold) {
            self.note(ChatEvent::PackSold, now_ms);
        }
        if small(prev.harvested, t.harvested) {
            self.note(ChatEvent::Harvest, now_ms);
        }
        if t.houses_done > prev.houses_done && t.houses_building < prev.houses_building {
            self.note(ChatEvent::HouseDone, now_ms);
        }
    }

    /// One brain tick: notices what happened, files the inbox, answers, chatters, sends.
    pub fn tick(&mut self, ctx: &mut Ctx<'_>) {
        let count_bag = ctx.now_ms >= self.next_bag_ms;
        if count_bag {
            self.next_bag_ms = ctx.now_ms + BAG_SAMPLE_MS;
        }
        let tally = Tally::of(ctx, count_bag);
        self.observe(&tally, ctx.now_ms);
        let mut heard: SmallVec<[ChatIn; 2]> = SmallVec::new();
        route_inbox(ctx, &mut heard);
        if heard.is_empty() && !self.has_work(ctx.now_ms) {
            return;
        }
        let facts = facts_of(ctx.persona, &ctx.state.me, ctx.world);
        let me = &ctx.state.me;
        let auction_quote = (ctx.now_ms >= self.next_ambient_ms)
            .then(|| auction_remark(ctx, self.last_auction_remark_ms))
            .flatten();
        let mut inp = Inputs {
            bot_id: ctx.persona.id,
            team: ctx.state.team(),
            now_ms: ctx.now_ms,
            now: ctx.now.into_std(),
            active: me.spawned && !me.dead,
            real_near: ctx.real_player_near,
            pos: me.pos,
            units: &ctx.state.units,
            cfg: &ctx.cfg.social,
            pcfg: &ctx.cfg.personality,
            style: Style::new(&ctx.persona.profile, &ctx.cfg.personality),
            facts,
            auction_quote,
            rng: &mut *ctx.rng,
        };
        let mut out: SmallVec<[Action; 2]> = SmallVec::new();
        self.step(&mut inp, ctx.hub, heard, &mut out);
        for action in out {
            send(ctx, action);
        }
    }

    /// The core: polls the LLM, sends what is due, considers what was heard, maybe chatters.
    pub fn step(
        &mut self,
        inp: &mut Inputs<'_>,
        svc: &dyn Services,
        heard: SmallVec<[ChatIn; 2]>,
        out: &mut SmallVec<[Action; 2]>,
    ) {
        self.poll_pending(inp, svc);
        self.flush_due(inp, svc, out);
        if !inp.cfg.enabled || !inp.active {
            // Nothing is said while dead or switched off; what was in flight is forgotten.
            self.pending = None;
            self.due = None;
            self.event = None;
            return;
        }
        for line in &heard {
            if self.busy() {
                break;
            }
            self.consider(inp, svc, line);
        }
        self.chatter(inp, svc);
    }

    // ---- answers in flight -------------------------------------------------------------

    fn poll_pending(&mut self, inp: &mut Inputs<'_>, svc: &dyn Services) {
        let Some(p) = self.pending.as_mut() else {
            return;
        };
        let result = match p.rx.try_recv() {
            Ok(r) => r,
            Err(TryRecvError::Empty) => {
                if inp.now_ms.saturating_sub(p.since_ms) > PENDING_TTL_MS {
                    self.pending = None;
                }
                return;
            }
            Err(TryRecvError::Closed) => LlmReply::Failed,
        };
        let Some(p) = self.pending.take() else {
            return;
        };
        let written = match &result {
            LlmReply::Text(text) => filtered(text, inp.style.level(), &svc.gate().stats),
            LlmReply::Busy | LlmReply::Failed => {
                self.llm_off_until_ms = inp.now_ms + LLM_BACKOFF_MS;
                None
            }
        };
        let at_ms = inp.now_ms + rand_range(inp.rng, LLM_DELAY_MS);
        match (p.ask, written) {
            (Ask::Reply(plan), Some(text)) => {
                self.due = Some(Due {
                    at_ms,
                    action: reply_action(&plan, text),
                    how: Sent::Llm,
                    who: plan.who.clone().into_boxed_str(),
                    told: 0,
                    joke: false,
                });
            }
            (Ask::Reply(plan), None) => {
                if plan.addressed {
                    self.schedule_canned(inp, svc, &plan);
                }
            }
            (Ask::Ambient(_), Some(text)) => {
                self.due = Some(Due {
                    at_ms,
                    action: Action::Say(text),
                    how: Sent::LlmAmbient,
                    who: "".into(),
                    told: 0,
                    joke: false,
                });
            }
            (Ask::Ambient(mut fallback), None) => {
                fallback.at_ms = at_ms;
                self.due = Some(*fallback);
            }
        }
    }

    fn flush_due(
        &mut self,
        inp: &mut Inputs<'_>,
        svc: &dyn Services,
        out: &mut SmallVec<[Action; 2]>,
    ) {
        let ready = self
            .due
            .as_ref()
            .is_some_and(|d| inp.now_ms >= d.at_ms && inp.active);
        if !ready {
            return;
        }
        let Some(mut due) = self.due.take() else {
            return;
        };
        let stats = &svc.gate().stats;
        // The last look before the wire: the language filter at the bot's level and the wire
        // check. A line that fails is dropped, not repaired.
        let text = match &mut due.action {
            Action::Say(t) | Action::Whisper { text: t, .. } | Action::Channel { text: t, .. } => t,
        };
        let Some(clean) = finalize(text, inp.style.level()) else {
            stats.refused.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if clean != *text {
            *text = clean;
        }
        let text = text.clone();
        match due.how {
            Sent::Llm => stats.llm_replies.fetch_add(1, Ordering::Relaxed),
            Sent::Canned => stats.canned_replies.fetch_add(1, Ordering::Relaxed),
            Sent::Chatter => stats.chatter.fetch_add(1, Ordering::Relaxed),
            Sent::Ambient => stats.ambient.fetch_add(1, Ordering::Relaxed),
            Sent::LlmAmbient => {
                stats.llm_ambient.fetch_add(1, Ordering::Relaxed);
                stats.ambient.fetch_add(1, Ordering::Relaxed)
            }
            Sent::Channel => stats.channel.fetch_add(1, Ordering::Relaxed),
        };
        if due.joke {
            stats.jokes.fetch_add(1, Ordering::Relaxed);
        }
        if due.told != 0 {
            svc.gate().note_told(due.told);
        }
        if matches!(due.how, Sent::Llm | Sent::Canned) {
            self.mem.said(&due.who, &text, inp.now_ms);
        }
        out.push(due.action);
    }

    // ---- hearing -----------------------------------------------------------------------

    /// Decides whether to answer `line` and starts the reply.
    fn consider(&mut self, inp: &mut Inputs<'_>, svc: &dyn Services, line: &ChatIn) {
        let whisper = line.kind == -3;
        if !whisper && line.kind != 0 {
            return;
        }
        let who = strip_gm(&line.name);
        if who.is_empty() || who.eq_ignore_ascii_case(inp.facts.name) {
            return;
        }
        let unit = inp.units.get(&line.sender_obj);
        let from_bot = match unit {
            Some(u) if u.kind == UnitKind::Character => svc.is_bot_char(u.id),
            // an NPC (or anything else that is not a player) talking
            Some(_) => return,
            None => svc.is_bot_name(who),
        };
        if from_bot && !inp.cfg.reply_to_bots {
            return;
        }
        let mentioned = mentions(&line.text, inp.facts.name);
        let addressed = whisper || mentioned;
        if !addressed {
            let near = unit.is_some_and(|u| u.dist2d(inp.pos) <= inp.cfg.reply_radius_m);
            if !near || inp.rng.f32() >= inp.style.reply_chance(inp.cfg.reply_chance) {
                return;
            }
        }
        // The bot's own limits first, so a bot that cannot answer does not claim the line.
        let now_ms = inp.now_ms;
        if now_ms < self.next_reply_ms
            || !self.burst.has_room(now_ms, BURST_SPAN_MS)
            || !self.mem.reply_allowed(who, now_ms)
        {
            return;
        }
        let gate = svc.gate();
        let key = line_key(who, &line.text);
        if addressed {
            gate.mark_claimed(key, inp.now);
        } else if !gate.claim_line(key, inp.now) {
            return;
        }
        if !gate.take_reply(inp.now, inp.cfg.replies_per_min) {
            return;
        }
        let plan = Plan {
            who: who.to_string(),
            character: unit.map(|u| u.id).or_else(|| {
                inp.team?
                    .remote
                    .iter()
                    .find(|r| r.name.eq_ignore_ascii_case(who))
                    .map(|r| r.char_id)
            }),
            whisper,
            addressed,
            text: line.text.to_string(),
            intent: canned::classify(&line.text),
        };
        self.start_reply(inp, svc, plan);
    }

    fn start_reply(&mut self, inp: &mut Inputs<'_>, svc: &dyn Services, plan: Plan) {
        let now_ms = inp.now_ms;
        self.next_reply_ms = now_ms + rand_range(inp.rng, inp.style.reply_gap(REPLY_GAP_MS));
        let _ = self.burst.try_take(now_ms, BURST_SPAN_MS);
        self.mem.heard(&plan.who, &plan.text, now_ms);
        let llm = svc.llm().filter(|_| now_ms >= self.llm_off_until_ms);
        match llm {
            Some(llm) => {
                // The newest line is the last one remembered: the player's.
                let messages = self.mem.messages(&plan.who, now_ms);
                let mut request = reply::build_request(
                    &inp.facts.persona_with(inp.style.llm()),
                    &plan.who,
                    plan.whisper,
                    messages,
                );
                if plan.whisper {
                    if let Some(memory) = plan
                        .character
                        .and_then(|id| svc.companion_memory(inp.bot_id, id))
                    {
                        request.system.push('\n');
                        request.system.push_str(&memory);
                    }
                }
                let rx = reply::spawn(llm, request);
                self.pending = Some(Pending {
                    rx,
                    ask: Ask::Reply(plan),
                    since_ms: now_ms,
                });
            }
            None => self.schedule_canned(inp, svc, &plan),
        }
    }

    fn schedule_canned(&mut self, inp: &mut Inputs<'_>, svc: &dyn Services, plan: &Plan) {
        let who = reply::flat_name(&plan.who);
        let who = if who == "someone" { String::new() } else { who };
        let tongue = inp.style.tongue();
        // small talk may get a coarse answer from a crude bot or a quip from a jokey one
        let mut joke = false;
        let mut flavoured = None;
        if plan.intent == Intent::Other && inp.rng.f32() < inp.style.crude_reply_chance() {
            flavoured = humor::crude_reply(tongue, &mut self.recent, inp.rng);
        }
        if flavoured.is_none()
            && matches!(plan.intent, Intent::Other | Intent::Question)
            && inp.rng.f32() < inp.style.quip_chance()
        {
            flavoured = humor::quip(tongue, &mut self.recent, inp.rng);
            joke = flavoured.is_some();
        }
        let line = flavoured.unwrap_or_else(|| {
            canned::reply(
                plan.intent,
                inp.facts.dominant,
                &who,
                &mut self.recent,
                inp.rng,
            )
        });
        let Some(text) = filtered(&line, inp.style.level(), &svc.gate().stats) else {
            return;
        };
        self.due = Some(Due {
            at_ms: inp.now_ms + rand_range(inp.rng, CANNED_DELAY_MS),
            action: reply_action(plan, text),
            how: Sent::Canned,
            who: plan.who.clone().into_boxed_str(),
            told: 0,
            joke,
        });
    }

    // ---- unsolicited lines -------------------------------------------------------------

    fn chatter(&mut self, inp: &mut Inputs<'_>, svc: &dyn Services) {
        let cfg = inp.cfg;
        if !cfg.bot_chatter {
            self.event = None;
            return;
        }
        if cfg.chatter_needs_player && !inp.real_near {
            // Nobody to hear it: the moment passes.
            self.event = None;
            return;
        }
        if self.busy() || inp.now_ms < self.next_chatter_ms {
            return;
        }
        if let Some((event, at_ms)) = self.event.take() {
            let fresh = inp.now_ms.saturating_sub(at_ms) <= EVENT_TTL_MS;
            if !fresh || inp.rng.f32() >= inp.style.remark_chance(event.base_chance()) {
                return;
            }
            let (kind, who, level) = match &event {
                ChatEvent::DuelDone(w) => (self.duel_kind(inp.now_ms), w.as_deref(), 0),
                ChatEvent::TradeDone(w) => (Chatter::TradeDone, w.as_deref(), 0),
                ChatEvent::LevelUp(lv) => (Chatter::LevelUp, None, *lv),
                ChatEvent::Died => (Chatter::Died, None, 0),
                ChatEvent::Ganked => (Chatter::Ganked, None, 0),
                ChatEvent::QuestDone => (Chatter::QuestDone, None, 0),
                ChatEvent::PackSold => (Chatter::PackSold, None, 0),
                ChatEvent::Harvest => (Chatter::Harvest, None, 0),
                ChatEvent::HouseDone => (Chatter::HouseDone, None, 0),
                ChatEvent::Loot => (Chatter::Loot, None, 0),
            };
            let who = who.map(reply::flat_name).unwrap_or_default();
            self.remark(inp, svc, kind, &who, level);
            return;
        }
        if inp.now_ms < self.next_scan_ms {
            return;
        }
        self.next_scan_ms = inp.now_ms + rand_range(inp.rng, SCAN_MS);
        if self.ambient(inp, svc) || self.channel_line(inp, svc) {
            return;
        }
        if inp.now_ms < self.next_hello_ms || inp.rng.f32() >= inp.style.hello_chance() {
            return;
        }
        let near = inp
            .units
            .values()
            .filter(|u| u.kind == UnitKind::Character && svc.is_bot_char(u.id))
            .filter(|u| u.dist2d(inp.pos) <= HELLO_RADIUS)
            .filter_map(|u| u.name.as_deref())
            .next()
            .map(reply::flat_name);
        if let Some(who) = near {
            self.next_hello_ms = inp.now_ms + rand_range(inp.rng, HELLO_GAP_MS);
            self.remark(inp, svc, Chatter::Hello, &who, 0);
        }
    }

    /// The remark at the end of a duel: won or lost while the outcome is fresh, else a plain gg.
    fn duel_kind(&self, now_ms: u64) -> Chatter {
        match self.last_duel {
            Some((Outcome::Won, at)) if now_ms.saturating_sub(at) <= DUEL_MEMORY_MS => {
                Chatter::DuelWon
            }
            Some((Outcome::Lost, at)) if now_ms.saturating_sub(at) <= DUEL_MEMORY_MS => {
                Chatter::DuelLost
            }
            _ => Chatter::DuelDone,
        }
    }

    /// Whether the bot's own cap on unasked lines has room.
    fn unasked_room(&self, inp: &Inputs<'_>) -> bool {
        self.unasked.count(inp.now_ms, UNASKED_SPAN_MS) < inp.style.unasked_limit()
    }

    /// Records an unasked line: it counts against the bot's cap and starts its gap.
    fn spoke_unasked(&mut self, inp: &mut Inputs<'_>) {
        let _ = self.unasked.try_take(inp.now_ms, UNASKED_SPAN_MS);
        let gap = inp.style.chatter_gap_ms(inp.cfg.chatter_gap_secs);
        self.next_chatter_ms = inp.now_ms + gap + rand_range(inp.rng, (0, gap / 2 + 1));
    }

    /// A remark of `kind` (a bot-to-bot line or a reaction to what happened).
    fn remark(
        &mut self,
        inp: &mut Inputs<'_>,
        svc: &dyn Services,
        kind: Chatter,
        who: &str,
        level: u8,
    ) {
        if !self.unasked_room(inp) {
            return;
        }
        if !svc.gate().take_chatter(inp.now, inp.cfg.chatter_per_min) {
            return;
        }
        let who = if who == "someone" { "" } else { who };
        let slots = Slots {
            name: who,
            level,
            zone: inp.facts.zone,
        };
        let line = lines::remark(kind, &slots, inp.style.tongue(), &mut self.recent, inp.rng);
        let Some(text) = filtered(&line, inp.style.level(), &svc.gate().stats) else {
            return;
        };
        self.spoke_unasked(inp);
        self.due = Some(Due {
            at_ms: inp.now_ms + rand_range(inp.rng, CANNED_DELAY_MS),
            action: Action::Say(text),
            how: Sent::Chatter,
            who: "".into(),
            told: 0,
            joke: false,
        });
    }

    /// Maybe an ambient line (small talk or a joke) for the real player nearby. Returns whether
    /// one was scheduled.
    fn ambient(&mut self, inp: &mut Inputs<'_>, svc: &dyn Services) -> bool {
        let per_min = inp.pcfg.ambient_per_min;
        if per_min == 0 {
            return false;
        }
        let now = inp.now_ms;
        let gap = inp.style.ambient_gap_ms();
        if self.next_ambient_ms == 0 {
            // The first chance comes after a part of the gap, so the bots that meet a player
            // together do not all speak at once.
            self.next_ambient_ms = now + spread(inp.rng, gap, 0.3, 1.0);
            return false;
        }
        if now < self.next_ambient_ms {
            return false;
        }
        self.next_ambient_ms = now + spread(inp.rng, gap, 0.6, 1.4);
        let gate = svc.gate();
        if !self.unasked_room(inp) || !gate.take_ambient(inp.now, per_min) {
            return false;
        }
        if let Some(quote) = inp
            .auction_quote
            .filter(|q| q.seen_ms > self.last_auction_remark_ms)
        {
            let line = format!(
                "I just saw {} at the auction house for as little as {} each.",
                quote.item,
                super::trade_run::coins(i64::from(quote.copper))
            );
            if let Some(text) = filtered(&line, inp.style.level(), &gate.stats) {
                self.last_auction_remark_ms = quote.seen_ms;
                self.spoke_unasked(inp);
                self.due = Some(Due {
                    at_ms: now + rand_range(inp.rng, CANNED_DELAY_MS),
                    action: Action::Say(text),
                    how: Sent::Ambient,
                    who: "".into(),
                    told: 0,
                    joke: false,
                });
                return true;
            }
        }
        let told = gate.told();
        let tongue = inp.style.tongue();
        let slots = Slots {
            zone: inp.facts.zone,
            ..Slots::default()
        };
        let picked = if inp.rng.f32() < inp.style.joke_chance() {
            humor::joke(&slots, tongue, &mut self.jokes, &told, inp.rng)
                .map(|(k, t, topic)| (k, t, topic.hint(), true))
        } else {
            lines::ambient(&slots, tongue, &mut self.jokes, &told, inp.rng)
                .map(|(k, t)| (k, t, lines::AMBIENT_HINT, false))
        };
        let Some((key, line, hint, joke)) = picked else {
            return false;
        };
        let Some(text) = filtered(&line, inp.style.level(), &gate.stats) else {
            return false;
        };
        self.spoke_unasked(inp);
        let due = Due {
            at_ms: now + rand_range(inp.rng, CANNED_DELAY_MS),
            action: Action::Say(text),
            how: Sent::Ambient,
            who: "".into(),
            told: key,
            joke,
        };
        match self.ask_llm_ambient(inp, svc, hint) {
            Some(rx) => {
                self.pending = Some(Pending {
                    rx,
                    ask: Ask::Ambient(Box::new(due)),
                    since_ms: now,
                });
            }
            None => self.due = Some(due),
        }
        true
    }

    /// Asks the LLM for an ambient line about `hint`, now and then and within its own budget.
    fn ask_llm_ambient(
        &mut self,
        inp: &mut Inputs<'_>,
        svc: &dyn Services,
        hint: &str,
    ) -> Option<oneshot::Receiver<LlmReply>> {
        if !inp.pcfg.llm_chatter
            || inp.now_ms < self.llm_off_until_ms
            || inp.rng.f32() >= LLM_AMBIENT_SHARE
        {
            return None;
        }
        let llm = svc.llm()?;
        if !svc.gate().take_llm_chatter(inp.now, LLM_CHATTER_PER_MIN) {
            return None;
        }
        let request = reply::build_remark_request(&inp.facts.persona_with(inp.style.llm()), hint);
        Some(reply::spawn(llm, request))
    }

    /// Maybe a line on one of the public channels (a chatty bot only). Returns whether one was
    /// scheduled.
    fn channel_line(&mut self, inp: &mut Inputs<'_>, svc: &dyn Services) -> bool {
        let per_hour = inp.pcfg.channel_lines_per_hour;
        let Some(gap) = inp.style.channel_gap_ms() else {
            return false;
        };
        if per_hour == 0 {
            return false;
        }
        let now = inp.now_ms;
        if self.next_channel_ms == 0 {
            self.next_channel_ms = now + spread(inp.rng, gap, 0.3, 1.0);
            return false;
        }
        if now < self.next_channel_ms {
            return false;
        }
        self.next_channel_ms = now + spread(inp.rng, gap, 0.6, 1.4);
        let gate = svc.gate();
        if !self.unasked_room(inp) || !gate.take_channel(inp.now, per_hour) {
            return false;
        }
        let told = gate.told();
        let slots = Slots {
            zone: inp.facts.zone,
            ..Slots::default()
        };
        let Some((kind, key, line)) =
            lines::channel(&slots, inp.style.tongue(), &mut self.jokes, &told, inp.rng)
        else {
            return false;
        };
        let Some(text) = filtered(&line, inp.style.level(), &gate.stats) else {
            return false;
        };
        self.spoke_unasked(inp);
        self.due = Some(Due {
            at_ms: now + rand_range(inp.rng, CANNED_DELAY_MS),
            action: Action::Channel { kind, text },
            how: Sent::Channel,
            who: "".into(),
            told: key,
            joke: false,
        });
        true
    }
}

/// The action that answers `plan` with `text`: a whisper to the sender or a local line.
fn reply_action(plan: &Plan, text: String) -> Action {
    if plan.whisper {
        Action::Whisper {
            to: plan.who.clone(),
            text,
        }
    } else {
        Action::Say(text)
    }
}

/// Whether `text` contains the bot's name as a word.
pub fn mentions(text: &str, name: &str) -> bool {
    let name = name.to_lowercase();
    if name.is_empty() {
        return false;
    }
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| w == name)
}

fn line_key(who: &str, text: &str) -> u64 {
    let mut h = FxHasher::default();
    who.to_lowercase().hash(&mut h);
    text.to_lowercase().hash(&mut h);
    h.finish()
}

/// Files the inbox: chat lines go to `heard` (and the detail panel), system lines to the ring
/// that goals read, the rest is dropped; shouts, trade and group-finding lines and whispers are
/// also handed to the market ([`crate::bot::market::forward_chat`]). Empties the inbox.
fn route_inbox(ctx: &mut Ctx<'_>, heard: &mut SmallVec<[ChatIn; 2]>) {
    if ctx.state.inbox.is_empty() {
        return;
    }
    let mut lines = std::mem::take(&mut ctx.state.inbox);
    lines.retain(|l| {
        !super::delivery::on_whisper(ctx, l)
            && !super::mediation::on_whisper(ctx, l)
            && !super::warzone::bounty_whisper(ctx, l)
            && !super::work::assignment::on_whisper(ctx, l)
            && !super::work::customer::on_whisper(ctx, l)
    });
    // (the market features hear the shout, trade and group-finding channels and the whispers
    // besides us; what they do with them is no business of the routing below)
    let market = &mut ctx.state.market;
    let (hub, world) = (ctx.hub, ctx.world);
    crate::bot::market::forward_chat(&lines, |kind, sender, text, at_ms| {
        market.on_chat(kind, sender, text, at_ms);
        // (the economy board counts what the world's players ask for and offer)
        hub.pop.hear(hub, world, kind, sender, text);
    });
    // (the guild feature hears the guild chat and the whispers that ask for an invitation)
    let mut claimed = crate::bot::guild::forward_chat(ctx, &lines);
    // (and the party feature hears the commands of the members of the bot's party and the "inv" whispers)
    claimed.extend(crate::bot::companion::forward_chat(ctx, &lines));
    let t = unix_ms();
    ctx.log
        .with_slot(|g| route_lines(lines, &mut g.social, t, heard));
    // a whisper the guild feature answers is not answered a second time
    if !claimed.is_empty() {
        heard.retain(|l| {
            !(l.kind == cs::chat_type::WHISPER
                && claimed
                    .iter()
                    .any(|c| c.eq_ignore_ascii_case(strip_gm(&l.name))))
        });
    }
    // a whisper that belongs to a negotiation of the market is the market's to answer
    if ctx.cfg.market.enabled && ctx.cfg.market.chat_ads {
        let market = &ctx.state.market;
        heard.retain(|l| !(l.kind == -3 && market.claims_whisper(&l.name)));
    }
}

/// The routing itself: chat lines to `heard` and the panel, system lines to the ring, the rest
/// dropped. `t` is the unix time in milliseconds for the panel.
fn route_lines(
    lines: SmallVec<[ChatIn; 2]>,
    social: &mut SlotSocial,
    t: u64,
    heard: &mut SmallVec<[ChatIn; 2]>,
) {
    for l in lines {
        match l.kind {
            -2 | -1 => social.push_system(SysLine {
                at_ms: l.at_ms,
                kind: l.kind,
                text: l.text,
            }),
            0 => {
                social.push_chat(t, &format!("{}: {}", l.name, l.text));
                heard.push(l);
            }
            -3 => {
                social.push_chat(t, &format!("{} (whisper): {}", l.name, l.text));
                heard.push(l);
            }
            _ => {}
        }
    }
}

/// Puts an action on the wire and into the log and the detail panel. The packet builders refuse
/// anything that is not a safe chat line.
fn send(ctx: &mut Ctx<'_>, action: Action) {
    let (packet, note) = match &action {
        Action::Say(text) => (say_packet(text), format!("(me) {text}")),
        Action::Whisper { to, text } => (
            whisper_packet(to, text),
            format!("(me to {}) {text}", strip_gm(to)),
        ),
        Action::Channel { kind, text } => (
            channel_packet(*kind, text),
            format!("(me, {}) {text}", lines::channel_name(*kind)),
        ),
    };
    let Some(packet) = packet else {
        ctx.log.debug("chat: refused to send an unsafe line");
        return;
    };
    ctx.out.push(packet);
    ctx.log.say(format!("chat: {note}"));
    let t = unix_ms();
    ctx.log.with_slot(|g| g.social.push_chat(t, &note));
}
