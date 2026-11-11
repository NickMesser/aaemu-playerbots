//! Tests of the life-like parts: the traits, the jump physics and schedule, when a bot jumps, the
//! mannerisms, the emote rules and limits, and the movement controller with all of it attached.
//! No world, no socket: the clock and the random source are passed in.

use super::*;
use crate::bot::motion::{Ground, MotionCfg, MotionEnv, MotionStatus, Pace, SWIM_Z};
use crate::bot::state::Unit;
use aa_proto::game::LEVEL_GAME;
use aa_proto::opcodes::cs as op;

const T0: u64 = 10_000;
const SHOW: Gate = Gate {
    audience: true,
    busy: false,
    town: false,
};

fn flat(_: f32, _: f32) -> Option<f32> {
    Some(130.0)
}

fn sea(_: f32, _: f32) -> Option<f32> {
    Some(60.0)
}

/// A seed whose bot jumps.
fn playful() -> u32 {
    (1u32..)
        .find(|&s| Traits::new(s, 1.0).jump_mult > 0.8)
        .unwrap()
}

/// A seed whose bot never jumps.
fn serious() -> u32 {
    (1u32..)
        .find(|&s| Traits::new(s, 1.0).jump_mult == 0.0)
        .unwrap()
}

// ---- reading what the bot sent ----------------------------------------------------------------

/// One `CSMoveUnit` taken apart.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Mv {
    time: u32,
    flags: u8,
    pos: [f32; 3],
    vel: [i16; 3],
    dir: i8,
    stance: u8,
    actor: u8,
}

fn mv(p: &Out) -> Mv {
    assert_eq!(p.opcode, op::CSMoveUnitPacket);
    let mut s = aa_proto::PacketStream::from_bytes(p.body.clone());
    s.read_bc().unwrap();
    assert_eq!(s.read_u8().unwrap(), 1);
    let time = s.read_u32().unwrap();
    let flags = s.read_u8().unwrap();
    let (x, y, z) = s.read_position().unwrap();
    let vel = [
        s.read_i16().unwrap(),
        s.read_i16().unwrap(),
        s.read_i16().unwrap(),
    ];
    s.read_i8().unwrap();
    s.read_i8().unwrap();
    let dir = s.read_i8().unwrap();
    for _ in 0..3 {
        s.read_i8().unwrap();
    }
    let stance = s.read_u8().unwrap();
    s.read_u8().unwrap();
    let actor = s.read_u8().unwrap();
    Mv {
        time,
        flags,
        pos: [x, y, z],
        vel,
        dir,
        stance,
        actor,
    }
}

fn moves(out: &[Out]) -> Vec<Mv> {
    out.iter()
        .filter(|p| p.level == LEVEL_GAME && p.opcode == op::CSMoveUnitPacket)
        .map(mv)
        .collect()
}

/// `(target object, emotion)` of every emote packet.
fn emotes(out: &[Out]) -> Vec<(u32, u32)> {
    out.iter()
        .filter(|p| p.opcode == op::CSExpressEmotionPacket)
        .map(|p| {
            let mut s = aa_proto::PacketStream::from_bytes(p.body.clone());
            assert_eq!(s.read_bc().unwrap(), 0x400);
            let target = s.read_bc().unwrap();
            (target, s.read_u32().unwrap())
        })
        .collect()
}

fn took_off(m: &Mv) -> bool {
    m.actor & 0x10 != 0 && m.actor & 0x04 == 0
}

// ---- a controller to drive ---------------------------------------------------------------------

struct Rig {
    motion: Motion,
    out: Vec<Out>,
    rng: fastrand::Rng,
    now: u64,
}

impl Rig {
    fn new(seed: u32, jump_rate: f32) -> Rig {
        let mut rig = Rig::plain();
        rig.rng = fastrand::Rng::with_seed(u64::from(seed) + 7);
        let life = Life::new(seed, jump_rate, true, T0, &mut rig.rng);
        rig.motion.attach_life(life);
        rig.life().gate = SHOW;
        rig
    }

    /// A controller without the life-like state (`lifelike = false`).
    fn plain() -> Rig {
        let mut motion = Motion::new(MotionCfg::default());
        motion.enter_world(0x400, [10.0, 10.0, 130.0], 0.0, T0);
        Rig {
            motion,
            out: Vec::new(),
            rng: fastrand::Rng::with_seed(1),
            now: T0,
        }
    }

    fn life(&mut self) -> &mut Life {
        self.motion.life_mut().unwrap()
    }

    fn tick(&mut self, ground: &dyn Ground, combat: bool, dt: u64) -> MotionStatus {
        self.now += dt;
        let mut env = MotionEnv {
            ground,
            now_ms: self.now,
            out: &mut self.out,
            rng: &mut self.rng,
            send_gap_ms: 250,
            combat,
        };
        self.motion.tick(&mut env)
    }

    /// One round of the bot loop: it sleeps its tick, or less when a jump needs it.
    fn step(&mut self, ground: &dyn Ground) -> MotionStatus {
        let dt = self
            .motion
            .wake_in(self.now)
            .map_or(250, |w| w.clamp(15, 250));
        self.tick(ground, false, dt)
    }

    /// Walks north for `secs` seconds of bot time, starting a new trip when one ends.
    fn run(&mut self, ground: &dyn Ground, secs: u64) {
        let end = self.now + secs * 1000;
        while self.now < end {
            if self.motion.status() != MotionStatus::Moving {
                let y = self.motion.pos()[1] + 250.0;
                self.motion.go_to(10.0, y, 0.0, Pace::Run, self.now);
            }
            self.step(ground);
        }
    }
}

// ---- the bot's character -----------------------------------------------------------------------

#[test]
fn traits_come_from_the_seed_and_some_bots_never_jump() {
    assert_eq!(Traits::new(42, 1.0), Traits::new(42, 1.0));
    let all: Vec<Traits> = (1..=2000).map(|s| Traits::new(s, 1.0)).collect();
    let never = all.iter().filter(|t| t.jump_mult == 0.0).count();
    assert!((180..=300).contains(&never), "{never} of 2000 never jump");
    let playful: Vec<f32> = all
        .iter()
        .map(|t| t.jump_mult)
        .filter(|m| *m > 0.0)
        .collect();
    let mean = playful.iter().sum::<f32>() / playful.len() as f32;
    assert!((0.9..=1.1).contains(&mean), "mean jump multiplier {mean}");
    assert!(playful.iter().all(|m| (0.35..=1.65).contains(m)));
    for t in &all {
        assert!((0.97..=1.03).contains(&t.speed_k));
        assert!((0.5..=1.5).contains(&t.fidget));
        assert!((0.0..=0.3).contains(&t.dawdle));
        assert!((0.5..=1.0).contains(&t.chatty));
    }
    // the configured rate scales the playful ones and zero or nonsense turns jumping off
    let seed = playful_seed_of(&all);
    let double = Traits::new(seed, 2.0).jump_mult;
    assert!((double - 2.0 * Traits::new(seed, 1.0).jump_mult).abs() < 1e-5);
    for rate in [0.0, -1.0, f32::NAN] {
        assert_eq!(Traits::new(seed, rate).jump_mult, 0.0, "rate {rate}");
    }
}

fn playful_seed_of(all: &[Traits]) -> u32 {
    all.iter().position(|t| t.jump_mult > 0.0).unwrap() as u32 + 1
}

#[test]
fn the_gate_shows_off_only_with_an_audience_and_no_business() {
    assert!(SHOW.show());
    assert!(!Gate::default().show());
    assert!(!Gate { busy: true, ..SHOW }.show());
    assert!(!Gate {
        audience: false,
        ..SHOW
    }
    .show());
}

// ---- the physics and the packet schedule of a jump ----------------------------------------------

#[test]
fn the_arc_is_the_clients_jump() {
    let a = Arc::for_height(FULL_HEIGHT);
    assert!((a.v0 - 8.062).abs() < 0.01, "v0 {}", a.v0);
    assert!((a.air_s() - 0.645).abs() < 0.005, "air {}", a.air_s());
    assert_eq!(a.lift(0.0), 0.0);
    let apex = a.air_s() / 2.0;
    assert!((a.lift(apex) - FULL_HEIGHT).abs() < 0.01);
    assert!(a.vz(apex).abs() < 0.01);
    assert!(a.lift(a.air_s()).abs() < 0.01);
    assert!(a.lift(a.air_s() * 2.0) == 0.0, "never below the ground");
    assert!(a.vz(0.0) > 0.0 && a.vz(a.air_s()) < 0.0);
    let hop = Arc::for_height(HOP_HEIGHT);
    assert!(hop.air_s() < a.air_s() && (hop.lift(hop.air_s() / 2.0) - HOP_HEIGHT).abs() < 0.01);
}

#[test]
fn a_jump_sends_take_off_apex_descent_touchdown_and_settle_in_time() {
    let mut j = Jump::start(5_000, JumpKind::Full);
    assert_eq!(j.air_ms(), 645);
    assert_eq!(j.due_at(), 5_000, "the take-off goes at once");
    let mut due = vec![];
    let mut steps = vec![];
    let mut now = 5_000;
    while !j.done() {
        if let Some(s) = j.step(now) {
            due.push(now);
            steps.push(s);
        }
        now += 1;
        assert!(now < 7_000);
    }
    // 0, 50 % (apex), 80 %, 100 % of 645 ms, and 90 ms after that
    assert_eq!(due, vec![5_000, 5_323, 5_516, 5_645, 5_735]);
    assert!(matches!(steps[0], AirStep::TakeOff { vz } if (vz - 8.06).abs() < 0.01));
    assert!(
        matches!(steps[1], AirStep::Air { lift, vz } if (lift - 1.3).abs() < 0.02 && vz.abs() < 0.3)
    );
    assert!(matches!(steps[2], AirStep::Air { lift, vz } if lift > 0.5 && vz < -4.0));
    assert_eq!(steps[3], AirStep::Land);
    assert_eq!(steps[4], AirStep::Settle);
    assert_eq!(j.step(9_000), None, "nothing after the settle");
    // the wire phases
    assert_eq!(steps[0].phase(), JumpPhase::TakeOff);
    assert_eq!(steps[1].phase(), JumpPhase::Air);
    assert_eq!(steps[3].phase(), JumpPhase::Land);
    assert_eq!(steps[4].phase(), JumpPhase::None);
    assert_eq!((steps[3].lift(), steps[3].vz()), (0.0, 0.0));
    // nothing is due early
    let mut j = Jump::start(0, JumpKind::Full);
    assert!(j.step(0).is_some());
    assert_eq!(j.step(300), None);
    assert_eq!(j.due_at(), 323);
}

#[test]
fn a_late_tick_skips_to_the_newest_packet_due() {
    let mut j = Jump::start(0, JumpKind::Full);
    assert!(matches!(j.step(0), Some(AirStep::TakeOff { .. })));
    // the bot was asleep for 700 ms: the touchdown wins over the packets on the way
    assert_eq!(j.step(700), Some(AirStep::Land));
    assert_eq!(j.step(701), None);
    assert_eq!(j.step(740), Some(AirStep::Settle));
    assert!(j.done());
    // asleep for two seconds: only the last packet is sent
    let mut j = Jump::start(0, JumpKind::Hop);
    assert!(j.step(0).is_some());
    assert_eq!(j.step(2_000), Some(AirStep::Settle));
    assert!(j.done());
    // a double knows it
    assert!(Jump::start(0, JumpKind::Double).again());
    assert!(!Jump::start(0, JumpKind::Full).again());
}

#[test]
fn the_jump_asks_the_bot_loop_for_its_next_packet() {
    let mut rng = fastrand::Rng::with_seed(1);
    let mut life = Life::new(playful(), 1.0, true, 0, &mut rng);
    assert_eq!(life.wake_at(0), None);
    life.jump = Some(Jump::start(1_000, JumpKind::Full));
    assert_eq!(life.wake_at(1_000), Some(1_000));
    assert!(life.jump_due(1_000));
    life.jump.as_mut().unwrap().step(1_000);
    assert_eq!(life.wake_at(1_000), Some(1_323));
    assert!(!life.jump_due(1_100));
    life.rejump_ms = 1_200;
    assert_eq!(life.wake_at(1_100), Some(1_200), "the earlier of the two");
    // a second jump that is due waits for the ordinary tick instead of spinning the loop
    life.jump = None;
    assert_eq!(life.wake_at(1_300), None);
    life.rejump_ms = 1_400;
    assert_eq!(life.wake_at(1_300), Some(1_400));
    life.end_jump();
    assert_eq!(
        (life.jump, life.rejump_ms, life.wake_at(1_000)),
        (None, 0, None)
    );
}

// ---- when a bot jumps ---------------------------------------------------------------------------

fn run_look() -> Look {
    Look {
        moving: true,
        running: true,
        straight_m: 20.0,
        step_up: false,
    }
}

fn stand_look() -> Look {
    Look {
        moving: false,
        running: false,
        straight_m: 0.0,
        step_up: false,
    }
}

/// Simulates `secs` seconds of 250 ms ticks and returns the jumps `(time ms, kind)`.
fn simulate(look: Look, mult: f32, secs: u64, seed: u64) -> Vec<(u64, JumpKind)> {
    let mut rng = fastrand::Rng::with_seed(seed);
    let mut s = JumpSched::new(0, mult, &mut rng);
    let mut jumps = vec![];
    for t in (0..secs * 1000).step_by(250) {
        if let Some(kind) = s.decide(t, &look, mult, &mut rng) {
            jumps.push((t, kind));
        }
    }
    jumps
}

#[test]
fn a_runner_jumps_about_once_in_thirty_to_sixty_seconds() {
    let jumps = simulate(run_look(), 1.0, 3600, 1);
    assert!(
        (60..=100).contains(&jumps.len()),
        "{} jumps an hour",
        jumps.len()
    );
    assert!(
        jumps.windows(2).all(|w| w[1].0 - w[0].0 >= 10_000),
        "the cooldown"
    );
    let doubles = jumps.iter().filter(|j| j.1 == JumpKind::Double).count();
    assert!(doubles * 100 <= jumps.len() * 25, "{doubles} doubles");
    assert!(
        jumps.iter().all(|j| j.1 != JumpKind::Hop),
        "no step, no hop"
    );
    // the multiplier scales the rate, zero never jumps
    let fast = simulate(run_look(), 2.0, 3600, 2).len();
    assert!(
        (120..=200).contains(&fast),
        "{fast} an hour at twice the rate"
    );
    assert!(simulate(run_look(), 0.0, 3600, 3).is_empty());
    assert!(simulate(run_look(), 0.5, 3600, 4).len() < jumps.len());
}

#[test]
fn a_runner_needs_a_straight_stretch_and_running_speed() {
    let short = Look {
        straight_m: 5.0,
        ..run_look()
    };
    assert!(simulate(short, 1.0, 1800, 5).is_empty(), "under six metres");
    let walk = Look {
        running: false,
        ..run_look()
    };
    assert!(
        simulate(walk, 1.0, 1800, 6).is_empty(),
        "a walk does not jump"
    );
}

#[test]
fn steps_make_hops_not_more_often_than_every_fifteen_seconds() {
    let step = Look {
        step_up: true,
        straight_m: 0.0,
        ..run_look()
    };
    let jumps = simulate(step, 1.0, 600, 7);
    assert!(
        (25..=41).contains(&jumps.len()),
        "{} hops in ten minutes",
        jumps.len()
    );
    assert!(jumps.iter().all(|j| j.1 == JumpKind::Hop));
    assert!(jumps.windows(2).all(|w| w[1].0 - w[0].0 >= 15_000));
    // a serious bot hops over nothing
    assert!(simulate(step, 0.0, 600, 8).is_empty());
}

#[test]
fn what_is_a_step() {
    assert!(is_step(Some(130.0), Some(130.5)));
    assert!(is_step(Some(130.0), Some(131.1)));
    assert!(!is_step(Some(130.0), Some(130.1)), "a bump");
    assert!(!is_step(Some(130.0), Some(132.0)), "a wall");
    assert!(!is_step(Some(130.0), Some(129.0)), "a drop");
    assert!(!is_step(Some(60.0), Some(60.6)), "the sea floor");
    assert!(!is_step(None, Some(130.5)));
    assert!(!is_step(Some(130.0), None));
}

#[test]
fn a_stuck_runner_hops_at_once_and_a_standing_one_forgets_it() {
    let mut rng = fastrand::Rng::with_seed(9);
    let mut s = JumpSched::new(0, 1.0, &mut rng);
    s.stuck();
    assert_eq!(
        s.decide(1_000, &run_look(), 1.0, &mut rng),
        Some(JumpKind::Hop)
    );
    assert_eq!(
        s.decide(1_250, &run_look(), 1.0, &mut rng),
        None,
        "cooldown"
    );
    // the bot stopped before it could: no hop out of the blue later
    let mut s = JumpSched::new(0, 1.0, &mut rng);
    s.stuck();
    assert_eq!(s.decide(1_000, &stand_look(), 1.0, &mut rng), None);
    assert_eq!(s.decide(1_250, &run_look(), 1.0, &mut rng), None);
}

#[test]
fn a_standing_bot_jumps_in_place_every_ninety_to_a_hundred_eighty_seconds() {
    let jumps = simulate(stand_look(), 1.0, 3600, 10);
    assert!(
        (18..=40).contains(&jumps.len()),
        "{} jumps an hour",
        jumps.len()
    );
    assert!(jumps.iter().all(|j| j.1 == JumpKind::Full));
    assert!(jumps.windows(2).all(|w| w[1].0 - w[0].0 >= 90_000));
    assert!(jumps[0].0 >= 90_000);
    assert!(
        simulate(stand_look(), 1.0, 85, 11).is_empty(),
        "not before 90 s"
    );
    assert!(simulate(stand_look(), 0.0, 3600, 12).is_empty());
}

#[test]
fn a_cheer_is_a_jump_at_the_next_chance() {
    let mut rng = fastrand::Rng::with_seed(13);
    let mut s = JumpSched::new(0, 1.0, &mut rng);
    s.cheer();
    assert_eq!(
        s.decide(100, &stand_look(), 1.0, &mut rng),
        Some(JumpKind::Full)
    );
    assert_eq!(s.decide(350, &stand_look(), 1.0, &mut rng), None);
    s.cheer();
    assert_eq!(
        s.decide(400, &run_look(), 1.0, &mut rng),
        None,
        "the cooldown holds"
    );
    assert_eq!(
        s.decide(10_200, &run_look(), 1.0, &mut rng),
        Some(JumpKind::Full)
    );
    // a bot that never jumps does not cheer with one
    let mut s = JumpSched::new(0, 0.0, &mut rng);
    s.cheer();
    assert_eq!(s.decide(100, &stand_look(), 0.0, &mut rng), None);
}

// ---- mannerisms ---------------------------------------------------------------------------------

#[test]
fn a_standing_bot_turns_one_to_three_times_two_to_six_seconds_apart() {
    for seed in 0..200 {
        let mut rng = fastrand::Rng::with_seed(seed);
        let mut f = Fidget::default();
        assert!(f.pending());
        let mut turns = vec![];
        let mut shuffles = 0;
        let mut last = 0;
        for t in (0..60_000u64).step_by(250) {
            match f.step(t, 1.0, &mut rng) {
                Fid::None => {}
                Fid::Turn(by) => {
                    let deg = by.abs().to_degrees();
                    assert!((29.9..=120.1).contains(&deg), "{deg} degrees");
                    let gap = t - last;
                    assert!((1_900..=6_300).contains(&gap), "{gap} ms between turns");
                    last = t;
                    turns.push(deg);
                }
                Fid::Shuffle { len, rel } => {
                    shuffles += 1;
                    assert!((0.5..=1.5).contains(&len) && rel.abs() <= 1.5);
                }
            }
        }
        assert!((1..=3).contains(&turns.len()), "{} turns", turns.len());
        assert!(shuffles <= 1);
        assert!(!f.pending(), "done after the mannerisms");
    }
}

#[test]
fn some_bots_shuffle_a_step_and_the_fidget_starts_over_on_a_command() {
    let mut shuffled = 0;
    for seed in 0..500 {
        let mut rng = fastrand::Rng::with_seed(seed);
        let mut f = Fidget::default();
        for t in (0..60_000u64).step_by(250) {
            if matches!(f.step(t, 1.0, &mut rng), Fid::Shuffle { .. }) {
                shuffled += 1;
            }
        }
    }
    assert!(
        (150..=250).contains(&shuffled),
        "{shuffled} of 500 shuffled"
    );
    let mut rng = fastrand::Rng::with_seed(1);
    let mut done = Fidget::finished();
    assert!(!done.pending());
    for t in (0..60_000u64).step_by(250) {
        assert_eq!(done.step(t, 1.0, &mut rng), Fid::None);
    }
    done.reset();
    assert!(done.pending(), "a new command starts it over");
}

#[test]
fn a_turn_at_a_corner_may_stop_the_bot_for_a_moment() {
    let mut rng = fastrand::Rng::with_seed(3);
    let mut d = Dawdle::default();
    // straight on: never
    for t in (0..20_000u64).step_by(250) {
        assert_eq!(d.on_heading(t, (0.0, 1.0), 1.0, &mut rng), None);
    }
    // a right angle: a pause of 0.3 to 1 s for a bot that always dawdles
    let mut d = Dawdle::default();
    assert_eq!(d.on_heading(0, (1.0, 0.0), 1.0, &mut rng), None);
    assert_eq!(
        d.on_heading(250, (0.0, 1.0), 1.0, &mut rng),
        None,
        "too soon to compare"
    );
    let pause = d
        .on_heading(450, (0.0, 1.0), 1.0, &mut rng)
        .expect("a corner");
    assert!((300..=1000).contains(&pause), "{pause} ms");
    // the next corner within eight seconds is not rolled for
    assert_eq!(d.on_heading(900, (1.0, 0.0), 1.0, &mut rng), None);
    assert_eq!(d.on_heading(1_400, (1.0, 0.0), 1.0, &mut rng), None);
    // a bot that never dawdles never stops
    let mut d = Dawdle::default();
    d.on_heading(0, (1.0, 0.0), 0.0, &mut rng);
    assert_eq!(d.on_heading(500, (0.0, 1.0), 0.0, &mut rng), None);
    // a gentle bend is no corner
    let mut d = Dawdle::default();
    d.on_heading(0, (1.0, 0.0), 1.0, &mut rng);
    assert_eq!(d.on_heading(500, (0.97, 0.243), 1.0, &mut rng), None);
}

// ---- emote limits and rules ---------------------------------------------------------------------

#[test]
fn the_process_wide_allowance_caps_a_crowd_of_bots() {
    let mut b = Budget::default();
    // a burst: three ambient, six social, then nothing
    let ambient = (0..10).filter(|_| b.take(Class::Ambient, 0)).count();
    let social = (0..20).filter(|_| b.take(Class::Social, 0)).count();
    assert_eq!((ambient, social), (3, 6));
    // refilled at 5 and 15 a minute
    assert!(!b.take(Class::Ambient, 11_000));
    assert!(b.take(Class::Ambient, 12_100));
    assert!(!b.take(Class::Ambient, 12_200));
    assert!(b.take(Class::Social, 4_100));
    // ten minutes of constant demand: the two classes together stay under the logging cap
    let mut b = Budget::default();
    let (mut a, mut s) = (0, 0);
    for t in (0..600_000u64).step_by(100) {
        a += usize::from(b.take(Class::Ambient, t));
        s += usize::from(b.take(Class::Social, t));
    }
    assert!((50..=56).contains(&a), "{a} ambient emotes in ten minutes");
    assert!((150..=160).contains(&s), "{s} social emotes in ten minutes");
    // the clock of the process only goes forward
    let mut b = Budget::default();
    assert!(b.take(Class::Ambient, 50_000));
    assert!(b.take(Class::Ambient, 10));
}

#[test]
fn an_emote_is_answered_with_a_fitting_emote() {
    assert_eq!(answer_to(emotion::BOW), emotion::SALUTE);
    assert_eq!(answer_to(emotion::THANKS), emotion::SALUTE);
    assert_eq!(answer_to(emotion::CLAP), emotion::CLAP);
    assert_eq!(answer_to(emotion::LAUGH), emotion::LAUGH);
    assert_eq!(answer_to(emotion::CELEBRATE), emotion::HAPPY);
    assert_eq!(answer_to(emotion::GREET), emotion::GREET);
    assert_eq!(answer_to(9999), emotion::GREET);
}

fn tick_in(now_ms: u64) -> TickIn {
    TickIn {
        now_ms,
        online_ms: 120_000,
        level: 10,
        quests_done: 0,
        near: None,
        moving: false,
        gate: SHOW,
    }
}

fn near(char_id: u32, dist: f32) -> Near {
    Near {
        char_id,
        obj: 0x500 + char_id,
        pos: [10.0 + dist, 10.0],
        dist,
    }
}

fn traits() -> Traits {
    Traits::new(5, 1.0)
}

/// Runs the emote logic for `secs` seconds in 250 ms ticks; returns `(time ms, act)` of every
/// emote.
fn play(
    e: &mut Emotes,
    from_ms: u64,
    secs: u64,
    rng: &mut fastrand::Rng,
    mut input: impl FnMut(&mut TickIn),
    mut budget: impl FnMut(Class) -> bool,
) -> Vec<(u64, u32)> {
    let mut seen = vec![];
    for t in (from_ms..from_ms + secs * 1000).step_by(250) {
        let mut i = tick_in(t);
        input(&mut i);
        for act in e.tick(&i, &traits(), rng, &mut budget) {
            if let Act::Emote { emotion, .. } = act {
                seen.push((t, emotion));
            }
        }
    }
    seen
}

#[test]
fn a_player_in_reach_is_greeted_once_in_ten_minutes_at_most() {
    let mut greeted_bots = 0;
    for seed in 0..40 {
        let mut rng = fastrand::Rng::with_seed(seed);
        let mut e = Emotes::default();
        let seen = play(
            &mut e,
            1_000,
            25 * 60,
            &mut rng,
            |i| i.near = Some(near(77, 4.0)),
            |_| true,
        );
        assert!(seen.iter().all(|s| s.1 == emotion::GREET), "{seen:?}");
        assert!(seen.len() <= 3, "{} greetings in 25 minutes", seen.len());
        assert!(
            seen.windows(2).all(|w| w[1].0 - w[0].0 >= 595_000),
            "{seen:?}"
        );
        greeted_bots += usize::from(!seen.is_empty());
    }
    // (the roll: 60 % greet)
    assert!((15..=38).contains(&greeted_bots), "{greeted_bots} of 40");
}

#[test]
fn nobody_is_greeted_out_of_reach_on_the_move_or_without_a_stage() {
    for seed in 0..60 {
        let go = |rng: &mut fastrand::Rng, f: &mut dyn FnMut(&mut TickIn)| {
            play(&mut Emotes::default(), 1_000, 60, rng, f, |_| true)
        };
        let mut rng = fastrand::Rng::with_seed(seed);
        // seven metres away is outside the greeting reach
        assert!(go(&mut rng, &mut |i| i.near = Some(near(77, 7.0))).is_empty());
        // the bot walks
        assert!(go(&mut rng, &mut |i| {
            i.near = Some(near(77, 3.0));
            i.moving = true;
        })
        .is_empty());
        // busy, or no audience (the stage is somebody else's)
        for gate in [Gate { busy: true, ..SHOW }, Gate::default()] {
            assert!(go(&mut rng, &mut |i| {
                i.near = Some(near(77, 3.0));
                i.gate = gate;
            })
            .is_empty());
        }
        // the allowance is spent
        assert!(play(
            &mut Emotes::default(),
            1_000,
            60,
            &mut rng,
            |i| i.near = Some(near(77, 3.0)),
            |_| false
        )
        .is_empty());
    }
}

#[test]
fn a_greeting_is_dropped_when_the_player_is_gone() {
    let mut dropped = 0;
    for seed in 0..60 {
        let mut rng = fastrand::Rng::with_seed(seed);
        let mut e = Emotes::default();
        // the player is there for the first tick only
        let seen = play(
            &mut e,
            1_000,
            20,
            &mut rng,
            |i| {
                if i.now_ms == 1_000 {
                    i.near = Some(near(77, 3.0));
                }
            },
            |_| true,
        );
        dropped += usize::from(seen.is_empty());
        // (a greeting due within 600 ms may still go if the player is there; here nobody is)
        assert!(seen.is_empty(), "{seen:?}");
    }
    assert_eq!(dropped, 60);
}

#[test]
fn an_emote_played_at_the_bot_is_answered_once_per_player_in_half_a_minute() {
    let (mut directed, mut nearby) = (0, 0);
    for seed in 0..200 {
        let mut rng = fastrand::Rng::with_seed(seed);
        let mut e = Emotes::default();
        let at_me = Expressed {
            obj: 0x500,
            target: 0x400,
            emotion: emotion::BOW,
        };
        // (eight metres: in earshot, but not close enough for a greeting of its own)
        let player = Some(near(0, 8.0));
        e.on_expressed(1_000, &at_me, 77, 5.0, true, &mut rng);
        // a second emote ten seconds later is not answered again
        e.on_expressed(11_000, &at_me, 77, 5.0, true, &mut rng);
        let seen = play(
            &mut e,
            1_000,
            30,
            &mut rng,
            |i| i.near = player.map(|n| Near { char_id: 77, ..n }),
            |_| true,
        );
        assert!(seen.len() <= 1, "{seen:?}");
        if let Some(&(t, id)) = seen.first() {
            assert_eq!(id, emotion::SALUTE);
            // (a first emote that was not answered leaves the second a chance)
            assert!(
                (1_600..=3_600).contains(&t) || (11_600..=13_600).contains(&t),
                "answered at {t}"
            );
            directed += 1;
        }
        // an emote nearby, not at the bot: seldom
        let mut e = Emotes::default();
        let elsewhere = Expressed { target: 0, ..at_me };
        e.on_expressed(1_000, &elsewhere, 77, 5.0, false, &mut rng);
        let seen = play(
            &mut e,
            1_000,
            10,
            &mut rng,
            |i| i.near = player.map(|n| Near { char_id: 77, ..n }),
            |_| true,
        );
        nearby += seen.len();
    }
    // (90 % per emote; a player who waves twice is answered 99 % of the time)
    assert!(
        (185..=200).contains(&directed),
        "{directed} of 200 answered"
    );
    assert!(
        (40..=80).contains(&nearby),
        "{nearby} of 200 answered an emote nearby"
    );
    // out of earshot, nothing
    let mut rng = fastrand::Rng::with_seed(1);
    let mut e = Emotes::default();
    let e1 = Expressed {
        obj: 0x500,
        target: 0x400,
        emotion: emotion::GREET,
    };
    for _ in 0..50 {
        e.on_expressed(1_000, &e1, 77, 12.0, true, &mut rng);
    }
    assert!(play(
        &mut e,
        1_000,
        10,
        &mut rng,
        |i| i.near = Some(near(77, 8.0)),
        |_| true
    )
    .is_empty());
}

#[test]
fn ambient_emotes_come_in_a_town_and_not_more_than_one_in_forty_five_seconds() {
    for seed in 0..20 {
        let mut rng = fastrand::Rng::with_seed(seed);
        let mut e = Emotes::default();
        let seen = play(
            &mut e,
            1_000,
            3600,
            &mut rng,
            |i| i.gate.town = true,
            |_| true,
        );
        assert!((15..=80).contains(&seen.len()), "{} an hour", seen.len());
        assert!(
            seen.windows(2).all(|w| w[1].0 - w[0].0 >= 44_000),
            "{seen:?}"
        );
        let ok = [
            emotion::STRETCH,
            emotion::YAWN,
            emotion::SIGH,
            emotion::LAUGH,
            emotion::CLAP,
        ];
        assert!(seen.iter().all(|s| ok.contains(&s.1)));
    }
    // not outside a town, not on the move, not for nobody, not sitting, not without allowance
    let mut rng = fastrand::Rng::with_seed(1);
    let none = |f: &mut dyn FnMut(&mut TickIn), sitting: bool, take: bool| {
        let mut e = Emotes {
            sitting,
            ..Emotes::default()
        };
        play(
            &mut e,
            1_000,
            1800,
            &mut fastrand::Rng::with_seed(2),
            f,
            |_| take,
        )
        .is_empty()
    };
    assert!(none(&mut |_| {}, false, true), "no town");
    assert!(none(
        &mut |i| {
            i.gate.town = true;
            i.moving = true;
        },
        false,
        true
    ));
    assert!(none(
        &mut |i| {
            i.gate = Gate {
                audience: false,
                town: true,
                busy: false,
            };
        },
        false,
        true
    ));
    assert!(none(&mut |i| i.gate.town = true, true, true), "sitting");
    assert!(
        none(&mut |i| i.gate.town = true, false, false),
        "no allowance"
    );
    let _ = &mut rng;
}

#[test]
fn a_level_or_a_quest_is_cheered_with_a_jump_and_a_celebration() {
    let mut rng = fastrand::Rng::with_seed(1);
    let mut e = Emotes::default();
    let t = |e: &mut Emotes, now: u64, level: u8, quests: u16, online: u64| {
        let i = TickIn {
            level,
            quests_done: quests,
            online_ms: online,
            ..tick_in(now)
        };
        e.tick(&i, &traits(), &mut fastrand::Rng::with_seed(now), |_| true)
    };
    let _ = &mut rng;
    // the first sight of the level or the quest log is the baseline
    assert!(t(&mut e, 1_000, 5, 3, 5_000).is_empty());
    assert!(t(&mut e, 1_250, 5, 3, 60_000).is_empty());
    // a level up: cheer at once, celebrate 400 ms later
    let acts = t(&mut e, 2_000, 6, 3, 61_000);
    assert!(acts.contains(&Act::Cheer));
    let acts = t(&mut e, 2_500, 6, 3, 61_500);
    assert!(
        acts.contains(&Act::Emote {
            emotion: emotion::CELEBRATE,
            target: 0
        }),
        "{acts:?}"
    );
    // a quest: the same, after the first seconds of a login (the quest log arrives then)
    assert!(t(&mut e, 20_000, 6, 4, 70_000).contains(&Act::Cheer));
    let mut late = Emotes::default();
    t(&mut late, 1_000, 6, 0, 5_000);
    assert!(
        t(&mut late, 2_000, 6, 7, 10_000).is_empty(),
        "the quest log of the login"
    );
    // nobody is cheering for itself
    let mut quiet = Emotes::default();
    t(&mut quiet, 1_000, 5, 0, 60_000);
    let i = TickIn {
        level: 6,
        gate: Gate::default(),
        ..tick_in(2_000)
    };
    assert!(quiet.tick(&i, &traits(), &mut rng, |_| true).is_empty());
}

#[test]
fn a_resting_bot_sits_after_a_while_and_stands_up_when_the_rest_ends() {
    for seed in 0..50 {
        let mut rng = fastrand::Rng::with_seed(seed);
        let mut e = Emotes::default();
        e.rest_begin(100_000, &mut rng);
        assert!(!e.sit_due(107_000), "not before eight seconds");
        assert!(e.sit_due(125_100), "by 25 seconds");
        assert_eq!(e.sit(107_000, SHOW, |_| true), None);
        let act = e.sit(125_100, SHOW, |_| true);
        assert_eq!(
            act,
            Some(Act::Emote {
                emotion: emotion::SIT,
                target: 0
            })
        );
        assert!(e.sitting);
        assert_eq!(e.sit(126_000, SHOW, |_| true), None, "it sits once");
        assert!(e.rest_end(), "a bot that sits stands up");
        assert!(!e.sitting);
        assert!(!e.rest_end(), "once");
    }
    // nobody to see it, or no allowance: later
    let mut rng = fastrand::Rng::with_seed(1);
    for (gate, take) in [(Gate::default(), true), (SHOW, false)] {
        let mut e = Emotes::default();
        e.rest_begin(0, &mut rng);
        assert_eq!(e.sit(30_000, gate, |_| take), None);
        assert!(!e.sitting);
        assert!(
            !e.sit_due(44_000) && e.sit_due(45_000),
            "again in fifteen seconds"
        );
    }
    // an emote a moment ago: the sit waits
    let mut e = Emotes::default();
    e.rest_begin(0, &mut rng);
    e.last_ms = 29_000;
    assert_eq!(e.sit(30_000, SHOW, |_| true), None);
    // a bot that rests no more does not sit
    let mut e = Emotes::default();
    e.rest_begin(0, &mut rng);
    e.rest_end();
    assert!(!e.sit_due(60_000));
}

// ---- the world as the bot loop sees it ----------------------------------------------------------

fn world(players: &[(u32, u32, f32)]) -> BotState {
    let mut st = BotState::new();
    st.me.pos = [10.0, 10.0, 130.0];
    st.me.obj = 0x400;
    st.me.level = 10;
    for &(obj, id, dx) in players {
        st.units.insert(
            obj,
            Unit {
                kind: UnitKind::Character,
                id,
                pos: [10.0 + dx, 10.0, 130.0],
                hp: 100,
                faction: 0,
                level: 5,
                flags: 0,
                target: 0,
                name: None,
            },
        );
    }
    st
}

fn moment(audience: bool, busy: bool) -> Moment {
    Moment {
        audience,
        busy,
        online_ms: 120_000,
    }
}

#[test]
fn the_nearest_real_player_is_found_and_our_own_bots_are_not() {
    let mut st = world(&[(0x500, 1, 8.0), (0x501, 2, 3.0), (0x502, 3, 5.0)]);
    st.units.insert(
        0x503,
        Unit {
            kind: UnitKind::Npc,
            id: 9,
            pos: [10.5, 10.0, 130.0],
            hp: 100,
            faction: 0,
            level: 1,
            flags: 0,
            target: 0,
            name: None,
        },
    );
    let n = nearest_player(&st, &|_| false, 10.0).unwrap();
    assert_eq!((n.char_id, n.obj), (2, 0x501));
    assert!((n.dist - 3.0).abs() < 1e-4);
    // id 2 is one of ours: the next one is
    assert_eq!(nearest_player(&st, &|id| id == 2, 10.0).unwrap().char_id, 3);
    assert!(nearest_player(&st, &|_| true, 10.0).is_none());
    assert!(
        nearest_player(&st, &|_| false, 2.0).is_none(),
        "out of range"
    );
    st.units.get_mut(&0x501).unwrap().flags = crate::bot::state::U_DEAD;
    assert_eq!(
        nearest_player(&st, &|_| false, 10.0).unwrap().char_id,
        3,
        "not the dead"
    );
}

#[test]
fn a_town_is_the_peace_aura_or_a_crowd() {
    let mut st = world(&[]);
    assert!(!in_town(&st));
    st.buffs.push((1, PEACE_AURA));
    assert!(in_town(&st));
    st.buffs.clear();
    let crowd: Vec<(u32, u32, f32)> = (0..7).map(|i| (0x600 + i, 100 + i, 5.0)).collect();
    let st7 = world(&crowd);
    assert!(!in_town(&st7), "seven are not a crowd");
    let crowd: Vec<(u32, u32, f32)> = (0..8).map(|i| (0x600 + i, 100 + i, 5.0)).collect();
    assert!(in_town(&world(&crowd)));
    let far: Vec<(u32, u32, f32)> = (0..8).map(|i| (0x600 + i, 100 + i, 40.0)).collect();
    assert!(!in_town(&world(&far)), "a crowd far away");
}

/// How many of `bots` bots (seeds 1..) answer with `what` within `secs` seconds of the loop.
fn bots_that(
    bots: u32,
    secs: u64,
    prepare: impl Fn(&mut Rig, &mut BotState, &mut fastrand::Rng),
    m: Moment,
    is_bot: &dyn Fn(u32) -> bool,
    what: impl Fn(&[Out]) -> bool,
) -> usize {
    let mut n = 0;
    for seed in 1..=bots {
        let mut rig = Rig::new(seed, 1.0);
        let mut st = world(&[(0x500, 77, 4.0)]);
        let mut rng = fastrand::Rng::with_seed(u64::from(seed));
        prepare(&mut rig, &mut st, &mut rng);
        for _ in 0..secs * 4 {
            rig.now += 250;
            st.now_ms = rig.now;
            tick_with(
                &mut rig.motion,
                &st,
                is_bot,
                m,
                &mut rig.out,
                &mut rng,
                &mut |_| true,
            );
        }
        n += usize::from(what(&rig.out));
    }
    n
}

#[test]
fn bots_greet_a_real_player_and_leave_our_own_alone() {
    let greeted = |out: &[Out]| emotes(out).contains(&(0, emotion::GREET));
    let none = |_: &mut Rig, _: &mut BotState, _: &mut fastrand::Rng| {};
    let n = bots_that(200, 20, none, moment(true, false), &|_| false, greeted);
    assert!((100..=140).contains(&n), "{n} of 200 greeted");
    // it is one of our bots: no greeting
    assert_eq!(
        bots_that(100, 20, none, moment(true, false), &|id| id == 77, greeted),
        0
    );
    // no audience, or busy
    assert_eq!(
        bots_that(100, 20, none, moment(false, false), &|_| false, greeted),
        0
    );
    assert_eq!(
        bots_that(100, 20, none, moment(true, true), &|_| false, greeted),
        0
    );
    // on a walk
    let walking = |r: &mut Rig, _: &mut BotState, _: &mut fastrand::Rng| {
        r.motion.go_to(10.0, 500.0, 0.0, Pace::Run, r.now);
    };
    assert_eq!(
        bots_that(100, 20, walking, moment(true, false), &|_| false, greeted),
        0
    );
}

#[test]
fn a_real_players_emote_is_answered_at_them_and_ours_are_not() {
    let prepare = |r: &mut Rig, st: &mut BotState, rng: &mut fastrand::Rng| {
        let e = Expressed {
            obj: 0x500,
            target: 0x400,
            emotion: emotion::GREET,
        };
        on_expressed_with(&mut r.motion, st, &|_| false, &e, rng);
    };
    let at_them = |out: &[Out]| emotes(out).contains(&(0x500, emotion::GREET));
    let n = bots_that(200, 10, prepare, moment(true, false), &|_| false, at_them);
    assert!((160..=195).contains(&n), "{n} of 200 answered");
    // the one who waved is one of ours
    let ours = |r: &mut Rig, st: &mut BotState, rng: &mut fastrand::Rng| {
        let e = Expressed {
            obj: 0x500,
            target: 0x400,
            emotion: emotion::GREET,
        };
        on_expressed_with(&mut r.motion, st, &|id| id == 77, &e, rng);
    };
    let any = |out: &[Out]| !emotes(out).is_empty();
    let n = bots_that(100, 10, ours, moment(true, true), &|id| id == 77, any);
    assert_eq!(n, 0);
    // the bot's own emote, or one of somebody out of view
    let mut rig = Rig::new(1, 1.0);
    let st = world(&[(0x500, 77, 4.0)]);
    let mut rng = fastrand::Rng::with_seed(1);
    for obj in [0x400, 0x999] {
        let e = Expressed {
            obj,
            target: 0x400,
            emotion: emotion::GREET,
        };
        for _ in 0..50 {
            on_expressed_with(&mut rig.motion, &st, &|_| false, &e, &mut rng);
        }
    }
    let far = Expressed {
        obj: 0x500,
        target: 0x400,
        emotion: emotion::GREET,
    };
    let st_far = world(&[(0x500, 77, 14.0)]);
    for _ in 0..50 {
        on_expressed_with(&mut rig.motion, &st_far, &|_| false, &far, &mut rng);
    }
    for _ in 0..40 {
        rig.now += 250;
        let mut st = world(&[(0x500, 77, 4.0)]);
        st.now_ms = rig.now;
        tick_with(
            &mut rig.motion,
            &st,
            &|_| false,
            moment(true, false),
            &mut rig.out,
            &mut rng,
            &mut |c| c != Class::Social,
        );
    }
    assert!(emotes(&rig.out).is_empty());
}

#[test]
fn a_level_up_is_celebrated_with_an_emote_and_a_jump() {
    let mut cheering = 0;
    for seed in 1..=60 {
        let mut rig = Rig::new(seed, 1.0);
        let mut st = world(&[]);
        let mut rng = fastrand::Rng::with_seed(u64::from(seed));
        for step in 0..60 {
            rig.now += 250;
            st.now_ms = rig.now;
            if step == 8 {
                st.me.level = 11;
            }
            tick_with(
                &mut rig.motion,
                &st,
                &|_| false,
                moment(true, false),
                &mut rig.out,
                &mut rng,
                &mut |_| true,
            );
            rig.step(&flat);
        }
        let celebrated = emotes(&rig.out).contains(&(0, emotion::CELEBRATE));
        assert!(celebrated, "bot {seed} did not celebrate");
        let jumped = moves(&rig.out).iter().any(took_off);
        let jumps = rig.motion.life().unwrap().traits.jump_mult > 0.0;
        assert_eq!(jumped, jumps, "bot {seed}: jumps={jumps}");
        cheering += usize::from(jumped);
    }
    assert!(cheering >= 45, "{cheering} of 60 jumped for joy");
}

#[test]
fn the_debug_trace_survives_bad_packets() {
    // (nothing to assert but that none of it panics)
    trace_packet(
        LEVEL_GAME,
        aa_proto::opcodes::sc::SCOneUnitMovementPacket,
        &[1, 2, 3],
    );
    trace_packet(
        LEVEL_GAME,
        aa_proto::opcodes::sc::SCOneUnitMovementPacket,
        &[],
    );
    trace_packet(2, aa_proto::opcodes::sc::SCOneUnitMovementPacket, &[0; 40]);
    trace_packet(LEVEL_GAME, 0x7777, &[0; 40]);
}

// ---- the controller with the life-like state attached --------------------------------------------

#[test]
fn a_standing_jump_is_the_arc_from_the_ground_and_back() {
    let mut rig = Rig::new(playful(), 1.0);
    rig.life().sched.cheer();
    for _ in 0..40 {
        rig.step(&flat);
    }
    let m = moves(&rig.out);
    assert!(m.len() >= 5, "{m:?}");
    // take-off: the event flag, the jump actor flag, the speed up (8.06 m/s)
    let up = m[0];
    assert_eq!((up.flags, up.actor, up.stance), (0x04, 0x10, 1));
    assert!((i32::from(up.vel[2]) - 4403).abs() <= 3, "vz {}", up.vel[2]);
    assert!((up.pos[2] - 130.0).abs() < 0.05);
    // on the way: no ground, the height above it, the vertical speed falling
    let air: Vec<&Mv> = m
        .iter()
        .filter(|p| p.flags == 0x00 && p.actor == 0x00)
        .collect();
    assert!(
        (2..=3).contains(&air.len()),
        "{} packets in the air",
        air.len()
    );
    assert!(air.iter().all(|p| p.pos[2] > 130.2));
    let apex = air.iter().map(|p| p.pos[2]).fold(0.0, f32::max) - 130.0;
    assert!((0.7..=1.32).contains(&apex), "apex {apex}");
    assert!(air.windows(2).all(|w| w[1].vel[2] < w[0].vel[2]));
    // touchdown (ground plus jump flag, no vertical speed), then the ordinary stopped update
    let land = m.iter().find(|p| p.actor == 0x14).expect("a touchdown");
    assert_eq!((land.flags, land.vel[2]), (0x04, 0));
    assert!((land.pos[2] - 130.0).abs() < 0.05);
    let last = m.last().unwrap();
    assert_eq!((last.flags, last.actor), (0x04, 0x04));
    assert!((last.pos[2] - 130.0).abs() < 0.05);
    // the order, the time moves on, and it all takes about three quarters of a second
    let kinds: Vec<u8> = m.iter().map(|p| p.actor).collect();
    let land_at = kinds.iter().position(|&a| a == 0x14).unwrap();
    assert_eq!(land_at, m.len() - 2);
    assert!(m.windows(2).all(|w| w[1].time > w[0].time), "{m:?}");
    let span = last.time - up.time;
    assert!((690..=800).contains(&span), "{span} ms");
    // the jump is over: the bot does not jump again (cooldown) and does not chatter
    assert!(rig.life().jump.is_none());
    let n = rig.out.len();
    for _ in 0..8 {
        rig.step(&flat);
    }
    assert_eq!(rig.out.len(), n);
    // the height is only in the packets: the bot stands on the ground
    assert_eq!(rig.motion.pos()[2], 130.0);
}

#[test]
fn a_runner_jumps_without_slowing_down_and_the_regular_updates_wait() {
    let mut rig = Rig::new(playful(), 60.0);
    rig.run(&flat, 85);
    let m = moves(&rig.out);
    let ups: Vec<usize> = (0..m.len()).filter(|&i| took_off(&m[i])).collect();
    assert!(
        ups.len() >= 4,
        "{} jumps in 85 s at 60 times the rate",
        ups.len()
    );
    let speed = rig.motion.speed(Pace::Run);
    let wire = (speed * 32767.0 / 60.0).round() as i16;
    for &i in &ups {
        let up = m[i];
        assert_eq!((up.flags, up.vel[1]), (0x06, wire), "run take-off {up:?}");
        // until the settle, every packet is part of the jump
        let end = (i + 1..m.len())
            .find(|&j| m[j].actor & 0x14 == 0x14)
            .expect("a touchdown");
        for p in &m[i + 1..end] {
            assert!(p.actor & 0x04 == 0 && p.flags & 0x02 != 0, "{p:?}");
            assert!(p.pos[2] >= 129.9);
        }
        assert_eq!(m[end].flags, 0x06);
        assert!((m[end].pos[2] - 130.0).abs() < 0.05);
        let after = m[(end + 1).min(m.len() - 1)];
        assert!(
            after.actor & 0x10 == 0x10 || after.actor & 0x04 == 0x04,
            "{after:?}"
        );
    }
    assert!(m.windows(2).all(|w| w[1].time > w[0].time));
    // the run goes on at the same pace: no packet pair is faster than the bot runs
    let (a, b) = (m[0], *m.last().unwrap());
    let v = (b.pos[1] - a.pos[1]) / ((b.time - a.time) as f32 / 1000.0);
    assert!(
        (v - speed).abs() < speed * 0.04,
        "{v} m/s over the whole run, not {speed}"
    );
    // the server's speed audit (25 m/s horizontal) is far away
    assert!(m.windows(2).all(|w| {
        let dt = (w[1].time - w[0].time) as f32 / 1000.0;
        dt < 0.04 || (w[1].pos[1] - w[0].pos[1]).abs() / dt < 12.0
    }));
}

/// Whether a bot with `setup` jumps in a ninety-second run (a cheer is armed, so it would).
fn jumps_with(rig: &mut Rig, ground: &dyn Ground, combat: bool, queued: bool) -> bool {
    rig.life().sched.cheer();
    rig.motion.go_to(10.0, 500.0, 0.0, Pace::Run, rig.now);
    for _ in 0..340 {
        if queued {
            rig.out.clear();
            rig.out.push(Out {
                level: LEVEL_GAME,
                opcode: op::CSInteractNPCPacket,
                body: vec![],
            });
        }
        let dt = rig
            .motion
            .wake_in(rig.now)
            .map_or(250, |w| w.clamp(15, 250));
        rig.tick(ground, combat, dt);
        if moves(&rig.out).iter().any(|m| m.actor & 0x10 != 0) {
            return true;
        }
    }
    false
}

#[test]
fn a_bot_jumps_only_when_it_may() {
    let baseline = |seed, rate| Rig::new(seed, rate);
    assert!(
        jumps_with(&mut baseline(playful(), 1.0), &flat, false, false),
        "the control"
    );
    // no real player near
    let mut r = baseline(playful(), 1.0);
    r.life().gate.audience = false;
    assert!(!jumps_with(&mut r, &flat, false, false), "no audience");
    // busy
    let mut r = baseline(playful(), 1.0);
    r.life().gate.busy = true;
    assert!(!jumps_with(&mut r, &flat, false, false), "busy");
    // in combat
    assert!(
        !jumps_with(&mut baseline(playful(), 1.0), &flat, true, false),
        "combat"
    );
    // sitting
    let mut r = baseline(playful(), 1.0);
    r.life().emotes.sitting = true;
    assert!(!jumps_with(&mut r, &flat, false, false), "sitting");
    // a packet the server checks the position for is queued
    assert!(
        !jumps_with(&mut baseline(playful(), 1.0), &flat, false, true),
        "range-checked packet"
    );
    // swimming
    assert!(
        !jumps_with(&mut baseline(playful(), 1.0), &sea, false, false),
        "swimming"
    );
    // a bot that never jumps, a rate of zero, and `lifelike = false`
    assert!(
        !jumps_with(&mut baseline(serious(), 1.0), &flat, false, false),
        "serious"
    );
    assert!(
        !jumps_with(&mut baseline(playful(), 0.0), &flat, false, false),
        "rate 0"
    );
    let mut r = Rig::plain();
    assert!(!jumps_with_plain(&mut r), "lifelike off");
}

fn jumps_with_plain(rig: &mut Rig) -> bool {
    rig.motion.go_to(10.0, 500.0, 0.0, Pace::Run, rig.now);
    for _ in 0..340 {
        rig.step(&flat);
    }
    moves(&rig.out)
        .iter()
        .any(|m| m.actor & 0x10 != 0 || m.actor & 0x04 == 0)
}

#[test]
fn a_bot_waiting_for_a_teleport_does_not_jump() {
    let mut rig = Rig::new(playful(), 1.0);
    rig.life().sched.cheer();
    rig.motion.begin_teleport(rig.now);
    for _ in 0..20 {
        rig.step(&flat);
    }
    assert!(rig.out.is_empty());
}

#[test]
fn a_pin_lands_the_bot_and_the_jump_is_over() {
    let mut rig = Rig::new(playful(), 1.0);
    rig.life().sched.cheer();
    while moves(&rig.out).len() < 2 {
        rig.step(&flat);
    }
    assert!(rig.life().jump.is_some(), "in the air");
    let pin = rig
        .motion
        .pin(rig.now, false)
        .expect("the server thinks it is in the air");
    let p = mv(&pin);
    assert_eq!(
        (p.flags, p.actor),
        (0x04, 0x04),
        "a stopped update on the ground"
    );
    assert!((p.pos[2] - 130.0).abs() < 0.05);
    assert!(rig.life().jump.is_none());
    let n = rig.out.len();
    for _ in 0..30 {
        rig.step(&flat);
    }
    assert_eq!(rig.out.len(), n, "no more packets of the jump");
}

#[test]
fn stopping_a_bot_in_the_air_tells_the_server_it_stands() {
    let mut rig = Rig::new(playful(), 60.0);
    rig.motion.go_to(10.0, 500.0, 0.0, Pace::Run, rig.now);
    rig.life().sched.cheer();
    while !moves(&rig.out).iter().any(took_off) {
        rig.step(&flat);
    }
    let n = rig.out.len();
    rig.motion.stop();
    rig.step(&flat);
    let m = moves(&rig.out[n..]);
    assert_eq!(m.len(), 1, "{m:?}");
    assert_eq!((m[0].flags, m[0].actor), (0x04, 0x04));
    assert!(rig.life().jump.is_none());
    for _ in 0..20 {
        rig.step(&flat);
    }
    assert_eq!(moves(&rig.out[n..]).len(), 1);
}

#[test]
fn a_cheer_jump_is_not_started_while_a_new_height_or_teleport_resets_the_bot() {
    let mut rig = Rig::new(playful(), 1.0);
    rig.life().sched.cheer();
    while moves(&rig.out).len() < 2 {
        rig.step(&flat);
    }
    rig.motion.teleported([50.0, 50.0, 130.0]);
    assert!(rig.life().jump.is_none() && rig.life().rejump_ms == 0);
}

#[test]
fn a_walker_pauses_at_a_corner_without_losing_the_way() {
    let route = [(40.0, 10.0), (40.0, 200.0)];
    let count_stops = |dawdle: f32| {
        let mut rig = Rig::new(serious(), 1.0);
        rig.life().traits.dawdle = dawdle;
        rig.motion.go_via(&route, 0.0, Pace::Run, rig.now);
        let mut arrived_at = 0;
        for _ in 0..260 {
            if rig.step(&flat) == MotionStatus::Arrived {
                arrived_at = rig.now - T0;
                break;
            }
        }
        assert!(arrived_at > 0, "the trip arrives");
        let m = moves(&rig.out);
        let stops: Vec<usize> = (0..m.len())
            .filter(|&i| m[i].flags == 0x04 && m[i].actor == 0x04)
            .collect();
        (m, stops, arrived_at)
    };
    let (m, stops, took) = count_stops(1.0);
    assert_eq!(stops.len(), 2, "a pause at the corner and the final stop");
    // the pause: a stopped update, then a restart after 0.3 to 1 s (and a tick)
    let i = stops[0];
    let gap = m[i + 1].time - m[i].time;
    assert!((250..=1_300).contains(&gap), "{gap} ms pause");
    let dy = (m[i + 1].pos[1] - m[i].pos[1]).abs() + (m[i + 1].pos[0] - m[i].pos[0]).abs();
    assert!(dy < 1.6, "the bot waited, not walked ({dy} m)");
    // the corner is where the route turns
    assert!((m[i].pos[0] - 40.0).abs() < 2.5, "{:?}", m[i].pos);
    let (_, stops, took_plain) = count_stops(0.0);
    assert_eq!(stops.len(), 1, "no pause for a bot that never dawdles");
    assert!(took > took_plain, "{took} against {took_plain}");
}

#[test]
fn a_pause_needs_a_stage_and_a_long_way_to_go() {
    let route = [(40.0, 10.0), (40.0, 20.0)];
    let mut rig = Rig::new(serious(), 1.0);
    rig.life().traits.dawdle = 1.0;
    rig.motion.go_via(&route, 0.0, Pace::Run, rig.now);
    while rig.step(&flat) == MotionStatus::Moving {}
    let m = moves(&rig.out);
    assert_eq!(
        m.iter().filter(|p| p.flags == 0x04).count(),
        1,
        "the way ahead is short"
    );
    // nobody watches
    let mut rig = Rig::new(serious(), 1.0);
    rig.life().traits.dawdle = 1.0;
    rig.life().gate.audience = false;
    rig.motion
        .go_via(&[(40.0, 10.0), (40.0, 200.0)], 0.0, Pace::Run, rig.now);
    while rig.step(&flat) == MotionStatus::Moving {}
    assert_eq!(
        moves(&rig.out).iter().filter(|p| p.flags == 0x04).count(),
        1
    );
}

#[test]
fn a_standing_bot_with_an_audience_turns_in_place() {
    let turns = |gate: Gate| {
        let mut rig = Rig::new(serious(), 1.0);
        rig.life().gate = gate;
        for _ in 0..240 {
            rig.now += 250;
            let mut r = fastrand::Rng::with_seed(rig.now);
            rig.motion.fidget_tick(rig.now, &mut r, &flat);
            rig.tick(&flat, false, 0);
        }
        moves(&rig.out)
    };
    let m = turns(SHOW);
    let stands: Vec<&Mv> = m
        .iter()
        .filter(|p| p.flags == 0x04 && p.actor == 0x04)
        .collect();
    assert!(
        (1..=4).contains(&stands.len()),
        "{} stopped updates",
        stands.len()
    );
    let steps = i32::from(stands[0].dir)
        .rem_euclid(128)
        .min(128 - i32::from(stands[0].dir).rem_euclid(128));
    let deg = steps as f32 * 360.0 / 128.0;
    assert!(
        (26.0..=124.0).contains(&deg),
        "the first turn is {deg} degrees"
    );
    // a shuffle walks at walk pace, or there is none
    assert!(m
        .iter()
        .filter(|p| p.flags == 0x02)
        .all(|p| p.actor == 0x05));
    assert!(turns(Gate::default()).is_empty(), "nobody to see it");
    assert!(turns(Gate { busy: true, ..SHOW }).is_empty(), "busy");
}

#[test]
fn getting_up_from_a_sit_is_a_stopped_update() {
    let mut rig = Rig::new(serious(), 1.0);
    rig.step(&flat);
    assert!(rig.out.is_empty());
    rig.motion.announce();
    rig.step(&flat);
    let m = moves(&rig.out);
    assert_eq!(m.len(), 1);
    assert_eq!((m[0].flags, m[0].actor), (0x04, 0x04));
    // a walking bot has no need of it
    rig.motion.go_to(10.0, 500.0, 0.0, Pace::Run, rig.now);
    rig.motion.announce();
    rig.step(&flat);
    assert_eq!(moves(&rig.out).last().unwrap().flags, 0x02);
}

#[test]
fn every_bot_runs_at_its_own_speed_within_three_percent() {
    let mut speeds = vec![];
    for seed in 1..=50 {
        let mut rig = Rig::new(seed, 1.0);
        let k = rig.life().traits.speed_k;
        let s = rig.motion.speed(Pace::Run);
        assert!((s - 5.4 * k).abs() < 1e-4 && (s / 5.4 - 1.0).abs() <= 0.0301);
        assert!((rig.motion.speed(Pace::Walk) - 1.8 * k).abs() < 1e-4);
        rig.motion.go_to(10.0, 500.0, 0.0, Pace::Run, rig.now);
        rig.step(&flat);
        let first = moves(&rig.out)[0];
        assert_eq!(first.vel[1], (s * 32767.0 / 60.0).round() as i16);
        speeds.push(s);
    }
    let (lo, hi) = speeds
        .iter()
        .fold((f32::MAX, 0.0f32), |a, &s| (a.0.min(s), a.1.max(s)));
    assert!(hi / lo > 1.03, "the bots differ: {lo} to {hi}");
    // and a bot without the life-like state runs at the configured speed
    assert_eq!(Rig::plain().motion.speed(Pace::Run), 5.4);
}

#[test]
fn the_packet_rate_stays_flat_with_the_life_like_parts_on() {
    let mut off = Rig::plain();
    let mut on = Rig::new(playful(), 1.0);
    on.life().traits.dawdle = 0.0;
    off.run(&flat, 300);
    on.run(&flat, 300);
    let (a, b) = (moves(&off.out).len() as f32, moves(&on.out).len() as f32);
    let ratio = b / a;
    assert!(
        (0.97..=1.06).contains(&ratio),
        "{b} against {a} packets in five minutes"
    );
    // the jumps are the only difference
    assert!(moves(&on.out).iter().any(took_off) || moves(&on.out).len() as f32 >= a);
    // and the plain bot is unchanged: ground flags only
    assert!(moves(&off.out).iter().all(|m| m.actor & 0x04 == 0x04));
}

#[test]
fn a_swimming_bot_sends_the_swim_stance_and_a_walking_one_the_relaxed_stance() {
    let mut rig = Rig::new(serious(), 1.0);
    rig.motion.go_to(40.0, 10.0, 0.0, Pace::Run, rig.now);
    while rig.step(&sea) == MotionStatus::Moving {}
    assert_eq!(rig.motion.pos()[2], SWIM_Z);
    let m = moves(&rig.out);
    assert!(m.iter().all(|p| p.stance == 2), "{m:?}");
    let mut rig = Rig::new(serious(), 1.0);
    rig.motion.go_to(40.0, 10.0, 0.0, Pace::Run, rig.now);
    while rig.step(&flat) == MotionStatus::Moving {}
    assert!(moves(&rig.out).iter().all(|p| p.stance == 1));
}

#[test]
fn the_life_of_a_bot_is_one_small_box() {
    // (a thousand bots must not cost a megabyte; the bot itself only holds the pointer)
    let size = std::mem::size_of::<Life>();
    assert!(size <= 512, "{size} bytes");
    assert_eq!(
        std::mem::size_of::<Option<Box<Life>>>(),
        std::mem::size_of::<usize>()
    );
}

#[test]
fn a_stale_second_jump_or_a_teleport_wait_does_not_spin_the_bot_loop() {
    // a second jump left over from the last trip is dropped by the next one (a chase never jumps)
    let mut rig = Rig::new(playful(), 1.0);
    rig.life().rejump_ms = rig.now + 100;
    rig.motion.chase_to(10.0, 500.0, 0.0, Pace::Run, rig.now);
    assert_eq!(rig.life().rejump_ms, 0);
    for _ in 0..40 {
        rig.step(&flat);
        assert_eq!(rig.motion.wake_in(rig.now), None);
    }
    // a bot in the air that has to wait for a teleport lands in its mind
    let mut rig = Rig::new(playful(), 1.0);
    rig.life().sched.cheer();
    while moves(&rig.out).len() < 2 {
        rig.step(&flat);
    }
    assert!(rig.life().jump.is_some());
    rig.motion.begin_teleport(rig.now);
    assert!(rig.life().jump.is_none());
    assert_eq!(rig.motion.wake_in(rig.now), None);
}
