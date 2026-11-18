//! What a watching client receives from a walking bot: one bot with the navigation mesh, the
//! planner pool and the life-like movement, ticked the way the bot task ticks it, on the real
//! `main_world` data. Every `CSMoveUnit` it sends is decoded and measured the way a client sees
//! the stream: how often updates come, how far the bot is from where its last update said it would
//! be (the snap a client has to make), and how often it halts in the middle of a trip.
//!
//! ```text
//! AA_COMPACT=<.../app/Data/compact.sqlite3> AA_SERVER_DATA=<.../app/Data> AA_BAKED=<data dir> \
//!   cargo test -p playerbots --test movement_real_data -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use aa_proto::cs::Out;
use aa_proto::opcodes::cs as op;
use aa_world::{World, WorldPaths};
use playerbots::bot::cmd::BotCmd;
use playerbots::bot::manner::{Gate, Life};
use playerbots::bot::motion::{Motion, MotionCfg, MotionEnv, MotionStatus, Pace};
use playerbots::config::Config;
use playerbots::hub::{Hub, HubParams};

fn world() -> Option<Arc<World>> {
    static WORLD: OnceLock<Option<Arc<World>>> = OnceLock::new();
    WORLD
        .get_or_init(|| {
            let compact = PathBuf::from(std::env::var_os("AA_COMPACT")?);
            let server_data = PathBuf::from(std::env::var_os("AA_SERVER_DATA")?);
            let baked = PathBuf::from(std::env::var_os("AA_BAKED")?);
            let paths = WorldPaths {
                compact,
                server_data,
                baked: Some(baked),
                world: "main_world".into(),
            };
            let w = World::load(&paths).expect("load main_world");
            w.nav().is_some().then(|| Arc::new(w))
        })
        .clone()
}

const ME: u32 = 0x400;
/// The client's velocity scale (`aa_proto::cs`): wire shorts per m/s.
const WIRE_PER_MPS: f32 = 32767.0 / 60.0;

/// One decoded `CSMoveUnit` (move type 1).
#[derive(Debug, Clone, Copy)]
struct Mv {
    time: u32,
    flags: u8,
    pos: [f32; 3],
    vel: [f32; 3],
    actor: u8,
}

impl Mv {
    fn moving(&self) -> bool {
        self.flags & 0x02 != 0
    }
    fn speed(&self) -> f32 {
        self.vel[0].hypot(self.vel[1])
    }
    fn on_ground(&self) -> bool {
        self.actor & 0x04 != 0 && self.actor & 0x10 == 0
    }
}

fn decode(o: &Out) -> Option<Mv> {
    if o.opcode != op::CSMoveUnitPacket {
        return None;
    }
    let mut s = aa_proto::PacketStream::from_bytes(o.body.clone());
    let _obj = s.read_bc().ok()?;
    if s.read_u8().ok()? != 1 {
        return None;
    }
    let time = s.read_u32().ok()?;
    let flags = s.read_u8().ok()?;
    let (x, y, z) = s.read_position().ok()?;
    let v = [s.read_i16().ok()?, s.read_i16().ok()?, s.read_i16().ok()?];
    for _ in 0..6 {
        s.read_i8().ok()?;
    }
    let _stance = s.read_u8().ok()?;
    let _alert = s.read_u8().ok()?;
    let actor = s.read_u8().ok()?;
    Some(Mv {
        time,
        flags,
        pos: [x, y, z],
        vel: v.map(|w| f32::from(w) / WIRE_PER_MPS),
        actor,
    })
}

/// One bot, its controller and the clock of its task.
struct Sim {
    world: Arc<World>,
    _hub: Hub,
    rx: tokio::sync::mpsc::Receiver<BotCmd>,
    motion: Motion,
    rng: fastrand::Rng,
    now_ms: u64,
    out: Vec<Out>,
    sent: Vec<Mv>,
    audience: bool,
    town: bool,
    tick_ms: u64,
    idle_tick_ms: u64,
    move_update_ms: u64,
    next_brain_ms: u64,
}

impl Sim {
    fn new(world: Arc<World>, seed: u64, audience: bool) -> Sim {
        let cfg = Config::default();
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let hub = Hub::new(HubParams {
            world: &world,
            baked_dir: None,
            supervisor: tx,
            llm: None,
        });
        let slot = hub.register(pb_web::BotSnapshot {
            id: 1,
            name: "Walker".into(),
            ..pb_web::BotSnapshot::default()
        });
        let rx = slot.attach();
        let mut motion = Motion::new(MotionCfg {
            run_speed: cfg.bots.move_speed,
            walk_speed: cfg.bots.walk_speed,
        });
        motion.attach_nav(playerbots::bot::nav::link_for_bot(
            &hub,
            &world,
            &cfg,
            slot.clone(),
            seed as u32,
        ));
        let mut rng = fastrand::Rng::with_seed(seed);
        motion.attach_life(Life::new(
            seed as u32,
            cfg.behavior.jump_rate,
            false,
            0,
            &mut rng,
        ));
        let start = world.race_start(1).expect("Nuian start");
        let z = world.ground_z(start.x, start.y).unwrap_or(start.z);
        motion.enter_world(ME, [start.x, start.y, z], 0.0, 1);
        Sim {
            world,
            _hub: hub,
            rx,
            motion,
            rng,
            now_ms: 1,
            out: Vec::new(),
            sent: Vec::new(),
            audience,
            town: false,
            tick_ms: cfg.bots.tick_ms,
            idle_tick_ms: cfg.bots.idle_tick_ms,
            move_update_ms: cfg.bots.move_update_ms,
            next_brain_ms: 0,
        }
    }

    /// The time from one brain tick to the next (`Bot::brain_period_ms`).
    fn brain_period(&self) -> u64 {
        if self.motion.busy() {
            self.tick_ms
        } else {
            self.idle_tick_ms
        }
    }

    /// Path answers the pool has given meanwhile, as the bot task's command channel delivers
    /// them between ticks. A search takes milliseconds, a tick period hundreds: an awaited answer
    /// is waited for (up to the tick period of wall time), as it would have come by then.
    fn deliver(&mut self) {
        let deadline = Instant::now() + Duration::from_millis(self.tick_ms);
        loop {
            match self.rx.try_recv() {
                Ok(BotCmd::PathReady { ticket, result }) => {
                    self.motion.on_path_ready(ticket, *result)
                }
                Ok(_) => {}
                Err(_) => {
                    if !self.motion.awaits_path() || Instant::now() >= deadline {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }

    /// One wake of the bot task, then the wait until the next one: a brain tick (the goal,
    /// `before`, then the movement) when one is due, else a movement tick only, scheduled as the
    /// bot loop schedules them (`playerbots::bot::next_wake_ms`).
    fn tick(&mut self, before: impl FnOnce(&mut Motion, u64)) {
        self.deliver();
        if let Some(l) = self.motion.life_mut() {
            l.gate = Gate {
                audience: self.audience,
                busy: false,
                town: self.town,
            };
        }
        let brain = self.now_ms >= self.next_brain_ms;
        if brain {
            before(&mut self.motion, self.now_ms);
        }
        let mut env = MotionEnv {
            ground: &*self.world,
            now_ms: self.now_ms,
            out: &mut self.out,
            rng: &mut self.rng,
            send_gap_ms: if self.audience {
                self.move_update_ms
            } else {
                1000
            },
            combat: false,
        };
        self.motion.tick(&mut env);
        self.sent
            .extend(self.out.drain(..).filter_map(|o| decode(&o)));
        if brain {
            self.next_brain_ms = self.now_ms + self.brain_period();
        }
        let move_due = if self.audience {
            self.motion.move_due_ms(self.move_update_ms)
        } else {
            None
        };
        self.now_ms = playerbots::bot::next_wake_ms(
            self.now_ms,
            self.next_brain_ms,
            move_due,
            self.motion.wake_in(self.now_ms),
        );
    }

    fn walkable_point(&mut self, min_r: f32, max_r: f32) -> (f32, f32) {
        let p = self.motion.pos();
        playerbots::bot::goals::random_point(&self.world, &mut self.rng, p, min_r, max_r, 200)
            .expect("a walkable point")
    }
}

/// What the stream looks like to a client.
#[derive(Debug, Default)]
struct Report {
    updates: usize,
    moving_secs: f32,
    gaps_ms: Vec<u64>,
    /// Metres between where an update puts the bot and where the one before said it would be.
    snaps_m: Vec<f32>,
    /// Stopped updates in the middle of a trip followed by a moving one within 1.5 s.
    halts: usize,
    /// Updates whose announced speed is more than 25 % off the speed actually covered.
    speed_off: usize,
    /// Moving updates whose speed is half or less (or twice or more) the one before: a switch
    /// between walking and running.
    pace_flips: usize,
    pairs: usize,
}

fn pct(v: &mut [f32], p: f32) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f32::total_cmp);
    v[((v.len() - 1) as f32 * p).round() as usize]
}

fn pct_u(v: &mut [u64], p: f32) -> u64 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    v[((v.len() - 1) as f32 * p).round() as usize]
}

fn measure(sent: &[Mv]) -> Report {
    let mut r = Report {
        updates: sent.len(),
        ..Report::default()
    };
    for w in sent.windows(2) {
        let (a, b) = (w[0], w[1]);
        let dt_ms = b.time.wrapping_sub(a.time);
        if !a.moving() || dt_ms == 0 || dt_ms > 5_000 {
            continue;
        }
        let dt = dt_ms as f32 / 1000.0;
        r.moving_secs += dt;
        if a.on_ground() && b.on_ground() {
            r.gaps_ms.push(u64::from(dt_ms));
            // dead reckoning from `a`, as a client extrapolates
            let px = a.pos[0] + a.vel[0] * dt;
            let py = a.pos[1] + a.vel[1] * dt;
            r.snaps_m.push((b.pos[0] - px).hypot(b.pos[1] - py));
            let covered = (b.pos[0] - a.pos[0]).hypot(b.pos[1] - a.pos[1]) / dt;
            r.pairs += 1;
            if b.moving() && (covered - a.speed()).abs() > 0.25 * a.speed().max(1.0) {
                r.speed_off += 1;
            }
            if b.moving() {
                let (sa, sb) = (a.speed().max(0.1), b.speed().max(0.1));
                if sa / sb >= 2.0 || sb / sa >= 2.0 {
                    r.pace_flips += 1;
                }
            }
        }
    }
    for (i, m) in sent.iter().enumerate() {
        if m.moving() || i == 0 || !sent[i - 1].moving() {
            continue;
        }
        let resumes = sent[i + 1..]
            .iter()
            .take_while(|n| n.time.wrapping_sub(m.time) <= 1_500)
            .any(Mv::moving);
        if resumes {
            r.halts += 1;
        }
    }
    r
}

/// What a client in view must get: the client's own pace, evenly, and updates that agree with
/// each other (the dead reckoning of one is where the next puts the bot).
fn assert_smooth(name: &str, r: &mut Report, max_snap_m: f32) {
    let per_s = r.gaps_ms.len() as f32 / r.moving_secs.max(1e-3);
    assert!(per_s >= 6.0, "{name}: {per_s:.1} updates a second");
    assert!(
        pct_u(&mut r.gaps_ms, 0.9) <= 150,
        "{name}: uneven or slow updates"
    );
    assert!(
        pct(&mut r.snaps_m, 0.99) <= max_snap_m,
        "{name}: the updates disagree with each other"
    );
}

fn print(name: &str, r: &mut Report) {
    let per_s = r.gaps_ms.len() as f32 / r.moving_secs.max(1e-3);
    println!(
        "{name}: {} updates, {:.0} s moving, {per_s:.1} moving updates/s; gap ms p10 {} p50 {} \
         p90 {} max {}; snap m p50 {:.2} p90 {:.2} p99 {:.2} max {:.2}; halts {} ({:.1}/min); \
         speed off {}/{}; pace flips {}",
        r.updates,
        r.moving_secs,
        pct_u(&mut r.gaps_ms, 0.1),
        pct_u(&mut r.gaps_ms, 0.5),
        pct_u(&mut r.gaps_ms, 0.9),
        pct_u(&mut r.gaps_ms, 1.0),
        pct(&mut r.snaps_m, 0.5),
        pct(&mut r.snaps_m, 0.9),
        pct(&mut r.snaps_m, 0.99),
        pct(&mut r.snaps_m, 1.0),
        r.halts,
        r.halts as f32 / (r.moving_secs / 60.0).max(1e-3),
        r.speed_off,
        r.pairs,
        r.pace_flips,
    );
}

/// Walks `trips` trips of `min_r..max_r` metres from wherever the bot is, each started once as
/// the wander and travel goals do, with a few seconds' rest between them.
fn trips(sim: &mut Sim, trips: usize, min_r: f32, max_r: f32) {
    for _ in 0..trips {
        let (x, y) = sim.walkable_point(min_r, max_r);
        sim.tick(|m, now| m.go_to(x, y, 1.0, Pace::Run, now));
        for _ in 0..4_000 {
            sim.tick(|_, _| {});
            if !sim.motion.busy() {
                break;
            }
        }
        let rest_until = sim.now_ms + 2_000 + sim.rng.u64(0..3_000);
        while sim.now_ms < rest_until {
            sim.tick(|_, _| {});
        }
    }
}

/// Chases a target that wanders at 4 m/s with the combat controller's rules for a melee fighter
/// (`combat::{must_approach, arrival_radius, chase_again}`): a chase while the target is out of
/// reach, aimed again when the target left the point it runs to, a stop within reach.
fn chase(sim: &mut Sim, secs: u64) {
    let p = sim.motion.pos();
    let mut target = [p[0] + 20.0, p[1]];
    let mut heading: f32 = sim.rng.f32() * std::f32::consts::TAU;
    use playerbots::bot::combat::{arrival_radius, chase_again, must_approach};
    const PREFERRED: f32 = 2.5;
    let mut last_goto: Option<(f32, f32)> = None;
    let end = sim.now_ms + secs * 1000;
    let mut last_ms = sim.now_ms;
    while sim.now_ms < end {
        let dt = (sim.now_ms - last_ms) as f32 / 1000.0;
        last_ms = sim.now_ms;
        heading += (sim.rng.f32() - 0.5) * 0.6;
        let (nx, ny) = (
            target[0] + heading.cos() * 4.0 * dt,
            target[1] + heading.sin() * 4.0 * dt,
        );
        if playerbots::bot::goals::walkable(&sim.world, nx, ny) {
            target = [nx, ny];
        } else {
            heading += std::f32::consts::PI;
        }
        let t = target;
        sim.tick(|m, now| {
            let p = m.pos();
            let dist = (t[0] - p[0]).hypot(t[1] - p[1]);
            if must_approach(dist, PREFERRED) {
                if chase_again(last_goto, [t[0], t[1], 0.0], m.is_moving()) {
                    m.chase_to(t[0], t[1], arrival_radius(PREFERRED), Pace::Run, now);
                    last_goto = Some((t[0], t[1]));
                }
            } else {
                if m.is_moving() {
                    m.stop();
                    last_goto = None;
                }
                m.face(t[0], t[1]);
            }
        });
    }
}

/// Follows a leader who runs from point to point and stands now and then, the way a companion
/// does (`companion::follow::decide`, a chase to the leader, a stop when close).
fn follow(sim: &mut Sim, secs: u64) {
    use playerbots::bot::companion::follow::{decide, Action, Inputs};
    let p = sim.motion.pos();
    let mut leader = [p[0] + 3.0, p[1], p[2]];
    let mut goal: Option<(f32, f32)> = None;
    let mut stand_until = 0u64;
    let end = sim.now_ms + secs * 1000;
    let mut last_ms = sim.now_ms;
    while sim.now_ms < end {
        let dt = (sim.now_ms - last_ms) as f32 / 1000.0;
        last_ms = sim.now_ms;
        if sim.now_ms >= stand_until {
            let (gx, gy) = match goal {
                Some(g) => g,
                None => {
                    let g = playerbots::bot::goals::random_point(
                        &sim.world,
                        &mut sim.rng,
                        leader,
                        30.0,
                        80.0,
                        200,
                    )
                    .expect("a walkable point");
                    goal = Some(g);
                    g
                }
            };
            let (dx, dy) = (gx - leader[0], gy - leader[1]);
            let d = dx.hypot(dy);
            let step = (5.4 * dt).min(d);
            if d > 0.1 {
                leader[0] += dx / d * step;
                leader[1] += dy / d * step;
            } else {
                goal = None;
                stand_until = sim.now_ms + 3_000 + sim.rng.u64(0..5_000);
            }
        }
        let l = leader;
        sim.tick(|m, now| {
            let a = decide(&Inputs {
                me: m.pos(),
                leader: l,
                moving: m.is_moving(),
                dest: m.destination(),
                hp_pct: 100,
                resting: false,
                threatened: false,
                slot: 0,
            });
            match a {
                Action::Walk {
                    x, y, radius, pace, ..
                } => m.chase_to(x, y, radius, pace, now),
                Action::Hold | Action::Rest => {
                    if m.is_moving() {
                        m.stop();
                    }
                }
                Action::Keep => {}
            }
        });
    }
}

#[test]
#[ignore = "needs AA_COMPACT, AA_SERVER_DATA and AA_BAKED with nav_main_world.bin"]
fn what_a_watching_client_receives_from_a_walking_bot() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA / AA_BAKED (with a mesh) not set; skipping");
        return;
    };
    for (name, audience) in [("watched", true), ("not watched", false)] {
        let mut sent = Vec::new();
        for seed in 1..=4u64 {
            let mut sim = Sim::new(world.clone(), seed, audience);
            trips(&mut sim, 12, 20.0, 260.0);
            sent.push(std::mem::take(&mut sim.sent));
        }
        let mut r = Report::default();
        for s in &sent {
            let mut one = measure(s);
            r.updates += one.updates;
            r.moving_secs += one.moving_secs;
            r.gaps_ms.append(&mut one.gaps_ms);
            r.snaps_m.append(&mut one.snaps_m);
            r.halts += one.halts;
            r.speed_off += one.speed_off;
            r.pace_flips += one.pace_flips;
            r.pairs += one.pairs;
        }
        print(&format!("trips, {name}"), &mut r);
        if audience {
            assert_smooth("trips", &mut r, 0.3);
            assert!(r.halts * 10 <= r.updates / 100, "trips: {} halts", r.halts);
        }
    }
    let mut sim = Sim::new(world.clone(), 5, true);
    sim.town = true;
    trips(&mut sim, 30, 15.0, 120.0);
    let mut r = measure(&sim.sent);
    print("short trips in a town, watched", &mut r);
    assert_smooth("town", &mut r, 0.3);
    assert_eq!(
        r.pace_flips, 0,
        "a trip switched between walking and running"
    );
    let mut sim = Sim::new(world.clone(), 7, true);
    follow(&mut sim, 300);
    let mut r = measure(&sim.sent);
    print("following a leader, watched", &mut r);
    assert_smooth("following", &mut r, 0.8);
    let mut sim = Sim::new(world.clone(), 9, true);
    trips(&mut sim, 3, 600.0, 900.0);
    let mut r = measure(&sim.sent);
    print("long trips, watched", &mut r);
    assert_smooth("long trips", &mut r, 0.3);
    let mut sim = Sim::new(world.clone(), 11, true);
    chase(&mut sim, 180);
    let mut r = measure(&sim.sent);
    print("chase, watched", &mut r);
    // (the target turns, and each new aim turns the bot)
    assert_smooth("chase", &mut r, 1.0);
    // (a melee fighter stops when it caught up with a slower target: about one stop in four
    // seconds, never one at every update)
    assert!(r.halts <= 70, "chase: {} halts in three minutes", r.halts);
    assert!(!matches!(sim.motion.status(), MotionStatus::Failed(_)));
}
