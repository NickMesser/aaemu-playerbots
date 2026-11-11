//! One bot: a single tokio task that logs in, lives in the world and survives disconnects.
//!
//! # Architecture
//!
//! [`run_bot`] is the task. It owns everything about the bot ([`Bot`]): the state, the movement
//! and combat controllers, the brain and the game connection. There is no reader task: the task
//! `select!`s over the connection, its tick timer, a one-second housekeeping timer and the command
//! channel, so a bot costs one task and no locks on the hot path.
//!
//! ```text
//!   run_bot ─► session (login, lobby) ─► world loop ─┬─ packet  ─► BotState::apply, Combat::on_event
//!        ▲                                           ├─ tick    ─► Brain::tick(Ctx) ─► Motion::tick ─► send
//!        └── reconnect with backoff ◄────────────────┼─ 1 s     ─► ping, publish to the hub
//!                                                    └─ command ─► Brain / pause / chat
//! ```
//!
//! The tick period is `bots.tick_ms` while the bot moves, fights or is threatened and
//! `bots.idle_tick_ms` otherwise. A [`Ctx`] is built around the task's own fields for each tick and
//! handed to the [`Brain`](brain::Brain); goals never see the connection, only the outbox
//! (`ctx.out`), which the task flushes after the tick.

pub mod avoid;
pub mod brain;
pub mod cmd;
pub mod combat;
pub mod companion;
pub mod coordinator;
pub mod ctx;
pub mod director;
pub mod farm;
pub mod gear;
pub mod goals;
pub mod guild;
pub mod manner;
pub mod market;
pub mod motion;
pub mod mount;
pub mod nav;
pub mod party;
pub mod place;
pub mod pvp;
pub mod rotation;
pub mod session;
pub mod state;
pub mod travel;
pub mod vehicle;
pub mod war;
pub mod watch;
pub mod zones;

use std::fmt::Write as _;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use aa_proto::cs::{self, Out};
use aa_proto::game::{ConnStats, GameConn, Packet, LEVEL_PROXY};
use aa_proto::opcodes::pp;
use aa_proto::sc::ServerEvent;
use aa_proto::sc_world::{DoodadInfo, WorldEvent};
use aa_world::{DoodadFlags, World};
use pb_web::Activity;
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::archetype::{Archetype, Archetypes};
use crate::config::{race_name, Config};
use crate::hub::{Hub, Logger};
use crate::issues::{Dir, IssueReport, LogLevel};
use crate::roster::{RosterEntry, SharedRoster};
use crate::secrets::Secret;

use brain::Brain;
use cmd::{BotCmd, GoalKind, PairKind, PairOrder, PairRole};
use combat::Combat;
use coordinator::CoordHandle;
use ctx::{Ctx, Persona};
use motion::{Motion, MotionCfg, MotionEnv};
use session::{Decoded, SessionError, Wish};
use state::{BotState, UnitKind};
use watch::classify::{self, Cause, Fault, Phase};
use watch::finding::{fault_report, malformed_report, stall_report};
use watch::Finding;

/// Concurrent logins (login server plus lobby), so a restart does not stampede the servers.
pub const LOGIN_PERMITS: usize = 4;
/// A session that lasted this long resets the reconnect backoff.
const HEALTHY_SESSION: Duration = Duration::from_secs(60);
/// Name rejections in a row after which the bot waits like after any other failure.
const MAX_NAME_RETRIES: u32 = 6;
/// How long a bot waits for the server's logout grant at shutdown.
const LEAVE_WAIT: Duration = Duration::from_secs(2);
/// Interval of the temple resurrection request while dead.
const RESURRECT_RETRY_MS: u64 = 5_000;
/// A real player within this many metres is an audience: the bot shows off (life-like movement)
/// and chats.
const WATCH_RADIUS: f32 = 150.0;
/// A real player within this many metres can see the bot, and the bot sends its movement at the
/// client's pace (`bots.move_update_ms`). The server shows a unit to the players in its own 64 m
/// region and the eight around it, so up to about 181 m away.
const VIEW_RADIUS: f32 = 190.0;
/// Gap between two movement updates of a bot that no real player can see (ms).
const UNSEEN_SEND_GAP_MS: u64 = 1_000;
/// The shortest sleep (ms) the movement controller may ask the bot loop for (the packets of a
/// jump).
const MIN_WAKE_MS: u64 = 15;
/// The bot's detail panel is refreshed this often.
const EXTRA_EVERY_SECS: u32 = 5;
/// The most points of the route the web UI gets for a bot.
const PATH_POINTS_MAX: usize = 32;
/// Lifetime of a bot between connects: the longest the connect step may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a bot waits for a free login slot before it counts as a failed attempt (and backs
/// off). Long: a big roster starting up queues for a while.
const PERMIT_WAIT: Duration = Duration::from_secs(180);
/// The longest a bot holds a login slot (login server, connect and lobby together); the slot goes
/// back to the others after that, whatever the individual steps are still doing.
const PERMIT_HOLD: Duration = Duration::from_secs(120);
/// A world session that has not heard a single packet from the server for this long is dead: the
/// server answers every ping (every 3 s) within a moment, so silence means the connection is gone
/// (a half-open TCP connection would otherwise sit there for hours).
pub const READ_IDLE_LIMIT: Duration = Duration::from_secs(25);

/// Everything the bot tasks share.
pub struct Shared {
    /// The configuration.
    pub cfg: Arc<Config>,
    /// The static world data.
    pub world: Arc<World>,
    /// The hub (web UI state, counters, markers).
    pub hub: Arc<Hub>,
    /// The persistent roster.
    pub roster: Arc<SharedRoster>,
    /// The archetypes.
    pub archetypes: Arc<Archetypes>,
    /// The pairing service.
    pub coord: CoordHandle,
    /// Limits concurrent logins.
    pub login_gate: tokio::sync::Semaphore,
    /// The bot account password (never printed).
    pub password: Secret,
}

/// Why a session ended.
enum SessionEnd {
    /// The process is shutting down (or this bot was despawned).
    Shutdown,
    /// Try again at once (the character name was replaced).
    Retry,
    /// The connection ended or could not be made.
    Ended {
        why: String,
        permanent: bool,
        online: Duration,
        /// The connection problem this end comes to, if the server is to blame for it.
        fault: Option<Fault>,
    },
}

/// Why the world loop returned.
enum WorldEnd {
    Shutdown,
    Lost(String, Cause),
}

/// Runs bot `id` until `cancel` fires: connect, play, reconnect with backoff, log out.
pub async fn run_bot(id: u32, shared: Arc<Shared>, cancel: CancellationToken) {
    let Some(entry) = shared.roster.entry(id) else {
        tracing::warn!(bot = id, "no roster entry; not starting");
        return;
    };
    let Some(archetype) = shared.archetypes.get(&entry.archetype).cloned() else {
        tracing::warn!(bot = id, archetype = %entry.archetype, "unknown archetype; not starting");
        return;
    };
    let span = tracing::info_span!("bot", id, name = %entry.char_name);
    let mut bot = Bot::new(entry, archetype, shared);
    bot.run(cancel).instrument(span).await;
}

/// The task state of one bot. See the module docs.
struct Bot {
    shared: Arc<Shared>,
    slot: Arc<crate::hub::BotSlot>,
    log: Logger,
    /// The roster entry (boxed: it is read at login and on renames only, and inline it would cost
    /// the per-bot budget about 140 bytes).
    entry: Box<RosterEntry>,
    persona: Persona,
    state: BotState,
    motion: Motion,
    combat: Combat,
    brain: Brain,
    rng: fastrand::Rng,
    out: Vec<Out>,
    cmd_rx: mpsc::Receiver<BotCmd>,
    clock: Instant,
    paused: bool,
    real_near: bool,
    /// A real player can see the bot (within [`VIEW_RADIUS`]).
    watched: bool,
    /// When the next brain tick is due (bot clock, ms); the loop may wake sooner to move on.
    next_brain_ms: u64,
    online_since: Option<Instant>,
    next_rez_ms: u64,
    /// The combat target was a character at the last look: when that fight ends the server-side
    /// target is cleared, so a duel opponent does not stay the bot's target.
    char_target: bool,
    last_stats: ConnStats,
    zone_key: Option<u16>,
    detail: String,
    secs: u32,
    sessions: u32,
    /// A connection problem was reported and the bot has not been healthy since: the failed
    /// reconnects of one outage are one episode, one report.
    outage_reported: bool,
}

impl Bot {
    fn new(entry: RosterEntry, archetype: Arc<Archetype>, shared: Arc<Shared>) -> Bot {
        let cfg = &shared.cfg;
        let temper = entry.temperament.with_spread(cfg.behavior.persona_spread);
        let slot = shared.hub.register(initial_snapshot(&entry, &archetype));
        let cmd_rx = slot.attach();
        // (`[personality] enabled = false`: every bot plays the neutral profile)
        let profile = if cfg.personality.enabled {
            entry.profile()
        } else {
            crate::roster::Profile::NEUTRAL
        };
        slot.with(|g| g.profile = Some(profile.view()));
        let persona = Persona {
            id: entry.id,
            name: entry.char_name.clone().into_boxed_str(),
            archetype,
            temper,
            profile,
        };
        let brain = Brain::new(&cfg.behavior, &persona.temper);
        let mut motion = Motion::new(MotionCfg {
            run_speed: cfg.bots.move_speed,
            walk_speed: cfg.bots.walk_speed,
        });
        motion.attach_nav(nav::link_for_bot(
            &shared.hub,
            &shared.world,
            cfg,
            slot.clone(),
            entry.id,
        ));
        let mut rng = fastrand::Rng::new();
        if cfg.behavior.lifelike {
            motion.attach_life(manner::Life::new(
                entry.id,
                cfg.behavior.jump_rate,
                cfg.behavior.emotes,
                0,
                &mut rng,
            ));
        }
        Bot {
            log: Logger::new(slot.clone()),
            slot,
            entry: Box::new(entry),
            persona,
            state: BotState::new(),
            motion,
            combat: Combat::new(),
            brain,
            rng,
            out: Vec::with_capacity(8),
            cmd_rx,
            clock: Instant::now(),
            paused: false,
            real_near: false,
            watched: false,
            next_brain_ms: 0,
            online_since: None,
            next_rez_ms: 0,
            char_target: false,
            last_stats: ConnStats::default(),
            zone_key: None,
            detail: String::new(),
            secs: 0,
            sessions: 0,
            outage_reported: false,
            shared,
        }
    }

    fn now_ms(&self) -> u64 {
        self.clock.elapsed().as_millis() as u64 + 1
    }

    // ---- the reconnect loop --------------------------------------------------------------

    async fn run(&mut self, cancel: CancellationToken) {
        let bots = &self.shared.cfg.bots;
        let (min, max) = (
            Duration::from_secs(bots.reconnect_min_secs),
            Duration::from_secs(bots.reconnect_max_secs),
        );
        let mut backoff = min;
        let mut name_retries = 0u32;
        loop {
            if cancel.is_cancelled() {
                break;
            }
            self.set_connecting();
            let end = self.session(&cancel).await;
            self.go_offline();
            match end {
                SessionEnd::Shutdown => break,
                SessionEnd::Retry if name_retries < MAX_NAME_RETRIES => {
                    name_retries += 1;
                    continue;
                }
                SessionEnd::Retry => {}
                SessionEnd::Ended {
                    why,
                    permanent,
                    online,
                    fault,
                } => {
                    name_retries = 0;
                    self.log.say(format!("offline: {why}"));
                    if online >= HEALTHY_SESSION {
                        backoff = min;
                        self.outage_reported = false;
                    } else {
                        self.shared.hub.pop.note_failed_session();
                    }
                    if let Some(f) = fault {
                        self.report_fault(f, &why);
                    }
                    if permanent {
                        backoff = max;
                    }
                }
            }
            let wait = jittered(backoff, &mut self.rng);
            tracing::debug!(secs = wait.as_secs(), "reconnecting");
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(wait) => {}
            }
            backoff = (backoff * 2).min(max);
        }
        self.go_offline();
    }

    fn set_connecting(&mut self) {
        self.slot.with(|g| {
            g.snap.activity = Activity::Connecting;
            g.snap.detail.clear();
            g.snap.detail.push_str("logging in");
        });
    }

    fn go_offline(&mut self) {
        // (the journal's sessions: the time of a session that reached the world)
        if let Some(since) = self.online_since.take() {
            let (secs, now) = (since.elapsed().as_secs(), crate::hub::unix_ms() / 1000);
            self.shared
                .hub
                .journal
                .update(self.entry.id, |j| j.sessions.logout(now, secs));
        }
        self.shared.hub.contest.release_bot(self.entry.id);
        self.shared.hub.events.release_bot(self.entry.id);
        self.shared.hub.farms.release_bot(self.entry.id);
        self.motion.leave_world();
        self.slot.set_offline();
    }

    // ---- one session ---------------------------------------------------------------------

    async fn session(&mut self, cancel: &CancellationToken) -> SessionEnd {
        let shared = self.shared.clone();
        let cfg = &shared.cfg;
        let permit = tokio::select! {
            _ = cancel.cancelled() => return SessionEnd::Shutdown,
            p = tokio::time::timeout(PERMIT_WAIT, shared.login_gate.acquire()) => match p {
                Ok(p) => p.ok(),
                Err(_) => {
                    return failed(
                        SessionError::Timeout("a free login slot"),
                        Duration::ZERO,
                        Phase::Login,
                    )
                }
            },
        };
        // The slot is held until the bot is in the world, but not longer than PERMIT_HOLD.
        let hold = Instant::now() + PERMIT_HOLD;
        let held_too_long = || {
            failed(
                SessionError::Timeout("the login slot"),
                Duration::ZERO,
                Phase::Login,
            )
        };
        self.sessions += 1;
        let ticket = tokio::select! {
            _ = cancel.cancelled() => return SessionEnd::Shutdown,
            r = tokio::time::timeout_at(hold, session::login_server(
                &cfg.server.login,
                &self.entry.account,
                shared.password.expose(),
                cfg.server.world_id,
            )) => match r {
                Ok(Ok(t)) => t,
                Ok(Err(e)) => return failed(e, Duration::ZERO, Phase::Login),
                Err(_) => return held_too_long(),
            },
        };
        let connect = GameConn::connect(ticket.cookie.game_addr);
        let connect_by = (Instant::now() + CONNECT_TIMEOUT).min(hold);
        let mut conn = tokio::select! {
            _ = cancel.cancelled() => return SessionEnd::Shutdown,
            r = tokio::time::timeout_at(connect_by, connect) => match r {
                Ok(Ok(c)) => c,
                Ok(Err(e)) => return failed(e.into(), Duration::ZERO, Phase::Connect),
                Err(_) => {
                    return failed(
                        SessionError::Timeout("the game server"),
                        Duration::ZERO,
                        Phase::Connect,
                    )
                }
            },
        };
        self.state.reset();
        self.combat.reset();
        self.char_target = false;
        self.last_stats = ConnStats::default();
        let name = self.entry.char_name.clone();
        let wish = Wish {
            name: &name,
            char_id: self.entry.char_id,
            race: self.entry.race,
            gender: self.entry.gender,
            ability: self.persona.archetype.ability,
        };
        let entered = tokio::select! {
            _ = cancel.cancelled() => {
                session::close(&mut conn).await;
                return SessionEnd::Shutdown;
            }
            r = tokio::time::timeout_at(hold, session::enter_game(
                &mut conn,
                &mut self.state,
                ticket.account_id,
                ticket.cookie.cookie,
                &wish,
                self.clock,
            )) => match r {
                Ok(r) => r,
                Err(_) => {
                    session::close(&mut conn).await;
                    return held_too_long();
                }
            },
        };
        // The slot goes back as soon as the lobby is over, whatever its outcome.
        drop(permit);
        let entered = match entered {
            Ok(e) => e,
            Err(e) if e.is_name_rejected() => {
                session::close(&mut conn).await;
                return self.rename();
            }
            Err(e) => {
                session::close(&mut conn).await;
                return failed(e, Duration::ZERO, Phase::Lobby);
            }
        };
        self.record_character(&entered);
        self.log.say(format!(
            "in the world at {:.0}, {:.0} ({})",
            self.state.me.pos[0], self.state.me.pos[1], ticket.world_name
        ));
        let started = Instant::now();
        let end = self.world_loop(&mut conn, cancel).await;
        self.count_packets(&conn);
        let online = started.elapsed();
        match end {
            WorldEnd::Shutdown => {
                session::leave(&mut conn, &mut self.state, LEAVE_WAIT).await;
                self.count_packets(&conn);
                SessionEnd::Shutdown
            }
            WorldEnd::Lost(why, cause) => {
                session::close(&mut conn).await;
                SessionEnd::Ended {
                    why,
                    permanent: false,
                    online,
                    fault: Some(Fault {
                        phase: Phase::World,
                        cause,
                    }),
                }
            }
        }
        // `ticket.login` drops here, after the game connection.
    }

    /// The server rejected the name: take a new one and try again.
    fn rename(&mut self) -> SessionEnd {
        let id = self.entry.id;
        let new = self
            .shared
            .roster
            .update(|r| (r.regenerate_name(id, &mut self.rng), true));
        match new {
            Some(name) => {
                self.log.say(format!(
                    "the server rejected {}; trying {name}",
                    self.entry.char_name
                ));
                self.persona.name = name.clone().into_boxed_str();
                self.entry.char_name = name;
                self.entry.char_id = None;
                SessionEnd::Retry
            }
            None => SessionEnd::Ended {
                why: "the roster entry is gone".into(),
                permanent: true,
                online: Duration::ZERO,
                fault: None,
            },
        }
    }

    /// Remembers the character the lobby ended up with.
    fn record_character(&mut self, entered: &session::Entered) {
        let id = self.entry.id;
        self.shared.hub.mark_bot_char(entered.char_id);
        self.slot.char_id.store(entered.char_id, Ordering::Relaxed);
        self.slot.obj_id.store(self.state.me.obj, Ordering::Relaxed);
        let changed = self.shared.roster.update(|r| {
            let mut changed = r.set_char_id(id, entered.char_id);
            if !entered.name.is_empty() && !entered.name.eq_ignore_ascii_case(&self.entry.char_name)
            {
                changed |= r.adopt_name(id, &entered.name);
            }
            (changed, changed)
        });
        if changed {
            if let Some(e) = self.shared.roster.entry(id) {
                *self.entry = e;
                self.persona.name = self.entry.char_name.clone().into_boxed_str();
            }
        }
        self.entry.char_id = Some(entered.char_id);
        self.shared.hub.mark_bot_name(&self.entry.char_name);
        if entered.created {
            self.log.say(format!("created character {}", entered.name));
        }
    }

    // ---- the world loop ------------------------------------------------------------------

    async fn world_loop(&mut self, conn: &mut GameConn, cancel: &CancellationToken) -> WorldEnd {
        let now = self.now_ms();
        self.motion
            .enter_world(self.state.me.obj, self.state.me.pos, self.state.me.yaw, now);
        self.online_since = Some(Instant::now());
        let unix = crate::hub::unix_ms() / 1000;
        self.shared
            .hub
            .journal
            .update(self.entry.id, |j| j.sessions.login(unix));
        self.paused = false;
        self.secs = 0;
        self.next_brain_ms = 0;
        self.publish(conn);
        let mut housekeeping = tokio::time::interval(Duration::from_secs(1));
        housekeeping.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let tick = tokio::time::sleep(Duration::from_millis(self.shared.cfg.bots.tick_ms));
        tokio::pin!(tick);
        let mut watch = session::ReadWatch::new(Instant::now(), READ_IDLE_LIMIT);
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return WorldEnd::Shutdown,
                _ = &mut tick => {
                    let brain = self.now_ms() >= self.next_brain_ms;
                    if brain {
                        if let Some(end) = self.on_tick() {
                            return end;
                        }
                    } else {
                        self.on_motion_tick();
                    }
                    if let Some(end) = self.flush(conn, cancel).await {
                        return end;
                    }
                    tick.as_mut().reset(Instant::now() + self.next_wake(brain));
                }
                _ = housekeeping.tick() => {
                    if let Some(quiet) = watch.expired(Instant::now()) {
                        return WorldEnd::Lost(
                            format!(
                                "no packet from the server for {} s (the connection is dead)",
                                quiet.as_secs()
                            ),
                            Cause::Silent,
                        );
                    }
                    self.housekeeping(conn);
                    if let Some(end) = self.flush(conn, cancel).await {
                        return end;
                    }
                }
                packet = conn.recv() => match packet {
                    Ok(p) => {
                        watch.heard(Instant::now());
                        self.on_packet(&p);
                        if let Some(end) = self.flush(conn, cancel).await {
                            return end;
                        }
                    }
                    Err(e) => {
                        let cause = session::cause_of(&e, false);
                        return WorldEnd::Lost(e.to_string(), cause);
                    }
                },
                cmd = self.cmd_rx.recv() => match cmd {
                    Some(c) => self.on_command(c),
                    // The slot was replaced: this task is stale.
                    None => return WorldEnd::Shutdown,
                },
            }
        }
    }

    /// Writes the outbox. Gives up at once when `cancel` fires (a shutdown must not wait for a
    /// dead connection) and treats a write that does not finish in time as a lost connection.
    async fn flush(&mut self, conn: &mut GameConn, cancel: &CancellationToken) -> Option<WorldEnd> {
        self.note_sent();
        match session::flush_until_cancelled(conn, &mut self.out, cancel).await {
            session::Flushed::Done => None,
            session::Flushed::Cancelled => Some(WorldEnd::Shutdown),
            session::Flushed::Failed(e) => {
                let cause = session::cause_of(&e, true);
                Some(WorldEnd::Lost(e.to_string(), cause))
            }
        }
    }

    /// Notes the packets about to be written in the trail, and registers those the server must
    /// answer with the expectation tracker.
    fn note_sent(&mut self) {
        if self.out.is_empty() {
            return;
        }
        let now = self.now_ms();
        let st = &mut self.state;
        let (pos, units) = (st.me.pos, &st.units);
        for o in &self.out {
            st.watch.packet(now, Dir::C2S, o.level, o.opcode);
            st.watch
                .tracker
                .on_sent(o, now, pos, |obj| units.get(&obj).map_or(0, |u| u.id));
        }
    }

    /// The time from one brain tick to the next: `bots.tick_ms` while the bot moves, fights or
    /// is busy, `bots.idle_tick_ms` otherwise.
    fn brain_period_ms(&self) -> u64 {
        let b = &self.shared.cfg.bots;
        let active = self.motion.busy()
            || self.combat.busy()
            || !self.state.threats.is_empty()
            || self.state.trade.active()
            || self.state.travel.busy()
            || self.state.vehicle.fast_tick()
            || !self.state.me.spawned;
        if active {
            b.tick_ms
        } else {
            b.idle_tick_ms
        }
    }

    /// How long the loop sleeps after a tick (`brain`: it was a brain tick, which schedules the
    /// next one). See [`next_wake_ms`].
    fn next_wake(&mut self, brain: bool) -> Duration {
        let now = self.now_ms();
        if brain {
            self.next_brain_ms = now + self.brain_period_ms();
        }
        let walking_in_view = self.watched && self.motion.is_moving();
        let at = next_wake_ms(
            now,
            self.next_brain_ms,
            walking_in_view.then_some(self.shared.cfg.bots.move_update_ms),
            self.motion.wake_in(now),
        );
        Duration::from_millis(at - now)
    }

    fn on_packet(&mut self, p: &Packet) {
        let now = self.now_ms();
        self.state.now_ms = now;
        let w = &mut self.state.watch;
        w.packet(now, Dir::S2C, p.level, p.opcode);
        w.conn.on_rx(now);
        w.on_raw(p.level, p.opcode, &p.body, self.state.me.obj, now);
        if tracing::enabled!(target: "pb_move", tracing::Level::TRACE) {
            manner::trace_packet(p.level, p.opcode, &p.body);
        }
        if aa_proto::sc_emote::is_expressed(p.level, p.opcode) {
            if let Ok(e) = aa_proto::sc_emote::read_expressed(&p.body) {
                manner::on_expressed(
                    &mut self.motion,
                    &self.state,
                    &self.shared.hub,
                    &e,
                    &mut self.rng,
                );
            }
        }
        if p.level == LEVEL_PROXY && p.opcode == pp::PongPacket {
            self.on_pong(p, now);
        }
        let (mut ev, ship) = match session::decode_checked(p) {
            Decoded::Event(ev) => (ev, None),
            Decoded::Ship { moved, motion } => (moved, Some(motion)),
            Decoded::Ignored => return,
            Decoded::Malformed(err) => {
                self.on_malformed(p, &err);
                return;
            }
        };
        // (the crops of the bot are not kept as doodads in view: the farm memory follows them)
        if self.shared.cfg.farm.enabled {
            if let ServerEvent::World(w) = &ev {
                if farm::observe(&mut self.state, &self.shared.world, w) {
                    farm::publish(
                        &self.shared.hub,
                        self.entry.id,
                        &self.state,
                        &self.shared.world,
                    );
                }
            }
        }
        if let ServerEvent::World(WorldEvent::DoodadsCreated(list)) = &mut ev {
            let world = &self.shared.world;
            list.retain(|d| keep_doodad(world, d));
            if list.is_empty() {
                return;
            }
        }
        self.state.apply(&ev);
        // (the guild lists the server sends name the guilds that exist: those names are taken)
        if let ServerEvent::Activity(aa_proto::sc_activity::ActivityEvent::Guild(g)) = &ev {
            guild::observe(&self.shared.hub.guilds, g);
        }
        crate::events::on_server_event(
            &self.shared.hub.events,
            &self.shared.world,
            &ev,
            &self.state,
        );
        let queued = self.out.len();
        self.combat
            .on_event(&ev, self.state.me.obj, now, &mut self.out);
        // (a ship's heading and helm follow its position event through the same path)
        if let Some(ship) = &ship {
            self.state.apply(ship);
            self.combat
                .on_event(ship, self.state.me.obj, now, &mut self.out);
        }
        self.pin_position(queued, now);
        self.clear_target_after_player_fight();
        self.drain_findings();
    }

    /// A pong: its first field is the bot clock at which the ping left.
    fn on_pong(&mut self, p: &Packet, now: u64) {
        let Some(head) = p.body.first_chunk::<8>() else {
            return;
        };
        let sent = i64::from_le_bytes(*head);
        // (`now` is the bot clock plus one, see `now_ms`)
        let rtt = (now as i64 - 1).saturating_sub(sent).max(0) as u64;
        self.state.watch.conn.on_pong(rtt);
    }

    /// A packet the parser rejects: one report per packet kind and session, with the head of the
    /// body.
    fn on_malformed(&mut self, p: &Packet, err: &str) {
        if self.state.watch.new_malformed(p.level, p.opcode) {
            let report = malformed_report(p.level, p.opcode, err, &p.body);
            self.log.say(format!(
                "the server sent {} that does not parse: {err}",
                p.name()
            ));
            self.file_issue(report);
        }
    }

    /// Files what the detectors in the state, the goals and the combat controller left.
    fn drain_findings(&mut self) {
        if !self.state.watch.has_findings() {
            return;
        }
        for f in self.state.watch.take_findings() {
            let report = f.report(&*self.shared.world);
            self.file_issue(report);
        }
    }

    /// The connection ended or could not be made: one report per outage.
    fn report_fault(&mut self, fault: Fault, why: &str) {
        if !classify::new_episode(&mut self.outage_reported) {
            return;
        }
        if matches!(fault.phase, Phase::Login | Phase::Connect) {
            // (no packet of this attempt: the trail of the previous session is not evidence)
            self.state.watch.trail.clear();
        }
        self.file_issue(fault_report(fault, why));
    }

    /// When a fight against a character (a duel) is over, clears the server-side target: it would
    /// stay the opponent, who may be a real player, and the server aims some commands at the
    /// sender's target. (The GM lines name the bot themselves as well, see [`crate::gm`].)
    fn clear_target_after_player_fight(&mut self) {
        match self.combat.target() {
            None => {
                if self.char_target {
                    self.out.push(cs::change_target(0));
                }
                self.char_target = false;
            }
            // (a target out of view: keep what we knew)
            Some(o) => {
                if let Some(u) = self.state.units.get(&o) {
                    self.char_target = u.kind == UnitKind::Character;
                }
            }
        }
    }

    /// Tells the server exactly where the bot is before the first packet at `out[queued..]`
    /// whose range it checks (loot, talk, sell, craft, cast): its picture of the bot's position
    /// lags by a second of walking when the movement updates are spaced out.
    fn pin_position(&mut self, queued: usize, now_ms: u64) {
        let combat = self.combat.target().is_some();
        motion::pin_before_range_checked(&mut self.motion, &mut self.out, queued, now_ms, combat);
    }

    fn on_command(&mut self, cmd: BotCmd) {
        // (the nav result is moved into the motion controller, not borrowed)
        let cmd = match cmd {
            BotCmd::PathReady { ticket, result } => {
                self.motion.on_path_ready(ticket, *result);
                return;
            }
            other => other,
        };
        // A pack carrier does not take a coordinator pairing (a duel or a party could take it
        // into a fight); the operator's explicit goals are still obeyed.
        let pack_busy =
            goals::trade_run::carried_pack_in(&self.shared.world, &self.state.equipment).is_some()
                || self.state.econ.lost_pack().is_some();
        match &cmd {
            BotCmd::Pause => {
                self.paused = true;
                self.motion.stop();
                self.combat.disengage();
            }
            BotCmd::Resume => self.paused = false,
            // (the packet builders refuse a line that would run as a command)
            BotCmd::Say(text) => self.out.extend(goals::social::say_packet(text)),
            BotCmd::Whisper { to, text } => {
                self.out.extend(goals::social::whisper_packet(to, text))
            }
            BotCmd::Profile(patch) => self.set_profile(patch),
            BotCmd::DuelWith(other) => self.pair_with(*other, PairKind::Duel),
            BotCmd::TradeWith(other) => self.pair_with(*other, PairKind::Trade),
            BotCmd::ServerLogLine { level, line } => self.on_server_log_line(*level, line),
            BotCmd::Pair(_) | BotCmd::Party(_) if pack_busy => {
                self.log
                    .say("carrying a trade pack; not taking a pairing or party");
            }
            BotCmd::Goal(_) | BotCmd::GoalWith { .. } | BotCmd::Pair(_) | BotCmd::Party(_) => {
                self.brain.command(&cmd)
            }
            BotCmd::PathReady { .. } => {}
        }
    }

    /// The operator (or the agent API) changed the bot's personality: it takes effect at once and
    /// is kept in the roster.
    fn set_profile(&mut self, patch: &crate::roster::ProfilePatch) {
        let profile = patch.apply(self.persona.profile);
        self.persona.profile = profile;
        self.entry.profile = Some(profile);
        let id = self.entry.id;
        self.shared
            .roster
            .update(|r| ((), r.set_profile(id, profile)));
        self.slot.with(|g| g.profile = Some(profile.view()));
        self.log.say(format!("personality: {}", profile.describe()));
    }

    /// The operator asked this bot to duel or trade with bot `other`.
    fn pair_with(&mut self, other: u32, kind: PairKind) {
        if goals::trade_run::carried_pack_in(&self.shared.world, &self.state.equipment).is_some()
            || self.state.econ.lost_pack().is_some()
        {
            self.log
                .say("carrying a trade pack; not starting a duel or trade");
            return;
        }
        let (partner_char, partner_name) = coordinator::Dispatch::who(&*self.shared.hub, other);
        let pos = self.state.me.pos;
        let order = PairOrder {
            kind,
            role: PairRole::Initiator,
            partner_bot: other,
            partner_char,
            partner_name: partner_name.into_boxed_str(),
            meet_at: (pos[0], pos[1]),
        };
        let goal = match kind {
            PairKind::Duel => GoalKind::Duel,
            PairKind::Trade => GoalKind::Trade,
        };
        self.brain.queue(goal, Some(order));
    }

    // ---- server issue reports ------------------------------------------------------------

    /// Files `report` for this bot: the bot's identity, position, zone, goal and last log lines are
    /// attached (unless the detector set them), then the hub counts it. Cheap, never waits for I/O.
    /// A detector in the bot loop calls this; goals call [`Ctx::report_issue`].
    fn file_issue(&self, report: IssueReport) {
        let (pos, char_id) = (self.state.me.pos, self.state.me.char_id);
        let report = self.state.watch.attach_trail(report, self.now_ms());
        let report = self
            .slot
            .with(|g| crate::issues::attach_context(report, g, char_id, Some(pos)));
        self.shared.hub.report_issue(report);
    }

    /// The server logged a warning or an error that names this bot's character: files a
    /// `ServerLogDuringAction` report (with the packet trail of the moment).
    fn on_server_log_line(&mut self, level: LogLevel, line: &str) {
        if classify::routine_server_line(crate::issues::logtail::split_line(line).message) {
            return;
        }
        let report = crate::issues::server_log_report(level, line, &self.persona.name);
        self.file_issue(report);
    }

    /// One brain tick: reactions, resurrection, goals, movement. Returns the reason to leave the
    /// loop when the connection should end.
    fn on_tick(&mut self) -> Option<WorldEnd> {
        let now_ms = self.now_ms();
        self.state.now_ms = now_ms;
        if let Some(end) = self.react() {
            return Some(end);
        }
        self.state.me.pos = self.motion.pos();
        self.state.me.yaw = self.motion.yaw();

        if self.state.me.dead && now_ms >= self.state.me.respawn_at_ms && now_ms >= self.next_rez_ms
        {
            self.out.push(cs::resurrect(false));
            self.next_rez_ms = now_ms + RESURRECT_RETRY_MS;
        }
        let moment = manner::Moment {
            audience: self.real_near,
            busy: self.life_busy(),
            online_ms: self
                .online_since
                .map_or(0, |t| t.elapsed().as_millis() as u64),
        };
        manner::tick(
            &mut self.motion,
            &self.state,
            &self.shared.hub,
            moment,
            &mut self.out,
            &mut self.rng,
        );
        let queued = self.out.len();
        self.motion.sync_place(&self.state.place);
        if !self.paused || self.state.me.dead {
            let mut ctx = Ctx {
                world: &self.shared.world,
                cfg: &self.shared.cfg,
                persona: &self.persona,
                state: &mut self.state,
                motion: &mut self.motion,
                combat: &mut self.combat,
                out: &mut self.out,
                coord: &self.shared.coord,
                hub: &self.shared.hub,
                log: &self.log,
                rng: &mut self.rng,
                now: Instant::now(),
                now_ms,
                real_player_near: self.real_near,
            };
            self.brain.tick(&mut ctx);
        }
        self.pin_position(queued, now_ms);
        self.motion_tick(now_ms);
        self.clear_target_after_player_fight();
        self.drain_findings();
        None
    }

    /// A tick between two brain ticks: the bot walks on and sends its movement update (a walker a
    /// real player can see gets one every `bots.move_update_ms`, as the client sends them), or
    /// the next packet of a jump. Nothing else runs.
    fn on_motion_tick(&mut self) {
        let now_ms = self.now_ms();
        self.state.now_ms = now_ms;
        self.motion_tick(now_ms);
    }

    /// Moves the bot by the time since the last movement tick and queues its updates.
    fn motion_tick(&mut self, now_ms: u64) {
        // A riding bot sends its updates for the mount (the server ignores a rider's own moves).
        // Synced here as well as in the duty so a detach between two brain ticks stops it at once.
        match self.state.travel.mount_ref() {
            Some(m) if m.mounted() && m.mate_obj != 0 => self.motion.set_move_obj(m.mate_obj),
            _ => self.motion.set_move_obj(0),
        }
        let send_gap_ms = if self.watched {
            self.shared.cfg.bots.move_update_ms
        } else {
            UNSEEN_SEND_GAP_MS
        };
        let mut env = MotionEnv {
            ground: self.state.place.ground(&self.shared.world),
            now_ms,
            out: &mut self.out,
            rng: &mut self.rng,
            send_gap_ms,
            combat: self.combat.target().is_some(),
        };
        self.motion.tick(&mut env);
        self.state.me.pos = self.motion.pos();
    }

    /// Whether the bot must not show off now: it fights, loots, trades, travels by portal, sails,
    /// is in an instance, is loading, paused or dead.
    fn life_busy(&self) -> bool {
        let st = &self.state;
        self.paused
            || st.me.dead
            || !st.me.spawned
            || self.combat.busy()
            || !st.threats.is_empty()
            || st.trade.active()
            || st.travel.busy()
            || st.vehicle.seat().is_some()
            || st.vehicle.boat().is_some()
            || st.place.in_instance()
            || st.place.loading()
    }

    /// The mate and the glide end with a death, a teleport or an instance load (the server
    /// removes the active mates then: `systems/instances/dungeon.rs:524`,
    /// `systems/vehicles/mod.rs:531`).
    fn mount_gone(&mut self) {
        let base = (
            self.shared.cfg.bots.move_speed,
            self.shared.cfg.bots.walk_speed,
        );
        mount::on_gone(&mut self.state, &mut self.motion, &mut self.out, base);
    }

    /// Answers what the state noticed: teleports, instance loads, death, resurrection, kicks.
    fn react(&mut self) -> Option<WorldEnd> {
        let r = std::mem::take(&mut self.state.reactions);
        if let Some(p) = r.teleport {
            self.motion.teleported(p);
            self.out.push(cs::teleport_ended(p[0], p[1], p[2]));
            self.mount_gone();
        }
        if r.load_instance {
            if self.state.place.needs_resolve() {
                self.state.place.resolve(&self.shared.world);
            }
            if r.world_changed {
                // (the target and the loot job belonged to the world the bot left)
                self.combat.reset();
            }
            self.motion.teleported(self.state.me.pos);
            self.out.push(cs::instance_loaded());
            self.state.place.loaded();
            self.mount_gone();
        }
        if r.died {
            self.motion.stop();
            self.combat.reset();
            self.mount_gone();
            self.log.say("died");
            // (the back slot also holds a glider, which is no loss)
            if r.pack_dropped
                && self
                    .state
                    .econ
                    .lost_pack()
                    .is_some_and(|p| !self.shared.world.specialty_buyers(p.item).is_empty())
            {
                self.log
                    .say("died carrying a trade pack; it is on the ground where the bot died");
            }
        }
        if let Some(p) = r.resurrected {
            self.motion.teleported(p);
            self.next_rez_ms = 0;
            if !self.brain.in_dungeon_run() {
                // A dropped pack is gone back for at once (the run rests first if the bot is
                // still weak); everything else rests as before.
                let lost = self.state.econ.lost_pack();
                let recover =
                    lost.is_some_and(|p| !self.shared.world.specialty_buyers(p.item).is_empty());
                if recover {
                    self.brain.queue(GoalKind::TradeRun, None);
                } else {
                    if lost.is_some() {
                        // (the back slot held a glider, not a trade pack)
                        self.state.econ.clear_lost_pack();
                    }
                    self.brain.queue(GoalKind::Rest, None);
                }
            }
            self.log.say("resurrected");
        }
        if let Some((reason, msg)) = r.kicked {
            return Some(WorldEnd::Lost(
                format!("kicked (reason {reason}): {msg}"),
                Cause::Kicked(reason),
            ));
        }
        if r.leave_granted {
            return Some(WorldEnd::Lost(
                "the server logged the character out".into(),
                Cause::Logout,
            ));
        }
        None
    }

    // ---- housekeeping and publishing -----------------------------------------------------

    fn housekeeping(&mut self, conn: &GameConn) {
        self.secs += 1;
        // The first housekeeping tick fires as the loop starts: ping then and every
        // `PING_EVERY` after (the server drops connections that have not pinged in its sweep).
        if (self.secs - 1).is_multiple_of(session::PING_EVERY.as_secs() as u32) {
            let ms = self.clock.elapsed().as_millis() as i64;
            self.out.push(cs::ping(ms));
        }
        let hub = &self.shared.hub;
        self.real_near = self
            .state
            .any_character_within(WATCH_RADIUS, |id| hub.is_bot_char(id));
        self.watched = self.real_near
            || self
                .state
                .any_character_within(VIEW_RADIUS, |id| hub.is_bot_char(id));
        self.feed_population();
        self.watch_server();
        self.publish(conn);
    }

    /// Hands the players in view to the census and, every ten seconds (staggered by bot id), the
    /// fresh auction price notes to the economy board.
    fn feed_population(&mut self) {
        let hub = &self.shared.hub;
        let now = crate::hub::unix_ms();
        crate::population::census::observe(
            &self.state,
            &hub.pop.census,
            &*self.shared.world,
            &|id| hub.is_bot_char(id),
            self.entry.id,
            now,
        );
        if self.secs.wrapping_add(self.entry.id).is_multiple_of(10) {
            if let Some(m) = self.state.market.get() {
                crate::population::economy::sample_notes(
                    &hub.pop.economy,
                    self.entry.id,
                    m.auction.prices(),
                    self.state.now_ms,
                    now,
                );
            }
        }
    }

    /// The once-a-second pass of the detectors that look at time: requests the server never
    /// answered, a stalled connection, a death nobody announced or undid.
    fn watch_server(&mut self) {
        let now = self.now_ms();
        let st = &mut self.state;
        let pos = st.me.pos;
        let rx = st.watch.conn.last_rx_ms();
        if !st.watch.tracker.is_empty() {
            for p in st.watch.tracker.take_due(now, pos, rx) {
                st.watch.push(Finding::NoResponse {
                    req: p.req,
                    id: p.id,
                    aux: p.aux,
                    waited_ms: now.saturating_sub(p.sent_ms).min(u64::from(u32::MAX)) as u32,
                });
            }
        }
        let (dead, respawn_at) = (st.me.dead, st.me.respawn_at_ms);
        if let Some(d) = st.watch.death.check(now, dead, respawn_at) {
            st.watch.push(Finding::Death(d));
        }
        let stall = st.watch.conn.check(now);
        self.drain_findings();
        if let Some(s) = stall {
            self.log.say(format!(
                "the server is slow: nothing for {} s, ping {} s",
                s.quiet_ms / 1000,
                s.rtt_ms / 1000
            ));
            self.file_issue(stall_report(s));
        }
    }

    fn count_packets(&mut self, conn: &GameConn) {
        let s = conn.stats();
        let l = self.last_stats;
        self.shared.hub.count(
            s.packets_in.saturating_sub(l.packets_in),
            s.bytes_in.saturating_sub(l.bytes_in),
            s.packets_out.saturating_sub(l.packets_out),
            s.bytes_out.saturating_sub(l.bytes_out),
        );
        self.last_stats = s;
    }

    fn activity(&self) -> Activity {
        if self.state.me.dead {
            return Activity::Dead;
        }
        if !self.state.me.spawned {
            return Activity::Connecting;
        }
        if self.paused {
            return Activity::Idle;
        }
        match self.brain.goal() {
            Some(g) => g.activity(self.motion.is_moving(), self.combat.target().is_some()),
            None => {
                if self.motion.is_moving() {
                    Activity::Traveling
                } else {
                    Activity::Idle
                }
            }
        }
    }

    /// Writes the bot's snapshot into its hub slot and adds the packet counters.
    fn publish(&mut self, conn: &GameConn) {
        self.count_packets(conn);
        let activity = self.activity();
        self.detail.clear();
        self.describe();
        let me = &self.state.me;
        let world = &self.shared.world;
        let zone_key = world.zone_key_at(me.pos[0], me.pos[1]);
        let zone_changed = zone_key != self.zone_key;
        self.zone_key = zone_key;
        // (every zone the bot stands in goes into the journal's `seen`, whatever it does)
        if let Some(k) = zone_key.filter(|_| zone_changed) {
            let (journal, id) = (&self.shared.hub.journal, self.entry.id);
            if !journal.read(id, |j| j.seen.has_zone(k)) {
                journal.update(id, |j| j.seen.add_zone(k));
            }
        }
        let online_secs = self
            .online_since
            .map_or(0, |t| t.elapsed().as_secs() as u32);
        let with_extra = self.secs.is_multiple_of(EXTRA_EVERY_SECS);
        let goal = self.brain.goal_name();
        let paused = self.paused;
        let has_house = !self.state.houses.is_empty();
        let (level, hp, pos, money) = (me.level, me.hp_pct(), me.pos, me.money);
        // (the guild part of the snapshot is rebuilt only when the guild changed)
        let char_id = me.char_id;
        let hub = &self.shared.hub;
        let guild_info =
            guild::snapshot_if_changed(&mut self.state.guild, |c| hub.is_bot_char(c), char_id);
        // (and so is the party part)
        let party_info =
            companion::snapshot_if_changed(&mut self.state.party, |c| hub.is_bot_char(c), char_id);
        let mother = if me.faction != 0 {
            world.mother_faction(me.faction)
        } else {
            0
        };
        self.slot.with(|g| {
            if let Some(info) = guild_info {
                g.snap.guild = info;
            }
            if let Some(info) = party_info {
                g.snap.party = info;
            }
            g.path.clear();
            self.motion.route_points(&mut g.path, PATH_POINTS_MAX);
            let s = &mut g.snap;
            s.level = level;
            s.hp_pct = hp;
            s.x = pos[0];
            s.y = pos[1];
            s.z = pos[2];
            s.activity = activity;
            s.gold = money;
            s.has_house = has_house;
            s.online_secs = online_secs;
            s.paused = paused;
            if s.goal != goal {
                s.goal.clear();
                s.goal.push_str(goal);
            }
            if s.detail != self.detail {
                s.detail.clear();
                s.detail.push_str(&self.detail);
            }
            if zone_changed {
                s.zone = zone_key.and_then(|k| world.zone_name(k)).map(str::to_owned);
                s.zone_key = zone_key.unwrap_or(0);
                s.continent = crate::population::region::continent_of_zone(&**world, s.zone_key);
            }
            if mother != 0 && s.mother_faction != mother {
                s.mother_faction = mother;
                s.alliance = crate::population::region::alliance_of_mother(mother);
            }
            if with_extra {
                g.extra.clear();
                g.extra.push(("account".into(), self.entry.account.clone()));
                g.extra.push((
                    "character".into(),
                    format!(
                        "{} (id {})",
                        self.entry.char_name,
                        self.entry.char_id.unwrap_or(0)
                    ),
                ));
                g.extra
                    .push(("persona".into(), self.persona.temper.dominant().into()));
                g.extra
                    .push(("kills".into(), self.combat.kills().to_string()));
                g.extra.push(("sessions".into(), self.sessions.to_string()));
                g.extra
                    .push(("quests".into(), goals::quest::summary(world, &self.state)));
                g.plots.clear();
                farm::plot_lines(&self.state, world, self.state.now_ms, &mut g.plots);
            }
        });
        self.slot.obj_id.store(self.state.me.obj, Ordering::Relaxed);
    }

    /// Fills `self.detail` with the one-line description of the current activity.
    fn describe(&mut self) {
        let me = &self.state.me;
        if me.dead {
            let left = me.respawn_at_ms.saturating_sub(self.now_ms()) / 1000;
            let _ = write!(self.detail, "dead, can respawn in {left}s");
        } else if self.paused {
            self.detail.push_str("paused");
        } else if let Some(obj) = self.combat.target().filter(|o| {
            self.state
                .units
                .get(o)
                .is_none_or(|u| u.kind != UnitKind::Character)
        }) {
            // (a character target is a duel opponent: the goal describes that)
            let name = self
                .state
                .units
                .get(&obj)
                .and_then(|u| self.shared.world.npc(u.id))
                .map_or("a monster", |t| &*t.name);
            let _ = write!(self.detail, "fighting {name}");
        } else if let Some(g) = self.brain.goal() {
            g.describe(&mut self.detail);
        }
    }
}

/// Builds the offline snapshot a bot has before its first login.
pub fn initial_snapshot(entry: &RosterEntry, archetype: &Archetype) -> pb_web::BotSnapshot {
    pb_web::BotSnapshot {
        id: entry.id,
        name: entry.char_name.clone(),
        race: race_name(entry.race).to_string(),
        class: archetype.name.clone(),
        level: 1,
        hp_pct: 100,
        alliance: crate::population::region::alliance_of_race(entry.race),
        activity: Activity::Offline,
        ..pb_web::BotSnapshot::default()
    }
}

fn failed(e: SessionError, online: Duration, phase: Phase) -> SessionEnd {
    SessionEnd::Ended {
        permanent: e.is_permanent(),
        fault: e.fault(phase),
        why: e.to_string(),
        online,
    }
}

/// When the bot loop wakes next (bot clock, ms) after a tick at `now`: at the next brain tick
/// (`next_brain_ms`), sooner every `move_every_ms` while the bot walks in view of a real player
/// (the movement update between brain ticks), and for the next packet of a jump (`jump_in`, at
/// least [`MIN_WAKE_MS`] away). Always after `now`.
pub fn next_wake_ms(
    now: u64,
    next_brain_ms: u64,
    move_every_ms: Option<u64>,
    jump_in: Option<u64>,
) -> u64 {
    let mut at = next_brain_ms;
    if let Some(every) = move_every_ms {
        at = at.min(now + every);
    }
    if let Some(wait) = jump_in {
        at = at.min(now + wait.max(MIN_WAKE_MS));
    }
    at.max(now + 1)
}

/// `d` scaled by a random factor in `0.5..1.5`, so a restart (or a server coming back) does not
/// reconnect everyone in the same second.
fn jittered(d: Duration, rng: &mut fastrand::Rng) -> Duration {
    d.mul_f32(0.5 + rng.f32())
}

/// Whether a doodad is worth remembering: it can be used, looted, crafted at, or a quest wants it
/// (or it is a trade pack on the ground, which the gank goal picks up).
fn keep_doodad(world: &World, d: &DoodadInfo) -> bool {
    d.has_loot
        || d.quest_glow != 0
        || world.is_pack_doodad(d.template)
        || world.is_dungeon_door(d.template)
        || world.is_mirage_door(d.template)
        || world
            .doodad(d.template)
            .is_some_and(|t| t.flags != DoodadFlags::NONE)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sizes of the per-bot structures, printed with `--nocapture` for the memory estimate in
    /// `docs/OPERATING.md`; the asserts keep them from growing unnoticed.
    #[test]
    fn per_bot_structures_stay_small() {
        use std::mem::size_of;
        let sizes = [
            ("Bot", size_of::<Bot>()),
            ("BotState", size_of::<BotState>()),
            ("Motion", size_of::<Motion>()),
            ("Combat", size_of::<Combat>()),
            ("Brain", size_of::<Brain>()),
            ("Goal", size_of::<goals::Goal>()),
            ("Place", size_of::<place::Place>()),
            ("BotCmd", size_of::<BotCmd>()),
            ("Persona", size_of::<Persona>()),
            ("Unit", size_of::<state::Unit>()),
            ("Doodad", size_of::<state::Doodad>()),
            ("Item", size_of::<state::Item>()),
        ];
        for (name, n) in sizes {
            println!("size_of {name}: {n} bytes");
        }
        // 3 KB plus the fight watch, the death/duel/gate notes and the unreachable-monster
        // pointer the behaviour fixes added (about 100 bytes).
        assert!(size_of::<Bot>() < 3200, "Bot is {} bytes", size_of::<Bot>());
        assert!(size_of::<goals::Goal>() <= 160);
        // the command channel slot: boxed orders keep it at the size it had before the travel
        // commands (a `PartyOrder` unboxed would add 8 bytes)
        assert!(
            size_of::<BotCmd>() <= 48,
            "BotCmd is {}",
            size_of::<BotCmd>()
        );
        assert!(size_of::<state::Unit>() <= 64);
    }

    #[test]
    fn a_walker_in_view_wakes_evenly_between_brain_ticks() {
        // brain ticks every 250 ms, movement every 125 ms: the wakes are 125 ms apart
        let (mut now, mut next_brain) = (1_000u64, 1_000u64);
        let mut wakes = Vec::new();
        for _ in 0..8 {
            if now >= next_brain {
                next_brain = now + 250;
            }
            let at = next_wake_ms(now, next_brain, Some(125), None);
            wakes.push(at - now);
            now = at;
        }
        assert!(wakes.iter().all(|&w| w == 125), "{wakes:?}");
        // nobody sees it: the brain tick only
        assert_eq!(next_wake_ms(1_000, 1_250, None, None), 1_250);
        // a jump wakes it for its next packet, but never sooner than the shortest wake
        assert_eq!(next_wake_ms(1_000, 1_250, Some(125), Some(40)), 1_040);
        assert_eq!(
            next_wake_ms(1_000, 1_250, None, Some(0)),
            1_000 + MIN_WAKE_MS
        );
        // an overdue brain tick is the next moment, never now
        assert_eq!(next_wake_ms(1_000, 900, None, None), 1_001);
    }

    #[test]
    fn jitter_spreads_the_wait_by_half_either_way() {
        let mut rng = fastrand::Rng::with_seed(3);
        let (mut lo, mut hi) = (Duration::MAX, Duration::ZERO);
        for _ in 0..2000 {
            let d = jittered(Duration::from_secs(8), &mut rng);
            assert!(
                d >= Duration::from_secs(4) && d < Duration::from_secs(12),
                "{d:?}"
            );
            lo = lo.min(d);
            hi = hi.max(d);
        }
        // the spread really is wide in both directions
        assert!(lo < Duration::from_secs(5), "{lo:?}");
        assert!(hi > Duration::from_secs(11), "{hi:?}");
    }

    #[test]
    fn the_first_snapshot_is_offline_with_class_and_race() {
        let archetypes = Archetypes::builtin().unwrap();
        let arch = archetypes.get("Darkrunner").unwrap();
        let entry = RosterEntry {
            id: 4,
            account: "pb4".into(),
            char_name: "Tester".into(),
            race: 4,
            gender: 2,
            archetype: "Darkrunner".into(),
            temperament: crate::roster::Temperament::NEUTRAL,
            char_id: None,
            profile: None,
        };
        let s = initial_snapshot(&entry, arch);
        assert_eq!(
            (s.id, s.name.as_str(), s.race.as_str()),
            (4, "Tester", "Elf")
        );
        assert_eq!(s.class, "Darkrunner");
        assert_eq!(s.activity, Activity::Offline);
    }

    #[test]
    fn failures_report_the_reason_and_permanence() {
        match failed(
            SessionError::Login(aa_proto::login::LoginError::Denied(3)),
            Duration::ZERO,
            Phase::Login,
        ) {
            SessionEnd::Ended {
                why,
                permanent,
                fault,
                ..
            } => {
                assert!(permanent);
                assert!(why.contains("denied"), "{why}");
                assert_eq!(fault.unwrap().cause, Cause::Denied(3));
            }
            _ => panic!("expected Ended"),
        }
        assert!(matches!(
            failed(SessionError::Timeout("x"), Duration::ZERO, Phase::Lobby),
            SessionEnd::Ended {
                permanent: false,
                ..
            }
        ));
    }
}
