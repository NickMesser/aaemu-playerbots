//! The [`EventBoard`]: the running events, their slots and the evidence the detectors judge.
//! See the [module docs](super).

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use aa_proto::sc_event::{EventId, TowerDefInfo};
use aa_proto::sc_world::WorldEvent;
use aa_world::{Events, World};
use parking_lot::Mutex;
use rustc_hash::FxHashSet;
use smallvec::SmallVec;

use super::detect::{self, Facts};
use super::{
    Caps, Join, Leave, Population, Source, View, HURT_BAR_MS, IDLE_BAR_MS, LEASE_MS, MAX_DEATHS,
    MAX_EVENTS,
};
use crate::issues::IssueReport;

/// A list that does not name an event ends it only when the board has known the event this long
/// (a list from before the start may reach a slow bot late).
const LIST_GRACE_MS: u64 = 5_000;
/// A participant reports "I am at the target" at least this often for the time to add up.
const NEAR_GAP_MS: u64 = 2_500;
/// Entries of the per-event bot tables.
const MAX_WAITING: usize = 64;
const MAX_BARRED: usize = 32;
/// An event the server never ended leaves the board this long after it should have been over.
const STALE_AFTER_CAP_MS: u64 = 120_000;

/// A bot's lease on a place in an event.
#[derive(Debug, Clone, Copy)]
struct Slot {
    bot: u32,
    lease_until: u64,
}

/// Most bots an issue report is filed for: the ones that stood in the event when it went wrong.
pub const MAX_WITNESSES: usize = 3;

/// An issue report the board made and the bots that witnessed what it says.
#[derive(Debug)]
pub struct Witnessed {
    /// The report, without a bot.
    pub report: IssueReport,
    /// The bots in the event at the time (empty when nobody was).
    pub bots: SmallVec<[u32; MAX_WITNESSES]>,
}

/// The kill-target NPCs of the current wave that died, each object counted once.
#[derive(Debug, Default)]
struct Kills {
    /// `(template, deaths)`.
    by_template: SmallVec<[(u32, u32); 6]>,
    /// Objects counted in this wave.
    seen: FxHashSet<u32>,
    /// Objects counted in the wave before (a corpse still in view is not a kill of this wave).
    before: FxHashSet<u32>,
}

impl Kills {
    fn note(&mut self, obj: u32, template: u32) -> bool {
        if self.before.contains(&obj) || !self.seen.insert(obj) {
            return false;
        }
        match self.by_template.iter_mut().find(|e| e.0 == template) {
            Some(e) => e.1 += 1,
            None => self.by_template.push((template, 1)),
        }
        true
    }

    fn next_wave(&mut self) {
        self.before = std::mem::take(&mut self.seen);
        self.by_template.clear();
    }

    fn of(&self, template: u32) -> u32 {
        self.by_template
            .iter()
            .find(|e| e.0 == template)
            .map_or(0, |e| e.1)
    }
}

#[derive(Debug)]
struct Running {
    id: EventId,
    zone_key: u32,
    first_seen: u64,
    saw_start: bool,
    step: u32,
    step_since: u64,
    target: Option<(u32, [f32; 3])>,
    slots: SmallVec<[Slot; 8]>,
    /// `(bot, ready at)`: when a bot's join delay is over.
    waiting: SmallVec<[(u32, u64); 8]>,
    /// `(bot, until)`; `u64::MAX` is until the event ends.
    barred: SmallVec<[(u32, u64); 4]>,
    deaths: SmallVec<[(u32, u8); 4]>,
    kills: Kills,
    // ---- evidence of the current wave ----
    wave_seen: bool,
    controller_seen: bool,
    near_ms: u64,
    last_near: u64,
    satisfied_since: Option<u64>,
    /// Findings already told: the `detect::TOLD_*` bits (the overdue one lasts the event).
    told: u8,
    // ---- totals, for the log ----
    joined_ever: u32,
    peak: u32,
    kills_total: u32,
}

impl Running {
    fn new(id: EventId, zone_key: u32, now: u64, saw_start: bool) -> Running {
        Running {
            id,
            zone_key,
            first_seen: now,
            saw_start,
            step: 0,
            step_since: now,
            target: None,
            slots: SmallVec::new(),
            waiting: SmallVec::new(),
            barred: SmallVec::new(),
            deaths: SmallVec::new(),
            kills: Kills::default(),
            wave_seen: false,
            controller_seen: false,
            near_ms: 0,
            last_near: 0,
            satisfied_since: None,
            told: 0,
            joined_ever: 0,
            peak: 0,
            kills_total: 0,
        }
    }

    /// Moves to wave `step` if it is later than the current one.
    fn set_step(&mut self, step: u32, now: u64) -> bool {
        if step <= self.step {
            return false;
        }
        self.step = step;
        self.step_since = now;
        self.kills.next_wave();
        self.wave_seen = false;
        self.near_ms = 0;
        self.last_near = 0;
        self.satisfied_since = None;
        self.told &= detect::TOLD_OVERDUE;
        true
    }

    fn view(&self) -> View {
        View {
            id: self.id,
            zone_key: self.zone_key,
            step: self.step,
            step_since_ms: self.step_since,
            first_seen_ms: self.first_seen,
            target: self.target,
            joined: self.slots.len() as u32,
        }
    }

    /// Drops expired leases, delays and bars; returns the bots whose lease ran out.
    fn purge(&mut self, now: u64) -> usize {
        let before = self.slots.len();
        self.slots.retain(|s| s.lease_until > now);
        self.waiting.retain(|w| w.1 + 600_000 > now);
        self.barred.retain(|b| b.1 > now);
        before - self.slots.len()
    }

    fn barred(&self, bot: u32, now: u64) -> bool {
        self.barred.iter().any(|b| b.0 == bot && b.1 > now)
    }

    fn bar(&mut self, bot: u32, until: u64) {
        match self.barred.iter_mut().find(|b| b.0 == bot) {
            Some(b) => b.1 = b.1.max(until),
            None => {
                if self.barred.len() >= MAX_BARRED {
                    self.barred.remove(0);
                }
                self.barred.push((bot, until));
            }
        }
    }
}

/// Numbers for the log and the tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Events the board has heard of.
    pub events: u64,
    /// Slots given.
    pub joins: u64,
    /// Asks turned away because the event held its bots already.
    pub refused_full: u64,
    /// Asks turned away because the events held their share of the bots.
    pub refused_share: u64,
    /// Kill-target NPCs counted (each object once).
    pub kills: u64,
    /// Issue reports made.
    pub reports: u64,
}

#[derive(Debug, Default)]
struct Inner {
    events: Vec<Running>,
    pop: Population,
    stats: Stats,
}

/// Where the board gets its time.
#[derive(Debug)]
enum Clock {
    Real(Instant),
    /// The tests move it by hand.
    Manual(AtomicU64),
}

/// The process-wide board of running events. See the [module docs](super).
pub struct EventBoard {
    clock: Clock,
    /// Events on the board: what bots read when they have no reason to lock it.
    running: AtomicU32,
    data: OnceLock<Arc<dyn Source>>,
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for EventBoard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventBoard")
            .field("running", &self.running.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Default for EventBoard {
    fn default() -> Self {
        EventBoard::new()
    }
}

impl EventBoard {
    /// An empty board; its clock starts now.
    pub fn new() -> EventBoard {
        EventBoard::with_clock(Clock::Real(Instant::now()))
    }

    /// An empty board whose clock only moves with [`EventBoard::advance`].
    pub fn manual() -> EventBoard {
        EventBoard::with_clock(Clock::Manual(AtomicU64::new(1_000)))
    }

    fn with_clock(clock: Clock) -> EventBoard {
        EventBoard {
            clock,
            running: AtomicU32::new(0),
            data: OnceLock::new(),
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Milliseconds on the board's clock (the same for every bot; never 0).
    pub fn now_ms(&self) -> u64 {
        match &self.clock {
            Clock::Real(t) => t.elapsed().as_millis() as u64 + 1_000,
            Clock::Manual(ms) => ms.load(Ordering::Relaxed),
        }
    }

    /// Moves a manual clock on (tests).
    pub fn advance(&self, ms: u64) {
        if let Clock::Manual(c) = &self.clock {
            c.fetch_add(ms, Ordering::Relaxed);
        }
    }

    /// Gives the board the static world (the first call wins).
    pub fn bind(&self, world: &Arc<World>) {
        self.data.get_or_init(|| world.clone());
    }

    /// Gives the board other event data than the world's (tests).
    pub fn bind_data(&self, data: Arc<dyn Source>) {
        self.data.get_or_init(|| data);
    }

    fn events(&self) -> Option<&Events> {
        self.data.get().map(|d| d.events())
    }

    /// Whether any event is running. One atomic read.
    pub fn any(&self) -> bool {
        self.running.load(Ordering::Relaxed) > 0
    }

    /// Whether a running event's waves wait for NPC `template` (a quest whose targets only
    /// exist during an event may be accepted only while one of them runs).
    pub fn runs_kill_target(&self, template: u32) -> bool {
        if !self.any() {
            return false;
        }
        let Some(defs) = self.events() else {
            return false;
        };
        let g = self.inner.lock();
        g.events.iter().any(|run| {
            defs.get(run.id.def).is_some_and(|d| {
                d.steps
                    .iter()
                    .any(|s| s.kills.iter().any(|k| k.template == template))
            })
        })
    }

    /// The running events.
    pub fn views(&self) -> SmallVec<[View; 4]> {
        self.inner.lock().events.iter().map(Running::view).collect()
    }

    /// The running event `id`.
    pub fn view(&self, id: EventId) -> Option<View> {
        let g = self.inner.lock();
        g.events.iter().find(|e| e.id == id).map(Running::view)
    }

    /// What the board counted so far.
    pub fn stats(&self) -> Stats {
        self.inner.lock().stats
    }

    /// Tells the board how many bots are online and how high they are (the hub does it about once
    /// a second while an event runs).
    pub fn set_population(&self, pop: Population) {
        self.inner.lock().pop = pop;
    }

    fn sync_running(&self, g: &Inner) {
        self.running.store(g.events.len() as u32, Ordering::Relaxed);
    }

    // ---- the packets -----------------------------------------------------------------------

    /// Posts an event packet. Every bot of the world sees the same packets, so a repeat changes
    /// nothing. `in_main_world`: the bot is not in an instance world (the list of an instance
    /// says nothing about the events of the main world).
    pub fn on_event(&self, ev: &WorldEvent, in_main_world: bool) {
        let now = self.now_ms();
        let mut g = self.inner.lock();
        let g = &mut *g;
        match ev {
            WorldEvent::TowerDefStart { id, zone_key } => {
                let e = self.ensure(g, *id, *zone_key, now, true);
                if let Some(e) = e {
                    e.saw_start = true;
                }
            }
            WorldEvent::TowerDefWave { id, zone_key, step } => {
                if let Some(e) = self.ensure(g, *id, *zone_key, now, false) {
                    if e.set_step(*step, now) {
                        self.log_wave(e);
                    }
                }
            }
            WorldEvent::TowerDefEnd { id, .. } => self.end(g, *id, "the server ended it", now),
            WorldEvent::TowerDefList(list) if in_main_world => self.on_list(g, list, now),
            _ => {}
        }
        self.sync_running(g);
    }

    fn on_list(&self, g: &mut Inner, list: &[TowerDefInfo], now: u64) {
        for info in list {
            let Some(e) = self.ensure(g, info.id, info.zone_key, now, false) else {
                continue;
            };
            if e.set_step(info.step, now) {
                self.log_wave(e);
            }
            let target = info
                .has_target()
                .then_some((info.target_obj, [info.pos.0, info.pos.1, info.pos.2]));
            if target != e.target {
                self.log_target(e, target);
            }
            e.target = target;
        }
        // an event the list leaves out is over
        let gone: SmallVec<[EventId; 2]> = g
            .events
            .iter()
            .filter(|e| {
                now.saturating_sub(e.first_seen) >= LIST_GRACE_MS
                    && !list.iter().any(|i| i.id == e.id)
            })
            .map(|e| e.id)
            .collect();
        for id in gone {
            self.end(g, id, "the list no longer names it", now);
        }
    }

    /// The event `id`, added when new (a list or a wave may be the first the board hears of it).
    fn ensure<'a>(
        &self,
        g: &'a mut Inner,
        id: EventId,
        zone_key: u32,
        now: u64,
        saw_start: bool,
    ) -> Option<&'a mut Running> {
        if let Some(i) = g.events.iter().position(|e| e.id == id) {
            return Some(&mut g.events[i]);
        }
        if g.events.len() >= MAX_EVENTS {
            return None;
        }
        g.stats.events += 1;
        let e = Running::new(id, zone_key, now, saw_start);
        self.log_start(&e);
        g.events.push(e);
        g.events.last_mut()
    }

    fn end(&self, g: &mut Inner, id: EventId, why: &str, now: u64) {
        let Some(i) = g.events.iter().position(|e| e.id == id) else {
            return;
        };
        let e = g.events.remove(i);
        let name = self.name(id.def);
        tracing::info!(
            target: "pb_events",
            "event ended: {name} (def {}, zone group {}) after {} s in wave {} ({why}); {} bots joined (peak {}), {} wave kills seen",
            id.def,
            id.zone_group,
            now.saturating_sub(e.first_seen) / 1000,
            e.step,
            e.joined_ever,
            e.peak,
            e.kills_total,
        );
    }

    fn name(&self, def: u32) -> String {
        self.events()
            .and_then(|e| e.get(def).map(|d| d.name.to_string()))
            .unwrap_or_else(|| format!("event {def}"))
    }

    fn log_start(&self, e: &Running) {
        tracing::info!(
            target: "pb_events",
            "event started: {} (def {}, zone group {}, zone {}){}",
            self.name(e.id.def),
            e.id.def,
            e.id.zone_group,
            e.zone_key,
            if e.saw_start { "" } else { " (found in progress)" },
        );
    }

    fn log_target(&self, e: &Running, target: Option<(u32, [f32; 3])>) {
        match target {
            Some((obj, p)) => tracing::info!(
                target: "pb_events",
                "event target: {} (def {}) defends unit {obj} at {:.0}, {:.0}, {:.0}",
                self.name(e.id.def),
                e.id.def,
                p[0],
                p[1],
                p[2],
            ),
            None => tracing::info!(
                target: "pb_events",
                "event target: {} (def {}) has no target unit in the server's list",
                self.name(e.id.def),
                e.id.def,
            ),
        }
    }

    fn log_wave(&self, e: &Running) {
        tracing::info!(
            target: "pb_events",
            "event wave: {} (def {}) is in wave {}",
            self.name(e.id.def),
            e.id.def,
            e.step,
        );
    }

    // ---- slots -----------------------------------------------------------------------------

    /// Whether `bot` has waited out its join delay for event `id`: the first call starts the
    /// delay (a random number of seconds in `delay_s`, drawn from the bot and the event, so every
    /// call agrees), later calls say whether it is over.
    pub fn ready(&self, id: EventId, bot: u32, delay_s: [u64; 2]) -> bool {
        let now = self.now_ms();
        let mut g = self.inner.lock();
        let Some(e) = g.events.iter_mut().find(|e| e.id == id) else {
            return false;
        };
        if let Some(w) = e.waiting.iter().find(|w| w.0 == bot) {
            return now >= w.1;
        }
        let (lo, hi) = (delay_s[0], delay_s[1].max(delay_s[0]));
        let span = hi - lo + 1;
        let draw = mix(u64::from(bot), u64::from(id.def), e.first_seen) % span;
        let at = now + (lo + draw) * 1000;
        if e.waiting.len() >= MAX_WAITING {
            e.waiting.remove(0);
        }
        e.waiting.push((bot, at));
        now >= at
    }

    /// Whether a slot is free for `bot` right now (changes nothing).
    pub fn can_join(&self, id: EventId, bot: u32, caps: &Caps) -> Join {
        let now = self.now_ms();
        let g = self.inner.lock();
        Self::check(&g, id, bot, caps, now).0
    }

    /// Takes a slot for `bot` in event `id`, or renews the lease it has.
    pub fn join(&self, id: EventId, bot: u32, caps: &Caps) -> Join {
        let now = self.now_ms();
        let mut g = self.inner.lock();
        let g = &mut *g;
        let (verdict, _) = Self::check(g, id, bot, caps, now);
        match verdict {
            Join::Joined => {
                let e = g.events.iter_mut().find(|e| e.id == id).expect("checked");
                e.slots.push(Slot {
                    bot,
                    lease_until: now + LEASE_MS,
                });
                e.joined_ever += 1;
                e.peak = e.peak.max(e.slots.len() as u32);
                g.stats.joins += 1;
            }
            Join::Already => {
                if let Some(s) = g
                    .events
                    .iter_mut()
                    .find(|e| e.id == id)
                    .and_then(|e| e.slots.iter_mut().find(|s| s.bot == bot))
                {
                    s.lease_until = now + LEASE_MS;
                }
            }
            Join::Full => g.stats.refused_full += 1,
            Join::OverShare => g.stats.refused_share += 1,
            Join::Barred | Join::Gone => {}
        }
        verdict
    }

    /// The verdict for `bot` asking for a slot in `id`, and the slots all events hold.
    fn check(g: &Inner, id: EventId, bot: u32, caps: &Caps, now: u64) -> (Join, u32) {
        let held = |e: &Running| e.slots.iter().filter(|s| s.lease_until > now).count() as u32;
        let total: u32 = g.events.iter().map(held).sum();
        let Some(e) = g.events.iter().find(|e| e.id == id) else {
            return (Join::Gone, total);
        };
        if e.barred(bot, now) {
            return (Join::Barred, total);
        }
        if e.slots.iter().any(|s| s.bot == bot && s.lease_until > now) {
            return (Join::Already, total);
        }
        if held(e) >= caps.per_event {
            return (Join::Full, total);
        }
        if let Some(cap) = g.pop.share_cap(caps.min_level, caps.max_share) {
            if total >= cap {
                return (Join::OverShare, total);
            }
        }
        (Join::Joined, total)
    }

    /// Renews the lease of `bot`; `false` when it holds no slot (the lease ran out, or the event
    /// is over).
    pub fn renew(&self, id: EventId, bot: u32) -> bool {
        let now = self.now_ms();
        let mut g = self.inner.lock();
        match g
            .events
            .iter_mut()
            .find(|e| e.id == id)
            .and_then(|e| e.slots.iter_mut().find(|s| s.bot == bot))
        {
            Some(s) if s.lease_until > now => {
                s.lease_until = now + LEASE_MS;
                true
            }
            _ => false,
        }
    }

    /// `bot` leaves event `id`.
    pub fn leave(&self, id: EventId, bot: u32, why: Leave) {
        let now = self.now_ms();
        let mut g = self.inner.lock();
        let Some(e) = g.events.iter_mut().find(|e| e.id == id) else {
            return;
        };
        e.slots.retain(|s| s.bot != bot);
        match why {
            Leave::Done => {}
            Leave::Hurt => e.bar(bot, now + HURT_BAR_MS),
            Leave::Idle => e.bar(bot, now + IDLE_BAR_MS),
            Leave::Died => {
                let n = match e.deaths.iter_mut().find(|d| d.0 == bot) {
                    Some(d) => {
                        d.1 = d.1.saturating_add(1);
                        d.1
                    }
                    None => {
                        e.deaths.push((bot, 1));
                        1
                    }
                };
                if n >= MAX_DEATHS {
                    e.bar(bot, u64::MAX);
                }
            }
        }
    }

    /// `bot` went offline: it leaves every event without a mark.
    pub fn release_bot(&self, bot: u32) {
        if !self.any() {
            return;
        }
        let mut g = self.inner.lock();
        for e in &mut g.events {
            e.slots.retain(|s| s.bot != bot);
        }
    }

    // ---- evidence --------------------------------------------------------------------------

    /// A participant of event `id` stands at the target (`near`): the time adds up for the
    /// "wave NPCs never showed" judgement.
    pub fn observe(&self, id: EventId, near: bool) {
        if !near {
            return;
        }
        let now = self.now_ms();
        let mut g = self.inner.lock();
        let Some(e) = g.events.iter_mut().find(|e| e.id == id) else {
            return;
        };
        if e.step == 0 {
            return;
        }
        if e.last_near != 0 && now - e.last_near <= NEAR_GAP_MS {
            e.near_ms += now - e.last_near;
        }
        e.last_near = now;
    }

    /// An NPC of the world died (a bot saw `obj` of `template` die): counts for every event whose
    /// current wave waits for that template, each object once.
    pub fn note_death(&self, obj: u32, template: u32) {
        let Some(events) = self.events() else {
            return;
        };
        if !events.is_wave_npc(template) {
            return;
        }
        let mut g = self.inner.lock();
        let g = &mut *g;
        for e in &mut g.events {
            let waits = events
                .get(e.id.def)
                .is_some_and(|d| d.is_kill_target(e.step, template));
            if waits && e.kills.note(obj, template) {
                e.kills_total += 1;
                g.stats.kills += 1;
            }
        }
    }

    /// A bot saw an NPC of `template` appear: a wave NPC of the current wave, or a controller.
    pub fn note_unit(&self, template: u32) {
        let Some(events) = self.events() else {
            return;
        };
        if !events.is_controller(template) && !events.is_wave_npc(template) {
            return;
        }
        let mut g = self.inner.lock();
        for e in &mut g.events {
            let Some(def) = events.get(e.id.def) else {
                continue;
            };
            if def.is_kill_target(e.step, template) {
                e.wave_seen = true;
            } else if def.is_controller(template) {
                e.controller_seen = true;
            }
        }
    }

    // ---- housekeeping ----------------------------------------------------------------------

    /// [`EventBoard::sweep_witnessed`] without the witnesses.
    pub fn sweep(&self, pop: Option<Population>) -> Vec<IssueReport> {
        self.sweep_witnessed(pop)
            .into_iter()
            .map(|w| w.report)
            .collect()
    }

    /// About once a second: expires leases, judges the evidence and returns the issue reports,
    /// each with the bots that were in the event (up to [`MAX_WITNESSES`]) and so saw it.
    /// `pop` replaces the population the share cap reads.
    pub fn sweep_witnessed(&self, pop: Option<Population>) -> Vec<Witnessed> {
        let now = self.now_ms();
        let mut g = self.inner.lock();
        let g = &mut *g;
        if let Some(p) = pop {
            g.pop = p;
        }
        let mut reports = Vec::new();
        let events = self.events();
        for e in &mut g.events {
            e.purge(now);
            let Some(def) = events.and_then(|ev| ev.get(e.id.def)) else {
                continue;
            };
            let facts = Facts {
                def,
                id: e.id,
                now,
                first_seen: e.first_seen,
                step: e.step,
                step_since: e.step_since,
                near_ms: e.near_ms,
                wave_seen: e.wave_seen,
                controller_seen: e.controller_seen,
                kills: &|t| e.kills.of(t),
                satisfied_since: e.satisfied_since,
                told: e.told,
                bots: e.slots.len() as u32,
            };
            let (findings, satisfied) = detect::judge(&facts);
            e.satisfied_since = satisfied;
            for f in findings {
                e.told |= f.bit();
                reports.push(Witnessed {
                    report: detect::report(&facts, f),
                    bots: e.slots.iter().map(|s| s.bot).take(MAX_WITNESSES).collect(),
                });
            }
        }
        // an event the server never ended and nobody told us about leaves the board
        let stale: SmallVec<[EventId; 2]> = g
            .events
            .iter()
            .filter(|e| {
                events.and_then(|ev| ev.get(e.id.def)).is_some_and(|d| {
                    now.saturating_sub(e.first_seen)
                        > (d.force_end_s * 1000.0) as u64 + STALE_AFTER_CAP_MS
                })
            })
            .map(|e| e.id)
            .collect();
        for id in stale {
            self.end(g, id, "it outlived its time cap", now);
        }
        g.stats.reports += reports.len() as u64;
        self.sync_running(g);
        reports
    }
}

/// A cheap well-mixed hash of three numbers (SplitMix64 steps).
fn mix(a: u64, b: u64, c: u64) -> u64 {
    let mut x = a
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(b.rotate_left(21))
        .wrapping_add(c.rotate_left(42));
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}
