//! The bots' journal: what each bot remembers between runs.
//!
//! The roster says who a bot is; the journal says what it has been through: the places it has
//! seen, the wild plots it planted, its play sessions, the notes the LLM or the operator left it,
//! what it learned about gear. One JSON file (`bots.journal`, default `journal.json` next to the
//! roster) holds every bot's [`Journal`], written atomically like the roster: a change only marks
//! the book dirty and wakes the saver task, which writes the file at most every
//! [`SAVE_INTERVAL`] on a blocking thread, and once more at shutdown.
//!
//! Each section belongs to one feature and lives in its own file:
//!
//! | Section | Type | Owner |
//! | --- | --- | --- |
//! | `seen` | [`Seen`] | exploring (zones and places the bot has been) |
//! | `wild` | [`WildPlots`] | wild farms (plots out in the open, which no server packet lists) |
//! | `sessions` | [`Sessions`] | presence (log-ins, play time) |
//! | `notes` | [`Notes`] | the LLM and the agent API (short memories in the bot's own words) |
//! | `gear` | [`GearNotes`] | gear (what the bot learned about its equipment) |
//!
//! A section must stay small and bounded (a few dozen entries at most: the file holds every bot)
//! and every field has a serde default, so a journal written by an older build loads.
//!
//! Bots read and change their journal through [`JournalBook::read`] and
//! [`JournalBook::update`] (`ctx.hub.journal`); both hold one short mutex and never wait for the
//! disk.

mod companions;
mod gear;
mod notes;
mod seen;
mod sessions;
mod wild;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub use companions::{Mediation, MediationReply, Preference, Relationships};
pub use gear::GearNotes;
pub use notes::{Note, Notes, MAX_NOTES, MAX_NOTE_CHARS};
pub use seen::{Seen, MAX_ZONES};
pub use sessions::Sessions;
pub use wild::{WildPlot, WildPlots, MAX_WILD_PLOTS};

use crate::roster::write_atomic;

/// File format version of the journal.
pub const JOURNAL_VERSION: u32 = 1;
/// The journal is written at most this often while bots change it.
pub const SAVE_INTERVAL: Duration = Duration::from_secs(10);

/// What one bot remembers. See the module docs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Journal {
    /// Zones and places the bot has been (the explore feature).
    pub seen: Seen,
    /// Plots planted out in the wild (the wild-farm feature).
    pub wild: WildPlots,
    /// Log-ins and play time (the presence feature).
    pub sessions: Sessions,
    /// Short memories (the LLM and the agent API).
    pub notes: Notes,
    /// What the bot learned about its gear (the gear feature).
    pub gear: GearNotes,
    /// Server-confirmed membership in a thief guild, retained after departure.
    pub thief_history: bool,
    /// Structured memories of characters the bot has played with.
    pub relationships: Relationships,
    /// Outstanding request to speak to another bot about a damaged relationship.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mediation: Option<Mediation>,
    /// Last time a player brought this bot a reply to a mediation request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_mediation_ms: Option<u64>,
    /// Number of production orders with a confirmed inventory increase.
    pub production_completed: u32,
    /// Number of proficiency-training recipes with a confirmed result.
    pub production_training_completed: u32,
    /// The current guildmaster's instruction for this member.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub production_dispatch: Option<crate::bot::goals::work::assignment::Dispatch>,
    /// A customer's pending craft and the stock reserved for the exchange.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub craft_commission: Option<crate::bot::goals::work::customer::Commission>,
    /// Trade runs by pack item and destination buyer, most recent first.
    pub trade_routes: Vec<TradeRoute>,
    /// Sport fishing turn-ins confirmed by equipment removal and purse credit.
    pub fish_turn_ins: u32,
    /// Copper received from fish stands.
    pub fish_sale_copper: u64,
    /// Outstanding player request and number of messages handed off.
    pub delivery: Option<crate::bot::goals::delivery::DeliveryOrder>,
    pub deliveries_completed: u32,
    pub scouting_reports: u32,
    /// Trade escorts that reached a buyer with the party leader and cargo.
    pub escorts_completed: u32,
    /// Unpaid commissions and confirmed contributions by server guild.
    pub guild_treasury: BTreeMap<u32, crate::bot::guild::treasury::Account>,
    /// Confirmed contracted purchases by server guild.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub guild_supply_receipts: BTreeMap<u32, crate::bot::guild::supply::Receipt>,
    /// Copper claimed from authenticated guild contribution mail by this guildmaster.
    pub guild_copper_received: u64,
    /// Recent sales whose items and copper changed as agreed.
    pub market_sales: Vec<crate::bot::guild::prices::Receipt>,
    /// Unsettled resale, including the purchase cost and any confirmed posting fee.
    pub auction_resale: Option<crate::bot::goals::auction::resale::Resale>,
    pub auction_resale_profit: i64,
    /// Places where this bot recovered a pack after a guild ambush.
    pub raid_sites: Vec<RaidSite>,
    /// Naval volleys that the server credited with a hit on the selected ship.
    pub naval_hits: u32,
    /// Pirate Treasure Chests whose loot was confirmed.
    pub treasure_chests: u32,
    /// Server-confirmed kills of characters reported for attacking trade runners.
    pub bounties_completed: u32,
    /// Confirmed feed, treatment and collection actions on guild livestock.
    pub livestock_actions: u32,
    pub sieges_won: u32,
    pub sieges_held: u32,
    pub sieges_declared: u32,
    /// The server's auction start timestamp for the most recent siege camp bid attempt.
    pub siege_camp_bid_cycle: i64,
    /// Earliest wall-clock time when this character can set out for another rift patrol.
    pub rift_patrol_after_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TradeRoute {
    pub item: u32,
    pub buyer: u32,
    #[serde(default)]
    pub sea: bool,
    pub sales: u32,
    pub failed_runs: u32,
    /// Latest gross price estimate, before expenses and mail settlement.
    pub quoted_copper: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_material_copper: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RaidSite {
    pub position: [f32; 2],
    pub recovered: u16,
}

impl Journal {
    pub fn note_raid_site(&mut self, position: [f32; 2]) {
        let index = self.raid_sites.iter().position(|site| {
            (site.position[0] - position[0]).hypot(site.position[1] - position[1]) <= 150.0
        });
        let recovered = index.map_or(1, |index| {
            self.raid_sites.remove(index).recovered.saturating_add(1)
        });
        self.raid_sites.insert(
            0,
            RaidSite {
                position,
                recovered,
            },
        );
        self.raid_sites.truncate(12);
    }

    pub fn note_trade_route(
        &mut self,
        item: u32,
        buyer: u32,
        sea: bool,
        quote: u32,
        materials: Option<u32>,
        sold: bool,
    ) {
        let index = self
            .trade_routes
            .iter()
            .position(|r| r.item == item && r.buyer == buyer && r.sea == sea);
        let mut route = index
            .map(|i| self.trade_routes.remove(i))
            .unwrap_or(TradeRoute {
                item,
                buyer,
                sea,
                ..TradeRoute::default()
            });
        route.quoted_copper = quote;
        if materials.is_some() {
            route.estimated_material_copper = materials;
        }
        if sold {
            route.sales = route.sales.saturating_add(1);
        } else {
            route.failed_runs = route.failed_runs.saturating_add(1);
        }
        self.trade_routes.insert(0, route);
        self.trade_routes.truncate(16);
    }
}

#[derive(Serialize, Deserialize)]
struct JournalFile {
    version: u32,
    #[serde(default)]
    bots: BTreeMap<u32, Journal>,
}

/// The journals of all bots, shared by the bot tasks through the hub. See the module docs.
#[derive(Debug)]
pub struct JournalBook {
    inner: parking_lot::Mutex<BTreeMap<u32, Journal>>,
    /// Where the book is written; `None` keeps it in memory only (tests, a disabled journal).
    path: Option<PathBuf>,
    /// Changed since the last successful write.
    dirty: AtomicBool,
    /// Wakes the saver.
    wake: Notify,
    /// Successful writes so far.
    writes: AtomicU64,
}

impl Default for JournalBook {
    fn default() -> Self {
        JournalBook::memory()
    }
}

/// The empty journal [`JournalBook::read`] shows for a bot that has none yet.
static EMPTY: std::sync::LazyLock<Journal> = std::sync::LazyLock::new(Journal::default);

impl JournalBook {
    /// A book that is never written to disk.
    pub fn memory() -> JournalBook {
        JournalBook {
            inner: parking_lot::Mutex::new(BTreeMap::new()),
            path: None,
            dirty: AtomicBool::new(false),
            wake: Notify::new(),
            writes: AtomicU64::new(0),
        }
    }

    /// Reads the journal file at `path`; a missing file is an empty book that will be written
    /// there.
    pub fn load(path: &Path) -> anyhow::Result<JournalBook> {
        let mut book = JournalBook::memory();
        book.path = Some(path.to_path_buf());
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(book),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let bots =
            JournalBook::parse(&text).with_context(|| format!("parsing {}", path.display()))?;
        *book.inner.get_mut() = bots;
        Ok(book)
    }

    fn parse(text: &str) -> anyhow::Result<BTreeMap<u32, Journal>> {
        let file: JournalFile = serde_json::from_str(text)?;
        ensure!(
            file.version == JOURNAL_VERSION,
            "journal version {} is not supported (expected {JOURNAL_VERSION})",
            file.version
        );
        Ok(file.bots)
    }

    /// The journal file text.
    pub fn to_json(&self) -> String {
        let bots = self.inner.lock().clone();
        to_json(bots)
    }

    /// Runs `f` on the journal of bot `id` (an empty one when the bot has none yet).
    pub fn read<R>(&self, id: u32, f: impl FnOnce(&Journal) -> R) -> R {
        let g = self.inner.lock();
        f(g.get(&id).unwrap_or(&EMPTY))
    }

    /// Runs `f` on the journal of bot `id` (created on first use) and marks the book for the next
    /// write. Nothing is written here: a bot's tick never waits for the disk.
    pub fn update<R>(&self, id: u32, f: impl FnOnce(&mut Journal) -> R) -> R {
        let r = f(self.inner.lock().entry(id).or_default());
        self.dirty.store(true, Ordering::Release);
        self.wake.notify_one();
        r
    }

    /// The ids of the bots that have a journal.
    pub fn ids(&self) -> Vec<u32> {
        self.inner.lock().keys().copied().collect()
    }

    /// Whether there are changes that are not on disk yet.
    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Acquire)
    }

    /// How many times the file was written.
    pub fn writes(&self) -> u64 {
        self.writes.load(Ordering::Relaxed)
    }

    /// Writes the book now, on this thread, if it changed (the end of the process).
    pub fn flush_blocking(&self) {
        let Some(path) = &self.path else { return };
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return;
        }
        let text = self.to_json();
        self.finish_write(write_atomic(path, &text));
    }

    fn finish_write(&self, result: anyhow::Result<()>) {
        match result {
            Ok(()) => {
                self.writes.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                tracing::warn!("saving the journal: {e:#}");
                self.dirty.store(true, Ordering::Release);
            }
        }
    }

    /// Like [`JournalBook::flush_blocking`], but the write runs on a blocking thread.
    pub async fn flush(self: &Arc<Self>) {
        let Some(path) = self.path.clone() else {
            return;
        };
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return;
        }
        let bots = self.inner.lock().clone();
        let result = tokio::task::spawn_blocking(move || write_atomic(&path, &to_json(bots)))
            .await
            .unwrap_or_else(|e| Err(anyhow::anyhow!("the writer thread failed: {e}")));
        self.finish_write(result);
    }

    /// Starts the saver task: after a change it waits `interval` for more changes, then writes the
    /// file once; when stopped it writes once more.
    pub fn spawn_saver(self: &Arc<Self>, interval: Duration) -> Saver {
        let stop = CancellationToken::new();
        let me = self.clone();
        let token = stop.clone();
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = token.cancelled() => break,
                    _ = me.wake.notified() => {}
                }
                tokio::select! {
                    _ = token.cancelled() => break,
                    _ = tokio::time::sleep(interval) => {}
                }
                me.flush().await;
                if me.is_dirty() {
                    // the write failed: try again after another interval
                    me.wake.notify_one();
                }
            }
            me.flush().await;
        });
        Saver { stop, handle }
    }
}

impl Drop for JournalBook {
    fn drop(&mut self) {
        // (a path out of a normal shutdown still keeps the changes)
        self.flush_blocking();
    }
}

fn to_json(bots: BTreeMap<u32, Journal>) -> String {
    let file = JournalFile {
        version: JOURNAL_VERSION,
        bots,
    };
    // Serialising plain data cannot fail; an empty file would be detected on the next load.
    serde_json::to_string_pretty(&file).unwrap_or_default()
}

/// The saver task of a [`JournalBook`]; [`Saver::stop`] writes what is left and ends it.
#[derive(Debug)]
pub struct Saver {
    stop: CancellationToken,
    handle: JoinHandle<()>,
}

impl Saver {
    /// Stops the saver after one last write of the journal, and waits for it.
    pub async fn stop(self) {
        self.stop.cancel();
        if let Err(e) = self.handle.await {
            tracing::warn!("journal saver: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let base = std::env::var_os("AA_TEST_TMP")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = base.join(format!(
            "pb-journal-{tag}-{}-{}",
            std::process::id(),
            fastrand::u64(..)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_bot_without_a_journal_reads_an_empty_one_and_update_creates_it() {
        let book = JournalBook::memory();
        assert!(book.read(3, |j| *j == Journal::default()));
        assert!(book.ids().is_empty());
        book.update(3, |j| j.notes.push(10, "met Bob at the docks"));
        assert_eq!(book.ids(), vec![3]);
        assert!(book.is_dirty());
        assert_eq!(book.read(3, |j| j.notes.lines.len()), 1);
        // a memory book is never written
        book.flush_blocking();
        assert_eq!(book.writes(), 0);
    }

    #[test]
    fn the_file_round_trips_and_a_missing_file_is_an_empty_book() {
        let dir = temp_dir("rt");
        let path = dir.join("journal.json");
        let book = JournalBook::load(&path).unwrap();
        assert!(book.ids().is_empty());
        book.update(1, |j| {
            j.seen.add_zone(12);
            j.sessions.count = 4;
        });
        book.update(2, |j| j.notes.push(5, "likes the docks"));
        book.flush_blocking();
        assert_eq!(book.writes(), 1);
        assert!(!book.is_dirty());
        let again = JournalBook::load(&path).unwrap();
        assert_eq!(again.ids(), vec![1, 2]);
        assert_eq!(again.read(1, |j| j.seen.zones.clone()), vec![12]);
        assert_eq!(again.read(1, |j| j.sessions.count), 4);
        // nothing changed: nothing is written
        again.flush_blocking();
        assert_eq!(again.writes(), 0);
        drop(again);
        drop(book);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_old_or_partial_journal_loads_and_a_wrong_version_does_not() {
        let partial = r#"{"version":1,"bots":{"7":{"seen":{"zones":[3]}}}}"#;
        let bots = JournalBook::parse(partial).unwrap();
        assert_eq!(bots[&7].seen.zones, vec![3]);
        assert_eq!(bots[&7].notes, Notes::default());
        assert!(JournalBook::parse(r#"{"version":9,"bots":{}}"#).is_err());
        assert!(JournalBook::parse(r#"{"version":1}"#).unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_saver_writes_once_for_many_changes_and_at_stop() {
        let dir = temp_dir("saver");
        let path = dir.join("journal.json");
        let book = Arc::new(JournalBook::load(&path).unwrap());
        let saver = book.spawn_saver(Duration::from_millis(30));
        for i in 0..20 {
            book.update(i, |j| j.sessions.count += 1);
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(book.writes(), 1);
        book.update(99, |j| j.sessions.count = 1);
        saver.stop().await;
        assert!(!book.is_dirty());
        let again = JournalBook::load(&path).unwrap();
        assert_eq!(again.ids().len(), 21);
        drop(again);
        drop(book);
        let _ = std::fs::remove_dir_all(dir);
    }
}
