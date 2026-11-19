//! The guild book: what all the bots of the process share about guilds.
//!
//! Every bot tells the book who it is every few seconds (a [`Card`]: character, faction, level,
//! guild, whether it can be invited or can sign a charter) and which guild it belongs to
//! ([`GuildBook::note_member`]). The book then answers the questions that cross bots:
//!
//! * how many guilds the bots run (`[guild] max_guilds` counts them, together with the foundings
//!   that are under way, [`GuildBook::reserve`]),
//! * which unguilded bot a guild officer may invite next ([`GuildBook::pick_recruit`]; an invited
//!   bot is claimed for [`CLAIM_MS`] so two officers do not invite the same one),
//! * which names are taken ([`GuildBook::name_taken`]; fed by the guild lists the server sends),
//! * how often a guild invites and talks, and how much all guilds say together.
//!
//! One mutex around plain data; every method takes the time as an argument (unix milliseconds in
//! the process, anything in the tests), so the book is tested without a clock. A card that is not
//! refreshed for [`CARD_FRESH_MS`] is an offline bot and is ignored.

use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

/// A card older than this is a bot that went away.
pub const CARD_FRESH_MS: u64 = 20_000;
/// An invited bot is not invited by another officer for this long (the invitation lasts 60 s).
pub const CLAIM_MS: u64 = 75_000;
/// A founding that has not ended after this long no longer counts against `max_guilds`.
pub const FOUNDING_TTL_MS: u64 = 10 * 60_000;
/// Most remembered guild names.
const MAX_NAMES: usize = 4_096;

/// The bot can accept a guild invitation now.
pub const CAN_JOIN: u8 = 1;
/// The bot can be a member of a founding team now.
pub const CAN_FOUND: u8 = 2;
/// The bot waits for a founding team.
pub const WISHING: u8 = 4;

/// What a bot says about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    /// Bot id.
    pub bot: u32,
    /// Character id.
    pub char_id: u32,
    /// Character name (what a guild invitation names).
    pub name: Box<str>,
    /// The mother faction (the alliance); a guild takes bots of one only.
    pub mother: u32,
    /// Character level.
    pub level: u8,
    /// The guild the bot is in (0 none).
    pub guild: u32,
    /// [`CAN_JOIN`], [`CAN_FOUND`], [`WISHING`].
    pub flags: u8,
    /// When the card was last refreshed (the book's clock).
    pub seen_ms: u64,
}

impl Card {
    /// Whether the bot can be invited now.
    pub fn can_join(&self) -> bool {
        self.guild == 0 && self.flags & CAN_JOIN != 0
    }

    /// Whether the bot can join a founding team now.
    pub fn can_found(&self) -> bool {
        self.guild == 0 && self.flags & CAN_FOUND != 0
    }

    /// Whether the bot waits for a founding team.
    pub fn wishing(&self) -> bool {
        self.flags & WISHING != 0
    }
}

/// A guild the bots belong to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookGuild {
    /// Guild id.
    pub id: u32,
    /// Guild name.
    pub name: Box<str>,
    /// The mother faction of the guild's bots.
    pub mother: u32,
    /// The bots that reported they belong to it.
    pub bots: SmallVec<[u32; 8]>,
    /// When the guild last invited somebody (the book's clock).
    pub last_invite_ms: u64,
    /// When the guild last had a chat line from a bot.
    pub last_line_ms: u64,
}

/// The guild a bot reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Membership {
    /// Guild id.
    pub id: u32,
    /// Guild name.
    pub name: Box<str>,
    /// The mother faction of the reporting bot.
    pub mother: u32,
}

#[derive(Debug, Clone, Copy)]
struct Founding {
    party: u32,
    since_ms: u64,
}

#[derive(Debug, Default)]
struct Inner {
    cards: FxHashMap<u32, Card>,
    guilds: FxHashMap<u32, BookGuild>,
    foundings: SmallVec<[Founding; 4]>,
    names: FxHashSet<Box<str>>,
    claims: SmallVec<[(u32, u64); 8]>,
    /// The start of the current minute of chat lines and the lines in it.
    lines: (u64, u32),
    /// When the first card came.
    first_card_ms: u64,
}

/// The process-wide guild book. See the module docs.
#[derive(Debug, Default)]
pub struct GuildBook {
    inner: Mutex<Inner>,
}

/// The lowercase key of a guild name.
fn key(name: &str) -> Box<str> {
    name.trim().to_lowercase().into_boxed_str()
}

impl GuildBook {
    /// Stores (or refreshes) a bot's card.
    pub fn update_card(&self, mut card: Card, now_ms: u64) {
        card.seen_ms = now_ms;
        let mut g = self.inner.lock();
        if g.first_card_ms == 0 {
            g.first_card_ms = now_ms.max(1);
        }
        g.cards.insert(card.bot, card);
    }

    /// The card of `bot`, if it is fresh.
    pub fn card(&self, bot: u32, now_ms: u64) -> Option<Card> {
        let g = self.inner.lock();
        g.cards
            .get(&bot)
            .filter(|c| now_ms.saturating_sub(c.seen_ms) <= CARD_FRESH_MS)
            .cloned()
    }

    /// Forgets the card of a bot that went offline.
    pub fn forget(&self, bot: u32) {
        self.inner.lock().cards.remove(&bot);
    }

    /// How long ago the first card arrived (0 before any); the bots wait until the ones that are
    /// logging in have reported their guilds before they found one.
    pub fn age_ms(&self, now_ms: u64) -> u64 {
        let g = self.inner.lock();
        if g.first_card_ms == 0 {
            0
        } else {
            now_ms.saturating_sub(g.first_card_ms)
        }
    }

    /// Records which guild `bot` belongs to (`None`: none). A guild without a bot disappears.
    pub fn note_member(&self, bot: u32, guild: Option<Membership>) {
        let mut g = self.inner.lock();
        for e in g.guilds.values_mut() {
            if guild.as_ref().is_none_or(|m| m.id != e.id) {
                e.bots.retain(|b| *b != bot);
            }
        }
        if let Some(m) = guild {
            let e = g.guilds.entry(m.id).or_insert_with(|| BookGuild {
                id: m.id,
                name: m.name.clone(),
                mother: m.mother,
                bots: SmallVec::new(),
                last_invite_ms: 0,
                last_line_ms: 0,
            });
            if !m.name.is_empty() {
                e.name = m.name.clone();
            }
            if m.mother != 0 {
                e.mother = m.mother;
            }
            if !e.bots.contains(&bot) {
                e.bots.push(bot);
            }
            if !m.name.is_empty() {
                g.names.insert(key(&m.name));
            }
        }
        g.guilds.retain(|_, e| !e.bots.is_empty());
    }

    /// The guilds the bots belong to.
    pub fn guild_count(&self) -> usize {
        self.inner.lock().guilds.len()
    }

    /// A copy of the guild `id`.
    pub fn guild(&self, id: u32) -> Option<BookGuild> {
        self.inner.lock().guilds.get(&id).cloned()
    }

    /// Whether an online bot reports this name and character in the specified guild.
    pub fn has_member(&self, guild: u32, name: &str, character: Option<u32>, now: u64) -> bool {
        let g = self.inner.lock();
        g.guilds.get(&guild).is_some_and(|guild| {
            guild.bots.iter().any(|bot| {
                g.cards.get(bot).is_some_and(|card| {
                    card.guild == guild.id
                        && now.saturating_sub(card.seen_ms) <= CARD_FRESH_MS
                        && card.name.eq_ignore_ascii_case(name)
                        && character.is_none_or(|id| card.char_id == id)
                })
            })
        })
    }

    /// Whether another guild may be founded: the guilds the bots run and the foundings under way
    /// are fewer than `max`.
    pub fn room_for_guild(&self, max: u32, now_ms: u64) -> bool {
        let mut g = self.inner.lock();
        g.foundings
            .retain(|f| now_ms.saturating_sub(f.since_ms) < FOUNDING_TTL_MS);
        (g.guilds.len() + g.foundings.len()) < max as usize
    }

    /// Reserves a place for the guild a team (`party`) is about to found. `false` when the bots
    /// run or found `max` guilds already.
    pub fn reserve(&self, party: u32, max: u32, now_ms: u64) -> bool {
        let mut g = self.inner.lock();
        g.foundings
            .retain(|f| now_ms.saturating_sub(f.since_ms) < FOUNDING_TTL_MS);
        if g.guilds.len() + g.foundings.len() >= max as usize {
            return false;
        }
        g.foundings.push(Founding {
            party,
            since_ms: now_ms,
        });
        true
    }

    /// The founding of `party` is over (the guild exists, or it failed).
    pub fn release(&self, party: u32) {
        self.inner.lock().foundings.retain(|f| f.party != party);
    }

    /// Foundings under way.
    pub fn foundings(&self) -> usize {
        self.inner.lock().foundings.len()
    }

    /// Remembers a guild name (heard from the server's guild list or refused by it).
    pub fn note_name(&self, name: &str) {
        let k = key(name);
        if k.is_empty() {
            return;
        }
        let mut g = self.inner.lock();
        if g.names.len() >= MAX_NAMES {
            return;
        }
        g.names.insert(k);
    }

    /// Whether a guild of that name is known (the server compares names exactly; the book
    /// ignores the case, which is stricter).
    pub fn name_taken(&self, name: &str) -> bool {
        self.inner.lock().names.contains(&key(name))
    }

    /// Chooses an unguilded bot of `mother` and at least `min_level` that `guild` may invite, and
    /// claims it. `skip` names bots that must not be chosen (the recruiter itself).
    pub fn pick_recruit(
        &self,
        mother: u32,
        min_level: u8,
        skip: u32,
        now_ms: u64,
        rng: &mut fastrand::Rng,
    ) -> Option<Card> {
        let mut g = self.inner.lock();
        g.claims.retain(|c| c.1 > now_ms);
        let claimed = |bot: u32, claims: &[(u32, u64)]| claims.iter().any(|c| c.0 == bot);
        let fits: SmallVec<[u32; 16]> = g
            .cards
            .values()
            .filter(|c| {
                c.bot != skip
                    && c.can_join()
                    && !c.wishing()
                    && c.mother == mother
                    && c.level >= min_level
                    && now_ms.saturating_sub(c.seen_ms) <= CARD_FRESH_MS
                    && !claimed(c.bot, &g.claims)
            })
            .map(|c| c.bot)
            .collect();
        if fits.is_empty() {
            return None;
        }
        // (the map's order is arbitrary but fixed: sort, then draw)
        let mut fits = fits;
        fits.sort_unstable();
        let bot = fits[rng.usize(..fits.len())];
        if g.claims.len() >= 8 {
            g.claims.remove(0);
        }
        g.claims.push((bot, now_ms + CLAIM_MS));
        g.cards.get(&bot).cloned()
    }

    /// Releases the claim on `bot` (the invitation was answered or never sent).
    pub fn unclaim(&self, bot: u32) {
        self.inner.lock().claims.retain(|c| c.0 != bot);
    }

    /// Takes the right to send a guild invitation for `guild`: at least `gap_ms` since the last
    /// one. Marks the invitation when it returns `true`.
    pub fn take_invite_slot(&self, guild: u32, now_ms: u64, gap_ms: u64) -> bool {
        let mut g = self.inner.lock();
        let Some(e) = g.guilds.get_mut(&guild) else {
            return false;
        };
        if now_ms.saturating_sub(e.last_invite_ms) < gap_ms {
            return false;
        }
        e.last_invite_ms = now_ms;
        true
    }

    /// Takes the right to say a chat line in `guild`: at least `gap_ms` since the last line of
    /// any bot of the guild and fewer than `per_min` lines of all guilds in the current minute.
    pub fn take_line(&self, guild: u32, now_ms: u64, gap_ms: u64, per_min: u32) -> bool {
        let mut g = self.inner.lock();
        if now_ms.saturating_sub(g.lines.0) >= 60_000 {
            g.lines = (now_ms, 0);
        }
        if g.lines.1 >= per_min {
            return false;
        }
        let gap_ok = g
            .guilds
            .get(&guild)
            .is_none_or(|e| now_ms.saturating_sub(e.last_line_ms) >= gap_ms);
        if !gap_ok {
            return false;
        }
        g.lines.1 += 1;
        if let Some(e) = g.guilds.get_mut(&guild) {
            e.last_line_ms = now_ms;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(bot: u32, mother: u32, level: u8, flags: u8) -> Card {
        Card {
            bot,
            char_id: 1000 + bot,
            name: format!("Bot{bot}").into(),
            mother,
            level,
            guild: 0,
            flags,
            seen_ms: 0,
        }
    }

    fn member(id: u32, name: &str) -> Option<Membership> {
        Some(Membership {
            id,
            name: name.into(),
            mother: 101,
        })
    }

    #[test]
    fn cards_go_stale() {
        let b = GuildBook::default();
        assert_eq!(b.age_ms(5_000), 0);
        b.update_card(card(1, 101, 10, CAN_JOIN), 1_000);
        assert!(b.card(1, 1_000 + CARD_FRESH_MS).is_some());
        assert!(b.card(1, 1_001 + CARD_FRESH_MS).is_none());
        assert!(b.card(2, 1_000).is_none());
        assert_eq!(b.age_ms(61_000), 60_000);
        b.forget(1);
        assert!(b.card(1, 1_000).is_none());
    }

    #[test]
    fn guilds_are_counted_by_the_bots_that_report_them() {
        let b = GuildBook::default();
        b.note_member(1, member(5, "Silent Wardens"));
        b.note_member(2, member(5, "Silent Wardens"));
        b.note_member(3, member(6, "Iron Pilgrims"));
        assert_eq!(b.guild_count(), 2);
        assert_eq!(b.guild(5).unwrap().bots.len(), 2);
        // a bot that moves to another guild leaves the first; an empty guild is gone
        b.note_member(3, member(5, "Silent Wardens"));
        assert_eq!(b.guild_count(), 1);
        b.note_member(1, None);
        b.note_member(2, None);
        b.note_member(3, None);
        assert_eq!(b.guild_count(), 0);
        // reporting twice does not duplicate
        b.note_member(1, member(5, "Silent Wardens"));
        b.note_member(1, member(5, "Silent Wardens"));
        assert_eq!(b.guild(5).unwrap().bots.len(), 1);
    }

    #[test]
    fn foundings_count_against_the_cap_until_released_or_old() {
        let b = GuildBook::default();
        assert!(b.reserve(1, 2, 0));
        assert!(b.reserve(2, 2, 0));
        assert!(!b.reserve(3, 2, 0), "two foundings fill a cap of two");
        assert_eq!(b.foundings(), 2);
        b.release(1);
        assert!(b.room_for_guild(2, 10));
        // a founding that never ended stops counting
        assert!(!b.room_for_guild(1, 10));
        assert!(b.room_for_guild(1, FOUNDING_TTL_MS + 1));
        // a guild that exists counts too
        let b = GuildBook::default();
        b.note_member(1, member(5, "A"));
        assert!(!b.reserve(1, 1, 0));
        assert!(b.reserve(1, 2, 0));
        // a cap of zero means none
        assert!(!GuildBook::default().reserve(1, 0, 0));
        assert!(!GuildBook::default().room_for_guild(0, 0));
    }

    #[test]
    fn names_are_compared_without_case() {
        let b = GuildBook::default();
        assert!(!b.name_taken("Silent Wardens"));
        b.note_name("Silent Wardens");
        assert!(b.name_taken("silent wardens"));
        assert!(b.name_taken("  SILENT WARDENS "));
        assert!(!b.name_taken("Silent Warden"));
        b.note_name("");
        assert!(!b.name_taken(""));
        // a guild a bot reports is taken too
        b.note_member(1, member(5, "Iron Pilgrims"));
        assert!(b.name_taken("Iron Pilgrims"));
    }

    #[test]
    fn a_recruit_is_an_unguilded_bot_of_the_right_faction_and_is_claimed() {
        let b = GuildBook::default();
        let mut rng = fastrand::Rng::with_seed(7);
        b.update_card(card(1, 101, 10, CAN_JOIN), 1_000);
        b.update_card(card(2, 101, 3, CAN_JOIN), 1_000); // too low
        b.update_card(card(3, 115, 10, CAN_JOIN), 1_000); // other faction
        b.update_card(card(4, 101, 10, 0), 1_000); // busy
        b.update_card(card(5, 101, 10, CAN_JOIN | WISHING), 1_000); // waits for a team
        let mut guilded = card(6, 101, 10, CAN_JOIN);
        guilded.guild = 9;
        b.update_card(guilded, 1_000);
        b.update_card(card(7, 101, 10, CAN_JOIN), 1_000); // the recruiter
        let got = b.pick_recruit(101, 5, 7, 2_000, &mut rng).unwrap();
        assert_eq!(got.bot, 1);
        // claimed: nobody else is left until the claim runs out
        assert!(b.pick_recruit(101, 5, 7, 3_000, &mut rng).is_none());
        let again = b.pick_recruit(101, 5, 7, 2_000 + CLAIM_MS + 1, &mut rng);
        // (the card went stale by then)
        assert!(again.is_none());
        b.update_card(card(1, 101, 10, CAN_JOIN), 2_000 + CLAIM_MS);
        assert_eq!(
            b.pick_recruit(101, 5, 7, 2_000 + CLAIM_MS + 1, &mut rng)
                .unwrap()
                .bot,
            1
        );
        // an answered invitation frees the bot
        b.unclaim(1);
        assert!(b
            .pick_recruit(101, 5, 7, 2_000 + CLAIM_MS + 2, &mut rng)
            .is_some());
    }

    #[test]
    fn the_recruit_is_drawn_among_the_candidates() {
        let b = GuildBook::default();
        for i in 1..=6 {
            b.update_card(card(i, 101, 10, CAN_JOIN), 1_000);
        }
        let mut seen = FxHashSet::default();
        for seed in 0..40 {
            let mut rng = fastrand::Rng::with_seed(seed);
            let got = b.pick_recruit(101, 5, 0, 1_000, &mut rng).unwrap();
            seen.insert(got.bot);
            b.unclaim(got.bot);
        }
        assert!(seen.len() >= 4, "{seen:?}");
    }

    #[test]
    fn a_guild_invites_at_a_pace() {
        let b = GuildBook::default();
        assert!(!b.take_invite_slot(5, 0, 1000), "no such guild");
        b.note_member(1, member(5, "A"));
        assert!(b.take_invite_slot(5, 100_000, 15_000));
        assert!(!b.take_invite_slot(5, 110_000, 15_000));
        assert!(b.take_invite_slot(5, 115_000, 15_000));
    }

    #[test]
    fn guild_chat_is_limited_per_guild_and_for_all_together() {
        let b = GuildBook::default();
        b.note_member(1, member(5, "A"));
        b.note_member(2, member(6, "B"));
        b.note_member(3, member(7, "C"));
        assert!(b.take_line(5, 100_000, 90_000, 2));
        assert!(
            !b.take_line(5, 110_000, 90_000, 2),
            "the guild spoke a moment ago"
        );
        assert!(b.take_line(6, 110_000, 90_000, 2));
        assert!(
            !b.take_line(7, 111_000, 90_000, 2),
            "two lines in the minute"
        );
        // the next minute starts afresh
        assert!(b.take_line(7, 161_000, 90_000, 2));
        assert!(b.take_line(5, 200_001, 90_000, 2));
    }
}
