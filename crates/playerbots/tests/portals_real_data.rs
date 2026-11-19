//! The portal travel against the real `main_world` data, without a server: the brain, the recording
//! duty and the far-trip journey run on a bot whose state is fed by hand the way the server's
//! packets would feed it.
//!
//! ```text
//! AA_COMPACT=<.../app/Data/compact.sqlite3> AA_SERVER_DATA=<.../app/Data> [AA_BAKED=<data dir>] \
//!   cargo test -p playerbots --test portals_real_data -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use aa_proto::cs::Out;
use aa_proto::opcodes::cs as op;
use aa_proto::sc::ServerEvent;
use aa_proto::sc_travel::PortalEntry;
use aa_proto::sc_world::{unit_type, UnitInfo, WorldEvent};
use aa_world::{World, WorldPaths};
use playerbots::archetype::Archetypes;
use playerbots::bot::brain::Brain;
use playerbots::bot::cmd::{BotCmd, GoalChoice, GoalKind};
use playerbots::bot::combat::Combat;
use playerbots::bot::coordinator::CoordHandle;
use playerbots::bot::ctx::{Ctx, Persona};
use playerbots::bot::motion::{Motion, MotionCfg, MotionEnv, Pace};
use playerbots::bot::state::{BotState, Item};
use playerbots::bot::travel::{self, Journey, JourneyStatus};
use playerbots::config::Config;
use playerbots::hub::{Hub, HubParams, Logger};
use playerbots::roster::Temperament;

const ME: u32 = 0x400;
const CHAR: u32 = 500;
const BOOK_ITEM: u64 = 9001;

fn world() -> Option<Arc<World>> {
    static WORLD: OnceLock<Option<Arc<World>>> = OnceLock::new();
    WORLD
        .get_or_init(|| {
            let compact = PathBuf::from(std::env::var_os("AA_COMPACT")?);
            let server_data = PathBuf::from(std::env::var_os("AA_SERVER_DATA")?);
            let baked = std::env::var_os("AA_BAKED").map(PathBuf::from);
            Some(Arc::new(
                World::load(&WorldPaths {
                    compact,
                    server_data,
                    baked,
                    world: "main_world".into(),
                })
                .expect("load main_world"),
            ))
        })
        .clone()
}

struct Rig {
    world: Arc<World>,
    cfg: Config,
    hub: Hub,
    log: Logger,
    coord: CoordHandle,
    persona: Persona,
    state: BotState,
    motion: Motion,
    combat: Combat,
    brain: Brain,
    out: Vec<Out>,
    rng: fastrand::Rng,
    now_ms: u64,
}

impl Rig {
    fn new(world: Arc<World>, at: [f32; 3], gm: bool) -> Rig {
        let mut cfg = Config::default();
        cfg.bots.gm_bootstrap = gm;
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let hub = Hub::new(HubParams {
            world: &world,
            baked_dir: None,
            supervisor: tx,
            llm: None,
        });
        let archetype = Archetypes::builtin()
            .unwrap()
            .get("Darkrunner")
            .unwrap()
            .clone();
        let slot = hub.register(pb_web::BotSnapshot {
            id: 1,
            name: "Tester".into(),
            ..pb_web::BotSnapshot::default()
        });
        let persona = Persona {
            id: 1,
            name: "Tester".into(),
            archetype,
            temper: Temperament::NEUTRAL,
            profile: playerbots::roster::Profile::NEUTRAL,
        };
        let mut state = BotState::new();
        state.me.char_id = CHAR;
        state.me.obj = ME;
        state.me.name = "Tester".into();
        state.me.spawned = true;
        state.me.level = 10;
        state.me.hp = 1000;
        state.me.hp_max = 1000;
        state.me.mp = 400;
        state.me.mp_max = 400;
        state.me.faction = world.race_faction(1).expect("Nuian faction");
        state.me.pos = at;
        state.bag.insert(
            0,
            Item {
                id: BOOK_ITEM,
                template: 4045,
                count: 1,
            },
        );
        state.bag.insert(
            1,
            Item {
                id: 9002,
                template: 417,
                count: 3,
            },
        );
        state.bag.insert(
            2,
            Item {
                id: 9003,
                template: 4053,
                count: 2,
            },
        );
        let mut motion = Motion::new(MotionCfg::default());
        motion.enter_world(ME, at, 0.0, 1);
        let brain = Brain::new(&cfg.behavior, &persona.temper);
        Rig {
            log: Logger::new(slot),
            coord: CoordHandle::detached(),
            cfg,
            hub,
            persona,
            state,
            motion,
            combat: Combat::new(),
            brain,
            out: Vec::new(),
            rng: fastrand::Rng::with_seed(7),
            now_ms: 1,
            world,
        }
    }

    fn ctx(&mut self) -> Ctx<'_> {
        Ctx {
            world: &self.world,
            cfg: &self.cfg,
            persona: &self.persona,
            state: &mut self.state,
            motion: &mut self.motion,
            combat: &mut self.combat,
            out: &mut self.out,
            coord: &self.coord,
            hub: &self.hub,
            log: &self.log,
            rng: &mut self.rng,
            now: tokio::time::Instant::now(),
            now_ms: self.now_ms,
            real_player_near: false,
        }
    }

    fn tick(&mut self, dt: u64) {
        self.now_ms += dt;
        self.state.now_ms = self.now_ms;
        self.state.me.pos = self.motion.pos();
        {
            let mut brain = std::mem::replace(
                &mut self.brain,
                Brain::new(&self.cfg.behavior, &self.persona.temper),
            );
            let mut ctx = self.ctx();
            brain.tick(&mut ctx);
            self.brain = brain;
        }
        let mut env = MotionEnv {
            ground: &*self.world,
            now_ms: self.now_ms,
            out: &mut self.out,
            rng: &mut self.rng,
            send_gap_ms: 250,
            combat: false,
        };
        self.motion.tick(&mut env);
        self.state.me.pos = self.motion.pos();
    }

    fn feed(&mut self, ev: WorldEvent) {
        let ev = ServerEvent::World(ev);
        self.state.apply(&ev);
        self.combat
            .on_event(&ev, self.state.me.obj, self.now_ms, &mut self.out);
    }

    /// The bot task answers a server teleport this way.
    fn teleport_to(&mut self, p: [f32; 3]) {
        self.feed(WorldEvent::TeleportUnit {
            reason: 0,
            error: 0,
            pos: (p[0], p[1], p[2]),
        });
        self.motion.teleported(p);
        self.state.reactions = Default::default();
    }

    /// Skill ids of the `CSStartSkill` packets queued since the last drain.
    fn casts(&self) -> Vec<u32> {
        self.out
            .iter()
            .filter(|o| o.opcode == op::CSStartSkillPacket)
            .map(|o| u32::from_le_bytes(o.body[..4].try_into().unwrap()))
            .collect()
    }

    fn sent(&self, opcode: u16) -> usize {
        self.out.iter().filter(|o| o.opcode == opcode).count()
    }
}

fn entry(id: u32, zone: u32, name: &str, off: (f32, f32)) -> PortalEntry {
    PortalEntry {
        id,
        name: name.into(),
        zone_id: zone,
        off_x: off.0,
        off_y: off.1,
        z: 100.0,
        z_rot: 0.0,
    }
}

fn gate_unit(obj: u32, owner: u32, pos: [f32; 3]) -> UnitInfo {
    UnitInfo {
        obj,
        unit_type: unit_type::NPC,
        id: 3891,
        name: "Portal".into(),
        owner_name: "Tester".into(),
        owner_id: owner,
        tl_id: 0,
        slave_parent: 0,
        pos: (pos[0], pos[1], pos[2]),
        level: 1,
        hp: 100,
        mp: 0,
        dead: false,
        has_loot: false,
        faction_id: 0,
        expedition_id: 0,
        yaw: 0,
        skills: Box::new([]),
        buffs: Box::new([]),
        backpack: 0,
        complete: true,
    }
}

fn start_of(world: &World) -> [f32; 3] {
    let s = world.race_start(1).expect("Nuian start");
    let z = world.ground_z(s.x, s.y).unwrap_or(s.z);
    [s.x, s.y, z]
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn the_bot_records_a_town_once_and_names_it() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let at = start_of(&world);
    let mut rig = Rig::new(world.clone(), at, false);
    assert!(
        playerbots::bot::travel::places::worth(&world, at, &[]).is_some(),
        "the race start is a town"
    );
    rig.brain
        .command(&BotCmd::Goal(GoalChoice::Forced(GoalKind::Rest)));
    let mut record_at = None;
    for i in 0..400 {
        rig.tick(1500);
        if rig.casts().contains(&11_215) {
            record_at = Some(i);
            break;
        }
        rig.out.clear();
    }
    assert!(record_at.is_some(), "no record cast in 10 minutes");
    assert!(
        rig.state.travel.busy(),
        "the goal waits while the cast is under way"
    );
    // the name travels in the skill object: a town name, at most 32 characters
    let cast = rig
        .out
        .iter()
        .find(|o| o.opcode == op::CSStartSkillPacket)
        .unwrap();
    let text = String::from_utf8_lossy(&cast.body).into_owned();
    println!("record cast body: {text:?}");
    // the server's answer: the new entry alone
    let off = (123.0, 456.0);
    rig.feed(WorldEvent::PortalBook(
        vec![entry(4100, 179, "Tester spot", off)].into(),
    ));
    rig.out.clear();
    for _ in 0..8 {
        rig.tick(1500);
    }
    assert!(!rig.state.travel.busy());
    let book = rig.state.travel.book().expect("a book");
    assert_eq!(book.private.len(), 1);
    assert_eq!(
        book.private[0].world,
        Some([at[0], at[1]]),
        "the spot where the bot stood is the entry's position"
    );
    // not again for a long while, and never near the entry
    for _ in 0..300 {
        rig.tick(1500);
        assert!(!rig.casts().contains(&11_215), "recorded twice");
        rig.out.clear();
    }
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn a_far_trip_opens_a_gate_to_a_book_entry_and_goes_through() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    // the destination: a recall point a few km away
    let home = start_of(&world);
    let dest_point = world
        .recall_points()
        .iter()
        .filter(|r| r.pos.dist2d(aa_world::Vec3::new(home[0], home[1], 0.0)) > 3000.0)
        .max_by(|a, b| a.id.cmp(&b.id))
        .expect("a recall point far from the start")
        .clone();
    let dest = [dest_point.pos.x, dest_point.pos.y];
    println!("destination {} at {dest:?}", dest_point.name);
    let mut rig = Rig::new(world.clone(), home, true);
    // a private entry 100 m from the destination: the zone cell is learned from a recall district
    rig.feed(WorldEvent::ReturnDistricts {
        portals: vec![entry(
            dest_point.id,
            u32::from(dest_point.zone_key),
            "district",
            (0.0, 0.0),
        )]
        .into(),
        bound: 0,
    });
    // (district entries resolve through the recall point; the entry itself is a district)
    rig.state.travel.book().unwrap();
    // the trip goes through a district gate: no ink needed
    rig.tick(100);
    rig.ctx().travel(dest[0], dest[1], 3.0, Pace::Run);
    assert!(
        !rig.motion.is_moving(),
        "a journey by portal does not walk: {:?}",
        rig.motion.status()
    );
    // the open cast (kind 1 = district) goes out after the cooldown check, within a second tick
    rig.tick(300);
    let casts = rig.casts();
    assert!(casts.contains(&11_216), "no open cast: {casts:?}");
    assert!(rig.state.travel.busy());
    // the gate shows up 10 s later, owned by the bot, 1.8 m away
    rig.out.clear();
    rig.tick(10_500);
    let g = [home[0] + 1.8, home[1], home[2]];
    rig.feed(WorldEvent::UnitState(Box::new(gate_unit(0x9000, CHAR, g))));
    rig.tick(300);
    assert_eq!(rig.sent(op::CSUsePortalPacket), 1, "the gate is used");
    // the server moves the bot
    rig.feed(WorldEvent::PortalUsed { gate: 0x9000 });
    rig.teleport_to([dest[0] + 20.0, dest[1], 150.0]);
    rig.out.clear();
    rig.tick(300);
    assert!(!rig.state.travel.busy(), "the journey is over");
    let s = travel::stats();
    println!("{}", s.line());
    assert!(s.opens_sent >= 1 && s.uses_sent >= 1 && s.arrivals >= 1);
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn a_journey_object_runs_on_its_own_and_fails_typed_without_a_route() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let home = start_of(&world);
    let mut rig = Rig::new(world, home, false);
    let mut j = Journey::new([home[0] + 9000.0, home[1]]);
    rig.now_ms += 100;
    let st = j.tick(&mut rig.ctx());
    assert!(
        matches!(st, JourneyStatus::Failed(travel::JourneyFail::NoRoute)),
        "{st:?}"
    );
    assert!(rig.out.is_empty());
}
