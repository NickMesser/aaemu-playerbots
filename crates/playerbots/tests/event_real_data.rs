//! Runs the event goal against the real `main_world` data without a server: the bot's state and
//! the event board are fed by hand the way the server's packets would feed them. Checks the whole
//! chain of the Crimson Rift: the packets post the event, a bot between small things joins, walks
//! to the target, fights the monsters of the wave and leaves when the event ends.
//!
//! ```text
//! AA_COMPACT=<.../app/Data/compact.sqlite3> AA_SERVER_DATA=<.../app/Data> [AA_BAKED=<data dir>] \
//!   cargo test -p playerbots --test event_real_data -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use aa_proto::cs::Out;
use aa_proto::opcodes::cs as op;
use aa_proto::sc::ServerEvent;
use aa_proto::sc_event::{EventId, TowerDefInfo};
use aa_proto::sc_world::{conflict_state, WorldEvent};
use aa_world::{World, WorldPaths};
use playerbots::archetype::Archetypes;
use playerbots::bot::brain::Brain;
use playerbots::bot::cmd::{BotCmd, GoalChoice, GoalKind};
use playerbots::bot::combat::Combat;
use playerbots::bot::coordinator::CoordHandle;
use playerbots::bot::ctx::{Ctx, Persona};
use playerbots::bot::motion::{Motion, MotionCfg, MotionEnv};
use playerbots::bot::state::{BotState, Unit, UnitKind};
use playerbots::config::Config;
use playerbots::events::{Join, Population};
use playerbots::hub::{Hub, HubParams, Logger};
use playerbots::roster::Temperament;

const RIFT: EventId = EventId {
    def: 3,
    zone_group: 20,
};
/// The Rift (the defended target) of the Crimson Rift.
const TARGET: [f32; 3] = [14_808.86, 12_063.09, 180.2];
const ARCHER: u32 = 8834;
const SOLDIER: u32 = 8827;
const SUMMONER: u32 = 8830;
const ME: u32 = 0x400;

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

fn dist2d(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt()
}

/// One bot without a network connection, sharing a hub with others when asked.
struct Rig {
    world: Arc<World>,
    cfg: Config,
    hub: Arc<Hub>,
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

fn hub(world: &Arc<World>) -> Arc<Hub> {
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let hub = Hub::new(HubParams {
        world,
        baked_dir: None,
        supervisor: tx,
        llm: None,
    });
    hub.events.bind(world);
    Arc::new(hub)
}

impl Rig {
    /// A level `level` bot `id` standing `away` metres south of the target.
    fn new(world: &Arc<World>, hub: &Arc<Hub>, id: u32, level: u8, away: f32) -> Rig {
        let mut cfg = Config::default();
        cfg.events.enabled = true;
        cfg.events.join_delay_secs = [0, 0];
        let archetypes = Archetypes::builtin().unwrap();
        let archetype = archetypes.get("Darkrunner").unwrap().clone();
        let slot = hub.register(pb_web::BotSnapshot {
            id,
            name: format!("Tester{id}"),
            level,
            activity: pb_web::Activity::Idle,
            ..pb_web::BotSnapshot::default()
        });
        let persona = Persona {
            id,
            name: format!("Tester{id}").into(),
            archetype,
            temper: Temperament::NEUTRAL,
            profile: playerbots::roster::Profile::NEUTRAL,
        };
        let mut state = BotState::new();
        state.me.char_id = 500 + id;
        state.me.obj = ME + id;
        state.me.spawned = true;
        state.me.level = level;
        state.me.hp = 5000;
        state.me.hp_max = 5000;
        state.me.mp = 400;
        state.me.mp_max = 400;
        state.me.faction = world.race_faction(1).expect("Nuian faction");
        let p = [TARGET[0], TARGET[1] - away, TARGET[2]];
        let z = world.ground_z(p[0], p[1]).unwrap_or(p[2]);
        state.me.pos = [p[0], p[1], z];
        state.skills = vec![(persona.archetype.starter_skill, 1)];
        let mut motion = Motion::new(MotionCfg::default());
        motion.enter_world(ME + id, state.me.pos, 0.0, 1);
        let brain = Brain::new(&cfg.behavior, &persona.temper);
        Rig {
            log: Logger::new(slot),
            coord: CoordHandle::detached(),
            cfg,
            hub: hub.clone(),
            persona,
            state,
            motion,
            combat: Combat::new(),
            brain,
            out: Vec::new(),
            rng: fastrand::Rng::with_seed(u64::from(id)),
            now_ms: 1,
            world: world.clone(),
        }
    }

    fn tick(&mut self, dt: u64) {
        self.now_ms += dt;
        self.state.now_ms = self.now_ms;
        self.state.me.pos = self.motion.pos();
        {
            let mut ctx = Ctx {
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
            };
            self.brain.tick(&mut ctx);
        }
        let mut env = MotionEnv {
            ground: &*self.world,
            now_ms: self.now_ms,
            out: &mut self.out,
            rng: &mut self.rng,
            send_gap_ms: 250,
            combat: self.combat.target().is_some(),
        };
        self.motion.tick(&mut env);
        self.state.me.pos = self.motion.pos();
    }

    fn war(&mut self, x: f32, y: f32) -> bool {
        let ctx = Ctx {
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
        };
        ctx.in_war_zone(x, y)
    }

    /// Feeds an event the way the bot task does: the state, the combat controller and the board.
    fn feed(&mut self, ev: WorldEvent) {
        let ev = ServerEvent::World(ev);
        self.state.apply(&ev);
        playerbots::events::on_server_event(&self.hub.events, &self.world, &ev, &self.state);
        self.combat
            .on_event(&ev, self.state.me.obj, self.now_ms, &mut self.out);
    }

    fn said(&self, needle: &str) -> bool {
        self.hub
            .slot(self.persona.id)
            .is_some_and(|s| s.with(|g| g.log.iter().any(|l| l.text.contains(needle))))
    }

    fn sent(&self, opcode: u16) -> usize {
        self.out.iter().filter(|o| o.opcode == opcode).count()
    }

    fn add_unit(&mut self, obj: u32, template: u32, pos: [f32; 3]) {
        let level = self.world.npc(template).map_or(40, |t| t.level);
        self.state.units.insert(
            obj,
            Unit {
                kind: UnitKind::Npc,
                id: template,
                pos,
                hp: 100,
                faction: 0,
                level,
                flags: 0,
                target: 0,
                name: None,
            },
        );
    }
}

fn start_the_rift(rig: &mut Rig, step: u32) {
    rig.feed(WorldEvent::TowerDefStart {
        id: RIFT,
        zone_key: 257,
    });
    list(rig, step);
}

fn list(rig: &mut Rig, step: u32) {
    rig.feed(WorldEvent::TowerDefList(
        vec![TowerDefInfo {
            id: RIFT,
            zone_key: 257,
            spot_id: 9846,
            target_obj: 0x5000,
            pos: (TARGET[0], TARGET[1], TARGET[2]),
            step,
        }]
        .into_boxed_slice(),
    ));
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn a_bot_joins_the_rift_fights_its_wave_and_leaves_when_it_ends() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let hub = hub(&world);
    // 300 m from the Rift, level 45
    let mut rig = Rig::new(&world, &hub, 1, 45, 300.0);
    // no event: the bot does what it did before
    for _ in 0..40 {
        rig.tick(250);
        assert_ne!(rig.brain.goal_name(), "event");
    }

    // the packets post the event; the bot is between small things and takes it
    start_the_rift(&mut rig, 2);
    assert!(hub.events.any());
    let mut joined_at = None;
    for i in 0..40 {
        rig.tick(250);
        if rig.brain.goal_name() == "event" {
            joined_at = Some(i);
            break;
        }
    }
    assert!(
        joined_at.is_some(),
        "never joined; goal {}",
        rig.brain.goal_name()
    );
    assert!(rig.said("event: joining the Crimson Rift"));
    assert_eq!(hub.events.view(RIFT).unwrap().joined, 1);

    // it walks to the Rift and stops within the hold radius
    let mut arrived = false;
    for _ in 0..(300 * 4) {
        rig.tick(250);
        if dist2d(rig.state.me.pos, TARGET) <= 25.0 && rig.said("event: at the Crimson Rift") {
            arrived = true;
            break;
        }
    }
    assert!(
        arrived,
        "still {:.0} m from the Rift",
        dist2d(rig.state.me.pos, TARGET)
    );
    assert!(rig.sent(op::CSMoveUnitPacket) > 10);

    // wave 2 comes: archers and infantrymen at the Rift, the summoner behind them, and a
    // monster that has nothing to do with the event
    let near = |dx: f32, dy: f32| [TARGET[0] + dx, TARGET[1] + dy, TARGET[2]];
    rig.add_unit(0x6001, ARCHER, near(20.0, 10.0));
    rig.add_unit(0x6002, 8826, near(25.0, 12.0));
    rig.add_unit(0x6003, SUMMONER, near(5.0, 5.0));
    rig.add_unit(0x6004, SOLDIER, near(10.0, 0.0));
    let mut engaged = None;
    for _ in 0..80 {
        rig.tick(250);
        if let Some(t) = rig.combat.target() {
            engaged = Some(t);
            break;
        }
    }
    let t = engaged.expect("the bot did not engage a wave monster");
    assert!(
        t == 0x6001 || t == 0x6002,
        "engaged {t:#x}: a kill target of wave 2 comes before a straggler, never the summoner"
    );
    for _ in 0..80 {
        rig.tick(250);
        if rig.sent(op::CSStartSkillPacket) > 0 {
            break;
        }
    }
    assert!(rig.sent(op::CSStartSkillPacket) > 0, "no attack was sent");
    // it dies by the bot's hand; the board counts the kill once however many bots see it
    rig.feed(WorldEvent::UnitDeath {
        obj: t,
        killer: rig.state.me.obj,
        respawn_wait_ms: 0,
        lost_exp: 0,
        reason: 0,
    });
    assert_eq!(hub.events.stats().kills, 1);
    // never the summoner, however long the bot waits
    for _ in 0..200 {
        rig.tick(250);
        assert_ne!(rig.combat.target(), Some(0x6003), "attacked a controller");
    }

    // the defended unit walks a path: the bot, which sees it, follows (the list says where it
    // stood when it was sent)
    let moved = [TARGET[0] + 90.0, TARGET[1], TARGET[2]];
    rig.add_unit(0x5000, 8828, moved);
    for _ in 0..600 {
        rig.tick(250);
        if dist2d(rig.state.me.pos, moved) <= 40.0 {
            break;
        }
    }
    assert!(
        dist2d(rig.state.me.pos, moved) <= 40.0,
        "{:.0} m from the walking target",
        dist2d(rig.state.me.pos, moved)
    );

    // the event ends
    rig.feed(WorldEvent::TowerDefEnd {
        id: RIFT,
        zone_key: 257,
    });
    rig.feed(WorldEvent::TowerDefList(Vec::new().into_boxed_slice()));
    for _ in 0..20 {
        rig.tick(250);
        if rig.brain.goal_name() != "event" {
            break;
        }
    }
    assert_ne!(
        rig.brain.goal_name(),
        "event",
        "still in the goal after the end"
    );
    assert!(rig.said("event: leaving the Crimson Rift"));
    assert!(!hub.events.any());
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn a_hurt_bot_leaves_and_stays_away_and_a_low_level_bot_never_comes() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let hub = hub(&world);
    let mut low = Rig::new(&world, &hub, 2, 30, 100.0);
    let mut rig = Rig::new(&world, &hub, 1, 45, 100.0);
    start_the_rift(&mut rig, 2);
    for _ in 0..200 {
        low.tick(250);
        rig.tick(250);
        if rig.brain.goal_name() == "event" && dist2d(rig.state.me.pos, TARGET) < 25.0 {
            break;
        }
    }
    assert_eq!(rig.brain.goal_name(), "event");
    assert_ne!(
        low.brain.goal_name(),
        "event",
        "level 30 against level 40 monsters"
    );
    // hit points drop below 40 percent
    rig.state.me.hp = 1500;
    for _ in 0..8 {
        rig.tick(250);
        if rig.brain.goal_name() != "event" {
            break;
        }
    }
    assert_ne!(rig.brain.goal_name(), "event");
    assert!(rig.said("event: leaving the Crimson Rift (hurt)"));
    // healed, it is still barred for a while: the board says no
    rig.state.me.hp = 5000;
    let caps = playerbots::events::Caps {
        per_event: 6,
        max_share: 1.0,
        min_level: 40,
    };
    assert_eq!(hub.events.can_join(RIFT, 1, &caps), Join::Barred);
    for _ in 0..80 {
        rig.tick(250);
        assert_ne!(rig.brain.goal_name(), "event");
    }
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn twenty_bots_fill_the_event_to_its_cap_and_no_further() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let hub = hub(&world);
    // 20 bots of level 45, 60 to 400 m from the Rift; 30 percent of 20 is 6, the per-event cap 6
    let mut rigs: Vec<Rig> = (1..=20)
        .map(|i| {
            let mut r = Rig::new(&world, &hub, i, 45, 60.0 + 17.0 * i as f32);
            r.cfg.events.max_bots_per_event = 6;
            r.cfg.events.max_share = 0.3;
            r
        })
        .collect();
    hub.events
        .set_population(Population::from_levels(vec![45u8; 20]));
    start_the_rift(&mut rigs[0], 2);
    for _ in 0..120 {
        for r in &mut rigs {
            r.tick(250);
        }
    }
    let in_event: Vec<u32> = rigs
        .iter()
        .filter(|r| r.brain.goal_name() == "event")
        .map(|r| r.persona.id)
        .collect();
    println!("in the event: {in_event:?}");
    assert_eq!(in_event.len(), 6, "{in_event:?}");
    assert_eq!(hub.events.view(RIFT).unwrap().joined, 6);
    let s = hub.events.stats();
    assert_eq!(s.joins, 6);
    // the rest went on with their lives
    let others = rigs
        .iter()
        .filter(|r| r.brain.goal_name() != "event")
        .count();
    assert_eq!(others, 14);
}

#[test]
#[ignore = "needs AA_COMPACT, AA_SERVER_DATA and AA_BAKED (real data with the zone grid)"]
fn the_everyday_goals_keep_out_of_a_zone_group_at_war() {
    let (Some(world), Some(_)) = (world(), std::env::var_os("AA_BAKED")) else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA / AA_BAKED not set; skipping");
        return;
    };
    let hub = hub(&world);
    let mut rig = Rig::new(&world, &hub, 1, 45, 300.0);
    let (rx, ry) = (TARGET[0], TARGET[1]);
    // nothing is at war until the server says so
    assert!(!rig.war(rx, ry));
    rig.feed(WorldEvent::ConflictZoneState {
        zone_group: 20,
        state: conflict_state::WAR,
        end_unix_s: 0,
    });
    // the feature is off by default: bots go where they went before
    rig.cfg.events.enabled = false;
    assert!(!rig.war(rx, ry));
    rig.cfg.events.enabled = true;
    assert!(rig.war(rx, ry), "Cinderstone Moor is zone group 20");
    // another zone group is fine, and so is the switch
    let nuian_start = world.race_start(1).unwrap();
    assert!(!rig.war(nuian_start.x, nuian_start.y));
    rig.cfg.events.avoid_war_zones = false;
    assert!(!rig.war(rx, ry));
    rig.cfg.events.avoid_war_zones = true;
    // peace again
    rig.feed(WorldEvent::ConflictZoneState {
        zone_group: 20,
        state: conflict_state::PEACE,
        end_unix_s: 0,
    });
    assert!(!rig.war(rx, ry));

    // a bot told to wander in a zone group at war finds no place to go; at peace it walks
    let wander = |war: bool| {
        let mut r = Rig::new(&world, &hub, 3, 45, 0.0);
        r.feed(WorldEvent::ConflictZoneState {
            zone_group: 20,
            state: if war {
                conflict_state::WAR
            } else {
                conflict_state::PEACE
            },
            end_unix_s: 0,
        });
        let home = r.state.me.pos;
        r.brain
            .command(&BotCmd::Goal(GoalChoice::Forced(GoalKind::Wander)));
        let mut farthest = 0.0f32;
        // the first legs: after a few failed ones the goal gives up and the brain picks another
        for _ in 0..60 {
            r.tick(250);
            farthest = farthest.max(dist2d(home, r.motion.pos()));
        }
        farthest
    };
    let at_peace = wander(false);
    let at_war = wander(true);
    println!("wandered {at_peace:.0} m at peace, {at_war:.0} m at war");
    assert!(at_peace > 3.0, "{at_peace}");
    assert!(at_war < 1.0, "{at_war}");
}
