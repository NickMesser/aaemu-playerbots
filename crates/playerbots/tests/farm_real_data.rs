//! The farm goal against the real `main_world` data and a small fake server (ignored by default).
//!
//! The fake server shows the bot the seed vendor, sells seeds the way the real server does (the
//! merchant must be within 3 m and sell the item, copper is taken, an item task with reason 54
//! brings the goods), plants what `CSCreateDoodad` names (the seed must be the crop's, the spot
//! within 30 m and in the bot's garden, the seed is consumed with reason 14), lets the crop ripen
//! after the growth time of the real data and gives the produce back for a harvest cast (reason
//! 17), so the whole cycle (buy, plant, wait, harvest, replant) runs in a few seconds without a
//! network. Terrain is not loaded: every point is walkable.
//!
//! ```text
//! AA_COMPACT=<.../app/Data/compact.sqlite3> AA_SERVER_DATA=<.../app/Data> \
//!   cargo test -p playerbots --test farm_real_data -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use aa_proto::cs::Out;
use aa_proto::opcodes::cs as op;
use aa_proto::pos::convert_long_to_coord;
use aa_proto::sc::ServerEvent;
use aa_proto::sc_world::{DoodadInfo, ItemChange, ItemTaskInfo, WorldEvent};
use aa_proto::stream::PacketStream;
use aa_world::{Crop, Vec3, World, WorldPaths};
use pb_web::{BoostRequest, PolicyPatch};
use playerbots::archetype::Archetypes;
use playerbots::bot::brain::Brain;
use playerbots::bot::cmd::{BotCmd, GoalChoice, GoalKind};
use playerbots::bot::combat::Combat;
use playerbots::bot::coordinator::CoordHandle;
use playerbots::bot::ctx::{Ctx, Persona};
use playerbots::bot::farm;
use playerbots::bot::goals;
use playerbots::bot::motion::{Motion, MotionCfg, MotionEnv};
use playerbots::bot::state::{BotState, HouseBrief, Unit, UnitKind, SLOT_BAG};
use playerbots::config::Config;
use playerbots::hub::{Hub, HubParams, Logger};
use playerbots::roster::Temperament;
use smallvec::smallvec;

const SEED: u32 = 15659; // Potato Eyes
const POTATO: u32 = 7992; // what they grow into
const ME: u32 = 0x400;
const CHAR: u32 = 500;
const VIEW_M: f32 = 90.0;
const GROWING: u32 = 1;

fn world() -> Option<Arc<World>> {
    static WORLD: OnceLock<Option<Arc<World>>> = OnceLock::new();
    WORLD
        .get_or_init(|| {
            let compact = PathBuf::from(std::env::var_os("AA_COMPACT")?);
            let server_data = PathBuf::from(std::env::var_os("AA_SERVER_DATA")?);
            let paths = WorldPaths {
                compact,
                server_data,
                baked: None,
                world: "main_world".into(),
            };
            Some(Arc::new(World::load(&paths).expect("load main_world")))
        })
        .clone()
}

/// A crop the fake server grows.
struct Planted {
    obj: u32,
    pos: [f32; 3],
    ripe_at: u64,
    ripe: bool,
}

#[derive(Default)]
struct Sim {
    next_obj: u32,
    next_item: u64,
    crops: Vec<Planted>,
    /// What the server refused, for the asserts.
    refused: Vec<String>,
    bought: u32,
    planted: u32,
    harvested: u32,
    /// Harvest casts under way: `(due, crop obj)`.
    casts: Vec<(u64, u32)>,
    farm_lists: u32,
    /// The reasons of the item tasks sent, for the asserts.
    reasons: Vec<u8>,
    /// The lines the bot said (GM commands among them).
    said: Vec<String>,
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
    sim: Sim,
    vendor: (u32, u32, [f32; 3]),
    garden: [f32; 2],
    crop: Crop,
}

impl Rig {
    fn new(world: Arc<World>, gm: bool, money: i64, wish_potatoes: bool) -> Rig {
        let farming = world.farm();
        let crop = farming
            .crop_of_seed(SEED)
            .expect("the potato is a crop")
            .clone();
        let npc = *farming
            .vendors_of(SEED)
            .first()
            .expect("a vendor sells Potato Eyes");
        let spawn = world
            .nearest_npc_spawn(npc, Vec3::new(0.0, 0.0, 0.0))
            .expect("the vendor spawns");
        let mut cfg = Config::default();
        cfg.farm.enabled = true;
        cfg.farm.public_farms = false;
        // a potato takes 600 s at rate 1: at 4 it ripens in 150 s of bot time
        cfg.farm.growth_rate = 4.0;
        cfg.bots.gm_bootstrap = gm;
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let hub = Hub::new(HubParams {
            world: &world,
            baked_dir: None,
            supervisor: tx,
            llm: None,
        });
        if wish_potatoes {
            hub.pop
                .set_policy(PolicyPatch {
                    boosts: Some(vec![BoostRequest {
                        template: POTATO,
                        hours: 0,
                    }]),
                    ..PolicyPatch::default()
                })
                .unwrap();
        }
        let archetypes = Archetypes::builtin().unwrap();
        let archetype = archetypes.get("Darkrunner").unwrap().clone();
        let slot = hub.register(pb_web::BotSnapshot {
            id: 1,
            name: "Farmer".into(),
            ..pb_web::BotSnapshot::default()
        });
        let persona = Persona {
            id: 1,
            name: "Farmer".into(),
            archetype,
            temper: Temperament::NEUTRAL,
            profile: playerbots::roster::Profile::NEUTRAL,
        };
        let mut state = BotState::new();
        state.me.char_id = CHAR;
        state.me.obj = ME;
        state.me.spawned = true;
        state.me.level = 10;
        state.me.hp = 1000;
        state.me.hp_max = 1000;
        state.me.mp = 400;
        state.me.mp_max = 400;
        state.me.labor = 50;
        state.me.money = money;
        state.me.faction = 1;
        state.me.pos = [spawn.x + 80.0, spawn.y, spawn.z];
        state.skills = vec![(persona.archetype.starter_skill, 1)];
        // a house with a garden next to the vendor
        let design = world
            .housing_designs()
            .iter()
            .find(|d| d.standard)
            .expect("a standard house design");
        let garden = [spawn.x + 25.0, spawn.y + 10.0];
        state.houses.push(HouseBrief {
            tl: 1,
            id: 5,
            template: design.design,
            pos: [garden[0], garden[1], spawn.z],
            build_left: 0,
        });
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
            rng: fastrand::Rng::with_seed(7),
            // (a bot farms only when it has been online for a while)
            now_ms: 200_000,
            sim: Sim {
                next_obj: 0x2000,
                next_item: 9_000,
                ..Sim::default()
            },
            vendor: (npc, 0x1001, [spawn.x, spawn.y, spawn.z]),
            garden,
            crop,
            world,
        }
    }

    fn feed(&mut self, ev: WorldEvent) {
        let ev = ServerEvent::World(ev);
        // (the bot loop hands every event to the farm memory first)
        if let ServerEvent::World(w) = &ev {
            farm::observe(&mut self.state, &self.world, w);
        }
        self.state.apply(&ev);
    }

    fn ctx_score(&mut self) -> f32 {
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
        goals::farm::score(&ctx, 0.0)
    }

    fn goal_is_farm(&self) -> bool {
        self.brain
            .goal()
            .is_some_and(|g| g.kind() == GoalKind::Farm)
    }

    fn force_farm(&mut self) {
        self.brain
            .command(&BotCmd::Goal(GoalChoice::Forced(GoalKind::Farm)));
    }

    fn tick(&mut self, dt: u64) {
        self.now_ms += dt;
        self.state.now_ms = self.now_ms;
        self.state.me.pos = self.motion.pos();
        self.refresh_view();
        self.progress_crops();
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
            combat: false,
        };
        self.motion.tick(&mut env);
        self.state.me.pos = self.motion.pos();
        self.serve();
    }

    /// Runs ticks until the Farm goal that is queued or running has ended (at most `limit_ms`).
    fn run_farm_goal(&mut self, limit_ms: u64) {
        self.force_farm();
        let end = self.now_ms + limit_ms;
        let mut started = false;
        while self.now_ms < end {
            self.tick(250);
            if self.goal_is_farm() {
                started = true;
            } else if started {
                return;
            }
        }
        panic!("the farm goal did not end within {limit_ms} ms");
    }

    fn refresh_view(&mut self) {
        let me = self.state.me.pos;
        let (_, obj, pos) = self.vendor;
        let d = ((pos[0] - me[0]).powi(2) + (pos[1] - me[1]).powi(2)).sqrt();
        if d <= VIEW_M {
            let npc = self.vendor.0;
            self.state.units.entry(obj).or_insert_with(|| Unit {
                kind: UnitKind::Npc,
                id: npc,
                pos,
                hp: 100,
                faction: 0,
                level: 1,
                flags: 0,
                target: 0,
                name: None,
            });
        } else {
            self.state.units.remove(&obj);
        }
    }

    /// Ripens the crops whose time has come and finishes the harvest casts.
    fn progress_crops(&mut self) {
        let now = self.now_ms;
        let ripe_phase = self.crop.ripe[0].0;
        let ripening: Vec<u32> = self
            .sim
            .crops
            .iter()
            .filter(|c| !c.ripe && c.ripe_at <= now)
            .map(|c| c.obj)
            .collect();
        for obj in ripening {
            for c in self.sim.crops.iter_mut().filter(|c| c.obj == obj) {
                c.ripe = true;
            }
            self.feed(WorldEvent::DoodadPhase {
                obj,
                phase: ripe_phase,
            });
        }
        let due: Vec<u32> = self
            .sim
            .casts
            .iter()
            .filter(|c| c.0 <= now)
            .map(|c| c.1)
            .collect();
        self.sim.casts.retain(|c| c.0 > now);
        for obj in due {
            let Some(i) = self.sim.crops.iter().position(|c| c.obj == obj) else {
                continue;
            };
            self.sim.crops.remove(i);
            self.sim.harvested += 1;
            // the harvest: the crop's produce (the average count), reason 17
            let produce: Vec<(u32, i32)> = self
                .crop
                .produce
                .iter()
                .map(|y| (y.item, i32::from(y.min.max(1))))
                .collect();
            for (item, n) in produce {
                self.give(17, item, n);
            }
            self.feed(WorldEvent::DoodadPhase {
                obj,
                phase: GROWING,
            });
            self.feed(WorldEvent::DoodadsRemoved(smallvec![obj]));
        }
    }

    // ---- the fake server's bag ----------------------------------------------------------

    fn task(&mut self, reason: u8, changes: Vec<ItemChange>) {
        self.sim.reasons.push(reason);
        self.feed(WorldEvent::ItemTask(Box::new(ItemTaskInfo {
            reason,
            changes: changes.into_iter().collect(),
            force_removed: Default::default(),
        })));
    }

    fn give(&mut self, reason: u8, template: u32, count: i32) {
        let existing = self
            .state
            .bag
            .iter()
            .find(|(_, i)| i.template == template && i.count > 0)
            .map(|(&slot, i)| (slot, i.id));
        let change = match existing {
            Some((slot, id)) => ItemChange::CountUpdate {
                slot_type: SLOT_BAG,
                slot: slot as u8,
                id,
                delta: count,
                template,
            },
            None => {
                let slot = (0u16..).find(|s| !self.state.bag.contains_key(s)).unwrap() as u8;
                self.sim.next_item += 1;
                ItemChange::Added {
                    action: 5,
                    slot_type: SLOT_BAG,
                    slot,
                    template,
                    id: self.sim.next_item,
                    grade: 0,
                    count,
                }
            }
        };
        self.task(reason, vec![change]);
    }

    fn take(&mut self, reason: u8, template: u32, count: i32, extra: Vec<ItemChange>) {
        let (slot, id) = self
            .state
            .find_item(template)
            .map(|(slot, i)| (slot, i.id))
            .expect("the item is in the bag");
        let mut changes = vec![ItemChange::CountUpdate {
            slot_type: SLOT_BAG,
            slot: slot as u8,
            id,
            delta: -count,
            template,
        }];
        changes.extend(extra);
        self.task(reason, changes);
    }

    // ---- the fake server's packets ------------------------------------------------------

    fn serve(&mut self) {
        let out = std::mem::take(&mut self.out);
        for o in out {
            let mut s = PacketStream::from_bytes(o.body.clone());
            match o.opcode {
                op::CSBuyItemsPacket => self.on_buy(&mut s),
                op::CSCreateDoodadPacket => self.on_plant(&mut s),
                op::CSStartSkillPacket => self.on_skill(&mut s),
                op::CSSendChatMessagePacket => {
                    let _kind = s.read_i16().unwrap();
                    let _ = (s.read_i16().unwrap(), s.read_i32().unwrap());
                    let _target = s.read_string().unwrap();
                    let text = s.read_string().unwrap();
                    self.sim.said.push(text);
                }
                op::CSRequestCommonFarmList => {
                    self.sim.farm_lists += 1;
                    self.feed(WorldEvent::CommonFarmList(Vec::new().into_boxed_slice()));
                }
                _ => {}
            }
        }
    }

    fn on_buy(&mut self, s: &mut PacketStream) {
        let npc = s.read_bc().unwrap();
        let _doodad = s.read_bc().unwrap();
        let _unk = s.read_u32().unwrap();
        let n = s.read_u8().unwrap();
        let _back = s.read_u8().unwrap();
        let Some(unit) = self.state.units.get(&npc).cloned() else {
            self.sim.refused.push(format!("buy: no merchant {npc}"));
            return;
        };
        let me = self.state.me.pos;
        let d = ((unit.pos[0] - me[0]).powi(2) + (unit.pos[1] - me[1]).powi(2)).sqrt();
        if d > 3.0 {
            self.sim
                .refused
                .push(format!("buy: merchant {d:.1} m away"));
            return;
        }
        let mut total = 0i64;
        let mut goods = Vec::new();
        for _ in 0..n {
            let (item, _grade, count, _cur) = (
                s.read_u32().unwrap(),
                s.read_u8().unwrap(),
                s.read_i32().unwrap(),
                s.read_u8().unwrap(),
            );
            if !self.world.farm().vendors_of(item).contains(&unit.id) {
                self.sim
                    .refused
                    .push(format!("buy: the merchant sells no {item}"));
                continue;
            }
            let price = self.world.item_facts(item).map_or(0, |f| f.price);
            total += i64::from(price) * i64::from(count);
            goods.push((item, count));
        }
        if total > self.state.me.money {
            self.sim
                .refused
                .push(format!("buy: {total} copper is too much"));
            return;
        }
        for (item, count) in goods {
            self.sim.bought += count as u32;
            self.give(54, item, count);
        }
        self.task(54, vec![ItemChange::Money(-(total as i32))]);
    }

    fn on_plant(&mut self, s: &mut PacketStream) {
        let doodad = s.read_u32().unwrap();
        let x = convert_long_to_coord(s.read_i64().unwrap());
        let y = convert_long_to_coord(s.read_i64().unwrap());
        let z = s.read_f32().unwrap();
        let _rot = s.read_f32().unwrap();
        let _scale = s.read_f32().unwrap();
        let item_id = s.read_u64().unwrap();
        let Some((slot, item)) = self
            .state
            .bag
            .iter()
            .find(|(_, i)| i.id == item_id)
            .map(|(&slot, i)| (slot, *i))
        else {
            self.sim
                .refused
                .push("plant: the item is not in the bag".into());
            return;
        };
        let Some(crop) = self.world.farm().crop_of_doodad(doodad) else {
            self.sim
                .refused
                .push(format!("plant: doodad {doodad} is no crop"));
            return;
        };
        if crop.seed != item.template {
            self.sim
                .refused
                .push("plant: the item is not the seed of the doodad".into());
            return;
        }
        let me = self.state.me.pos;
        let range = ((x - me[0]).powi(2) + (y - me[1]).powi(2) + (z - me[2]).powi(2)).sqrt();
        if range > 30.0 {
            self.sim
                .refused
                .push(format!("plant: {range:.1} m is too far"));
            return;
        }
        // the garden is a square of side 2 * garden radius around the house
        let radius = self
            .world
            .housing_design(self.state.houses[0].template)
            .map_or(4.0, goals::housing::garden_radius);
        if (x - self.garden[0]).abs() >= radius || (y - self.garden[1]).abs() >= radius {
            self.sim
                .refused
                .push(format!("plant: ({x:.0}, {y:.0}) is outside the garden"));
            return;
        }
        let spacing = f32::from(crop.radius_mm) / 1000.0 * 2.0;
        if self
            .sim
            .crops
            .iter()
            .any(|c| ((c.pos[0] - x).powi(2) + (c.pos[1] - y).powi(2)).sqrt() < spacing.max(0.2))
        {
            self.sim
                .refused
                .push("plant: too close to another crop".into());
            return;
        }
        let grow = crop.grow_ms_at(self.cfg.farm.growth_rate);
        self.take(14, item.template, 1, Vec::new());
        let _ = slot;
        self.sim.next_obj += 1;
        let obj = self.sim.next_obj;
        self.sim.crops.push(Planted {
            obj,
            pos: [x, y, z],
            ripe_at: self.now_ms + grow,
            ripe: false,
        });
        self.sim.planted += 1;
        self.feed(WorldEvent::DoodadsCreated(
            [DoodadInfo {
                obj,
                template: doodad,
                pos: (x, y, z),
                owner_obj: 0,
                parent_obj: 0,
                phase: GROWING,
                owner_id: CHAR,
                quest_glow: 0,
                has_loot: false,
            }]
            .into_iter()
            .collect(),
        ));
    }

    fn on_skill(&mut self, s: &mut PacketStream) {
        let skill = s.read_u32().unwrap();
        let _caster_type = s.read_u8().unwrap();
        let _caster = s.read_bc().unwrap();
        let target_type = s.read_u8().unwrap();
        if target_type != 4 {
            return;
        }
        let obj = s.read_bc().unwrap();
        let Some(c) = self.sim.crops.iter().find(|c| c.obj == obj) else {
            self.sim.refused.push(format!("harvest: no crop {obj}"));
            return;
        };
        if !c.ripe {
            self.sim
                .refused
                .push("harvest: the crop is not ripe".into());
            return;
        }
        if self.crop.harvest_skill(self.crop.ripe[0].0) != Some(skill) {
            self.sim
                .refused
                .push(format!("harvest: skill {skill} does not harvest it"));
            return;
        }
        let me = self.state.me.pos;
        let d = ((c.pos[0] - me[0]).powi(2) + (c.pos[1] - me[1]).powi(2)).sqrt();
        if d > 5.0 {
            self.sim
                .refused
                .push(format!("harvest: the crop is {d:.1} m away"));
            return;
        }
        let cast = self
            .world
            .skill(skill)
            .map_or(1_000, |k| u64::from(k.casting_time));
        self.sim.casts.push((self.now_ms + cast.max(500), obj));
    }
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn a_bot_buys_seeds_plants_waits_harvests_and_replants() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let mut rig = Rig::new(world, false, 100_000, true);
    let potato = POTATO;
    // nothing is due, there is room and money: the goal wants to run
    let score = rig.ctx_score();
    assert!(
        score > 0.0,
        "a bot with money, a garden and a vendor wants to farm: {score}"
    );

    // 1. buy, walk to the garden, plant
    rig.run_farm_goal(10 * 60_000);
    assert!(rig.sim.refused.is_empty(), "{:?}", rig.sim.refused);
    assert!(rig.sim.bought >= 1, "seeds were bought");
    assert!(rig.sim.planted >= 1, "seeds were planted");
    assert_eq!(rig.sim.planted as usize, rig.sim.crops.len());
    let mem = rig.state.market.farm().expect("the bot has a farm memory");
    assert_eq!(mem.plots.len(), rig.sim.crops.len());
    assert!(mem.plots.iter().all(|p| p.honest && p.obj != 0));
    assert!(
        mem.plots.iter().all(|p| p.seed == SEED),
        "the wish picked the potato"
    );
    assert_eq!(mem.planted, rig.sim.planted);
    assert!(rig.state.me.money < 100_000, "the seeds cost money");
    eprintln!(
        "bought {} planted {} money {}",
        rig.sim.bought, rig.sim.planted, rig.state.me.money
    );
    // what is growing is on the board
    assert!(
        rig.hub.farms.units(potato) > 0.0,
        "the board knows what grows"
    );
    // the goal does not run again at once ...
    assert_eq!(
        rig.ctx_score(),
        0.0,
        "nothing is due and the bot just farmed"
    );

    // 2. the crops ripen; the goal wants to harvest
    let wait = rig.crop.grow_ms_at(rig.cfg.farm.growth_rate) + 5_000;
    let until = rig.now_ms + wait;
    while rig.now_ms < until {
        rig.tick(250);
    }
    assert!(rig.sim.crops.iter().all(|c| c.ripe));
    assert!(rig.state.market.farm().unwrap().due(rig.now_ms) >= 1);
    let score = rig.ctx_score();
    assert!(score > 0.5, "a due plot is worth a trip: {score}");

    // 3. harvest and replant
    let before = rig.sim.planted;
    rig.run_farm_goal(10 * 60_000);
    assert!(rig.sim.refused.is_empty(), "{:?}", rig.sim.refused);
    assert!(rig.sim.harvested >= 1, "a crop was harvested");
    assert!(rig.state.item_count(potato) >= 1, "the bag holds potatoes");
    assert!(
        rig.state.market.clean_count(potato) >= 1,
        "potatoes of bought seeds are the bot's own to sell"
    );
    assert!(rig.sim.reasons.contains(&17));
    assert!(
        rig.sim.planted > before,
        "the seeds of the harvest went into the ground again"
    );
    let mem = rig.state.market.farm().unwrap();
    assert!(mem.harvested >= 1);
    assert_eq!(mem.harvest_honest, None, "the harvest window is closed");
    eprintln!(
        "harvested {} potatoes {} (clean {}), replanted {}",
        rig.sim.harvested,
        rig.state.item_count(potato),
        rig.state.market.clean_count(potato),
        rig.sim.planted - before
    );
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn a_wish_makes_the_goal_worth_more_and_a_bot_without_money_or_a_vendor_does_not_farm() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let wished = Rig::new(world.clone(), false, 100_000, true).ctx_score();
    let calm = Rig::new(world.clone(), false, 100_000, false).ctx_score();
    eprintln!("score with a wish {wished:.3}, without {calm:.3}");
    assert!(calm > 0.0 && wished > calm, "{wished} vs {calm}");
    // no money and no GM: no seeds to buy, nothing to plant
    assert_eq!(Rig::new(world.clone(), false, 0, true).ctx_score(), 0.0);
    // with a GM the bot can mint seeds
    assert!(Rig::new(world.clone(), true, 0, true).ctx_score() > 0.0);
    // the switch is off by default
    let mut off = Rig::new(world, false, 100_000, true);
    off.cfg.farm.enabled = false;
    assert_eq!(off.ctx_score(), 0.0);
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn minted_seeds_never_make_clean_produce() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    // no money: the seeds can only come from a GM command (the fake server does not know
    // `/item add`, so the test adds them by hand when the bot asks)
    let mut rig = Rig::new(world, true, 0, true);
    let potato = POTATO;
    rig.force_farm();
    let end = rig.now_ms + 10 * 60_000;
    let mut minted = false;
    while rig.now_ms < end && rig.sim.planted == 0 {
        rig.tick(250);
        // the GM line left the bot: answer it like the server would
        if !minted && rig.log_has("/item add self") {
            minted = true;
            rig.give(11, SEED, 3);
        }
    }
    assert!(minted, "the bot asked for GM seeds");
    assert!(rig.sim.planted >= 1);
    let mem = rig.state.market.farm().unwrap();
    assert!(
        mem.plots.iter().all(|p| !p.honest),
        "minted seeds make dishonest plots"
    );
    // wait for the crops, then harvest
    let wait = rig.crop.grow_ms_at(rig.cfg.farm.growth_rate) + 5_000;
    let until = rig.now_ms + wait;
    while rig.now_ms < until {
        rig.tick(250);
    }
    rig.run_farm_goal(10 * 60_000);
    assert!(rig.sim.harvested >= 1);
    assert!(rig.state.item_count(potato) >= 1);
    assert_eq!(
        rig.state.market.clean_count(potato),
        0,
        "produce of minted seeds is never the bot's own to sell"
    );
    assert_eq!(rig.state.market.clean_count(SEED), 0);
}

impl Rig {
    /// Whether the bot has sent a GM line that starts with `prefix` (the log keeps them at debug
    /// level, so the test looks at what `Ctx::gm` wrote to the chat packets).
    fn log_has(&self, prefix: &str) -> bool {
        self.sim.said.iter().any(|l| l.contains(prefix))
    }
}
