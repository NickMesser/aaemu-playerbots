//! The coordinator: pairs bots that need a partner (duels, player trade).
//!
//! A goal that needs a partner calls [`CoordHandle::request_partner`]; the coordinator task
//! (one for the whole process) keeps the waiting bots and sends two of them a [`BotCmd::Pair`]
//! with their roles when they fit together. The bot that has waited longer is the `Initiator`.
//!
//! # Who is paired
//!
//! For every kind of request the coordinator looks at all pairs of waiting bots and picks the
//! best one by a score of distance plus level difference. A pair is only allowed when
//!
//! * the two were not paired with each other in the last [`REPAIR_COOLDOWN`] (so the same two
//!   bots do not duel or trade back to back),
//! * they are not further apart than [`NEAR_M`] plus [`NEAR_GROWTH_M`] for every second the
//!   longer-waiting one has been waiting, and
//! * their levels differ by at most [`LEVEL_DIFF`] plus one for every [`LEVEL_GROWTH_SECS`]
//!   seconds of waiting.
//!
//! The limits grow with the waiting time, so a lone bot is eventually paired with anybody; a
//! request that finds nobody within [`REQUEST_TTL`] is dropped. The sweep that expires requests
//! also looks for pairs again, because waiting relaxes the limits.
//!
//! # Parties for dungeons
//!
//! A bot that wants a dungeon sends a [`Wish`] ([`CoordHandle::request_party`]); the coordinator
//! keeps the wishes for [`WISH_TTL`] and, whenever one arrives or the sweep runs, asks
//! [`party::plan`] for a party of bots that fit one dungeon (level, alliance, distance, the
//! dungeons each bot can still enter) and sends every member a [`BotCmd::Party`] with the same
//! [`PartyOrder`]. The sites come from [`Coordinator::with_sites`]; without them wishes are never
//! answered. A formed party is remembered for [`FORMED_TTL`] (see [`Coordinator::formed`]).

use std::sync::Arc;
use std::time::{Duration, Instant};

use smallvec::SmallVec;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::cmd::{BotCmd, PairKind, PairOrder, PairRole};
use super::guild::forming::Wishes as GuildWishes;
use super::guild::plan::GuildWish;
use super::party::{self, PartyOrder, PartyPurpose, Site, Wish};
use super::pvp::squad::{self, Standby};
use crate::hub::Hub;

/// Where the coordinator's orders go: the hub in production, a recorder in tests.
pub trait Dispatch: Send + Sync {
    /// Queues `cmd` for bot `bot`.
    fn send(&self, bot: u32, cmd: BotCmd) -> Result<(), String>;
    /// Character id and name of a bot (0 and empty when unknown).
    fn who(&self, bot: u32) -> (u32, String);
    /// Level of a bot (0 when unknown); level differences are not held against unknown levels.
    fn level(&self, _bot: u32) -> u8 {
        0
    }
    /// What the bots share about guilds (the founding teams reserve their places there).
    fn guild_book(&self) -> Option<&super::guild::GuildBook> {
        None
    }
}

impl Dispatch for Hub {
    fn send(&self, bot: u32, cmd: BotCmd) -> Result<(), String> {
        self.slot(bot)
            .ok_or_else(|| format!("no bot {bot}"))?
            .send(cmd)
    }

    fn who(&self, bot: u32) -> (u32, String) {
        match self.slot(bot) {
            Some(s) => (
                s.char_id.load(std::sync::atomic::Ordering::Relaxed),
                s.with(|g| g.snap.name.clone()),
            ),
            None => (0, String::new()),
        }
    }

    fn level(&self, bot: u32) -> u8 {
        self.slot(bot).map_or(0, |s| s.with(|g| g.snap.level))
    }

    fn guild_book(&self) -> Option<&super::guild::GuildBook> {
        Some(&self.guilds)
    }
}

/// How long a request waits for a partner before it is dropped.
pub const REQUEST_TTL: Duration = Duration::from_secs(120);
/// How long a wish for a dungeon waits for a party before it is dropped.
pub const WISH_TTL: Duration = Duration::from_secs(150);
/// How long a formed party stays in the coordinator's list.
pub const FORMED_TTL: Duration = Duration::from_secs(45 * 60);
/// Most remembered parties.
const FORMED_MAX: usize = 32;
/// Two bots that were paired are not paired with each other again for this long.
pub const REPAIR_COOLDOWN: Duration = Duration::from_secs(90);
/// Distance (metres) within which a fresh request may be paired.
pub const NEAR_M: f32 = 150.0;
/// The distance limit grows by this many metres per second of waiting.
pub const NEAR_GROWTH_M: f32 = 40.0;
/// Level difference allowed for a fresh request.
pub const LEVEL_DIFF: u32 = 4;
/// The level limit grows by one per this many seconds of waiting.
pub const LEVEL_GROWTH_SECS: u64 = 8;
/// A level step weighs this many metres in the score.
const LEVEL_WEIGHT_M: f32 = 25.0;
/// Seconds between two sweeps (expiry and another look for pairs).
const SWEEP_SECS: u64 = 3;
/// The longest `[dungeons] relax_after_secs` that can still form a smaller party: a wish is dropped
/// at [`WISH_TTL`] before the sweep plans, so the wait has to end with a sweep or two to spare.
pub const MAX_RELAX_SECS: u64 = 140;
const _: () = assert!(MAX_RELAX_SECS + 2 * SWEEP_SECS < WISH_TTL.as_secs());
/// Most remembered pairings.
const RECENT_MAX: usize = 64;
/// Capacity of the coordinator's inbox.
const INBOX: usize = 256;

/// A message to the coordinator.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CoordMsg {
    /// Bot `bot` at `at` wants a partner for `kind`.
    Request {
        /// What for.
        kind: PairKind,
        /// The asking bot.
        bot: u32,
        /// Where it stands (x, y).
        at: (f32, f32),
    },
    /// The bot no longer wants a partner or a dungeon (it gave up or was paired by hand).
    Cancel {
        /// The bot.
        bot: u32,
    },
    /// A bot wants to run a dungeon with a party.
    Party(Wish),
    /// A bot wants to sign a charter with others and found a guild.
    Guild(GuildWish),
    /// A bot waits at an ambush post and offers itself to a squad (see [`squad`]).
    Ambush(Standby),
}

/// The cheap, cloneable end bots talk to the coordinator with.
#[derive(Debug, Clone)]
pub struct CoordHandle {
    tx: mpsc::Sender<CoordMsg>,
}

impl CoordHandle {
    /// Asks for a partner. False when the coordinator's inbox is full (ask again later).
    pub fn request_partner(&self, kind: PairKind, bot: u32, at: (f32, f32)) -> bool {
        self.tx
            .try_send(CoordMsg::Request { kind, bot, at })
            .is_ok()
    }

    /// Asks for a party for an instance dungeon. False when the coordinator's inbox is full.
    pub fn request_party(&self, wish: Wish) -> bool {
        self.tx.try_send(CoordMsg::Party(wish)).is_ok()
    }

    /// Asks for a founding team for a guild. False when the coordinator's inbox is full.
    pub fn request_guild(&self, wish: GuildWish) -> bool {
        self.tx.try_send(CoordMsg::Guild(wish)).is_ok()
    }

    /// Offers a bot waiting at an ambush post to a squad. False when the coordinator's inbox is
    /// full (offer again later).
    pub fn standby(&self, offer: Standby) -> bool {
        self.tx.try_send(CoordMsg::Ambush(offer)).is_ok()
    }

    /// Withdraws a request.
    pub fn cancel(&self, bot: u32) {
        let _ = self.tx.try_send(CoordMsg::Cancel { bot });
    }

    /// A handle nobody listens to, for tests and for bots running without a coordinator.
    pub fn detached() -> CoordHandle {
        let (tx, _rx) = mpsc::channel(1);
        CoordHandle { tx }
    }
}

struct Waiting {
    kind: PairKind,
    bot: u32,
    at: (f32, f32),
    since: Instant,
}

/// Two bots that were paired, smaller id first.
struct Recent {
    a: u32,
    b: u32,
    at: Instant,
}

/// A bot that wants a dungeon.
struct Wishing {
    wish: Wish,
    since: Instant,
}

/// A bot waiting at an ambush post.
struct Standing {
    offer: Standby,
    since: Instant,
}

/// A party that was formed.
#[derive(Debug, Clone, PartialEq)]
pub struct Formed {
    /// Coordinator-assigned id.
    pub party_id: u32,
    /// The dungeon (zone group).
    pub zone_group: u32,
    /// The members' bot ids, the leader first.
    pub bots: SmallVec<[u32; party::MAX_PARTY]>,
    /// When it was formed.
    pub at: Instant,
}

/// The coordinator task state.
pub struct Coordinator {
    rx: mpsc::Receiver<CoordMsg>,
    sink: Arc<dyn Dispatch>,
    waiting: Vec<Waiting>,
    recent: Vec<Recent>,
    sites: Vec<Site>,
    wishing: Vec<Wishing>,
    standing: Vec<Standing>,
    formed: Vec<Formed>,
    next_party: u32,
    /// Seconds a dungeon wish waits for a full party before a smaller one is formed.
    relax_after_secs: u64,
    /// The wishes for a founding team (set by [`Coordinator::with_guilds`]).
    guilds: Option<GuildWishes>,
}

/// Creates the coordinator and the handle bots use.
pub fn channel(sink: Arc<dyn Dispatch>) -> (CoordHandle, Coordinator) {
    let (tx, rx) = mpsc::channel(INBOX);
    (
        CoordHandle { tx },
        Coordinator {
            rx,
            sink,
            waiting: Vec::new(),
            recent: Vec::new(),
            sites: Vec::new(),
            wishing: Vec::new(),
            standing: Vec::new(),
            formed: Vec::new(),
            next_party: 1,
            relax_after_secs: party::RELAX_AFTER_SECS,
            guilds: None,
        },
    )
}

impl Coordinator {
    /// Gives the coordinator the dungeons it may send parties to ([`party::sites`]).
    pub fn with_sites(mut self, sites: Vec<Site>) -> Coordinator {
        self.sites = sites;
        self
    }

    /// Sets how long a dungeon wish waits for a full party before a smaller one is formed
    /// (`[dungeons] relax_after_secs`).
    pub fn with_party_relax(mut self, secs: u64) -> Coordinator {
        self.relax_after_secs = secs;
        self
    }

    /// Lets the coordinator form founding teams of `party_size` bots for guilds, while the bots
    /// run fewer than `max_guilds` of them (see [`super::guild::forming`]).
    pub fn with_guilds(mut self, party_size: u8, max_guilds: u32) -> Coordinator {
        self.guilds = Some(GuildWishes::new(party_size, max_guilds));
        self
    }

    /// Number of bots waiting for a founding team.
    pub fn guild_wishes(&self) -> usize {
        self.guilds.as_ref().map_or(0, GuildWishes::len)
    }

    /// The parties formed lately, oldest first.
    pub fn formed(&self) -> &[Formed] {
        &self.formed
    }

    /// Number of bots waiting for a party.
    pub fn wishing(&self) -> usize {
        self.wishing.len()
    }

    /// Runs until `cancel` fires.
    pub async fn run(mut self, cancel: CancellationToken) {
        let mut sweep = tokio::time::interval(Duration::from_secs(SWEEP_SECS));
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                msg = self.rx.recv() => match msg {
                    Some(m) => self.handle(m, Instant::now()),
                    None => return,
                },
                _ = sweep.tick() => self.expire(Instant::now()),
            }
        }
    }

    /// Handles one message; pairs bots when two fit together.
    pub fn handle(&mut self, msg: CoordMsg, now: Instant) {
        match msg {
            CoordMsg::Cancel { bot } => {
                self.waiting.retain(|w| w.bot != bot);
                self.wishing.retain(|w| w.wish.bot != bot);
                if let Some(g) = self.guilds.as_mut() {
                    g.cancel(bot);
                }
                self.standing.retain(|s| s.offer.bot != bot);
            }
            CoordMsg::Guild(wish) => {
                if let Some(g) = self.guilds.as_mut() {
                    g.add(wish, now);
                    self.form_guilds(now);
                }
            }
            CoordMsg::Ambush(offer) => {
                // (an offer renews the earlier one but keeps its place in the line)
                match self.standing.iter_mut().find(|s| s.offer.bot == offer.bot) {
                    Some(s) => {
                        s.offer = offer;
                        s.since = now;
                    }
                    None => self.standing.push(Standing { offer, since: now }),
                }
                self.form_squads();
            }
            CoordMsg::Party(wish) => {
                self.wishing.retain(|w| w.wish.bot != wish.bot);
                self.wishing.push(Wishing { wish, since: now });
                self.form_parties(now);
            }
            CoordMsg::Request { kind, bot, at } => {
                self.waiting.retain(|w| w.bot != bot);
                self.waiting.push(Waiting {
                    kind,
                    bot,
                    at,
                    since: now,
                });
                self.pair_up(now);
            }
        }
    }

    /// Drops requests that waited too long, then looks for pairs again (the limits relax with
    /// the waiting time).
    fn expire(&mut self, now: Instant) {
        self.waiting
            .retain(|w| now.saturating_duration_since(w.since) < REQUEST_TTL);
        self.recent
            .retain(|r| now.saturating_duration_since(r.at) < REPAIR_COOLDOWN);
        self.wishing
            .retain(|w| now.saturating_duration_since(w.since) < WISH_TTL);
        self.formed
            .retain(|f| now.saturating_duration_since(f.at) < FORMED_TTL);
        if let Some(g) = self.guilds.as_mut() {
            g.expire(now);
        }
        self.standing
            .retain(|s| now.saturating_duration_since(s.since) < squad::STANDBY_TTL);
        self.pair_up(now);
        self.form_parties(now);
        self.form_guilds(now);
        self.form_squads();
    }

    /// Forms founding teams out of the wishes until no more can be formed and orders every
    /// member: the first one founds, the others sign.
    fn form_guilds(&mut self, now: Instant) {
        let (Some(wishes), Some(book)) = (self.guilds.as_mut(), self.sink.guild_book()) else {
            return;
        };
        let unix = crate::hub::unix_ms();
        while let Some(team) = wishes.form(now, book, unix, self.next_party) {
            self.next_party = self.next_party.wrapping_add(1).max(1);
            tracing::info!(party = team.party_id, bots = ?team.members, "guild founding team formed");
            for (i, &bot) in team.members.iter().enumerate() {
                let order = PartyOrder {
                    party_id: team.party_id,
                    leader: team.members[0],
                    members: team.members.iter().copied().collect(),
                    purpose: PartyPurpose::Guild,
                };
                if let Err(e) = self.sink.send(bot, BotCmd::Party(Box::new(order))) {
                    tracing::debug!("guild order to {bot}: {e}");
                    if i == 0 {
                        // (no founder, no founding: the place in the book is free again)
                        book.release(team.party_id);
                    }
                }
            }
        }
    }

    /// Number of bots waiting at an ambush post for a squad.
    pub fn standing(&self) -> usize {
        self.standing.len()
    }

    /// Forms ambush squads out of the standby offers and orders every member.
    fn form_squads(&mut self) {
        if self.standing.len() < 2 {
            return;
        }
        let offers: Vec<Standby> = self.standing.iter().map(|s| s.offer).collect();
        let (squads, used) = squad::plan(&offers);
        if squads.is_empty() {
            return;
        }
        for &i in used.iter().rev() {
            self.standing.remove(i);
        }
        for q in squads {
            let party_id = self.next_party;
            self.next_party = self.next_party.wrapping_add(1).max(1);
            tracing::info!(party = party_id, bots = ?q.members, "ambush squad formed");
            for &bot in &q.members {
                let order = PartyOrder {
                    party_id,
                    leader: q.members[0],
                    members: q.members.clone(),
                    purpose: PartyPurpose::Ambush {
                        x: q.post.0.round() as i32,
                        y: q.post.1.round() as i32,
                    },
                };
                if let Err(e) = self.sink.send(bot, BotCmd::Party(Box::new(order))) {
                    tracing::debug!("squad order to {bot}: {e}");
                }
            }
        }
    }

    /// Forms parties out of the wishes until no more can be formed, and orders every member.
    fn form_parties(&mut self, now: Instant) {
        while self.wishing.len() >= party::MIN_PARTY && !self.sites.is_empty() {
            let wishes: Vec<(Wish, u64)> = self
                .wishing
                .iter()
                .map(|w| (w.wish, now.saturating_duration_since(w.since).as_secs()))
                .collect();
            let Some(f) = party::plan(&wishes, &self.sites, self.relax_after_secs) else {
                return;
            };
            let bots: SmallVec<[u32; party::MAX_PARTY]> =
                f.members.iter().map(|&i| wishes[i].0.bot).collect();
            // (the members leave the wish list, highest index first)
            let mut gone: SmallVec<[usize; party::MAX_PARTY]> = f.members.clone();
            gone.sort_unstable_by(|a, b| b.cmp(a));
            for i in gone {
                self.wishing.remove(i);
            }
            let party_id = self.next_party;
            self.next_party = self.next_party.wrapping_add(1).max(1);
            tracing::info!(
                party = party_id,
                zone_group = f.site.zone_group,
                bots = ?bots,
                "dungeon party formed"
            );
            for &bot in &bots {
                let order = PartyOrder {
                    party_id,
                    leader: bots[0],
                    members: bots.clone(),
                    purpose: PartyPurpose::Dungeon {
                        zone_group: f.site.zone_group,
                    },
                };
                if let Err(e) = self.sink.send(bot, BotCmd::Party(Box::new(order))) {
                    tracing::debug!("party order to {bot}: {e}");
                }
            }
            if self.formed.len() >= FORMED_MAX {
                self.formed.remove(0);
            }
            self.formed.push(Formed {
                party_id,
                zone_group: f.site.zone_group,
                bots,
                at: now,
            });
        }
    }

    /// Number of bots waiting for a partner.
    pub fn waiting(&self) -> usize {
        self.waiting.len()
    }

    fn recently_paired(&self, a: u32, b: u32, now: Instant) -> bool {
        let (a, b) = (a.min(b), a.max(b));
        self.recent
            .iter()
            .any(|r| r.a == a && r.b == b && now.saturating_duration_since(r.at) < REPAIR_COOLDOWN)
    }

    /// Pairs the best-fitting waiting bots until no pair is left.
    fn pair_up(&mut self, now: Instant) {
        while self.waiting.len() >= 2 {
            let Some((i, j)) = self.best_pair(now) else {
                return;
            };
            // `i` waited longer (the list is in request order): it is the initiator.
            let second = self.waiting.remove(j);
            let first = self.waiting.remove(i);
            let meet = (
                (first.at.0 + second.at.0) / 2.0,
                (first.at.1 + second.at.1) / 2.0,
            );
            self.remember(first.bot, second.bot, now);
            self.pair(first.kind, first.bot, second.bot, meet);
        }
    }

    fn remember(&mut self, a: u32, b: u32, now: Instant) {
        if self.recent.len() >= RECENT_MAX {
            self.recent.remove(0);
        }
        self.recent.push(Recent {
            a: a.min(b),
            b: a.max(b),
            at: now,
        });
    }

    /// The indices `(i, j)`, `i < j`, of the best allowed pair.
    fn best_pair(&self, now: Instant) -> Option<(usize, usize)> {
        let mut best: Option<(f32, usize, usize)> = None;
        for (i, a) in self.waiting.iter().enumerate() {
            let a_level = self.sink.level(a.bot);
            for (j, b) in self.waiting.iter().enumerate().skip(i + 1) {
                if a.kind != b.kind || a.bot == b.bot || self.recently_paired(a.bot, b.bot, now) {
                    continue;
                }
                let waited = now
                    .saturating_duration_since(a.since.min(b.since))
                    .as_secs();
                let dist = ((a.at.0 - b.at.0).powi(2) + (a.at.1 - b.at.1).powi(2)).sqrt();
                if dist > NEAR_M + NEAR_GROWTH_M * waited as f32 {
                    continue;
                }
                let b_level = self.sink.level(b.bot);
                let diff = if a_level == 0 || b_level == 0 {
                    0
                } else {
                    u32::from(a_level.abs_diff(b_level))
                };
                if diff > LEVEL_DIFF + (waited / LEVEL_GROWTH_SECS) as u32 {
                    continue;
                }
                let score = dist + LEVEL_WEIGHT_M * diff as f32;
                if best.is_none_or(|x| score < x.0) {
                    best = Some((score, i, j));
                }
            }
        }
        best.map(|(_, i, j)| (i, j))
    }

    fn pair(&self, kind: PairKind, first: u32, second: u32, meet_at: (f32, f32)) {
        for (bot, role, partner) in [
            (first, PairRole::Initiator, second),
            (second, PairRole::Responder, first),
        ] {
            let (partner_char, partner_name) = self.sink.who(partner);
            let order = PairOrder {
                kind,
                role,
                partner_bot: partner,
                partner_char,
                partner_name: partner_name.into_boxed_str(),
                meet_at,
            };
            if let Err(e) = self.sink.send(bot, BotCmd::Pair(order)) {
                tracing::debug!("pairing {bot}: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::collections::HashMap;

    #[derive(Default)]
    struct Recorder {
        sent: Mutex<Vec<(u32, BotCmd)>>,
        levels: Mutex<HashMap<u32, u8>>,
        book: crate::bot::guild::GuildBook,
    }

    impl Dispatch for Recorder {
        fn send(&self, bot: u32, cmd: BotCmd) -> Result<(), String> {
            self.sent.lock().push((bot, cmd));
            Ok(())
        }
        fn who(&self, bot: u32) -> (u32, String) {
            (bot * 100, format!("Bot{bot}"))
        }
        fn level(&self, bot: u32) -> u8 {
            self.levels.lock().get(&bot).copied().unwrap_or(0)
        }
        fn guild_book(&self) -> Option<&crate::bot::guild::GuildBook> {
            Some(&self.book)
        }
    }

    fn coordinator() -> (Arc<Recorder>, CoordHandle, Coordinator) {
        let rec = Arc::new(Recorder::default());
        let (h, c) = channel(rec.clone());
        (rec, h, c)
    }

    fn req(kind: PairKind, bot: u32, at: (f32, f32)) -> CoordMsg {
        CoordMsg::Request { kind, bot, at }
    }

    /// Who was told to pair with whom: `(bot, partner, role)` in sending order.
    fn pairs(rec: &Recorder) -> Vec<(u32, u32, PairRole)> {
        rec.sent
            .lock()
            .iter()
            .map(|(bot, cmd)| match cmd {
                BotCmd::Pair(o) => (*bot, o.partner_bot, o.role),
                other => panic!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn two_requests_of_a_kind_make_a_pair_with_opposite_roles() {
        let (rec, _h, mut c) = coordinator();
        let now = Instant::now();
        c.handle(req(PairKind::Duel, 1, (0.0, 0.0)), now);
        assert_eq!(c.waiting(), 1);
        assert!(rec.sent.lock().is_empty());
        c.handle(req(PairKind::Duel, 2, (10.0, 20.0)), now);
        assert_eq!(c.waiting(), 0);
        let sent = rec.sent.lock();
        assert_eq!(sent.len(), 2);
        let (bot, BotCmd::Pair(first)) = &sent[0] else {
            panic!()
        };
        assert_eq!(
            (*bot, first.role, first.partner_bot),
            (1, PairRole::Initiator, 2)
        );
        assert_eq!((first.partner_char, &*first.partner_name), (200, "Bot2"));
        assert_eq!(first.meet_at, (5.0, 10.0));
        let (bot, BotCmd::Pair(second)) = &sent[1] else {
            panic!()
        };
        assert_eq!(
            (*bot, second.role, second.partner_bot),
            (2, PairRole::Responder, 1)
        );
    }

    #[test]
    fn different_kinds_do_not_pair_and_cancel_withdraws() {
        let (rec, _h, mut c) = coordinator();
        let now = Instant::now();
        c.handle(req(PairKind::Duel, 1, (0.0, 0.0)), now);
        c.handle(req(PairKind::Trade, 2, (0.0, 0.0)), now);
        assert_eq!(c.waiting(), 2);
        c.handle(CoordMsg::Cancel { bot: 1 }, now);
        assert_eq!(c.waiting(), 1);
        // a bot asking twice waits once and is never paired with itself
        c.handle(req(PairKind::Trade, 2, (1.0, 1.0)), now);
        assert_eq!(c.waiting(), 1);
        assert!(rec.sent.lock().is_empty());
    }

    #[test]
    fn old_requests_expire() {
        let (_rec, _h, mut c) = coordinator();
        let t0 = Instant::now();
        c.handle(req(PairKind::Duel, 1, (0.0, 0.0)), t0);
        c.expire(t0 + Duration::from_secs(60));
        assert_eq!(c.waiting(), 1);
        c.expire(t0 + REQUEST_TTL + Duration::from_secs(1));
        assert_eq!(c.waiting(), 0);
    }

    #[test]
    fn handles_report_a_full_inbox() {
        let (_rec, h, _c) = coordinator();
        let mut accepted = 0;
        for i in 0..1000 {
            if h.request_partner(PairKind::Duel, i, (0.0, 0.0)) {
                accepted += 1;
            }
        }
        assert_eq!(accepted, INBOX);
        // a detached handle has nobody listening: every request is refused
        let d = CoordHandle::detached();
        assert!(!d.request_partner(PairKind::Duel, 1, (0.0, 0.0)));
    }

    #[test]
    fn the_nearest_waiting_bot_is_chosen() {
        let (rec, _h, mut c) = coordinator();
        let t = Instant::now();
        // 1 far away, 2 close to the newcomer 3: 3 pairs with 2
        c.handle(req(PairKind::Duel, 1, (100.0, 0.0)), t);
        c.handle(req(PairKind::Duel, 2, (-40.0, 0.0)), t);
        // 1 and 2 are 140 m apart (allowed), so they were paired at once
        assert_eq!(c.waiting(), 0);
        assert_eq!(pairs(&rec)[0], (1, 2, PairRole::Initiator));
        rec.sent.lock().clear();
        // a cluster: 4 waits far from 5; 6 arrives next to 5 and takes it
        c.handle(req(PairKind::Duel, 4, (1000.0, 0.0)), t);
        c.handle(req(PairKind::Duel, 5, (0.0, 0.0)), t);
        assert_eq!(c.waiting(), 2, "1000 m apart: not paired yet");
        c.handle(req(PairKind::Duel, 6, (5.0, 0.0)), t);
        let p = pairs(&rec);
        assert_eq!(p[0], (5, 6, PairRole::Initiator));
        assert_eq!(c.waiting(), 1);
    }

    #[test]
    fn levels_must_be_close_until_the_wait_relaxes_the_limit() {
        let (rec, _h, mut c) = coordinator();
        rec.levels.lock().extend([(1, 5), (2, 30), (3, 8)]);
        let t = Instant::now();
        c.handle(req(PairKind::Duel, 1, (0.0, 0.0)), t);
        c.handle(req(PairKind::Duel, 2, (0.0, 0.0)), t);
        assert_eq!(c.waiting(), 2, "25 levels apart");
        // 3 is three levels from 1 and takes it, though 2 waited longer than 3
        c.handle(req(PairKind::Duel, 3, (0.0, 0.0)), t);
        assert_eq!(pairs(&rec)[0], (1, 3, PairRole::Initiator));
        rec.sent.lock().clear();
        // 2 stays alone; a later look pairs it with nobody until somebody comes
        c.expire(t + Duration::from_secs(30));
        assert_eq!(c.waiting(), 1);
        // a level 14 newcomer is 16 levels from bot 2: only the waiting time relaxes the limit
        rec.levels.lock().insert(4, 14);
        c.handle(
            req(PairKind::Duel, 4, (0.0, 0.0)),
            t + Duration::from_secs(40),
        );
        assert_eq!(c.waiting(), 2, "bot 2 waited 40 s: limit 4 + 5 levels");
        c.expire(t + Duration::from_secs(100));
        assert_eq!(c.waiting(), 0, "bot 2 waited 100 s: limit 4 + 12 levels");
        assert_eq!(pairs(&rec)[0], (2, 4, PairRole::Initiator));
    }

    #[test]
    fn the_same_two_bots_are_not_paired_back_to_back() {
        let (rec, _h, mut c) = coordinator();
        let t = Instant::now();
        c.handle(req(PairKind::Duel, 1, (0.0, 0.0)), t);
        c.handle(req(PairKind::Duel, 2, (5.0, 0.0)), t);
        assert_eq!(pairs(&rec).len(), 2);
        rec.sent.lock().clear();
        // they ask again right after: nothing, they wait
        let t1 = t + Duration::from_secs(20);
        c.handle(req(PairKind::Duel, 1, (0.0, 0.0)), t1);
        c.handle(req(PairKind::Duel, 2, (5.0, 0.0)), t1);
        assert!(rec.sent.lock().is_empty());
        assert_eq!(c.waiting(), 2);
        // a third bot goes with the one that waited longer
        c.handle(req(PairKind::Duel, 3, (-30.0, 0.0)), t1);
        let p = pairs(&rec);
        assert_eq!(p[0].0, 1, "{p:?}");
        assert!(p[0].1 == 3, "{p:?}");
        rec.sent.lock().clear();
        // and after the cooldown the old pair may meet again
        let t2 = t + REPAIR_COOLDOWN + Duration::from_secs(5);
        c.handle(req(PairKind::Duel, 1, (0.0, 0.0)), t2);
        c.expire(t2);
        assert_eq!(pairs(&rec)[0], (2, 1, PairRole::Initiator));
    }

    #[test]
    fn distance_limit_grows_with_waiting() {
        let (rec, _h, mut c) = coordinator();
        let t = Instant::now();
        c.handle(req(PairKind::Trade, 1, (0.0, 0.0)), t);
        c.handle(req(PairKind::Trade, 2, (900.0, 0.0)), t);
        assert_eq!(c.waiting(), 2);
        c.expire(t + Duration::from_secs(10));
        assert_eq!(c.waiting(), 2, "150 + 400 m after 10 s");
        c.expire(t + Duration::from_secs(20));
        assert_eq!(c.waiting(), 0, "150 + 800 m after 20 s");
        let p = pairs(&rec);
        assert_eq!(p[0], (1, 2, PairRole::Initiator));
        // trade pairing carries the same order shape
        let sent = rec.sent.lock();
        let BotCmd::Pair(o) = &sent[0].1 else {
            panic!()
        };
        assert_eq!((o.kind, o.meet_at), (PairKind::Trade, (450.0, 0.0)));
    }

    // ---- parties for dungeons ----------------------------------------------------------------

    fn site(idx: u16, group: u32, min: u8, max: u8, party: u8, at: (f32, f32)) -> Site {
        Site {
            idx,
            zone_group: group,
            level_min: min,
            level_max: max,
            max_party: party,
            at,
        }
    }

    fn wish(bot: u32, at: (f32, f32), level: u8) -> Wish {
        Wish {
            bot,
            at,
            level,
            alliance: 7,
            skip: 0,
            raise: false,
            jump: false,
        }
    }

    fn parties(rec: &Recorder) -> Vec<(u32, PartyOrder)> {
        rec.sent
            .lock()
            .iter()
            .filter_map(|(bot, cmd)| match cmd {
                BotCmd::Party(o) => Some((*bot, (**o).clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn three_wishes_for_one_dungeon_form_a_party_led_by_the_highest_level() {
        let (rec, _h, c) = coordinator();
        let mut c = c.with_sites(vec![site(0, 50, 18, 55, 3, (100.0, 100.0))]);
        let t0 = Instant::now();
        c.handle(CoordMsg::Party(wish(1, (0.0, 0.0), 20)), t0);
        c.handle(CoordMsg::Party(wish(2, (10.0, 0.0), 30)), t0);
        assert!(parties(&rec).is_empty(), "two are not enough at first");
        c.handle(CoordMsg::Party(wish(3, (20.0, 0.0), 25)), t0);
        let sent = parties(&rec);
        assert_eq!(sent.len(), 3, "every member is told");
        let order = &sent[0].1;
        assert!(sent.iter().all(|(_, o)| o == order), "one order for all");
        assert_eq!(order.leader, 2, "the highest level leads");
        assert_eq!(order.members[0], 2);
        let mut m: Vec<u32> = order.members.iter().copied().collect();
        m.sort_unstable();
        assert_eq!(m, [1, 2, 3]);
        assert_eq!(order.purpose, PartyPurpose::Dungeon { zone_group: 50 });
        assert_eq!(c.wishing(), 0);
        assert_eq!(c.formed().len(), 1);
        assert_eq!(c.formed()[0].party_id, order.party_id);
    }

    #[test]
    fn five_wishes_fill_a_five_player_dungeon_and_four_wait_for_the_relax_time() {
        let (rec, _h, c) = coordinator();
        let mut c = c.with_sites(vec![site(0, 50, 18, 55, 5, (0.0, 0.0))]);
        let t0 = Instant::now();
        for b in 1..=4u32 {
            c.handle(CoordMsg::Party(wish(b, (b as f32, 0.0), 20)), t0);
        }
        assert!(parties(&rec).is_empty(), "four are not enough at first");
        c.handle(CoordMsg::Party(wish(5, (5.0, 0.0), 25)), t0);
        let sent = parties(&rec);
        assert_eq!(sent.len(), 5, "every member is told");
        let order = &sent[0].1;
        assert!(sent.iter().all(|(_, o)| o == order), "one order for all");
        assert_eq!(order.leader, 5, "the highest level leads");
        let mut m: Vec<u32> = order.members.iter().copied().collect();
        m.sort_unstable();
        assert_eq!(m, [1, 2, 3, 4, 5]);
        assert_eq!(c.wishing(), 0);
        // four go after the configured relax time, not before
        let (rec, _h, c) = coordinator();
        let mut c = c
            .with_sites(vec![site(0, 50, 18, 55, 5, (0.0, 0.0))])
            .with_party_relax(30);
        for b in 1..=4u32 {
            c.handle(CoordMsg::Party(wish(b, (b as f32, 0.0), 20)), t0);
        }
        assert!(parties(&rec).is_empty());
        c.expire(t0 + Duration::from_secs(29));
        assert!(parties(&rec).is_empty(), "not yet");
        c.expire(t0 + Duration::from_secs(31));
        assert_eq!(parties(&rec).len(), 4, "a party of four after the wait");
    }

    #[test]
    fn a_pair_is_formed_only_after_the_wait_and_never_across_alliances() {
        let (rec, _h, c) = coordinator();
        let mut c = c.with_sites(vec![site(0, 50, 18, 55, 3, (0.0, 0.0))]);
        let t0 = Instant::now();
        c.handle(CoordMsg::Party(wish(1, (0.0, 0.0), 20)), t0);
        c.handle(CoordMsg::Party(wish(2, (5.0, 0.0), 20)), t0);
        assert!(parties(&rec).is_empty());
        c.expire(t0 + Duration::from_secs(party::RELAX_AFTER_SECS - 5));
        assert!(parties(&rec).is_empty(), "not yet");
        c.expire(t0 + Duration::from_secs(party::RELAX_AFTER_SECS + 1));
        assert_eq!(parties(&rec).len(), 2, "a party of two after the wait");
        // a bot of the other alliance is left out
        let (rec, _h, c) = coordinator();
        let mut c = c.with_sites(vec![site(0, 50, 18, 55, 3, (0.0, 0.0))]);
        c.handle(CoordMsg::Party(wish(1, (0.0, 0.0), 20)), t0);
        c.handle(CoordMsg::Party(wish(2, (5.0, 0.0), 20)), t0);
        let foe = Wish {
            alliance: 8,
            ..wish(3, (6.0, 0.0), 20)
        };
        c.handle(CoordMsg::Party(foe), t0);
        assert!(parties(&rec).is_empty());
        c.expire(t0 + Duration::from_secs(party::RELAX_AFTER_SECS + 1));
        let sent = parties(&rec);
        assert_eq!(sent.len(), 2);
        assert!(sent.iter().all(|(b, _)| *b != 3));
        assert_eq!(c.wishing(), 1, "the foe goes on waiting");
    }

    #[test]
    fn the_level_the_range_and_the_blocked_dungeons_decide_who_goes_where() {
        let (rec, _h, c) = coordinator();
        // a low dungeon (18-25) and a high one (31-55), both at the same place
        let mut c = c.with_sites(vec![
            site(0, 50, 18, 25, 3, (0.0, 0.0)),
            site(1, 45, 31, 55, 5, (0.0, 0.0)),
        ]);
        let t0 = Instant::now();
        // too high for the low one, too low for the high one without a raise: nobody goes
        c.handle(CoordMsg::Party(wish(1, (0.0, 0.0), 28)), t0);
        c.handle(CoordMsg::Party(wish(2, (1.0, 0.0), 28)), t0);
        c.handle(CoordMsg::Party(wish(3, (2.0, 0.0), 28)), t0);
        assert!(parties(&rec).is_empty());
        c.handle(CoordMsg::Cancel { bot: 1 }, t0);
        c.handle(CoordMsg::Cancel { bot: 2 }, t0);
        c.handle(CoordMsg::Cancel { bot: 3 }, t0);
        assert_eq!(c.wishing(), 0, "a cancel withdraws the wish");
        // GM bots below the range may be raised: the dungeon with the smaller gap wins
        for b in 1..=3u32 {
            let w = Wish {
                raise: true,
                ..wish(b, (b as f32, 0.0), 10)
            };
            c.handle(CoordMsg::Party(w), t0);
        }
        let sent = parties(&rec);
        assert_eq!(sent.len(), 3);
        assert_eq!(sent[0].1.zone_group(), 50, "18 is closer to 10 than 31 is");
        // bots that cannot enter the low dungeon (the server refused it) go to the high one
        let (rec, _h, c) = coordinator();
        let mut c = c.with_sites(vec![
            site(0, 50, 18, 25, 3, (0.0, 0.0)),
            site(1, 45, 18, 55, 5, (0.0, 0.0)),
        ]);
        for b in 1..=3u32 {
            let w = Wish {
                skip: 1 << 0,
                ..wish(b, (b as f32, 0.0), 20)
            };
            c.handle(CoordMsg::Party(w), t0);
        }
        // the high dungeon holds five: three wait for the relax time before going
        assert!(parties(&rec).is_empty());
        c.expire(t0 + Duration::from_secs(party::RELAX_AFTER_SECS + 1));
        let sent = parties(&rec);
        assert_eq!(sent.len(), 3);
        assert_eq!(sent[0].1.zone_group(), 45);
    }

    #[test]
    fn a_dungeon_party_does_not_disturb_duels_and_a_second_wish_replaces_the_first() {
        let (rec, _h, c) = coordinator();
        let mut c = c.with_sites(vec![site(0, 50, 18, 55, 3, (0.0, 0.0))]);
        let t0 = Instant::now();
        c.handle(CoordMsg::Party(wish(1, (0.0, 0.0), 20)), t0);
        c.handle(CoordMsg::Party(wish(1, (1.0, 0.0), 21)), t0);
        assert_eq!(c.wishing(), 1, "one wish per bot");
        c.handle(req(PairKind::Duel, 7, (0.0, 0.0)), t0);
        c.handle(req(PairKind::Duel, 8, (3.0, 0.0)), t0);
        assert_eq!(pairs(&rec).len(), 2, "the duel pair is still made");
        assert_eq!(c.wishing(), 1);
        // without sites a wish is never answered
        let (rec, _h, c) = coordinator();
        let mut c = c;
        for b in 1..=3u32 {
            c.handle(CoordMsg::Party(wish(b, (0.0, 0.0), 20)), t0);
        }
        assert!(parties(&rec).is_empty());
        assert_eq!(c.wishing(), 3);
        // wishes that waited too long are dropped
        c.expire(t0 + WISH_TTL + Duration::from_secs(1));
        assert_eq!(c.wishing(), 0);
    }

    fn guild_card(bot: u32, mother: u32, level: u8, flags: u8) -> crate::bot::guild::Card {
        crate::bot::guild::Card {
            bot,
            char_id: bot * 100,
            name: format!("Bot{bot}").into(),
            mother,
            level,
            guild: 0,
            flags,
            seen_ms: 0,
        }
    }

    fn guild_wish(bot: u32, mother: u32, level: u8) -> GuildWish {
        GuildWish {
            bot,
            mother,
            level,
            can_pay: true,
        }
    }

    /// `n` bots of faction 7 that can sign a charter (their cards are in the book).
    fn charter_bots(rec: &Recorder, n: u32) {
        use crate::bot::guild::book::{CAN_FOUND, CAN_JOIN, WISHING};
        for b in 1..=n {
            rec.book.update_card(
                guild_card(b, 7, 10 + b as u8, CAN_JOIN | CAN_FOUND | WISHING),
                crate::hub::unix_ms(),
            );
        }
    }

    fn guild_orders(rec: &Recorder) -> Vec<(u32, PartyOrder)> {
        rec.sent
            .lock()
            .iter()
            .map(|(bot, cmd)| match cmd {
                BotCmd::Party(o) => (*bot, (**o).clone()),
                other => panic!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn five_wishes_for_a_guild_make_a_team_and_the_founder_comes_first() {
        let (rec, _h, c) = coordinator();
        let mut c = c.with_guilds(5, 3);
        charter_bots(&rec, 5);
        let t0 = Instant::now();
        for b in 1..=4u32 {
            c.handle(CoordMsg::Guild(guild_wish(b, 7, 10 + b as u8)), t0);
        }
        assert!(rec.sent.lock().is_empty(), "four are too few");
        assert_eq!(c.guild_wishes(), 4);
        c.handle(CoordMsg::Guild(guild_wish(5, 7, 15)), t0);
        let sent = guild_orders(&rec);
        assert_eq!(sent.len(), 5);
        let order = &sent[0].1;
        assert_eq!(order.purpose, PartyPurpose::Guild);
        assert_eq!(order.leader, 5, "the highest level founds");
        assert_eq!(sent[0].0, 5, "and is told first");
        assert!(
            sent.iter().all(|(_, o)| o == order),
            "one order for everybody"
        );
        assert_eq!(order.members.len(), 5);
        assert_eq!(c.guild_wishes(), 0);
        assert_eq!(
            rec.book.foundings(),
            1,
            "the guild has its place in the book"
        );
    }

    #[test]
    fn guilds_are_not_founded_without_the_switch_and_not_beyond_the_limit() {
        // (no `with_guilds`: the wishes are ignored)
        let (rec, _h, mut c) = coordinator();
        charter_bots(&rec, 5);
        let t0 = Instant::now();
        for b in 1..=5u32 {
            c.handle(CoordMsg::Guild(guild_wish(b, 7, 10)), t0);
        }
        assert!(rec.sent.lock().is_empty());
        assert_eq!(c.guild_wishes(), 0);
        // one guild at most: the second team waits until the first founding is over
        let (rec, _h, c) = coordinator();
        let mut c = c.with_guilds(2, 1);
        charter_bots(&rec, 4);
        for b in 1..=4u32 {
            c.handle(CoordMsg::Guild(guild_wish(b, 7, 10)), t0);
        }
        assert_eq!(guild_orders(&rec).len(), 2, "one team of two");
        assert_eq!(c.guild_wishes(), 2, "the others wait");
        let party = guild_orders(&rec)[0].1.party_id;
        rec.book.release(party);
        c.expire(t0 + Duration::from_secs(SWEEP_SECS));
        assert_eq!(guild_orders(&rec).len(), 4, "the place is free again");
    }

    #[test]
    fn a_guild_wish_is_cancelled_with_the_bot_and_expires() {
        let (rec, _h, c) = coordinator();
        let mut c = c.with_guilds(5, 3);
        charter_bots(&rec, 2);
        let t0 = Instant::now();
        c.handle(CoordMsg::Guild(guild_wish(1, 7, 10)), t0);
        c.handle(CoordMsg::Guild(guild_wish(2, 7, 10)), t0);
        assert_eq!(c.guild_wishes(), 2);
        c.handle(CoordMsg::Cancel { bot: 1 }, t0);
        assert_eq!(c.guild_wishes(), 1);
        c.expire(t0 + crate::bot::guild::plan::WISH_TTL);
        assert_eq!(c.guild_wishes(), 0);
    }

    #[test]
    fn a_guild_team_does_not_disturb_duels_and_dungeon_parties() {
        let (rec, _h, c) = coordinator();
        let mut c = c.with_guilds(2, 3);
        charter_bots(&rec, 2);
        let t0 = Instant::now();
        c.handle(CoordMsg::Guild(guild_wish(1, 7, 10)), t0);
        c.handle(req(PairKind::Duel, 7, (0.0, 0.0)), t0);
        c.handle(req(PairKind::Duel, 8, (3.0, 0.0)), t0);
        assert_eq!(pairs(&rec).len(), 2, "the duel pair is made");
        assert_eq!(c.guild_wishes(), 1, "the wish waits");
    }

    fn standby(bot: u32, x: f32, alliance: u32) -> Standby {
        Standby {
            bot,
            at: (x, 0.0),
            alliance,
            level: 30,
            max_squad: 3,
        }
    }

    /// The ambush orders sent so far: `(bot, order)`.
    fn ambushes(rec: &Recorder) -> Vec<(u32, PartyOrder)> {
        rec.sent
            .lock()
            .iter()
            .map(|(bot, cmd)| match cmd {
                BotCmd::Party(o) => (*bot, (**o).clone()),
                other => panic!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn bots_waiting_close_together_are_ordered_into_an_ambush_squad() {
        let (rec, _h, mut c) = coordinator();
        let t0 = Instant::now();
        c.handle(CoordMsg::Ambush(standby(1, 0.0, 148)), t0);
        assert_eq!(c.standing(), 1);
        // another alliance, and one too far away, do not join
        c.handle(CoordMsg::Ambush(standby(2, 10.0, 149)), t0);
        c.handle(CoordMsg::Ambush(standby(3, 5_000.0, 148)), t0);
        assert!(rec.sent.lock().is_empty());
        c.handle(CoordMsg::Ambush(standby(4, 30.0, 148)), t0);
        let sent = ambushes(&rec);
        assert_eq!(sent.len(), 2, "one order per member");
        for (bot, o) in &sent {
            assert_eq!(o.members.as_slice(), [1, 4]);
            assert_eq!(o.leader, 1);
            assert_eq!(o.purpose, PartyPurpose::Ambush { x: 0, y: 0 });
            assert!(o.has_member(*bot));
        }
        assert_eq!(sent[0].1.party_id, sent[1].1.party_id);
        // the squad's members left the list, the others still wait
        assert_eq!(c.standing(), 2);
    }

    #[test]
    fn a_standby_offer_is_renewed_withdrawn_and_expires() {
        let (rec, _h, mut c) = coordinator();
        let t0 = Instant::now();
        c.handle(CoordMsg::Ambush(standby(1, 0.0, 148)), t0);
        c.handle(CoordMsg::Ambush(standby(1, 2.0, 148)), t0);
        assert_eq!(c.standing(), 1, "one offer per bot");
        c.handle(CoordMsg::Cancel { bot: 1 }, t0);
        assert_eq!(c.standing(), 0);
        c.handle(CoordMsg::Ambush(standby(5, 0.0, 148)), t0);
        c.expire(t0 + squad::STANDBY_TTL + Duration::from_secs(1));
        assert_eq!(c.standing(), 0);
        assert!(rec.sent.lock().is_empty());
    }
}
