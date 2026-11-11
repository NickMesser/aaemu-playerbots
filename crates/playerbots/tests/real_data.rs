//! Runs the brain, the goals, the combat controller and the motion controller against the real
//! `main_world` data, without a server: the bot's state is fed by hand the way the server's packets
//! would feed it.
//!
//! ```text
//! AA_COMPACT=<.../app/Data/compact.sqlite3> AA_SERVER_DATA=<.../app/Data> [AA_BAKED=<data dir>] \
//!   cargo test -p playerbots --test real_data -- --ignored --nocapture
//! ```

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use aa_proto::cs::{self, Out};
use aa_proto::opcodes::cs as op;
use aa_proto::sc::ServerEvent;
use aa_proto::sc_activity::ActivityEvent;
use aa_proto::sc_world::{EquipSlot, LootItem, WorldEvent};
use aa_world::{Relation, World, WorldPaths};
use playerbots::archetype::Archetypes;
use playerbots::bot::brain::Brain;
use playerbots::bot::cmd::{BotCmd, GoalChoice, GoalKind, PairKind, PairOrder, PairRole};
use playerbots::bot::combat::{Combat, CombatStatus, LostReason, FIGHT_TIMEOUT_MS, NO_DAMAGE_MS};
use playerbots::bot::coordinator::CoordHandle;
use playerbots::bot::ctx::{Ctx, Persona};
use playerbots::bot::motion::{Motion, MotionCfg, MotionEnv};
use playerbots::bot::state::{BotState, Unit, UnitKind};
use playerbots::config::Config;
use playerbots::gm::{GmWire, Pre};
use playerbots::hub::{Hub, HubParams, Logger};
use playerbots::roster::Temperament;

fn world() -> Option<Arc<World>> {
    static WORLD: OnceLock<Option<Arc<World>>> = OnceLock::new();
    WORLD
        .get_or_init(|| {
            let compact = PathBuf::from(std::env::var_os("AA_COMPACT")?);
            let server_data = PathBuf::from(std::env::var_os("AA_SERVER_DATA")?);
            let baked = std::env::var_os("AA_BAKED").map(PathBuf::from);
            let paths = WorldPaths {
                compact,
                server_data,
                baked,
                world: "main_world".into(),
            };
            Some(Arc::new(World::load(&paths).expect("load main_world")))
        })
        .clone()
}

const ME: u32 = 0x400;

/// One bot without a network connection.
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
    fn new(world: Arc<World>, archetype: &str, seed: u64) -> Rig {
        let cfg = Config::default();
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let hub = Hub::new(HubParams {
            world: &world,
            baked_dir: None,
            supervisor: tx,
            llm: None,
        });
        let archetypes = Archetypes::builtin().unwrap();
        let archetype = archetypes.get(archetype).unwrap().clone();
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
        let start = world.race_start(1).expect("Nuian start");
        let z = world.ground_z(start.x, start.y).unwrap_or(start.z);
        let mut state = BotState::new();
        state.me.char_id = 500;
        state.me.obj = ME;
        state.me.spawned = true;
        state.me.level = 1;
        state.me.hp = 1000;
        state.me.hp_max = 1000;
        state.me.mp = 400;
        state.me.mp_max = 400;
        state.me.faction = world.race_faction(1).expect("Nuian faction");
        state.me.pos = [start.x, start.y, z];
        state.skills = vec![(persona.archetype.starter_skill, 1)];
        let mut motion = Motion::new(MotionCfg::default());
        motion.enter_world(ME, state.me.pos, 0.0, 1);
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
            rng: fastrand::Rng::with_seed(seed),
            now_ms: 1,
            world,
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

    /// Feeds an event the way the bot task does.
    fn feed(&mut self, ev: WorldEvent) {
        let ev = ServerEvent::World(ev);
        self.state.apply(&ev);
        self.combat
            .on_event(&ev, self.state.me.obj, self.now_ms, &mut self.out);
    }

    fn sent(&self, opcode: u16) -> usize {
        self.out.iter().filter(|o| o.opcode == opcode).count()
    }

    fn force(&mut self, kind: GoalKind) {
        self.brain.command(&BotCmd::Goal(GoalChoice::Forced(kind)));
    }

    /// Feeds an activity event (duels, quests).
    fn feed_activity(&mut self, ev: ActivityEvent) {
        let ev = ServerEvent::Activity(ev);
        self.state.apply(&ev);
        self.combat
            .on_event(&ev, self.state.me.obj, self.now_ms, &mut self.out);
    }

    /// The goals the brain leaves out of its choice for this bot right now.
    fn gate(&mut self) -> playerbots::bot::cmd::GoalMask {
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
        playerbots::bot::brain::gate(&ctx)
    }

    /// A tick context around the rig's parts, for calling `Ctx` methods directly.
    fn with_ctx<R>(&mut self, f: impl FnOnce(&mut Ctx<'_>) -> R) -> R {
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
        f(&mut ctx)
    }

    /// Whether the bot's event log (the web UI's) has a line with `needle`.
    fn said(&self, needle: &str) -> bool {
        self.hub
            .slot(1)
            .is_some_and(|s| s.with(|g| g.log.iter().any(|l| l.text.contains(needle))))
    }
}

fn dist2d(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt()
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn wandering_stays_near_home_on_land_and_sends_movement() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let mut rig = Rig::new(world.clone(), "Darkrunner", 1);
    let home = rig.state.me.pos;
    rig.force(GoalKind::Wander);
    let mut farthest = 0.0f32;
    let has_terrain = world.baked().terrain.is_some();
    for _ in 0..1200 {
        rig.tick(250);
        farthest = farthest.max(dist2d(home, rig.motion.pos()));
        if has_terrain {
            let p = rig.motion.pos();
            let g = world.ground_z(p[0], p[1]).expect("inside the terrain grid");
            assert!(
                g >= 99.0 || (p[2] - 98.5).abs() < 0.01,
                "walked into water at {p:?}"
            );
        }
    }
    assert!(farthest > 8.0, "never left home ({farthest} m)");
    assert!(farthest < 160.0, "strayed {farthest} m");
    assert!(rig.sent(op::CSMoveUnitPacket) > 10);
    assert!(rig.brain.goal().is_some() || rig.brain.goal_name() == "-");
}

/// A weak monster hostile to the Nuian faction.
fn weak_hostile(world: &World, faction: u32) -> u32 {
    world
        .npc_templates()
        .iter()
        .find(|t| {
            t.grade == 1
                && (1..=3).contains(&t.level)
                && world.relation(faction, t.faction_id) == Relation::Hostile
                && world.npc_spawns_anywhere(t.id)
        })
        .map(|t| t.id)
        .expect("a weak hostile monster")
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn grinding_fights_a_monster_in_view_and_loots_it() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let mut rig = Rig::new(world.clone(), "Darkrunner", 2);
    let template = weak_hostile(&world, rig.state.me.faction);
    let level = world.npc(template).unwrap().level;
    let mob = 0x900;
    let me = rig.state.me.pos;
    rig.state.units.insert(
        mob,
        Unit {
            kind: UnitKind::Npc,
            id: template,
            pos: [me[0] + 14.0, me[1], me[2]],
            hp: 100,
            faction: 0,
            level,
            flags: 0,
            target: 0,
            name: None,
        },
    );
    rig.force(GoalKind::Grind);
    // The bot walks into range, faces the monster and attacks.
    for _ in 0..80 {
        rig.tick(250);
        if rig.sent(op::CSStartSkillPacket) > 0 {
            break;
        }
    }
    assert_eq!(
        rig.combat.target(),
        Some(mob),
        "the bot should have engaged"
    );
    assert!(rig.sent(op::CSChangeTargetPacket) >= 1);
    assert!(rig.sent(op::CSStartSkillPacket) >= 1, "no attack was sent");
    assert!(
        dist2d(rig.state.me.pos, rig.state.units[&mob].pos) <= 4.0,
        "a melee archetype should be next to the monster"
    );

    // The monster dies by the bot's hand; the corpse has loot.
    rig.feed(WorldEvent::UnitDeath {
        obj: mob,
        killer: ME,
        respawn_wait_ms: 0,
        lost_exp: 0,
        reason: 0,
    });
    rig.feed(WorldEvent::LootableState {
        owner_obj: mob,
        owner_type: 1,
        has_loot: true,
    });
    for _ in 0..20 {
        rig.tick(250);
        if rig.sent(op::CSLootOpenBagPacket) > 0 {
            break;
        }
    }
    assert_eq!(
        rig.sent(op::CSLootOpenBagPacket),
        1,
        "the corpse was not looted"
    );
    rig.feed(WorldEvent::LootBag {
        owner_obj: mob,
        owner_type: 1,
        items: [LootItem {
            index: 0,
            template: 500,
            count: 1,
        }]
        .into_iter()
        .collect(),
        loot_all: true,
    });
    assert_eq!(rig.sent(op::CSLootItemPacket), 1);
    assert_eq!(rig.combat.kills(), 1);
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn the_automatic_brain_runs_for_half_an_hour_without_trouble() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let mut rig = Rig::new(world, "Reaper", 3);
    let mut seen = BTreeSet::new();
    for _ in 0..(30 * 60 * 4) {
        rig.tick(250);
        seen.insert(rig.brain.goal_name());
        rig.out.clear();
    }
    println!("goals seen: {seen:?}");
    println!(
        "stubs switched off: {:?}",
        GoalKind::ALL
            .iter()
            .filter(|g| rig.brain.is_disabled(**g))
            .map(|g| g.name())
            .collect::<Vec<_>>()
    );
    assert!(
        seen.contains("wander") || seen.contains("grind") || seen.contains("rest"),
        "no implemented goal ever ran: {seen:?}"
    );
    // Every goal is implemented now: none may be switched off as a stub.
    for kind in GoalKind::ALL {
        assert!(
            !rig.brain.is_disabled(kind),
            "{kind:?} was switched off as not implemented"
        );
    }
}

/// The dungeon and sail goals are implemented: the operator can force them (the web UI offers
/// both) on a bot that cannot do them right now (no party partners, no coast, no boat scroll in a
/// rig without GM rights) without harm. They end with a reason of their own, the brain never
/// switches them off as stubs and the bot goes on with the automatic choice.
#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn forcing_a_travel_goal_the_bot_cannot_do_now_does_no_harm() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    for kind in [GoalKind::Dungeon, GoalKind::Sail] {
        let mut rig = Rig::new(world.clone(), "Darkrunner", 11);
        rig.force(kind);
        let mut names = BTreeSet::new();
        for _ in 0..(5 * 60 * 4) {
            rig.tick(250);
            rig.out.clear();
            names.insert(rig.brain.goal_name());
        }
        assert!(
            !rig.brain.is_disabled(kind),
            "{kind:?} was switched off as a stub"
        );
        assert!(!rig.said("not implemented"), "{kind:?}: still a stub");
        println!("{kind:?}: goals seen {names:?}");
    }
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn a_hurt_pack_carrier_does_not_leave_the_trade_run_for_a_rest() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    // a pack that some NPC buys, carried by a bot 150 m from the nearest buyer
    let pack = world
        .pack_crafts()
        .map(|c| c.product.0)
        .find(|&p| {
            world
                .specialty_buyers(p)
                .iter()
                .any(|&b| !world.npc_spawns(b).is_empty())
        })
        .expect("a pack with a placed buyer");
    let buyer = *world
        .specialty_buyers(pack)
        .iter()
        .find(|&&b| !world.npc_spawns(b).is_empty())
        .unwrap();
    let spawn = world.npc_spawns(buyer)[0];
    let mut rig = Rig::new(world.clone(), "Darkrunner", 5);
    let start = [spawn.x + 150.0, spawn.y, spawn.z];
    rig.state.me.pos = start;
    rig.motion.enter_world(ME, start, 0.0, rig.now_ms);
    rig.state.equipment.push(EquipSlot {
        slot: 26,
        template: pack,
        id: 77,
        grade: 0,
    });

    rig.brain.queue(GoalKind::TradeRun, None);
    for _ in 0..8 {
        rig.tick(250);
    }
    assert_eq!(
        rig.brain.goal_name(),
        "trade_run",
        "the run carries the pack"
    );

    // badly hurt: a carrier does not leave the run for a rest in the open (the run itself rests
    // to 80 % before it plans and crafts, and a pack that stands still is no safer on the road)
    rig.state.me.hp = 200;
    for _ in 0..16 {
        rig.tick(250);
        assert_eq!(
            rig.brain.goal_name(),
            "trade_run",
            "a hurt carrier keeps the run"
        );
    }
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn a_trade_run_only_fights_back_against_close_attackers() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    // a bot with a pack, 400 m from the nearest buyer
    let pack = world
        .pack_crafts()
        .map(|c| c.product.0)
        .find(|&p| {
            world
                .specialty_buyers(p)
                .iter()
                .any(|&b| !world.npc_spawns(b).is_empty())
        })
        .expect("a pack with a placed buyer");
    let buyer = *world
        .specialty_buyers(pack)
        .iter()
        .find(|&&b| !world.npc_spawns(b).is_empty())
        .unwrap();
    let spawn = world.npc_spawns(buyer)[0];
    let mut rig = Rig::new(world.clone(), "Darkrunner", 6);
    let start = [spawn.x + 400.0, spawn.y, spawn.z];
    rig.state.me.pos = start;
    rig.motion.enter_world(ME, start, 0.0, rig.now_ms);
    rig.state.equipment.push(EquipSlot {
        slot: 26,
        template: pack,
        id: 77,
        grade: 0,
    });
    rig.brain.queue(GoalKind::TradeRun, None);
    for _ in 0..8 {
        rig.tick(250);
    }
    assert_eq!(rig.brain.goal_name(), "trade_run");
    assert!(rig.motion.is_moving(), "on the way to the buyer");

    // a monster attacks from far behind: the bot keeps going
    let template = weak_hostile(&world, rig.state.me.faction);
    let level = world.npc(template).unwrap().level;
    let mob = 0x900;
    let place = |rig: &mut Rig, dx: f32| {
        let me = rig.state.me.pos;
        rig.state.units.insert(
            mob,
            Unit {
                kind: UnitKind::Npc,
                id: template,
                pos: [me[0] + dx, me[1], me[2]],
                hp: 100,
                faction: 0,
                level,
                flags: 0,
                target: 0,
                name: None,
            },
        );
        let now = rig.now_ms;
        rig.state.threats.clear();
        rig.state.threats.push((mob, now));
    };
    for _ in 0..8 {
        place(&mut rig, 40.0);
        rig.tick(250);
        assert_eq!(rig.combat.target(), None, "no detour for a far attacker");
        assert_eq!(rig.brain.goal_name(), "trade_run");
    }
    assert!(rig.motion.is_moving());

    // a close attacker cannot be outrun: the controller fights back
    place(&mut rig, 8.0);
    rig.tick(250);
    assert_eq!(rig.combat.target(), Some(mob));

    // and lets go of it once it falls far behind
    place(&mut rig, 40.0);
    rig.tick(250);
    assert_eq!(
        rig.combat.target(),
        None,
        "a defence is dropped when the attacker is far"
    );
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn a_carrier_in_a_rest_only_fights_close_attackers() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    // any trade pack: the back slot is what makes the bot a carrier
    let pack = world
        .pack_crafts()
        .map(|c| c.product.0)
        .next()
        .expect("a pack craft");
    let mut rig = Rig::new(world.clone(), "Darkrunner", 7);
    rig.state.equipment.push(EquipSlot {
        slot: 26,
        template: pack,
        id: 77,
        grade: 0,
    });
    rig.brain.queue(GoalKind::Rest, None);
    for _ in 0..4 {
        rig.tick(250);
    }
    assert_eq!(rig.brain.goal_name(), "rest");

    let template = weak_hostile(&world, rig.state.me.faction);
    let mob = 0x901;
    let attack_from = |rig: &mut Rig, dx: f32| {
        place_mob(rig, template, mob, dx);
        let now = rig.now_ms;
        rig.state.threats.clear();
        rig.state.threats.push((mob, now));
    };
    // standing (rest) with a pack: a far attacker is not chased
    attack_from(&mut rig, 40.0);
    rig.tick(250);
    assert_eq!(
        rig.combat.target(),
        None,
        "a carrier does not chase a far attacker"
    );
    // a close one is fought back
    attack_from(&mut rig, 8.0);
    rig.tick(250);
    assert_eq!(rig.combat.target(), Some(mob));
}

/// A monster of `template` in view, `dx` metres east of the bot and not hurt by anything.
fn place_mob(rig: &mut Rig, template: u32, obj: u32, dx: f32) {
    let level = rig.world.npc(template).unwrap().level;
    let me = rig.state.me.pos;
    rig.state.units.insert(
        obj,
        Unit {
            kind: UnitKind::Npc,
            id: template,
            pos: [me[0] + dx, me[1], me[2]],
            hp: 100,
            faction: 0,
            level,
            flags: 0,
            target: 0,
            name: None,
        },
    );
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn a_bot_at_zero_hit_points_is_dead_without_a_death_event_and_never_rests() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let mut rig = Rig::new(world, "Darkrunner", 21);
    rig.force(GoalKind::Wander);
    for _ in 0..8 {
        rig.tick(250);
    }
    assert_eq!(rig.brain.goal_name(), "wander");
    // the server reports the hit points only
    rig.feed(WorldEvent::UnitPoints {
        obj: ME,
        hp: 0,
        mp: 100,
    });
    assert!(rig.state.me.dead);
    assert!(
        std::mem::take(&mut rig.state.reactions.died),
        "the bot task is told to ask for the resurrection"
    );
    assert!(rig.state.me.respawn_at_ms > rig.now_ms);
    for _ in 0..(120 * 4) {
        rig.tick(250);
        assert_eq!(
            rig.brain.goal_name(),
            "-",
            "a dead bot has no goal, least of all a rest"
        );
    }
    // the hit points come back with no resurrection notice
    rig.feed(WorldEvent::UnitPoints {
        obj: ME,
        hp: 100,
        mp: 100,
    });
    assert!(!rig.state.me.dead);
    assert!(rig.state.reactions.resurrected.is_some());
    rig.tick(250);
    rig.tick(250);
    assert_ne!(rig.brain.goal_name(), "-", "alive again, the brain goes on");
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn goals_that_cannot_succeed_are_left_out_of_the_choice() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let both = (1 << GoalKind::TradeRun.index()) | (1 << GoalKind::Housing.index());
    let mut rig = Rig::new(world.clone(), "Reaper", 22);
    assert!(!rig.cfg.bots.gm_bootstrap);
    assert_eq!(
        rig.gate(),
        both,
        "level 1 without GM help: no trade run (level), no housing (design item)"
    );
    // with GM help both can be tried
    rig.cfg.bots.gm_bootstrap = true;
    assert_eq!(rig.gate(), 0);
    rig.cfg.bots.gm_bootstrap = false;

    // a long automatic run never picks them
    let mut seen = BTreeSet::new();
    for _ in 0..(20 * 60 * 4) {
        rig.tick(250);
        seen.insert(rig.brain.goal_name());
        rig.out.clear();
    }
    println!("goals seen: {seen:?}");
    assert!(
        !seen.contains("trade_run") && !seen.contains("housing"),
        "{seen:?}"
    );

    // a bot that carries a pack can always try to sell it (housing stays out)
    let pack = world
        .pack_crafts()
        .map(|c| c.product.0)
        .find(|&p| !world.specialty_buyers(p).is_empty())
        .expect("a pack with a buyer");
    let mut rig = Rig::new(world, "Reaper", 23);
    rig.state.equipment.push(EquipSlot {
        slot: 26,
        template: pack,
        id: 77,
        grade: 0,
    });
    assert_eq!(rig.gate(), 1 << GoalKind::Housing.index());
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn a_monster_nobody_can_hurt_is_given_up_early_and_left_alone_longer_than_a_fight_lasts() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let mut rig = Rig::new(world.clone(), "Darkrunner", 31);
    let template = weak_hostile(&world, rig.state.me.faction);
    let mob = 0x900;
    place_mob(&mut rig, template, mob, 6.0);
    rig.force(GoalKind::Grind);

    // The monster stays at full hit points and never hits back (a wall between, a ledge): the
    // controller gives it up long before the fight timeout.
    let mut engaged_at = None;
    let mut lost_at = None;
    for _ in 0..(3 * 60 * 4) {
        rig.tick(250);
        if engaged_at.is_none() && rig.combat.target() == Some(mob) {
            engaged_at = Some(rig.now_ms);
        }
        if engaged_at.is_some() && lost_at.is_none() && rig.combat.target().is_none() {
            lost_at = Some(rig.now_ms);
            break;
        }
    }
    let (engaged, lost) = (
        engaged_at.expect("the bot engaged the monster"),
        lost_at.expect("the bot gave the monster up"),
    );
    let fought = lost - engaged;
    assert!(
        fought > NO_DAMAGE_MS && fought < NO_DAMAGE_MS + 15_000,
        "gave up after {fought} ms"
    );
    let loss = rig.combat.last_loss().expect("a loss was recorded");
    assert_eq!(loss.reason, LostReason::Stalled);
    assert!(!loss.dealt);
    assert!(loss.unreachable());

    // And the goal does not walk back to it: not within the time a whole fight takes, not
    // within the rest of the 10 minutes it is left alone for.
    assert!(rig.state.avoids(mob, template, rig.now_ms));
    for _ in 0..(((FIGHT_TIMEOUT_MS + 60_000) / 250) as usize) {
        rig.tick(250);
        assert_ne!(
            rig.combat.target(),
            Some(mob),
            "went back to the monster at {} ms after the loss",
            rig.now_ms - lost
        );
    }
    assert!(rig.state.avoids(mob, template, rig.now_ms));
}

/// A bot that stands next to a monster and has cast at it.
fn rig_in_a_fight(world: &Arc<World>, seed: u64) -> (Rig, u32, u32) {
    let mut rig = Rig::new(world.clone(), "Darkrunner", seed);
    let template = weak_hostile(world, rig.state.me.faction);
    let mob = 0x900;
    place_mob(&mut rig, template, mob, 10.0);
    rig.force(GoalKind::Grind);
    for _ in 0..120 {
        rig.tick(250);
        if rig.sent(op::CSStartSkillPacket) > 0 {
            break;
        }
    }
    assert_eq!(rig.combat.target(), Some(mob));
    assert!(rig.sent(op::CSStartSkillPacket) > 0, "no attack was sent");
    (rig, mob, template)
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn a_refused_line_of_sight_makes_the_bot_take_another_position() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let (mut rig, mob, _) = rig_in_a_fight(&world, 32);
    let skill = rig.persona.archetype.starter_skill;
    let before = rig.motion.pos();
    rig.out.clear();
    // 12 = skill_obstacle
    rig.feed(WorldEvent::SkillStarted {
        caster_obj: ME,
        skill,
        tl: 1,
        target_obj: mob,
        result: 12,
    });
    assert_eq!(rig.combat.casts_obstructed(), 1);
    assert_eq!(rig.combat.casts_refused(), 0, "not a failure of the skill");
    // for the next second the bot casts nothing but walks to another place
    for _ in 0..5 {
        rig.tick(250);
    }
    assert_eq!(
        rig.sent(op::CSStartSkillPacket),
        0,
        "no cast while the position is changed"
    );
    let moved = dist2d(before, rig.motion.pos());
    assert!(moved > 0.5, "moved only {moved} m");
    // then the fight goes on from there
    for _ in 0..80 {
        rig.tick(250);
    }
    assert!(
        rig.sent(op::CSStartSkillPacket) > 0,
        "the bot attacks again"
    );
    assert_eq!(rig.combat.target(), Some(mob));
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn a_target_that_keeps_refusing_for_line_of_sight_is_given_up_and_avoided() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let (mut rig, mob, template) = rig_in_a_fight(&world, 33);
    let skill = rig.persona.archetype.starter_skill;
    let mut lost = false;
    for _ in 0..6 {
        rig.feed(WorldEvent::SkillStarted {
            caster_obj: ME,
            skill,
            tl: 1,
            target_obj: mob,
            result: 12,
        });
        // wait out the hold; the target may be given up on the way
        for _ in 0..8 {
            rig.tick(250);
        }
        if rig.combat.target() != Some(mob) {
            lost = true;
            break;
        }
    }
    assert!(lost, "the target was never given up");
    let loss = rig.combat.last_loss().unwrap();
    assert_eq!(loss.reason, LostReason::Blocked);
    assert!(loss.unreachable());
    assert_eq!(rig.combat.casts_refused(), 0);
    assert!(rig.state.avoids(mob, template, rig.now_ms));
    assert_ne!(rig.combat.status(), CombatStatus::Fighting);
}

fn start_duel(rig: &mut Rig, foe: u32) {
    let me = rig.state.me.pos;
    rig.state.units.insert(
        foe,
        Unit {
            kind: UnitKind::Character,
            id: 900,
            pos: [me[0] + 8.0, me[1], me[2]],
            hp: 1000,
            faction: rig.state.me.faction,
            level: 1,
            flags: 0,
            target: 0,
            name: Some("Foe".into()),
        },
    );
    rig.brain.command(&BotCmd::Pair(PairOrder {
        kind: PairKind::Duel,
        role: PairRole::Initiator,
        partner_bot: 2,
        partner_char: 900,
        partner_name: "Foe".into(),
        meet_at: (me[0], me[1]),
    }));
    rig.tick(250);
    assert_eq!(rig.brain.goal_name(), "duel");
    rig.feed_activity(ActivityEvent::DuelStartCountdown);
    rig.feed_activity(ActivityEvent::DuelStarted {
        challenger_obj: ME,
        challenged_obj: foe,
    });
    rig.tick(250);
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn a_duel_nobody_loses_hit_points_in_is_nudged_and_then_left_by_walking_away() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let mut rig = Rig::new(world, "Darkrunner", 41);
    let foe = 0x500;
    start_duel(&mut rig, foe);
    assert_eq!(rig.combat.target(), Some(foe));
    let started = rig.now_ms;
    let me = rig.state.me.pos;
    let flag = [(me[0] + rig.state.units[&foe].pos[0]) / 2.0, me[1], 0.0];

    // nobody is hurt: first the nudge, 45 s into the fight
    let run_to = |rig: &mut Rig, ms: u64| {
        while rig.now_ms < started + ms {
            rig.tick(250);
            rig.out.clear();
        }
    };
    run_to(&mut rig, 44_000);
    assert!(!rig.said("closing in on the opponent"));
    run_to(&mut rig, 47_000);
    assert!(rig.said("closing in on the opponent"), "no nudge");
    assert!(!rig.combat.duel.withdrawing());
    assert_eq!(rig.combat.target(), Some(foe), "still fighting");

    // still nothing: 30 s later the bot leaves
    run_to(&mut rig, 74_000);
    assert!(!rig.combat.duel.withdrawing());
    run_to(&mut rig, 77_000);
    assert!(rig.said("leaving it"), "no walk away");
    assert!(rig.combat.duel.withdrawing());
    assert_ne!(rig.combat.target(), Some(foe), "the fight is dropped");

    // and walks out of the duel area (the server counts 75 m from the flag as a surrender)
    run_to(&mut rig, 77_000 + 60_000);
    let away = dist2d(rig.motion.pos(), flag);
    assert!(away > 75.0, "only {away:.0} m from the flag");
    assert_ne!(rig.combat.target(), Some(foe));
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn a_duel_with_hits_landing_is_left_alone() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let mut rig = Rig::new(world, "Darkrunner", 42);
    let foe = 0x500;
    start_duel(&mut rig, foe);
    let started = rig.now_ms;
    let mut hp = 1000;
    let mut next_hit = started + 20_000;
    while rig.now_ms < started + 200_000 {
        rig.tick(250);
        rig.out.clear();
        if rig.now_ms >= next_hit {
            hp -= 20;
            next_hit += 20_000;
            // the foe loses hit points, then the bot does
            rig.feed(WorldEvent::UnitPoints {
                obj: foe,
                hp,
                mp: 100,
            });
            rig.state.units.get_mut(&foe).unwrap().hp = hp;
            rig.feed(WorldEvent::UnitPoints {
                obj: ME,
                hp: hp + 5,
                mp: 100,
            });
        }
    }
    assert!(!rig.said("closing in"), "nudged a duel that was going on");
    assert!(!rig.combat.duel.withdrawing());
    assert_eq!(rig.combat.target(), Some(foe));
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn the_hub_reports_the_byte_rates() {
    use pb_web::BotHub;
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let rig = Rig::new(world, "Darkrunner", 51);
    rig.hub.count(10, 6_000, 4, 1_000);
    std::thread::sleep(std::time::Duration::from_millis(1_100));
    rig.hub.tick();
    let s = rig.hub.stats();
    assert!(
        (3_000.0..7_000.0).contains(&s.bytes_in_per_sec),
        "{}",
        s.bytes_in_per_sec
    );
    assert!(
        (500.0..1_500.0).contains(&s.bytes_out_per_sec),
        "{}",
        s.bytes_out_per_sec
    );
    assert!(s.packets_in_per_sec > 0.0 && s.packets_out_per_sec > 0.0);
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn a_target_the_server_does_not_know_is_dropped_after_three_refusals() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let (mut rig, mob, template) = rig_in_a_fight(&world, 34);
    let skill = rig.persona.archetype.starter_skill;
    for i in 0..3 {
        // 9 = no_target
        rig.feed(WorldEvent::SkillStarted {
            caster_obj: ME,
            skill,
            tl: 1,
            target_obj: mob,
            result: 9,
        });
        rig.tick(250);
        if i < 2 {
            assert_eq!(
                rig.combat.target(),
                Some(mob),
                "not yet after {} refusals",
                i + 1
            );
        }
    }
    assert_ne!(rig.combat.target(), Some(mob));
    let loss = rig.combat.last_loss().expect("a loss was recorded");
    assert_eq!(loss.reason, LostReason::Gone);
    assert!(
        !rig.state.units.contains_key(&mob),
        "the unit the server does not know is forgotten"
    );
    assert!(rig.state.avoids(mob, template, rig.now_ms));
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn a_defence_against_a_unit_nobody_can_hurt_is_not_taken_up_again() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let mut rig = Rig::new(world.clone(), "Darkrunner", 35);
    let template = weak_hostile(&world, rig.state.me.faction);
    let mob = 0x900;
    place_mob(&mut rig, template, mob, 6.0);
    rig.force(GoalKind::Wander);
    // the monster "targets" the bot all the time and neither side ever loses a hit point
    let threaten = |rig: &mut Rig| {
        let now = rig.now_ms;
        rig.state.threats.clear();
        rig.state.threats.push((mob, now));
    };
    let mut lost_at = None;
    for _ in 0..(3 * 60 * 4) {
        threaten(&mut rig);
        rig.tick(250);
        if rig.combat.last_loss().is_some() {
            lost_at = Some(rig.now_ms);
            break;
        }
    }
    let lost_at = lost_at.expect("the defence was given up");
    assert_eq!(rig.combat.last_loss().unwrap().reason, LostReason::Stalled);
    // it keeps targeting the bot, but the controller does not go back to it for longer than a
    // fight lasts (this is the loop that cost 30 s of every 45 s)
    while rig.now_ms < lost_at + FIGHT_TIMEOUT_MS {
        threaten(&mut rig);
        rig.tick(250);
        assert_ne!(
            rig.combat.target(),
            Some(mob),
            "defended again {} ms after giving up",
            rig.now_ms - lost_at
        );
    }
    // a monster that does hurt the bot is another matter
    rig.feed(WorldEvent::UnitPoints {
        obj: ME,
        hp: 900,
        mp: 400,
    });
    threaten(&mut rig);
    rig.tick(250);
    assert_eq!(rig.combat.target(), Some(mob), "defends when hurt");
}

// ---- GM lines: who they hit -------------------------------------------------------------------

/// The message of a chat packet (`i16 type, i16, i32, str target, str message, ..`).
fn chat_message(o: &Out) -> Option<String> {
    if o.opcode != op::CSSendChatMessagePacket {
        return None;
    }
    let b = &o.body;
    let tlen = u16::from_le_bytes([b[8], b[9]]) as usize;
    let at = 10 + tlen;
    let mlen = u16::from_le_bytes([b[at], b[at + 1]]) as usize;
    Some(String::from_utf8_lossy(&b[at + 2..at + 2 + mlen]).into_owned())
}

/// What precedes the GM line at `wire[i]`: a cleared target, a targeted house, or nothing.
fn pre_of(wire: &[Out], i: usize, line: &str) -> Pre {
    let prev = i.checked_sub(1).map(|j| &wire[j]);
    match prev {
        Some(p) if p.opcode == op::CSChangeTargetPacket && p.body == cs::change_target(0).body => {
            Pre::ClearTarget
        }
        Some(p) if p.opcode == op::CSChangeTargetPacket && line.starts_with("/build") => {
            Pre::HouseTarget
        }
        _ => Pre::None,
    }
}

/// The GM lines of `wire` with what precedes each.
fn gm_wires(wire: &[Out]) -> Vec<GmWire> {
    wire.iter()
        .enumerate()
        .filter_map(|(i, o)| {
            let line = chat_message(o).filter(|m| m.starts_with('/'))?;
            let pre = pre_of(wire, i, &line);
            Some(GmWire { line, pre })
        })
        .collect()
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn every_gm_line_the_goals_write_names_the_bot_and_follows_a_cleared_target() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let mut rig = Rig::new(world.clone(), "Reaper", 61);
    rig.cfg.bots.gm_bootstrap = true;
    rig.cfg.bots.long_travel_teleport_m = 100.0;
    let mut wire: Vec<Out> = Vec::new();
    for goal in [GoalKind::TradeRun, GoalKind::Trade, GoalKind::Housing] {
        rig.force(goal);
        for _ in 0..(4 * 90) {
            rig.tick(250);
            wire.append(&mut rig.out);
        }
    }
    let lines = gm_wires(&wire);
    let seen: BTreeSet<&str> = lines
        .iter()
        .map(|w| w.line.split_whitespace().next().unwrap())
        .collect();
    println!("{} GM lines, commands seen: {seen:?}", lines.len());
    for w in lines.iter().take(12) {
        println!("  {:?} {}", w.pre, w.line);
    }
    assert!(!lines.is_empty(), "the goals wrote no GM line at all");
    assert!(seen.contains("/item"), "{seen:?}");
    for w in &lines {
        assert!(
            w.is_self_only("Tester"),
            "this line could hit somebody else: {w:?}"
        );
    }
    // nothing else on the wire looks like a command
    for o in &wire {
        if let Some(m) = chat_message(o) {
            if m.starts_with('/') {
                continue;
            }
            assert!(!cs::is_command_like(&m), "{m:?}");
        }
    }
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn the_ctx_entry_points_make_only_self_addressed_gm_lines_and_plain_chat() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let mut rig = Rig::new(world, "Reaper", 62);
    // GM commands are off: nothing is sent
    assert!(!rig.with_ctx(|c| c.gm("/level 36")));
    assert!(!rig.with_ctx(|c| c.gm_build(0x123, 3)));
    assert!(rig.out.is_empty());

    rig.cfg.bots.gm_bootstrap = true;
    // /level and friends: the target is cleared first, the bot is named
    assert!(rig.with_ctx(|c| c.gm("/level 36")));
    assert_eq!(rig.out.len(), 2);
    assert_eq!(rig.out[0], cs::change_target(0));
    assert_eq!(
        chat_message(&rig.out[1]).as_deref(),
        Some("/level Tester 36")
    );
    rig.out.clear();
    assert!(rig.with_ctx(|c| c.gm("/labor 500")));
    assert!(rig.with_ctx(|c| c.gm("/move 100.5 200.5 130.0")));
    let lines = gm_wires(&rig.out);
    assert_eq!(
        lines.iter().map(|w| w.line.as_str()).collect::<Vec<_>>(),
        ["/labor Tester 500", "/move Tester 100.5 200.5 130.0"]
    );
    assert!(lines.iter().all(|w| w.pre == Pre::ClearTarget));
    rig.out.clear();
    // the explicit self forms need no clearing
    assert!(rig.with_ctx(|c| c.gm("/item add self 8000 5")));
    assert!(rig.with_ctx(|c| c.gm("/gold add self 2")));
    assert_eq!(rig.out.len(), 2);
    assert!(rig
        .out
        .iter()
        .all(|o| o.opcode == op::CSSendChatMessagePacket));
    rig.out.clear();
    // /build targets the house right in front of the command
    assert!(rig.with_ctx(|c| c.gm_build(0x123, 5)));
    assert_eq!(rig.out.len(), 2);
    assert_eq!(rig.out[0], cs::change_target(0x123));
    assert_eq!(chat_message(&rig.out[1]).as_deref(), Some("/build 5"));
    rig.out.clear();
    assert!(!rig.with_ctx(|c| c.gm_build(0, 5)), "no house, no build");

    // everything that could reach somebody else is refused
    for line in [
        "/level Bob 36",
        "/gold add Bob 5",
        "/gold add target 5",
        "/item add target 1 1",
        "/build 5",
        "/kick Bob",
        "/shutdown",
        "/move tome",
        "\0/level 5",
        "/level 5\n/kill",
    ] {
        assert!(!rig.with_ctx(|c| c.gm(line)), "{line:?} was sent");
    }
    assert!(rig.out.is_empty(), "{:?}", rig.out);

    // a bot whose name cannot be an argument sends no target-or-self command at all
    rig.persona.name = "Two Words".into();
    assert!(!rig.with_ctx(|c| c.gm("/level 36")));
    assert!(rig.with_ctx(|c| c.gm("/item add self 1 1")));
    rig.out.clear();

    // chat: a command never goes out as chat, however it is hidden
    for text in [
        "/gold add self 99999",
        " /gold add self 1",
        "\0/gold add self 1",
        "\0\0/shutdown",
        "\u{200b}/x",
        "\u{ff0f}x",
        "",
        "a\nb",
    ] {
        rig.with_ctx(|c| c.say(text));
        assert!(rig.out.is_empty(), "{text:?} -> {:?}", rig.out);
    }
    rig.with_ctx(|c| c.say("hello there"));
    assert_eq!(rig.out.len(), 1);
    assert_eq!(chat_message(&rig.out[0]).as_deref(), Some("hello there"));
}

// ---- trade requests ---------------------------------------------------------------------------

/// A character `char_id` in view as unit `obj`, asking the bot for a trade.
fn trade_request_from(rig: &mut Rig, obj: u32, char_id: u32) {
    // something to put up, so that a goal that is started does not give up at once
    rig.state.bag.insert(
        30,
        playerbots::bot::state::Item {
            id: 77,
            template: 8001,
            count: 10,
        },
    );
    let me = rig.state.me.pos;
    rig.state.units.insert(
        obj,
        Unit {
            kind: UnitKind::Character,
            id: char_id,
            pos: [me[0] + 3.0, me[1], me[2]],
            hp: 1000,
            faction: rig.state.me.faction,
            level: 1,
            flags: 0,
            target: 0,
            name: Some("Visitor".into()),
        },
    );
    rig.feed_activity(ActivityEvent::CanStartTrade { requester_obj: obj });
}

fn declines(rig: &Rig, obj: u32) -> bool {
    rig.out.iter().any(|o| {
        o.opcode == op::CSCannotStartTradePacket && o.body == cs::cannot_start_trade(obj, 0).body
    })
}

fn accepts(rig: &Rig, obj: u32) -> bool {
    rig.out
        .iter()
        .any(|o| o.opcode == op::CSStartTradePacket && o.body == cs::start_trade(obj).body)
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn a_real_player_cannot_open_a_trade_with_a_bot_unless_the_operator_allows_it() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let foe = 0x600;
    let run = |rig: &mut Rig| {
        for _ in 0..8 {
            rig.tick(250);
        }
    };
    // default: a stranger is declined and never answered with a start
    let mut rig = Rig::new(world.clone(), "Reaper", 63);
    trade_request_from(&mut rig, foe, 9001);
    run(&mut rig);
    assert!(
        declines(&rig, foe),
        "the request of a real player must be declined"
    );
    assert!(!accepts(&rig, foe), "a real player got a trade window");

    // one of our own bots is welcome
    let mut rig = Rig::new(world.clone(), "Reaper", 64);
    rig.hub.mark_bot_char(9002);
    trade_request_from(&mut rig, foe, 9002);
    run(&mut rig);
    assert!(!declines(&rig, foe), "a bot's request is not declined");
    assert!(accepts(&rig, foe), "a bot's request is accepted");

    // the operator may let players in ...
    let mut rig = Rig::new(world.clone(), "Reaper", 65);
    rig.cfg.behavior.trade_with_players = true;
    trade_request_from(&mut rig, foe, 9003);
    run(&mut rig);
    assert!(!declines(&rig, foe));
    assert!(accepts(&rig, foe), "trade_with_players lets a player in");

    // ... but never to a bot whose bag may hold GM-made goods
    let mut rig = Rig::new(world, "Reaper", 66);
    rig.cfg.behavior.trade_with_players = true;
    rig.cfg.bots.gm_bootstrap = true;
    trade_request_from(&mut rig, foe, 9004);
    run(&mut rig);
    assert!(
        declines(&rig, foe),
        "no trade with players while the bot can mint goods"
    );
    assert!(!accepts(&rig, foe));
}

/// Bots walk real navmesh routes around the Nuian start: they arrive, every step is a plausible
/// step, and while a route is followed the bot stays on or beside the mesh.
#[test]
#[ignore = "needs AA_COMPACT, AA_SERVER_DATA and AA_BAKED with nav_main_world.bin"]
fn nav_motion_walks_real_routes_around_the_nuian_start() {
    use aa_world::Vec3;
    use playerbots::bot::motion::{MotionStatus, Pace};

    let Some(world) = world() else { return };
    let Some(mesh) = world.nav() else {
        eprintln!("no nav_main_world.bin in AA_BAKED: skipped");
        return;
    };
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
        name: "Tester".into(),
        ..pb_web::BotSnapshot::default()
    });
    let _rx = slot.attach();
    let start = world.race_start(1).expect("Nuian start");
    let origin = [
        start.x,
        start.y,
        world.ground_z(start.x, start.y).unwrap_or(start.z),
    ];
    let mut rng = fastrand::Rng::with_seed(11);
    let mut out = Vec::new();
    let (mut trips, mut routed, mut jumps, mut off_mesh) = (0u32, 0u32, 0u32, 0u32);
    let (mut walked_sum, mut straight_sum) = (0.0f64, 0.0f64);
    for _ in 0..60 {
        // a goal 25 to 140 m away that has ground and is near the mesh
        let (ang, r) = (rng.f32() * std::f32::consts::TAU, 25.0 + rng.f32() * 115.0);
        let (x, y) = (origin[0] + ang.cos() * r, origin[1] + ang.sin() * r);
        let Some(z) = world.ground_z(x, y).filter(|z| *z > 101.0) else {
            continue;
        };
        if mesh.locate(Vec3::new(x, y, z), 6.0).is_none() {
            continue;
        }
        let mut motion = Motion::new(MotionCfg::default());
        motion.attach_nav(playerbots::bot::nav::link_for_bot(
            &hub,
            &world,
            &cfg,
            slot.clone(),
            1,
        ));
        motion.enter_world(ME, origin, 0.0, 1);
        let before = hub.nav.metrics().snapshot(true);
        motion.go_to(x, y, 1.0, Pace::Run, 1);
        let mut now = 1u64;
        let mut prev = origin;
        let (mut walked, mut samples, mut on) = (0.0f32, 0u32, 0u32);
        let mut status = MotionStatus::Moving;
        for _ in 0..800 {
            now += 250;
            let mut env = MotionEnv {
                ground: &*world,
                now_ms: now,
                out: &mut out,
                rng: &mut rng,
                send_gap_ms: 250,
                combat: false,
            };
            status = motion.tick(&mut env);
            let p = motion.pos();
            let step = dist2d(prev, p);
            walked += step;
            if step > 5.4 * 0.25 * 1.03 {
                jumps += 1;
            }
            samples += 1;
            if mesh.locate(Vec3::new(p[0], p[1], p[2]), 1.5).is_some() {
                on += 1;
            }
            prev = p;
            if status != MotionStatus::Moving {
                break;
            }
        }
        assert_eq!(status, MotionStatus::Arrived, "to ({x:.0}, {y:.0})");
        let after = hub.nav.metrics().snapshot(true);
        trips += 1;
        if after.found > before.found && after.legacy_fallbacks == before.legacy_fallbacks {
            routed += 1;
            walked_sum += f64::from(walked);
            straight_sum += f64::from(dist2d(origin, [x, y, 0.0]) - 1.0);
            // a route is followed: the bot is on the mesh (or the hop to an off-mesh goal)
            off_mesh += samples - on;
            assert!(
                on as f32 >= samples as f32 * 0.9,
                "{on} of {samples} on the mesh"
            );
        }
    }
    let s = hub.nav.metrics().snapshot(true);
    eprintln!(
        "{trips} trips, {routed} on a route, detour {:.3}, {jumps} jumps, {off_mesh} samples off the mesh; {s:?}",
        walked_sum / straight_sum.max(1.0)
    );
    assert!(trips >= 30, "only {trips} trips");
    assert_eq!(jumps, 0);
    assert!(
        routed * 2 >= trips,
        "{routed} of {trips} trips used the mesh"
    );
}
