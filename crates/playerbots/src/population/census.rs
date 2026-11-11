//! The census: which real players the bots can see, and where.
//!
//! Every bot looks at the characters around it once a second (its entity view, about 128 m) and
//! hands the ones that are not its own kind to the process-wide [`Census`]: character id, name,
//! level, side, continent, zone and position. An entry lives [`TTL_MS`] after the last sighting
//! and the table holds [`MAX_PLAYERS`] at most. A bot sees only what is near it, so the census is a
//! *lower bound* of the players online; the server API (see [`super::serverapi`]) gives the exact
//! count and the web UI shows the difference as "unlocated".

use parking_lot::Mutex;
use pb_web::{Alliance, Continent};
use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use super::region::{alliance_of_faction, place_at, Geo};
use crate::bot::state::{BotState, UnitKind};

/// A player is counted this long after a bot last saw it, in milliseconds.
pub const TTL_MS: u64 = 60_000;
/// The most players the census holds (the one seen longest ago goes first).
pub const MAX_PLAYERS: usize = 2048;

/// What a bot saw of a player.
#[derive(Debug, Clone, PartialEq)]
pub struct Seen {
    pub name: Box<str>,
    pub level: u8,
    /// Map position.
    pub pos: [f32; 2],
    pub alliance: Alliance,
    pub continent: Continent,
    pub zone_key: u16,
    /// Unix milliseconds of the sighting.
    pub at_ms: u64,
    /// The bot that saw the player.
    pub by: u32,
}

/// The table of players in view. See the module docs.
#[derive(Debug, Default)]
pub struct Census {
    players: Mutex<FxHashMap<u32, Seen>>,
}

impl Census {
    /// Records sightings of players, each as `(character id, what was seen)`.
    pub fn see_all(&self, batch: impl IntoIterator<Item = (u32, Seen)>) {
        let mut g = self.players.lock();
        for (id, mut seen) in batch {
            match g.get_mut(&id) {
                Some(old) => {
                    // a duel flag or a transformation hides the side for a moment: keep the last
                    // side that was known
                    if seen.alliance == Alliance::Unknown {
                        seen.alliance = old.alliance;
                    }
                    *old = seen;
                }
                None => {
                    if g.len() >= MAX_PLAYERS {
                        let oldest = g.iter().min_by_key(|(_, s)| s.at_ms).map(|(k, _)| *k);
                        if let Some(k) = oldest {
                            g.remove(&k);
                        }
                    }
                    g.insert(id, seen);
                }
            }
        }
    }

    /// Forgets the players nobody has seen for [`TTL_MS`].
    pub fn prune(&self, now_ms: u64) {
        self.players
            .lock()
            .retain(|_, s| now_ms.saturating_sub(s.at_ms) < TTL_MS);
    }

    /// The players seen within [`TTL_MS`] of `now_ms`, most recent first.
    pub fn fresh(&self, now_ms: u64) -> Vec<(u32, Seen)> {
        let mut v: Vec<(u32, Seen)> = self
            .players
            .lock()
            .iter()
            .filter(|(_, s)| now_ms.saturating_sub(s.at_ms) < TTL_MS)
            .map(|(k, s)| (*k, s.clone()))
            .collect();
        v.sort_by(|a, b| b.1.at_ms.cmp(&a.1.at_ms).then(a.0.cmp(&b.0)));
        v
    }

    /// Players per side `[Nuia, Haranya]` seen within [`TTL_MS`]; players of no side (a faction the
    /// data does not place) are not counted.
    pub fn per_side(&self, now_ms: u64) -> [u32; 2] {
        let mut n = [0u32; 2];
        for s in self.players.lock().values() {
            if now_ms.saturating_sub(s.at_ms) >= TTL_MS {
                continue;
            }
            if let Some(i) = s.alliance.side() {
                n[i] += 1;
            }
        }
        n
    }

    /// Number of entries (fresh or not).
    pub fn len(&self) -> usize {
        self.players.lock().len()
    }

    /// Whether the table is empty.
    pub fn is_empty(&self) -> bool {
        self.players.lock().is_empty()
    }
}

/// One bot's once-a-second look at the characters in its view: the ones that are neither the bot
/// itself nor one of ours go to the census. `is_ours` says whether a character id is one of our
/// bots. Returns how many players were seen.
pub fn observe(
    state: &BotState,
    census: &Census,
    geo: &dyn Geo,
    is_ours: &dyn Fn(u32) -> bool,
    bot: u32,
    now_ms: u64,
) -> usize {
    let mut batch: SmallVec<[(u32, Seen); 8]> = SmallVec::new();
    for u in state.units.values() {
        if u.kind != UnitKind::Character || u.id == state.me.char_id || is_ours(u.id) {
            continue;
        }
        let place = place_at(geo, u.pos[0], u.pos[1]);
        batch.push((
            u.id,
            Seen {
                name: u.name.clone().unwrap_or_default(),
                level: u.level,
                pos: [u.pos[0], u.pos[1]],
                alliance: alliance_of_faction(geo, u.faction),
                continent: place.continent,
                zone_key: place.zone_key,
                at_ms: now_ms,
                by: bot,
            },
        ));
    }
    let n = batch.len();
    if n > 0 {
        census.see_all(batch);
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::state::Unit;
    use crate::population::region::fake::Table;

    fn seen(name: &str, alliance: Alliance, at_ms: u64) -> Seen {
        Seen {
            name: name.into(),
            level: 10,
            pos: [10.0, 10.0],
            alliance,
            continent: Continent::West,
            zone_key: 5,
            at_ms,
            by: 1,
        }
    }

    #[test]
    fn players_expire_after_the_ttl_and_are_counted_per_side() {
        let c = Census::default();
        c.see_all([
            (1, seen("A", Alliance::Nuia, 1_000)),
            (2, seen("B", Alliance::Haranya, 1_000)),
            (3, seen("C", Alliance::Haranya, 30_000)),
            (4, seen("D", Alliance::Other, 30_000)),
            (5, seen("E", Alliance::Unknown, 30_000)),
        ]);
        assert_eq!(c.per_side(31_000), [1, 2]);
        assert_eq!(c.fresh(31_000).len(), 5);
        // A and B are older than the ttl a minute after their sighting
        assert_eq!(c.per_side(61_000), [0, 1]);
        assert_eq!(c.fresh(61_000).len(), 3);
        c.prune(61_000);
        assert_eq!(c.len(), 3);
        c.prune(200_000);
        assert!(c.is_empty());
    }

    #[test]
    fn a_new_sighting_replaces_the_old_and_keeps_the_known_side() {
        let c = Census::default();
        c.see_all([(7, seen("Dana", Alliance::Haranya, 1_000))]);
        let mut again = seen("Dana", Alliance::Unknown, 5_000);
        again.pos = [99.0, 5.0];
        c.see_all([(7, again)]);
        let v = c.fresh(6_000);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].1.alliance, Alliance::Haranya, "the side is kept");
        assert_eq!(v[0].1.pos, [99.0, 5.0], "the position is the new one");
        assert_eq!(v[0].1.at_ms, 5_000);
        // a side that is known replaces the old one
        c.see_all([(7, seen("Dana", Alliance::Nuia, 7_000))]);
        assert_eq!(c.fresh(8_000)[0].1.alliance, Alliance::Nuia);
    }

    #[test]
    fn the_most_recent_player_is_listed_first() {
        let c = Census::default();
        c.see_all([
            (1, seen("old", Alliance::Nuia, 1_000)),
            (2, seen("new", Alliance::Nuia, 9_000)),
            (3, seen("mid", Alliance::Nuia, 5_000)),
        ]);
        let names: Vec<_> = c.fresh(10_000).into_iter().map(|(_, s)| s.name).collect();
        assert_eq!(
            names,
            vec!["new".into(), "mid".into(), "old".into()] as Vec<Box<str>>
        );
    }

    #[test]
    fn the_table_is_bounded_and_drops_the_oldest() {
        let c = Census::default();
        for i in 0..MAX_PLAYERS as u32 {
            c.see_all([(i + 1, seen("p", Alliance::Nuia, 1_000 + u64::from(i)))]);
        }
        assert_eq!(c.len(), MAX_PLAYERS);
        c.see_all([(99_999, seen("late", Alliance::Nuia, 500_000))]);
        assert_eq!(c.len(), MAX_PLAYERS);
        let ids: Vec<u32> = c.fresh(500_001).into_iter().map(|(i, _)| i).collect();
        assert!(ids.contains(&99_999));
        assert!(!ids.contains(&1), "the oldest sighting went");
    }

    fn unit(kind: UnitKind, id: u32, x: f32, faction: u32, name: Option<&str>) -> Unit {
        Unit {
            kind,
            id,
            pos: [x, 20.0, 5.0],
            hp: 100,
            faction,
            level: 33,
            flags: 0,
            target: 0,
            name: name.map(Into::into),
        }
    }

    #[test]
    fn a_bot_reports_the_players_in_view_but_not_itself_its_kin_or_monsters() {
        let mut st = BotState::new();
        st.me.char_id = 50;
        // obj id -> unit
        st.units
            .insert(1, unit(UnitKind::Character, 101, 500.0, 108, Some("Haru")));
        st.units
            .insert(2, unit(UnitKind::Character, 102, 1500.0, 101, Some("Nia")));
        st.units
            .insert(3, unit(UnitKind::Character, 50, 10.0, 101, Some("me")));
        st.units
            .insert(4, unit(UnitKind::Character, 777, 10.0, 101, Some("ourbot")));
        st.units.insert(5, unit(UnitKind::Npc, 9, 10.0, 115, None));
        let census = Census::default();
        let ours = |id: u32| id == 777;
        let n = observe(&st, &census, &Table, &ours, 3, 10_000);
        assert_eq!(n, 2);
        let mut seen = census.fresh(10_001);
        seen.sort_by_key(|(id, _)| *id);
        assert_eq!(seen[0].0, 101);
        assert_eq!(&*seen[0].1.name, "Haru");
        assert_eq!(seen[0].1.alliance, Alliance::Haranya);
        assert_eq!(
            seen[0].1.continent,
            Continent::West,
            "where it stands, not its side"
        );
        assert_eq!(seen[0].1.zone_key, 5);
        assert_eq!(seen[0].1.by, 3);
        assert_eq!(seen[0].1.level, 33);
        assert_eq!(seen[1].0, 102);
        assert_eq!(seen[1].1.alliance, Alliance::Nuia);
        assert_eq!(seen[1].1.continent, Continent::East);
        // nobody around: no lock is taken, nothing is recorded
        let empty = BotState::new();
        assert_eq!(observe(&empty, &census, &Table, &ours, 3, 20_000), 0);
    }
}
