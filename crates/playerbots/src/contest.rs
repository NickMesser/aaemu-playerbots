//! What the bots are working on, as one small process-wide view.
//!
//! Many bots start in the same few places and want the same quests, so they all hunt the same
//! three boars or search the same herb patch. Each bot *claims* the monster it fights, the doodad
//! it harvests and the spawn spot it walks to; the others see the claims in [`Contest`] and
//! prefer targets nobody holds, so the bots spread over the spawn points of one template instead
//! of queueing at one.
//!
//! A claim is a hint, not a lock: it expires by itself ([`CLAIM_MS`]), a bot that finds nothing
//! else still takes a claimed target, and a bot that goes offline drops its claims. The table is
//! bounded ([`MAX_KEYS`] keys, two holders per key at most are kept inline) and sits behind one
//! short [`Mutex`]: bots touch it a few times a second when they choose a target, never on a hot
//! path.

use std::time::Instant;

use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use smallvec::SmallVec;

/// How long a claim holds without being renewed.
pub const CLAIM_MS: u64 = 45_000;
/// Most keys the table keeps.
pub const MAX_KEYS: usize = 512;
/// Metres per side of a spawn-spot cell.
pub const CELL_M: f32 = 20.0;

const TAG_UNIT: u64 = 1 << 56;
const TAG_DOODAD: u64 = 2 << 56;
const TAG_SPOT: u64 = 3 << 56;
const TAG_PLOT: u64 = 4 << 56;

/// The key of a monster by its object id.
pub fn unit_key(obj: u32) -> u64 {
    TAG_UNIT | u64::from(obj)
}

/// The key of a doodad by its object id.
pub fn doodad_key(obj: u32) -> u64 {
    TAG_DOODAD | u64::from(obj)
}

/// The key of the spawn-spot cell around (x, y).
pub fn spot_key(x: f32, y: f32) -> u64 {
    let cx = (x / CELL_M).floor() as i32 as u32;
    let cy = (y / CELL_M).floor() as i32 as u32;
    TAG_SPOT | (u64::from(cx) << 32) | u64::from(cy)
}

/// Metres per side of a planting cell.
pub const PLOT_CELL_M: f32 = 3.0;

/// The key of the planting cell around (x, y): a bot that is about to plant claims it so that the
/// others look for another spot.
pub fn plot_key(x: f32, y: f32) -> u64 {
    let cx = (x / PLOT_CELL_M).floor() as i32 as u32;
    let cy = (y / PLOT_CELL_M).floor() as i32 as u32;
    TAG_PLOT | (u64::from(cx) << 32) | u64::from(cy)
}

/// `(bot, claim ends at ms)`.
type Holders = SmallVec<[(u32, u64); 2]>;

/// The claims of all bots. See the module docs.
#[derive(Debug)]
pub struct Contest {
    started: Instant,
    claims: Mutex<FxHashMap<u64, Holders>>,
}

impl Default for Contest {
    fn default() -> Self {
        Contest::new()
    }
}

impl Contest {
    /// An empty table; its clock starts now.
    pub fn new() -> Contest {
        Contest {
            started: Instant::now(),
            claims: Mutex::new(FxHashMap::default()),
        }
    }

    /// Milliseconds on the table's clock (the same for every bot).
    pub fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// Bot `bot` works on `key` for the next [`CLAIM_MS`].
    pub fn claim(&self, key: u64, bot: u32) {
        self.claim_at(key, bot, self.now_ms());
    }

    /// Bot `bot` is done with `key`.
    pub fn release(&self, key: u64, bot: u32) {
        let mut map = self.claims.lock();
        if let Some(h) = map.get_mut(&key) {
            h.retain(|e| e.0 != bot);
            if h.is_empty() {
                map.remove(&key);
            }
        }
    }

    /// Drops every claim of `bot` (it went offline).
    pub fn release_bot(&self, bot: u32) {
        let mut map = self.claims.lock();
        map.retain(|_, h| {
            h.retain(|e| e.0 != bot);
            !h.is_empty()
        });
    }

    /// How many bots other than `bot` hold `key` now.
    pub fn others(&self, key: u64, bot: u32) -> usize {
        self.others_at(key, bot, self.now_ms())
    }

    /// The lowest bot id holding `key` now: the one that goes first when several want the same
    /// single target (the others wait in id order rather than all casting at it).
    pub fn first_holder(&self, key: u64) -> Option<u32> {
        self.first_holder_at(key, self.now_ms())
    }

    /// Keys held at all (expired claims not yet swept are counted).
    pub fn len(&self) -> usize {
        self.claims.lock().len()
    }

    /// Whether nothing is claimed.
    pub fn is_empty(&self) -> bool {
        self.claims.lock().is_empty()
    }

    // The same with an explicit clock, for the tests.

    fn claim_at(&self, key: u64, bot: u32, now_ms: u64) {
        let until = now_ms + CLAIM_MS;
        let mut map = self.claims.lock();
        if !map.contains_key(&key) && map.len() >= MAX_KEYS {
            Self::make_room(&mut map, now_ms);
        }
        let h = map.entry(key).or_default();
        match h.iter_mut().find(|e| e.0 == bot) {
            Some(e) => e.1 = until,
            None => {
                h.retain(|e| e.1 > now_ms);
                h.push((bot, until));
            }
        }
    }

    fn others_at(&self, key: u64, bot: u32, now_ms: u64) -> usize {
        self.claims.lock().get(&key).map_or(0, |h| {
            h.iter().filter(|e| e.0 != bot && e.1 > now_ms).count()
        })
    }

    fn first_holder_at(&self, key: u64, now_ms: u64) -> Option<u32> {
        self.claims
            .lock()
            .get(&key)
            .and_then(|h| h.iter().filter(|e| e.1 > now_ms).map(|e| e.0).min())
    }

    /// Sweeps the expired claims; when the table is still full, the key that expires first goes.
    fn make_room(map: &mut FxHashMap<u64, Holders>, now_ms: u64) {
        map.retain(|_, h| {
            h.retain(|e| e.1 > now_ms);
            !h.is_empty()
        });
        if map.len() >= MAX_KEYS {
            let oldest = map
                .iter()
                .min_by_key(|(_, h)| h.iter().map(|e| e.1).max().unwrap_or(0))
                .map(|(k, _)| *k);
            if let Some(k) = oldest {
                map.remove(&k);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_claim_is_seen_by_the_others_but_not_by_its_holder() {
        let c = Contest::new();
        let k = unit_key(42);
        assert_eq!(c.others_at(k, 1, 0), 0);
        c.claim_at(k, 1, 0);
        assert_eq!(c.others_at(k, 1, 10), 0, "own claim");
        assert_eq!(c.others_at(k, 2, 10), 1);
        c.claim_at(k, 3, 20);
        assert_eq!(c.others_at(k, 2, 30), 2);
        assert_eq!(c.others_at(unit_key(43), 2, 30), 0, "another key");
    }

    #[test]
    fn the_lowest_holder_goes_first() {
        let c = Contest::new();
        let k = unit_key(9);
        assert_eq!(c.first_holder_at(k, 0), None);
        c.claim_at(k, 7, 0);
        c.claim_at(k, 3, 0);
        assert_eq!(c.first_holder_at(k, 10), Some(3));
        c.release(k, 3);
        assert_eq!(c.first_holder_at(k, 10), Some(7));
        assert_eq!(c.first_holder_at(k, CLAIM_MS), None, "expired");
    }

    #[test]
    fn claims_expire_unless_renewed() {
        let c = Contest::new();
        let k = spot_key(100.0, 200.0);
        c.claim_at(k, 1, 0);
        assert_eq!(c.others_at(k, 2, CLAIM_MS - 1), 1);
        assert_eq!(c.others_at(k, 2, CLAIM_MS), 0);
        c.claim_at(k, 1, CLAIM_MS - 5);
        assert_eq!(c.others_at(k, 2, CLAIM_MS + 100), 1, "renewed");
    }

    #[test]
    fn release_and_release_bot_free_the_keys() {
        let c = Contest::new();
        let (a, b) = (unit_key(1), doodad_key(1));
        c.claim_at(a, 7, 0);
        c.claim_at(b, 7, 0);
        c.claim_at(b, 8, 0);
        c.release(a, 7);
        assert_eq!(c.others_at(a, 9, 1), 0);
        assert_eq!(c.len(), 1);
        c.release_bot(7);
        assert_eq!(c.others_at(b, 9, 1), 1, "only bot 8 is left on it");
        c.release_bot(8);
        assert!(c.is_empty());
    }

    #[test]
    fn the_table_is_bounded() {
        let c = Contest::new();
        for i in 0..(MAX_KEYS as u32 * 3) {
            c.claim_at(unit_key(i), 1, u64::from(i));
        }
        assert!(c.len() <= MAX_KEYS, "{} keys", c.len());
        // the newest claims survive, the oldest went first
        assert_eq!(c.others_at(unit_key(MAX_KEYS as u32 * 3 - 1), 2, 2_000), 1);
        assert_eq!(c.others_at(unit_key(0), 2, 2_000), 0);
    }

    #[test]
    fn expired_claims_make_room_before_live_ones_are_dropped() {
        let c = Contest::new();
        for i in 0..MAX_KEYS as u32 {
            c.claim_at(unit_key(i), 1, 0);
        }
        // everything has expired by now; a new key sweeps the table
        c.claim_at(unit_key(10_000), 2, CLAIM_MS + 1);
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn spot_keys_group_a_cell_and_tell_cells_apart() {
        assert_eq!(spot_key(1.0, 1.0), spot_key(19.9, 19.9));
        assert_ne!(spot_key(19.9, 0.0), spot_key(20.1, 0.0));
        assert_ne!(spot_key(-0.1, 0.0), spot_key(0.1, 0.0));
        assert_ne!(unit_key(5), doodad_key(5));
        assert_ne!(unit_key(5), spot_key(5.0, 0.0));
    }
}
