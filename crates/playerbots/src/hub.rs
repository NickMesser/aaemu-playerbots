//! The hub: what the bots publish for the operator web UI and how commands reach them.
//!
//! Every bot owns one [`BotSlot`] (a snapshot, a bounded event log, a quest summary and its
//! command channel). The bot task writes its slot about once a second; the web UI only ever
//! reads. [`Hub`] implements [`pb_web::BotHub`] on top of the slots plus process-wide numbers:
//! packet and byte counters (atomics the bot tasks add to), the resident memory of the process,
//! the optional LLM budget, map markers and the world map.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::{Mutex, RwLock};
use pb_web::{
    Activity, BotDetail, BotHub, BotSnapshot, Command, DirectorView, EconomyView, HubStats,
    IssueAction, IssueDetail, IssueSummary, LlmStats, LogLine, MapInfo, Marker, MarkerKind,
    PolicyPatch, QuestLine, WorldRect, WorldView,
};
use rustc_hash::{FxHashMap, FxHashSet};
use tokio::sync::mpsc;

use crate::bot::cmd::{BotCmd, GoalChoice};
use crate::bot::goals::social::gate::{BotNames, ChatHub};
use crate::bot::goals::social::SlotSocial;
use crate::bot::nav::NavService;
use crate::contest::Contest;
use crate::issues::logtail::{words, LineHook};
use crate::issues::{view, IssueReport, IssueTasks, Issues, LogLevel};
use crate::population::select::{Selector, StopRule};
use crate::population::Population;

/// Entries of a bot's event log.
pub const LOG_LINES: usize = 32;
/// Capacity of a bot's command channel.
pub const CMD_QUEUE: usize = 8;
/// How often the process memory is read at most.
const RSS_REFRESH: Duration = Duration::from_secs(2);
/// Window of the packet-rate figures.
const RATE_WINDOW: Duration = Duration::from_secs(1);
/// Server log lines the hub hands to bots per second, all together.
const LOG_FEED_PER_SEC: u32 = 20;
/// The world rectangle assumed when no map was baked (`main_world`).
const DEFAULT_WORLD: WorldRect = WorldRect {
    min_x: 0.0,
    min_y: 0.0,
    max_x: 35840.0,
    max_y: 38912.0,
};

/// Unix time in milliseconds.
pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Everything about one bot that the web UI may read.
#[derive(Debug, Default)]
pub struct SlotInner {
    /// The snapshot the map and the bot list show.
    pub snap: BotSnapshot,
    /// Event log, oldest first, at most [`LOG_LINES`] lines.
    pub log: VecDeque<LogLine>,
    /// Quest summary.
    pub quests: Vec<QuestLine>,
    /// Extra rows of the detail panel.
    pub extra: Vec<(String, String)>,
    /// The route the bot is walking, map-plane `[x, y]` points (empty when none). Written by the
    /// bot task (owned by the motion feature), shown as `BotDetail::path`.
    pub path: Vec<[f32; 2]>,
    /// The bot's crops, refreshed with the extra rows (the farming goal), shown as
    /// `BotDetail::plots`.
    pub plots: Vec<pb_web::PlotLine>,
    /// What the bot wears (the gear feature), shown as `BotDetail::gear`.
    pub gear: Option<pb_web::GearView>,
    /// The bot's personality, shown as `BotDetail::profile`.
    pub profile: Option<pb_web::ProfileView>,
    /// Chat rings: the system lines goals read and the chat lines the detail panel shows.
    pub social: SlotSocial,
    /// What the agent API shows that only the bot knows (the goals it may start now, the
    /// director's plan, what is around it), filled in by its brain (owned by the agent feature).
    pub agent: crate::agent::AgentSlot,
}

/// The public face of one bot.
#[derive(Debug)]
pub struct BotSlot {
    /// Bot id.
    pub id: u32,
    inner: Mutex<SlotInner>,
    tx: RwLock<mpsc::Sender<BotCmd>>,
    /// Character id once known (0 before).
    pub char_id: AtomicU32,
    /// Object id in the world while spawned (0 otherwise).
    pub obj_id: AtomicU32,
}

impl BotSlot {
    /// A slot with the given first snapshot and a command channel nobody listens to yet.
    pub fn new(snap: BotSnapshot) -> Arc<BotSlot> {
        let (tx, _rx) = mpsc::channel(CMD_QUEUE);
        Arc::new(BotSlot {
            id: snap.id,
            inner: Mutex::new(SlotInner {
                snap,
                ..SlotInner::default()
            }),
            tx: RwLock::new(tx),
            char_id: AtomicU32::new(0),
            obj_id: AtomicU32::new(0),
        })
    }

    /// Opens a fresh command channel for a (re)started bot task and returns its receiver.
    pub fn attach(&self) -> mpsc::Receiver<BotCmd> {
        let (tx, rx) = mpsc::channel(CMD_QUEUE);
        *self.tx.write() = tx;
        rx
    }

    /// Queues a command for the bot.
    pub fn send(&self, cmd: BotCmd) -> Result<(), String> {
        use mpsc::error::TrySendError;
        self.tx.read().try_send(cmd).map_err(|e| match e {
            TrySendError::Full(_) => format!("bot {} is busy (command queue full)", self.id),
            TrySendError::Closed(_) => format!("bot {} is not running", self.id),
        })
    }

    /// Runs `f` on the slot data.
    pub fn with<R>(&self, f: impl FnOnce(&mut SlotInner) -> R) -> R {
        f(&mut self.inner.lock())
    }

    /// Appends a line to the event log (dropping the oldest at [`LOG_LINES`]).
    pub fn push_log(&self, text: String) {
        let mut g = self.inner.lock();
        if g.log.len() >= LOG_LINES {
            g.log.pop_front();
        }
        g.log.push_back(LogLine { t: unix_ms(), text });
    }

    /// A copy of the snapshot.
    pub fn snapshot(&self) -> BotSnapshot {
        self.inner.lock().snap.clone()
    }

    /// Marks the bot offline (keeps name, class and the rest).
    pub fn set_offline(&self) {
        let mut g = self.inner.lock();
        g.snap.activity = Activity::Offline;
        g.snap.detail.clear();
        g.snap.partner = None;
        g.snap.online_secs = 0;
        g.path.clear();
        g.plots.clear();
        self.obj_id.store(0, Ordering::Relaxed);
    }
}

/// A bot's handle for writing log lines (to its slot ring and to `tracing`).
#[derive(Debug, Clone)]
pub struct Logger {
    slot: Arc<BotSlot>,
}

impl Logger {
    /// A logger for `slot`.
    pub fn new(slot: Arc<BotSlot>) -> Logger {
        Logger { slot }
    }

    /// A notable event: shown in the web UI and logged at info level.
    pub fn say(&self, text: impl Into<String>) {
        let text = text.into();
        tracing::info!("{text}");
        self.slot.push_log(text);
    }

    /// A detail: logged at debug level only.
    pub fn debug(&self, text: impl AsRef<str>) {
        tracing::debug!("{}", text.as_ref());
    }

    /// Runs `f` on the bot's slot data (the chat rings live there).
    pub fn with_slot<R>(&self, f: impl FnOnce(&mut SlotInner) -> R) -> R {
        self.slot.with(f)
    }
}

/// Packet and byte counters of all bots.
#[derive(Debug, Default)]
pub struct Counters {
    pub packets_in: AtomicU64,
    pub packets_out: AtomicU64,
    pub bytes_in: AtomicU64,
    pub bytes_out: AtomicU64,
}

/// A request to the supervisor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupCmd {
    /// Start `n` more bots.
    Spawn(u32),
    /// Stop `n` bots.
    Despawn(u32),
    /// Start `n` bots that match `sel` (the roster grows with matching races when it has too few);
    /// `owned`: the population director started them and may stop them again.
    SpawnWhere { n: u32, sel: Selector, owned: bool },
    /// Stop up to `n` bots that match `sel` and are safe to stop (see
    /// [`crate::population::select::stop_rank`]).
    DespawnWhere {
        n: u32,
        sel: Selector,
        rule: StopRule,
    },
    /// Start the roster bots `ids` that do not run (queued for the ramp like every start);
    /// `presence`: the presence schedules asked, and the bots are theirs to stop again.
    StartIds { ids: Box<[u32]>, presence: bool },
    /// Stop those of the bots `ids` that are safe to stop now (see
    /// [`crate::population::select::stop_rank`]; never one in the party of a real player); queued
    /// ones leave the queue. `presence`: only the bots the presence schedules started.
    StopIds { ids: Box<[u32]>, presence: bool },
}

#[derive(Debug)]
struct Rates {
    at: Instant,
    packets_in: u64,
    packets_out: u64,
    bytes_in: u64,
    bytes_out: u64,
    in_per_sec: f32,
    out_per_sec: f32,
    bytes_in_per_sec: f32,
    bytes_out_per_sec: f32,
}

/// The counter values [`Rates::update`] turns into rates.
#[derive(Debug, Clone, Copy, Default)]
struct Totals {
    packets_in: u64,
    packets_out: u64,
    bytes_in: u64,
    bytes_out: u64,
}

impl Rates {
    /// Rates that start counting at `now` from zero totals.
    fn new(now: Instant) -> Rates {
        Rates {
            at: now,
            packets_in: 0,
            packets_out: 0,
            bytes_in: 0,
            bytes_out: 0,
            in_per_sec: 0.0,
            out_per_sec: 0.0,
            bytes_in_per_sec: 0.0,
            bytes_out_per_sec: 0.0,
        }
    }

    /// Recomputes the per-second rates from the counters if a window has passed.
    fn update(&mut self, now: Instant, t: Totals) {
        let dt = now.saturating_duration_since(self.at);
        if dt < RATE_WINDOW {
            return;
        }
        let secs = dt.as_secs_f32();
        self.in_per_sec = t.packets_in.saturating_sub(self.packets_in) as f32 / secs;
        self.out_per_sec = t.packets_out.saturating_sub(self.packets_out) as f32 / secs;
        self.bytes_in_per_sec = t.bytes_in.saturating_sub(self.bytes_in) as f32 / secs;
        self.bytes_out_per_sec = t.bytes_out.saturating_sub(self.bytes_out) as f32 / secs;
        self.packets_in = t.packets_in;
        self.packets_out = t.packets_out;
        self.bytes_in = t.bytes_in;
        self.bytes_out = t.bytes_out;
        self.at = now;
    }
}

struct Rss {
    sys: sysinfo::System,
    pid: Option<sysinfo::Pid>,
    refreshed: Instant,
    bytes: u64,
    baseline: u64,
}

impl Rss {
    fn new() -> Rss {
        let mut r = Rss {
            sys: sysinfo::System::new(),
            pid: sysinfo::get_current_pid().ok(),
            refreshed: Instant::now()
                .checked_sub(RSS_REFRESH)
                .unwrap_or_else(Instant::now),
            bytes: 0,
            baseline: 0,
        };
        r.refresh(true);
        r.baseline = r.bytes;
        r
    }

    fn refresh(&mut self, force: bool) {
        if !force && self.refreshed.elapsed() < RSS_REFRESH {
            return;
        }
        self.refreshed = Instant::now();
        if let Some(pid) = self.pid {
            self.sys.refresh_processes_specifics(
                sysinfo::ProcessesToUpdate::Some(&[pid]),
                true,
                sysinfo::ProcessRefreshKind::nothing().with_memory(),
            );
            if let Some(p) = self.sys.process(pid) {
                self.bytes = p.memory();
            }
        }
    }
}

#[derive(Default)]
struct Markers {
    /// Static trade outlets, computed once.
    outlets: Vec<Marker>,
    /// Houses by bot id.
    houses: FxHashMap<u32, Marker>,
    /// Duel flags by key.
    flags: FxHashMap<u32, Marker>,
}

struct MapAsset {
    info: MapInfo,
    png: Option<Arc<Vec<u8>>>,
}

/// What [`Hub::new`] needs besides the world.
pub struct HubParams<'a> {
    /// The static world (trade outlets, map calibration).
    pub world: &'a aa_world::World,
    /// The baked data directory (the map image lives there).
    pub baked_dir: Option<&'a Path>,
    /// Where `Spawn` / `Despawn` commands go.
    pub supervisor: mpsc::Sender<SupCmd>,
    /// The LLM add-on, if on.
    pub llm: Option<Arc<pb_llm::Llm>>,
}

/// The hub. See the module docs.
pub struct Hub {
    slots: RwLock<FxHashMap<u32, Arc<BotSlot>>>,
    /// Character ids of all our bots, to tell them from real players.
    bot_chars: RwLock<FxHashSet<u32>>,
    /// Chat limits and the names of our bots.
    pub chat: ChatHub,
    /// Which bot works on which monster, doodad and spawn spot (so the bots spread out).
    pub contest: Contest,
    /// The world events (rifts) that are running and the bots that take part, see
    /// [`crate::events`].
    pub events: crate::events::EventBoard,
    /// Navigation: the planner pool and the counters, see [`crate::bot::nav`].
    pub nav: NavService,
    /// What the leaders of dungeon parties tell their members (owned by the dungeons feature).
    pub board: crate::bot::party::PartyBoard,
    /// What the bots share about guilds: who can be invited, the guilds, the foundings (owned by
    /// the guild feature, see [`crate::bot::guild`]).
    pub guilds: crate::bot::guild::GuildBook,
    /// The bots of this process in the parties of real players (owned by the party feature, see
    /// [`crate::bot::companion`]).
    pub companions: crate::bot::companion::Board,
    /// What the bots grow (owned by the farming feature, see [`crate::bot::farm`]).
    pub farms: crate::bot::farm::board::FarmBoard,
    /// Server issue reports, see [`crate::issues`]; [`Hub::report_issue`] is the way in.
    issue_svc: Arc<Issues>,
    /// Server log lines handed to bots in the current second (second, lines).
    log_feed: Mutex<(u64, u32)>,
    /// Packet counters; bot tasks add to them.
    pub counters: Counters,
    rates: Mutex<Rates>,
    rss: Mutex<Rss>,
    started: Instant,
    markers: Mutex<Markers>,
    map: MapAsset,
    supervisor: mpsc::Sender<SupCmd>,
    llm: Option<Arc<pb_llm::Llm>>,
    /// What the bots know about the world's players and goods, and the population director.
    pub pop: Population,
    /// What every bot remembers between runs, see [`crate::journal`].
    pub journal: Arc<crate::journal::JournalBook>,
    /// Recent confirmed market prices shared across guild members.
    pub market_prices: crate::bot::guild::prices::Prices,
    /// Recent, server-confirmed trade-pack attacks eligible for bounty patrols.
    pub bounties: crate::bot::pvp::bounties::Board,
    /// The log-in and log-out schedules of the bots (owned by the presence feature, see
    /// [`crate::presence`]).
    pub presence: crate::presence::Presence,
    /// The zone names an `act` of the agent API may use (owned by the agent feature).
    pub zones: crate::agent::ZoneNames,
}

impl Hub {
    /// Builds the hub: computes the trade outlet markers and loads the map image once.
    pub fn new(p: HubParams<'_>) -> Hub {
        let outlets = trade_outlets(p.world);
        let map = load_map(p.world, p.baked_dir);
        tracing::info!(
            outlets = outlets.len(),
            map_image = map.png.is_some(),
            "hub ready"
        );
        Hub {
            slots: RwLock::new(FxHashMap::default()),
            bot_chars: RwLock::new(FxHashSet::default()),
            chat: ChatHub::default(),
            contest: Contest::new(),
            events: Default::default(),
            nav: NavService::default(),
            board: Default::default(),
            guilds: Default::default(),
            companions: Default::default(),
            farms: Default::default(),
            issue_svc: Arc::new(Issues::disabled()),
            log_feed: Mutex::new((0, 0)),
            counters: Counters::default(),
            rates: Mutex::new(Rates::new(Instant::now())),
            rss: Mutex::new(Rss::new()),
            started: Instant::now(),
            markers: Mutex::new(Markers {
                outlets,
                ..Markers::default()
            }),
            map,
            supervisor: p.supervisor,
            llm: p.llm,
            pop: Population::inert(),
            journal: Arc::new(crate::journal::JournalBook::memory()),
            market_prices: Default::default(),
            bounties: Default::default(),
            presence: crate::presence::Presence::default(),
            zones: crate::agent::ZoneNames::from_world(p.world),
        }
    }

    /// Uses `pop` for the census, the economy board and the population director (the default does
    /// nothing). Call before the hub is shared.
    pub fn with_population(mut self, pop: Population) -> Hub {
        self.pop = pop;
        self
    }

    /// The hub with the bots' journal (read at start, written by its saver task).
    pub fn with_journal(mut self, journal: Arc<crate::journal::JournalBook>) -> Hub {
        let mut sales: Vec<_> = journal
            .ids()
            .into_iter()
            .flat_map(|id| journal.read(id, |j| j.market_sales.clone()))
            .collect();
        sales.sort_unstable_by_key(|sale| sale.at_ms);
        for sale in sales {
            self.market_prices.remember(sale);
        }
        self.journal = journal;
        self
    }

    /// Hands an order to the supervisor without waiting.
    pub(crate) fn order(&self, cmd: SupCmd) -> Result<(), String> {
        self.supervisor
            .try_send(cmd)
            .map_err(|_| "the supervisor is busy, try again".to_string())
    }

    /// Adds a slot for a bot (or returns the existing one).
    pub fn register(&self, snap: BotSnapshot) -> Arc<BotSlot> {
        self.chat.names.add(&snap.name);
        let mut slots = self.slots.write();
        slots
            .entry(snap.id)
            .or_insert_with(|| BotSlot::new(snap))
            .clone()
    }

    /// Remembers that character `char_id` belongs to one of our bots.
    pub fn mark_bot_char(&self, char_id: u32) {
        if char_id != 0 && !self.bot_chars.read().contains(&char_id) {
            self.bot_chars.write().insert(char_id);
        }
    }

    /// Whether character `char_id` is one of our bots.
    pub fn is_bot_char(&self, char_id: u32) -> bool {
        self.bot_chars.read().contains(&char_id)
    }

    pub fn bot_id_of(&self, char_id: u32) -> Option<u32> {
        if char_id == 0 {
            return None;
        }
        self.slots
            .read()
            .iter()
            .find_map(|(&id, slot)| (slot.char_id.load(Ordering::Relaxed) == char_id).then_some(id))
    }

    /// Remembers the character name of one of our bots (also called when a name changes).
    pub fn mark_bot_name(&self, name: &str) {
        self.chat.names.add(name);
    }

    /// The LLM add-on, if it is on.
    pub fn llm(&self) -> Option<&Arc<pb_llm::Llm>> {
        self.llm.as_ref()
    }

    /// The slot of bot `id`.
    pub fn slot(&self, id: u32) -> Option<Arc<BotSlot>> {
        self.slots.read().get(&id).cloned()
    }

    /// All slots.
    pub fn slots(&self) -> Vec<Arc<BotSlot>> {
        self.slots.read().values().cloned().collect()
    }

    /// Adds packet numbers from a bot task.
    pub fn count(&self, packets_in: u64, bytes_in: u64, packets_out: u64, bytes_out: u64) {
        let c = &self.counters;
        c.packets_in.fetch_add(packets_in, Ordering::Relaxed);
        c.bytes_in.fetch_add(bytes_in, Ordering::Relaxed);
        c.packets_out.fetch_add(packets_out, Ordering::Relaxed);
        c.bytes_out.fetch_add(bytes_out, Ordering::Relaxed);
    }

    /// Call about once a second: refreshes the packet rates and (every 2 s) the memory figure.
    pub fn tick(&self) {
        let c = &self.counters;
        self.rates.lock().update(
            Instant::now(),
            Totals {
                packets_in: c.packets_in.load(Ordering::Relaxed),
                packets_out: c.packets_out.load(Ordering::Relaxed),
                bytes_in: c.bytes_in.load(Ordering::Relaxed),
                bytes_out: c.bytes_out.load(Ordering::Relaxed),
            },
        );
        self.rss.lock().refresh(false);
        self.nav.tick();
        self.tick_events();
        self.pop.tick(self);
        self.presence.tick(self);
    }

    /// The short view of bot `id`'s journal for the detail panel.
    pub fn journal_view(&self, id: u32) -> pb_web::JournalView {
        self.journal.read(id, |j| pb_web::JournalView {
            zones_seen: j.seen.zones.len() as u32,
            sessions: j.sessions.count,
            played_secs: j.sessions.total_secs,
            wild_plots: j.wild.plots.len() as u32,
            notes: j.notes.lines.iter().map(|n| n.text.clone()).collect(),
        })
    }

    /// While an event runs: tells the board how many bots are online and how high they are, and
    /// files the issues its detectors found.
    fn tick_events(&self) {
        if !self.events.any() {
            return;
        }
        let levels = self.slots().into_iter().filter_map(|s| {
            s.with(|g| {
                (!matches!(g.snap.activity, Activity::Offline | Activity::Connecting))
                    .then_some(g.snap.level)
            })
        });
        let pop = crate::events::Population::from_levels(levels);
        for w in self.events.sweep_witnessed(Some(pop)) {
            // The bots that stood in the event saw the problem: each files it with its own
            // context, so one rift that goes wrong is an issue seen by several bots.
            let mut filed = false;
            for bot in w.bots {
                if let Some(slot) = self.slot(bot) {
                    let r = slot.with(|inner| {
                        crate::issues::attach_context(w.report.clone(), inner, 0, None)
                    });
                    self.report_issue(r);
                    filed = true;
                }
            }
            if !filed {
                self.report_issue(w.report);
            }
        }
    }

    /// Records (or replaces) the house marker of a bot.
    pub fn set_house(&self, bot: u32, x: f32, y: f32, label: String) {
        self.markers.lock().houses.insert(
            bot,
            Marker {
                kind: MarkerKind::House,
                x,
                y,
                label,
            },
        );
    }

    /// Removes the house marker of a bot.
    pub fn clear_house(&self, bot: u32) {
        self.markers.lock().houses.remove(&bot);
    }

    /// Records (or replaces) a duel flag marker; `key` identifies the duel.
    pub fn set_duel_flag(&self, key: u32, x: f32, y: f32, label: String) {
        self.markers.lock().flags.insert(
            key,
            Marker {
                kind: MarkerKind::DuelFlag,
                x,
                y,
                label,
            },
        );
    }

    /// Removes a duel flag marker.
    pub fn clear_duel_flag(&self, key: u32) {
        self.markers.lock().flags.remove(&key);
    }

    /// Seconds since the hub was created.
    pub fn uptime_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    // ---- server issue reports ------------------------------------------------------------

    /// Uses `issues` for the reports of this hub (the default drops them). Call before the hub is
    /// shared; `Hub::start_issues` starts the background work.
    pub fn with_issues(mut self, issues: Arc<Issues>) -> Hub {
        self.issue_svc = issues;
        self
    }

    /// The issue service.
    pub fn issue_service(&self) -> &Arc<Issues> {
        &self.issue_svc
    }

    /// Files a report of a problem with the server (see [`crate::issues`]). Cheap and never
    /// blocks on I/O: the report is deduplicated and counted, the slow work happens in the
    /// background. Detectors call [`crate::bot::ctx::Ctx::report_issue`] (goals) or the bot's
    /// `file_issue` (the bot loop), which attach the bot's context first. Returns the fingerprint
    /// of the issue, `None` when issue reports are switched off.
    pub fn report_issue(&self, report: IssueReport) -> Option<String> {
        self.issue_svc.report(report)
    }

    /// Starts the background tasks of the issue reports (saving, evidence and queue entries, the
    /// server log tail). Stop them with [`IssueTasks::stop`] after the bots are gone.
    pub fn start_issues(self: &Arc<Hub>) -> IssueTasks {
        let hub = self.clone();
        let hook: LineHook = Arc::new(move |level, line| hub.feed_server_log(level, line));
        self.issue_svc.start(Some(hook))
    }

    /// A `WARN` / `ERROR` line of the server log: bots whose character it names are told, so each
    /// can file a report with its own trail. At most [`LOG_FEED_PER_SEC`] lines a second.
    fn feed_server_log(&self, level: LogLevel, line: &str) {
        if !feed_allowed(&mut self.log_feed.lock(), unix_ms(), LOG_FEED_PER_SEC) {
            return;
        }
        deliver_log_line(&self.chat.names, &|| self.slots(), level, line);
    }

    /// A filtered spawn needs a bot to start: a roster entry of a matching race, or a configured
    /// race to create one with. Refuses early (and in words) what the supervisor could not do.
    fn check_spawn_filter(&self, sel: &Selector) -> Result<(), String> {
        if sel.is_all() || !sel.races(self.pop.races()).is_empty() {
            return Ok(());
        }
        let every_race: Vec<u8> = (1..=8).collect();
        if sel.races(&every_race).is_empty() {
            return Err(
                "no race matches that filter: the Nuia races start on the west continent, \
                 the Haranya races on the east continent"
                    .to_string(),
            );
        }
        let has_entry = self.slots().iter().any(|s| {
            s.with(|g| crate::config::race_id(&g.snap.race))
                .is_some_and(|r| sel.matches_race(r))
        });
        if has_entry {
            Ok(())
        } else {
            Err("no bot and no configured race (bots.races) matches that filter".to_string())
        }
    }

    fn per_bot_bytes(rss: &Rss, online: u32) -> u64 {
        if online == 0 {
            0
        } else {
            rss.bytes.saturating_sub(rss.baseline) / u64::from(online)
        }
    }
}

/// Whether one more server log line may be handed to the bots in the second of `now_ms`:
/// `window` is `(second, lines so far in it)`.
fn feed_allowed(window: &mut (u64, u32), now_ms: u64, per_sec: u32) -> bool {
    let sec = now_ms / 1000;
    if window.0 != sec {
        *window = (sec, 0);
    }
    if window.1 >= per_sec {
        return false;
    }
    window.1 += 1;
    true
}

/// Sends a server log line to every bot whose character name it contains (as a word) and returns
/// their ids. `slots` is only called when the line names one of ours.
fn deliver_log_line(
    names: &BotNames,
    slots: &dyn Fn() -> Vec<Arc<BotSlot>>,
    level: LogLevel,
    line: &str,
) -> Vec<u32> {
    let mut told: Vec<u32> = Vec::new();
    let mut all: Option<Vec<Arc<BotSlot>>> = None;
    for word in words(line) {
        if word.chars().count() < 2 || !names.contains(word) {
            continue;
        }
        for slot in all.get_or_insert_with(slots).iter() {
            if told.contains(&slot.id) || !slot.with(|g| g.snap.name.eq_ignore_ascii_case(word)) {
                continue;
            }
            told.push(slot.id);
            // (a busy or stopped bot simply misses it)
            let _ = slot.send(BotCmd::ServerLogLine {
                level,
                line: line.to_string(),
            });
        }
    }
    told
}

/// Specialty NPC spawns: where a trade pack can be sold.
fn trade_outlets(world: &aa_world::World) -> Vec<Marker> {
    let mut out = Vec::new();
    for t in world.npc_templates().iter().filter(|t| t.is_specialty()) {
        for p in world.npc_spawns(t.id) {
            out.push(Marker {
                kind: MarkerKind::TradeOutlet,
                x: p.x,
                y: p.y,
                label: t.name.to_string(),
            });
        }
    }
    out
}

/// `name` if it is a plain file name: one normal path component, so no separator, no `..`, no
/// drive or root, nothing absolute. (`dir.join("/etc/passwd")` replaces `dir` altogether.)
pub fn plain_file_name(name: &str) -> Option<&str> {
    use std::path::Component;
    if name.is_empty() || name.contains(['\0', '/', '\\', ':']) {
        return None;
    }
    let mut parts = Path::new(name).components();
    match (parts.next(), parts.next()) {
        (Some(Component::Normal(n)), None) if n == std::ffi::OsStr::new(name) => Some(name),
        _ => None,
    }
}

fn load_map(world: &aa_world::World, baked_dir: Option<&Path>) -> MapAsset {
    let Some(cal) = world.baked().map.as_ref() else {
        return MapAsset {
            info: MapInfo {
                image_url: None,
                width: 896,
                height: 973,
                world: DEFAULT_WORLD,
            },
            png: None,
        };
    };
    let png = baked_dir.and_then(|dir| {
        // (the calibration file is data from disk: it must not point outside the data directory)
        let Some(name) = plain_file_name(&cal.image) else {
            tracing::warn!(
                "the baked map names its image {:?}, which is not a plain file name; no map image",
                cal.image
            );
            return None;
        };
        let path = dir.join(name);
        match std::fs::read(&path) {
            Ok(bytes) => Some(Arc::new(bytes)),
            Err(e) => {
                tracing::warn!("world map image {}: {e}", path.display());
                None
            }
        }
    });
    MapAsset {
        info: MapInfo {
            image_url: png.as_ref().map(|_| "/map.png".to_string()),
            width: cal.width,
            height: cal.height,
            world: WorldRect {
                min_x: cal.world.min_x,
                min_y: cal.world.min_y,
                max_x: cal.world.max_x,
                max_y: cal.world.max_y,
            },
        },
        png,
    }
}

impl BotHub for Hub {
    fn snapshots(&self) -> Vec<BotSnapshot> {
        self.slots.read().values().map(|s| s.snapshot()).collect()
    }

    fn detail(&self, id: u32) -> Option<BotDetail> {
        let slot = self.slot(id)?;
        Some(slot.with(|g| BotDetail {
            snapshot: g.snap.clone(),
            log: g.log.iter().cloned().collect(),
            quests: g.quests.clone(),
            extra: g.extra.clone(),
            chat: g.social.chat_lines(),
            path: g.path.clone(),
            plots: g.plots.clone(),
            gear: g.gear.clone(),
            profile: g.profile.clone(),
            journal: Some(self.journal_view(id)),
        }))
    }

    fn stats(&self) -> HubStats {
        let slots = self.slots();
        let mut counts = [0u32; Activity::ALL.len()];
        let mut online = 0u32;
        for s in &slots {
            let a = s.with(|g| g.snap.activity);
            if let Some(i) = Activity::ALL.iter().position(|x| *x == a) {
                counts[i] += 1;
            }
            if !matches!(a, Activity::Offline | Activity::Connecting) {
                online += 1;
            }
        }
        let issue_counts = self.issue_svc.stats().counts;
        let rates = self.rates.lock();
        let rss = self.rss.lock();
        HubStats {
            bots_total: slots.len() as u32,
            bots_online: online,
            rss_bytes: rss.bytes,
            bytes_per_bot: Hub::per_bot_bytes(&rss, online),
            packets_in_per_sec: rates.in_per_sec,
            packets_out_per_sec: rates.out_per_sec,
            bytes_in_per_sec: rates.bytes_in_per_sec,
            bytes_out_per_sec: rates.bytes_out_per_sec,
            uptime_secs: self.uptime_secs(),
            open_issues: issue_counts.open,
            queued_issues: issue_counts.queued,
            nav: Some(self.nav.stats()),
            llm: self.llm.as_ref().map(|l| {
                let u = l.usage();
                let s = &self.chat.gate.stats;
                LlmStats {
                    requests: u.requests,
                    failures: u.failures,
                    tokens_today: u.tokens_today,
                    budget_left: u.budget_left,
                    rate_limited: u.rate_limited,
                    chat_replies: s.llm_replies.load(Ordering::Relaxed),
                    director_picks: s.director_picks.load(Ordering::Relaxed),
                    plans: u.plans,
                    plan_steps: u.plan_steps,
                    notes: u.notes,
                }
            }),
            activity_counts: Activity::ALL
                .iter()
                .zip(counts)
                .filter(|(_, n)| *n > 0)
                .map(|(a, n)| (*a, n))
                .collect(),
        }
    }

    fn markers(&self) -> Vec<Marker> {
        let m = self.markers.lock();
        let mut out = Vec::with_capacity(m.outlets.len() + m.houses.len() + m.flags.len());
        out.extend(m.outlets.iter().cloned());
        out.extend(m.houses.values().cloned());
        out.extend(m.flags.values().cloned());
        out
    }

    fn map_info(&self) -> MapInfo {
        self.map.info.clone()
    }

    fn map_image(&self) -> Option<Vec<u8>> {
        self.map.png.as_ref().map(|p| p.as_ref().clone())
    }

    fn issues(&self) -> Vec<IssueSummary> {
        self.issue_svc.summaries()
    }

    fn issue(&self, fingerprint: &str) -> Option<IssueDetail> {
        self.issue_svc.detail(fingerprint)
    }

    fn issue_action(&self, fingerprint: &str, action: IssueAction) -> Result<(), String> {
        self.issue_svc.action(fingerprint, view::action(action))
    }

    fn world(&self) -> WorldView {
        self.pop.world_view(self)
    }

    fn economy(&self) -> EconomyView {
        let totals = self.farms.totals();
        let grown: Vec<u32> = totals.iter().map(|t| t.0).take(32).collect();
        let mut view = self.pop.economy_view(&grown);
        let growing: FxHashMap<u32, f32> = totals.into_iter().collect();
        if !growing.is_empty() {
            for row in &mut view.items {
                row.growing = growing.get(&row.template).copied().unwrap_or(0.0);
            }
        }
        view
    }

    fn director(&self) -> DirectorView {
        let mut view = self.pop.director_view(self);
        view.presence = self.presence.view();
        view
    }

    fn set_policy(&self, patch: PolicyPatch) -> Result<(), String> {
        self.pop.set_policy(patch)
    }

    fn observe(&self, id: u32) -> Option<pb_web::Observation> {
        crate::agent::observe(self, id)
    }

    fn zones(&self) -> Vec<pb_web::ZoneEntry> {
        self.zones.entries()
    }

    fn command(&self, bot: Option<u32>, cmd: Command) -> Result<(), String> {
        cmd.validate(bot)?;
        // (an `act` may name its zone; the bots only know zone keys)
        let cmd = crate::agent::resolve_zones(&self.zones, cmd)?;
        match bot {
            Some(id) => {
                if let Command::Note { text } = &cmd {
                    return crate::agent::write_note(self, id, text);
                }
                let slot = self.slot(id).ok_or_else(|| format!("no bot {id}"))?;
                slot.send(to_bot_cmd(cmd)?)
            }
            None => match cmd {
                Command::Spawn { count } => self
                    .supervisor
                    .try_send(SupCmd::Spawn(count))
                    .map_err(|_| "the supervisor is busy, try again".to_string()),
                Command::Despawn { count } => self
                    .supervisor
                    .try_send(SupCmd::Despawn(count))
                    .map_err(|_| "the supervisor is busy, try again".to_string()),
                Command::SpawnWhere { count, filter } => {
                    let sel = Selector::from_filter(&filter)?;
                    self.check_spawn_filter(&sel)?;
                    self.order(SupCmd::SpawnWhere {
                        n: count,
                        sel,
                        owned: false,
                    })
                }
                Command::DespawnWhere { count, filter } => self.order(SupCmd::DespawnWhere {
                    n: count,
                    sel: Selector::from_filter(&filter)?,
                    rule: StopRule::default(),
                }),
                other => {
                    // Pause, Resume and Goal reach every bot. A full queue only skips that bot.
                    let bot_cmd = to_bot_cmd(other)?;
                    let mut busy = 0usize;
                    for s in self.slots() {
                        if s.send(bot_cmd.clone()).is_err() {
                            busy += 1;
                        }
                    }
                    if busy > 0 {
                        tracing::debug!(busy, "global command skipped some bots");
                    }
                    Ok(())
                }
            },
        }
    }
}

fn to_bot_cmd(cmd: Command) -> Result<BotCmd, String> {
    Ok(match cmd {
        Command::Pause => BotCmd::Pause,
        Command::Resume => BotCmd::Resume,
        Command::Goal { goal } => {
            BotCmd::Goal(GoalChoice::parse(&goal).ok_or_else(|| format!("unknown goal {goal:?}"))?)
        }
        Command::DuelWith { other } => BotCmd::DuelWith(other),
        Command::TradeWith { other } => BotCmd::TradeWith(other),
        Command::Say { text } => BotCmd::Say(text.trim().to_string()),
        Command::Whisper { to, text } => BotCmd::Whisper {
            to: to.trim().to_string(),
            text: text.trim().to_string(),
        },
        Command::Spawn { .. }
        | Command::Despawn { .. }
        | Command::SpawnWhere { .. }
        | Command::DespawnWhere { .. } => {
            return Err("spawn and despawn are global commands".into())
        }
        Command::Act(args) => crate::agent::act_command(&args)?,
        Command::Profile(patch) => BotCmd::Profile(Box::new(profile_patch(&patch))),
        // (the hub writes a note into the journal itself, see `BotHub::command`)
        Command::Note { .. } => return Err("a note is kept by the hub, not sent to the bot".into()),
    })
}

/// The roster's form of a web profile patch (the web layer checked the values).
fn profile_patch(p: &pb_web::ProfilePatch) -> crate::roster::ProfilePatch {
    crate::roster::ProfilePatch {
        chatty: p.chatty,
        crude: p.crude,
        joker: p.joker,
        explorer: p.explorer,
        outlaw: p.outlaw,
        gearhead: p.gearhead,
        dedication: p.dedication,
        chronotype: p
            .chronotype
            .as_deref()
            .and_then(crate::roster::Chronotype::parse),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(id: u32) -> BotSnapshot {
        BotSnapshot {
            id,
            name: format!("Bot{id}"),
            ..BotSnapshot::default()
        }
    }

    #[test]
    fn the_map_image_must_be_a_plain_file_name() {
        for ok in ["map_main_world.png", "map.png", "a b.png", "x"] {
            assert_eq!(plain_file_name(ok), Some(ok), "{ok}");
        }
        for bad in [
            "",
            ".",
            "..",
            "../map.png",
            "..\\map.png",
            "sub/map.png",
            "sub\\map.png",
            "/etc/passwd",
            "\\windows\\win.ini",
            "C:\\windows\\win.ini",
            "C:map.png",
            "map.png:stream",
            "\\\\server\\share\\x.png",
            "map\0.png",
            "./map.png",
        ] {
            assert_eq!(plain_file_name(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_slot_keeps_a_bounded_log_and_delivers_commands() {
        let slot = BotSlot::new(snap(1));
        for i in 0..100 {
            slot.push_log(format!("line {i}"));
        }
        slot.with(|g| {
            assert_eq!(g.log.len(), LOG_LINES);
            assert_eq!(g.log.front().unwrap().text, "line 68");
            assert_eq!(g.log.back().unwrap().text, "line 99");
        });

        // nobody listens yet: the command is dropped with an error
        let mut rx = slot.attach();
        slot.send(BotCmd::Pause).unwrap();
        assert_eq!(rx.try_recv().unwrap(), BotCmd::Pause);
        for _ in 0..CMD_QUEUE {
            slot.send(BotCmd::Resume).unwrap();
        }
        let err = slot.send(BotCmd::Resume).unwrap_err();
        assert!(err.contains("busy"), "{err}");
        // a restart gets a new channel; the old one is closed
        drop(rx);
        let err = slot.send(BotCmd::Pause).unwrap_err();
        assert!(err.contains("not running"), "{err}");
        let mut rx = slot.attach();
        slot.send(BotCmd::Pause).unwrap();
        assert_eq!(rx.try_recv().unwrap(), BotCmd::Pause);
    }

    #[test]
    fn a_server_log_line_reaches_the_bots_it_names() {
        let names = BotNames::default();
        let slots: Vec<_> = (1..=3).map(|i| BotSlot::new(snap(i))).collect();
        for s in &slots {
            names.add(&s.snapshot().name);
        }
        let mut rx: Vec<_> = slots.iter().map(|s| s.attach()).collect();
        let all = || slots.clone();

        let told = deliver_log_line(
            &names,
            &all,
            LogLevel::Warn,
            "14:03:09 [WARN] aaemu_game::x - bot1 moved too fast, then BOT1 again",
        );
        assert_eq!(
            told,
            vec![1],
            "one message per bot, names are case-insensitive"
        );
        assert_eq!(
            rx[0].try_recv().unwrap(),
            BotCmd::ServerLogLine {
                level: LogLevel::Warn,
                line: "14:03:09 [WARN] aaemu_game::x - bot1 moved too fast, then BOT1 again".into()
            }
        );
        assert!(rx[0].try_recv().is_err());
        assert!(rx[1].try_recv().is_err() && rx[2].try_recv().is_err());

        // two bots named in one line
        let told = deliver_log_line(
            &names,
            &all,
            LogLevel::Error,
            "duel of Bot2 and Bot3 failed",
        );
        assert_eq!(told, vec![2, 3]);
        assert!(matches!(
            rx[1].try_recv(),
            Ok(BotCmd::ServerLogLine {
                level: LogLevel::Error,
                ..
            })
        ));
        assert!(rx[2].try_recv().is_ok());

        // a name inside another word, a stranger, a line without names: nobody (and no slot scan)
        let scanned = std::cell::Cell::new(false);
        let spy = || {
            scanned.set(true);
            slots.clone()
        };
        for line in ["Bot10 only", "player Zed failed", "nothing to see", ""] {
            assert!(
                deliver_log_line(&names, &spy, LogLevel::Warn, line).is_empty(),
                "{line}"
            );
        }
        assert!(!scanned.get());

        // a bot that is not running misses the line without trouble
        drop(rx);
        assert_eq!(
            deliver_log_line(&names, &all, LogLevel::Warn, "Bot1 again"),
            vec![1]
        );
    }

    #[test]
    fn at_most_so_many_log_lines_reach_the_bots_per_second() {
        let mut w = (0u64, 0u32);
        for _ in 0..5 {
            assert!(feed_allowed(&mut w, 10_500, 5));
        }
        assert!(!feed_allowed(&mut w, 10_999, 5));
        assert!(feed_allowed(&mut w, 11_000, 5), "a new second");
        let mut none = (0u64, 0u32);
        assert!(!feed_allowed(&mut none, 1_000, 0));
    }

    #[test]
    fn offline_keeps_the_identity() {
        let slot = BotSlot::new(BotSnapshot {
            activity: Activity::Grinding,
            detail: "x".into(),
            partner: Some(4),
            online_secs: 9,
            ..snap(2)
        });
        slot.obj_id.store(5, Ordering::Relaxed);
        slot.with(|g| g.path = vec![[1.0, 2.0], [3.0, 4.0]]);
        slot.set_offline();
        assert!(
            slot.with(|g| g.path.is_empty()),
            "a route is not kept offline"
        );
        let s = slot.snapshot();
        assert_eq!(s.name, "Bot2");
        assert_eq!(s.activity, Activity::Offline);
        assert!(s.detail.is_empty() && s.partner.is_none() && s.online_secs == 0);
        assert_eq!(slot.obj_id.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn web_commands_map_to_bot_commands() {
        assert_eq!(to_bot_cmd(Command::Pause), Ok(BotCmd::Pause));
        assert_eq!(
            to_bot_cmd(Command::Goal {
                goal: "auto".into()
            }),
            Ok(BotCmd::Goal(GoalChoice::Auto))
        );
        assert_eq!(
            to_bot_cmd(Command::Goal {
                goal: "trade_run".into()
            }),
            Ok(BotCmd::Goal(GoalChoice::Forced(
                crate::bot::cmd::GoalKind::TradeRun
            )))
        );
        assert_eq!(
            to_bot_cmd(Command::Say {
                text: "  hi  ".into()
            }),
            Ok(BotCmd::Say("hi".into()))
        );
        assert_eq!(
            to_bot_cmd(Command::Whisper {
                to: " Bob ".into(),
                text: " hi ".into()
            }),
            Ok(BotCmd::Whisper {
                to: "Bob".into(),
                text: "hi".into()
            })
        );
        assert_eq!(
            to_bot_cmd(Command::Goal {
                goal: "wander".into()
            }),
            Ok(BotCmd::Goal(GoalChoice::Forced(
                crate::bot::cmd::GoalKind::Wander
            )))
        );
        for (name, kind) in [
            ("dungeon", crate::bot::cmd::GoalKind::Dungeon),
            ("sail", crate::bot::cmd::GoalKind::Sail),
            ("auction", crate::bot::cmd::GoalKind::Auction),
            ("market", crate::bot::cmd::GoalKind::Market),
            ("guild", crate::bot::cmd::GoalKind::Guild),
            ("gank", crate::bot::cmd::GoalKind::Gank),
            ("warzone", crate::bot::cmd::GoalKind::Warzone),
            ("event", crate::bot::cmd::GoalKind::Event),
            ("companion", crate::bot::cmd::GoalKind::Companion),
            ("farm", crate::bot::cmd::GoalKind::Farm),
            ("explore", crate::bot::cmd::GoalKind::Explore),
            ("gear", crate::bot::cmd::GoalKind::Gear),
        ] {
            assert_eq!(
                to_bot_cmd(Command::Goal { goal: name.into() }),
                Ok(BotCmd::Goal(GoalChoice::Forced(kind)))
            );
        }
        assert!(to_bot_cmd(Command::Goal { goal: "fly".into() }).is_err());
        assert!(to_bot_cmd(Command::Spawn { count: 1 }).is_err());
    }

    fn totals(packets_in: u64, packets_out: u64, bytes_in: u64, bytes_out: u64) -> Totals {
        Totals {
            packets_in,
            packets_out,
            bytes_in,
            bytes_out,
        }
    }

    #[test]
    fn rates_come_from_counter_deltas_once_a_window_has_passed() {
        let t0 = Instant::now();
        let mut r = Rates::new(t0);
        r.packets_in = 100;
        r.packets_out = 10;
        r.bytes_in = 4_000;
        r.bytes_out = 1_000;
        // less than a window: nothing changes
        r.update(
            t0 + Duration::from_millis(400),
            totals(900, 90, 90_000, 9_000),
        );
        assert_eq!((r.in_per_sec, r.bytes_in_per_sec), (0.0, 0.0));
        // two seconds later: (500 - 100) / 2 and (50 - 10) / 2, and the same for the bytes
        r.update(t0 + Duration::from_secs(2), totals(500, 50, 24_000, 5_000));
        assert!((r.in_per_sec - 200.0).abs() < 0.01, "{}", r.in_per_sec);
        assert!((r.out_per_sec - 20.0).abs() < 0.01, "{}", r.out_per_sec);
        assert!(
            (r.bytes_in_per_sec - 10_000.0).abs() < 0.01,
            "{}",
            r.bytes_in_per_sec
        );
        assert!(
            (r.bytes_out_per_sec - 2_000.0).abs() < 0.01,
            "{}",
            r.bytes_out_per_sec
        );
        // counters never run backwards into a huge rate
        r.update(t0 + Duration::from_secs(4), totals(10, 5, 10, 5));
        assert_eq!((r.in_per_sec, r.out_per_sec), (0.0, 0.0));
        assert_eq!((r.bytes_in_per_sec, r.bytes_out_per_sec), (0.0, 0.0));
    }
}
