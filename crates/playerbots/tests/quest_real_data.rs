//! The quest goal against the real `main_world` data and a small fake server (ignored by default).
//!
//! The fake server shows the bot the NPCs and doodads around it, answers the quest packets the
//! way the real server does (accept, talk, kill and use credit, turn-in) and records what the bot
//! did, so the whole quest flow can be run in a few seconds without a network.
//!
//! ```text
//! AA_COMPACT=<.../app/Data/compact.sqlite3> AA_SERVER_DATA=<.../app/Data> [AA_BAKED=<data dir>] \
//!   cargo test -p playerbots --test quest_real_data -- --ignored --nocapture
//! ```

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use aa_proto::cs::Out;
use aa_proto::opcodes::cs as op;
use aa_proto::sc::ServerEvent;
use aa_proto::sc_activity::{quest_status, ActivityEvent, QuestRow};
use aa_proto::sc_world::WorldEvent;
use aa_proto::stream::PacketStream;
use aa_world::{DoodadFlags, ObjKind, QuestReport, QuestStart, Vec3, World, WorldPaths};
use playerbots::archetype::Archetypes;
use playerbots::bot::brain::Brain;
use playerbots::bot::cmd::{BotCmd, GoalChoice, GoalKind};
use playerbots::bot::combat::Combat;
use playerbots::bot::coordinator::CoordHandle;
use playerbots::bot::ctx::{Ctx, Persona};
use playerbots::bot::motion::{Motion, MotionCfg, MotionEnv};
use playerbots::bot::state::{BotState, Doodad, Item, Unit, UnitKind};
use playerbots::config::Config;
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
const VIEW_M: f32 = 90.0;

/// One bot without a network connection, plus the fake server around it.
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
}

#[derive(Default)]
struct Sim {
    objs: HashMap<(u8, u32, i32, i32), u32>,
    next_obj: u32,
    /// Dead monsters and when they come back.
    dead: Vec<(u32, u64)>,
    /// Casts a monster has taken.
    hits: HashMap<u32, u8>,
    active: BTreeMap<u32, QuestRow>,
    completed: Vec<u32>,
    next_row: i64,
    /// What the server refused, for the asserts.
    refused: Vec<String>,
    started: Vec<u32>,
    /// Casts of an item, by item template.
    item_casts: HashMap<u32, u32>,
    /// `(item template, target NPC template)` of every item cast (0: not an NPC).
    item_targets: Vec<(u32, u32)>,
    /// The server takes item casts and does nothing with them: no credit, the item stays.
    deaf_items: bool,
    /// The server credits an item cast this long after it (0 = at once), like its quest engine.
    item_delay_ms: u64,
    /// Credits still to come: `(due bot clock, item template)`.
    item_pending: Vec<(u64, u32)>,
    /// Item casts credited, by item template.
    item_credited: HashMap<u32, u32>,
    /// How the experience of the character grows.
    xp: Xp,
    /// Monsters the bot killed.
    kills: u32,
}

/// The experience model of the fake server.
#[derive(Default, Clone, Copy)]
enum Xp {
    /// Two quests a level up to level 10 (the first quests of a start area).
    #[default]
    Slow,
    /// A quest turned in brings the bot to the level of the quest (or a level per three quests
    /// done), and every twelve kills give a level, up to `cap`.
    FollowsQuests { cap: u8 },
}

impl Rig {
    fn new(world: Arc<World>, race: u8, seed: u64) -> Rig {
        let mut cfg = Config::default();
        if let Some(p) = std::env::var_os("AA_COMPACT") {
            cfg.data.compact = PathBuf::from(p);
        }
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let hub = Hub::new(HubParams {
            world: &world,
            baked_dir: None,
            supervisor: tx,
            llm: None,
        });
        let archetypes = Archetypes::builtin().unwrap();
        let archetype = archetypes.get("Darkrunner").unwrap().clone();
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
        let start = world.race_start(race).expect("race start");
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
        state.me.race = race;
        state.me.faction = world.race_faction(race).expect("race faction");
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
            sim: Sim {
                next_obj: 0x2000,
                next_row: 1,
                ..Sim::default()
            },
            world,
        }
    }

    fn tick(&mut self, dt: u64) {
        self.now_ms += dt;
        self.state.now_ms = self.now_ms;
        self.state.me.pos = self.motion.pos();
        self.refresh_view();
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
        self.credit_due_items();
        self.credit_spheres();
        self.serve();
    }

    /// Credits the sphere objectives the bot stands in.
    fn credit_spheres(&mut self) {
        let me = self.state.me.pos;
        let mut hits: Vec<(u32, usize)> = Vec::new();
        for (&q, row) in &self.sim.active {
            let Some(quest) = self.world.quest(q) else {
                continue;
            };
            let mut idx = 0usize;
            for step in quest.progress.iter() {
                for o in step.objectives.iter() {
                    if let ObjKind::Sphere { .. } = o.kind {
                        if idx < 5 && row.objectives[idx] < 1 {
                            if let Some(s) = self.world.quest_sphere(q, o.component) {
                                let d = ((s.x - me[0]).powi(2) + (s.y - me[1]).powi(2)).sqrt();
                                if d <= s.radius {
                                    hits.push((q, idx));
                                }
                            }
                        }
                    }
                    idx += 1;
                }
            }
        }
        for (q, idx) in hits {
            let Some(row) = self.sim.active.get_mut(&q) else {
                continue;
            };
            row.objectives[idx] = 1;
            let row = *row;
            self.feed_activity(ActivityEvent::QuestUpdated {
                quest: Box::new(row),
                component: 0,
                p: [0; 4],
            });
            self.update_status(q);
        }
    }

    /// Credits the item casts whose delay has passed.
    fn credit_due_items(&mut self) {
        let now = self.now_ms;
        let due: Vec<u32> = self
            .sim
            .item_pending
            .iter()
            .filter(|p| p.0 <= now)
            .map(|p| p.1)
            .collect();
        self.sim.item_pending.retain(|p| p.0 > now);
        for template in due {
            *self.sim.item_credited.entry(template).or_default() += 1;
            self.credit(
                |k| matches!(k, ObjKind::UseItem { item, .. } if *item == template),
                1,
                None,
            );
        }
    }

    fn feed_world(&mut self, ev: WorldEvent) {
        let ev = ServerEvent::World(ev);
        self.state.apply(&ev);
        self.combat
            .on_event(&ev, self.state.me.obj, self.now_ms, &mut self.out);
    }

    fn feed_activity(&mut self, ev: ActivityEvent) {
        self.state.apply(&ServerEvent::Activity(ev));
    }

    fn obj_for(&mut self, key: (u8, u32, i32, i32)) -> u32 {
        let next = &mut self.sim.next_obj;
        *self.sim.objs.entry(key).or_insert_with(|| {
            *next += 1;
            *next
        })
    }

    /// Shows the bot the NPCs and doodads within [`VIEW_M`] and hides the others.
    fn refresh_view(&mut self) {
        let me = self.state.me.pos;
        let center = Vec3::new(me[0], me[1], me[2]);
        let mut want_units: Vec<(u32, u32, [f32; 3])> = Vec::new();
        let mut want_doodads: Vec<(u32, u32, [f32; 3])> = Vec::new();
        let world = self.world.clone();
        world.npc_spawns_within(center, VIEW_M, |t, p| {
            want_units.push((t, 0, [p.x, p.y, p.z]));
        });
        world.doodad_spawns_within(center, VIEW_M, |t, p| {
            if world
                .doodad(t)
                .is_some_and(|d| d.flags != DoodadFlags::NONE)
            {
                want_doodads.push((t, 0, [p.x, p.y, p.z]));
            }
        });
        let mut keep_units = Vec::new();
        for (t, _, p) in want_units {
            let key = (0u8, t, p[0] as i32, p[1] as i32);
            let obj = self.obj_for(key);
            keep_units.push(obj);
            if self.sim.dead.iter().any(|d| d.0 == obj) {
                continue;
            }
            self.state.units.entry(obj).or_insert_with(|| Unit {
                kind: UnitKind::Npc,
                id: t,
                pos: p,
                hp: 100,
                faction: 0,
                level: world.npc(t).map_or(1, |n| n.level),
                flags: 0,
                target: 0,
                name: None,
            });
        }
        let gone: Vec<u32> = self
            .state
            .units
            .keys()
            .copied()
            .filter(|o| !keep_units.contains(o))
            .collect();
        for o in gone {
            self.state.units.remove(&o);
        }
        let mut keep_doodads = Vec::new();
        for (t, _, p) in want_doodads {
            let key = (1u8, t, p[0] as i32, p[1] as i32);
            let obj = self.obj_for(key);
            keep_doodads.push(obj);
            self.state.doodads.entry(obj).or_insert(Doodad {
                template: t,
                pos: p,
                phase: 0,
            });
        }
        self.state.doodads.retain(|o, _| keep_doodads.contains(o));
        // respawns
        let now = self.now_ms;
        let back: Vec<u32> = self
            .sim
            .dead
            .iter()
            .filter(|d| now >= d.1)
            .map(|d| d.0)
            .collect();
        self.sim.dead.retain(|d| now < d.1);
        for o in back {
            self.state.units.remove(&o);
            self.sim.hits.remove(&o);
        }
    }

    // ---- the fake server ----------------------------------------------------------------

    fn serve(&mut self) {
        let out = std::mem::take(&mut self.out);
        for o in out {
            let mut s = PacketStream::from_bytes(o.body.clone());
            match o.opcode {
                op::CSStartQuestContextPacket => {
                    let (q, npc, doodad, _sphere) = (
                        s.read_u32().unwrap(),
                        s.read_bc().unwrap(),
                        s.read_bc().unwrap(),
                        s.read_u32().unwrap(),
                    );
                    self.on_start_quest(q, npc, doodad);
                }
                op::CSQuestTalkMadePacket => {
                    let npc = s.read_bc().unwrap();
                    let _quest = s.read_u32().unwrap();
                    if let Some(t) = self.state.units.get(&npc).map(|u| u.id) {
                        self.credit(
                            |k| matches!(k, ObjKind::Talk { npc: n } if *n == t),
                            1,
                            None,
                        );
                    }
                }
                op::CSCompleteQuestContextPacket => {
                    let q = s.read_u32().unwrap();
                    let npc = s.read_bc().unwrap();
                    let doodad = s.read_bc().unwrap();
                    self.on_complete(q, npc, doodad);
                }
                op::CSDropQuestContextPacket => {
                    let q = s.read_u32().unwrap();
                    if let Some(mut row) = self.sim.active.remove(&q) {
                        row.status = quest_status::DROPPED;
                        self.feed_activity(ActivityEvent::QuestUpdated {
                            quest: Box::new(row),
                            component: 0,
                            p: [0; 4],
                        });
                    }
                }
                op::CSStartSkillPacket => self.on_skill(&o),
                op::CSLootOpenBagPacket => {}
                _ => {}
            }
        }
    }

    fn on_start_quest(&mut self, q: u32, npc: u32, doodad: u32) {
        let Some(quest) = self.world.quest(q).cloned() else {
            self.sim.refused.push(format!("start {q}: unknown quest"));
            return;
        };
        if self.sim.active.contains_key(&q) || self.sim.completed.contains(&q) {
            self.sim
                .refused
                .push(format!("start {q}: already have/done"));
            return;
        }
        let ok = if npc != 0 {
            self.state.units.get(&npc).is_some_and(|u| {
                quest
                    .starts
                    .iter()
                    .any(|s| matches!(s, QuestStart::Npc(n) if *n == u.id))
            })
        } else {
            self.state.doodads.get(&doodad).is_some_and(|d| {
                quest
                    .starts
                    .iter()
                    .any(|s| matches!(s, QuestStart::Doodad(n) if *n == d.template))
            })
        };
        if !ok {
            self.sim
                .refused
                .push(format!("start {q}: giver {npc}/{doodad} does not start it"));
            return;
        }
        let row = QuestRow {
            id: self.sim.next_row,
            template: q,
            status: quest_status::PROGRESS,
            objectives: [0; 5],
            left_time_ms: -1,
            component: 0,
            acceptor_type: 0,
            acceptor_id: 0,
        };
        self.sim.next_row += 1;
        self.sim.active.insert(q, row);
        self.sim.started.push(q);
        for &(item, count) in quest.supplies.iter() {
            self.give_item(item, count as i32);
        }
        self.feed_activity(ActivityEvent::QuestStarted {
            quest: Box::new(row),
            component: 0,
        });
        // the server counts an item the quest hands over towards a gather objective
        for &(item, count) in quest.supplies.iter() {
            self.credit(
                |k| matches!(k, ObjKind::Gather { item: i, .. } if *i == item),
                count as i32,
                None,
            );
        }
        self.update_status(q);
    }

    /// Adds `amount` to the counter of every active objective `matches` accepts.
    fn credit(&mut self, matches: impl Fn(&ObjKind) -> bool, amount: i32, drop_item: Option<u32>) {
        let ids: Vec<u32> = self.sim.active.keys().copied().collect();
        for q in ids {
            let Some(quest) = self.world.quest(q).cloned() else {
                continue;
            };
            let mut changed = false;
            let mut idx = 0usize;
            for step in quest.progress.iter() {
                for o in step.objectives.iter() {
                    if idx < 5 && matches(&o.kind) {
                        let need = playerbots_need(&o.kind);
                        let row = self.sim.active.get_mut(&q).unwrap();
                        if row.objectives[idx] < need {
                            row.objectives[idx] = (row.objectives[idx] + amount).min(need);
                            changed = true;
                        }
                    }
                    idx += 1;
                }
            }
            if changed {
                let row = self.sim.active[&q];
                self.feed_activity(ActivityEvent::QuestUpdated {
                    quest: Box::new(row),
                    component: 0,
                    p: [0; 4],
                });
                self.update_status(q);
            }
        }
        if let Some(item) = drop_item {
            self.give_item(item, 1);
        }
    }

    fn give_item(&mut self, item: u32, count: i32) {
        let slot = (0..50u16)
            .find(|s| !self.state.bag.contains_key(s))
            .unwrap_or(0);
        self.state.bag.insert(
            slot,
            Item {
                id: 9000 + u64::from(slot),
                template: item,
                count,
            },
        );
    }

    /// Moves a quest whose objectives are all met to ready (and completes automatic ones).
    fn update_status(&mut self, q: u32) {
        let Some(quest) = self.world.quest(q).cloned() else {
            return;
        };
        let Some(row) = self.sim.active.get(&q).copied() else {
            return;
        };
        if row.status != quest_status::PROGRESS {
            return;
        }
        let mut idx = 0usize;
        let mut all = true;
        for step in quest.progress.iter() {
            for o in step.objectives.iter() {
                if idx < 5 && row.objectives[idx] < playerbots_need(&o.kind) {
                    all = false;
                }
                idx += 1;
            }
        }
        if !all {
            return;
        }
        let mut row = row;
        row.status = quest_status::READY;
        self.sim.active.insert(q, row);
        self.feed_activity(ActivityEvent::QuestUpdated {
            quest: Box::new(row),
            component: 0,
            p: [0; 4],
        });
        if quest.report == QuestReport::Auto {
            self.finish(q);
        }
    }

    fn finish(&mut self, q: u32) {
        self.sim.active.remove(&q);
        self.sim.completed.push(q);
        self.feed_activity(ActivityEvent::QuestCompleted {
            quest_id: q,
            component: 0,
            block_mask: 0,
        });
    }

    fn on_complete(&mut self, q: u32, npc: u32, doodad: u32) {
        let Some(row) = self.sim.active.get(&q).copied() else {
            self.sim.refused.push(format!("complete {q}: not active"));
            self.feed_activity(ActivityEvent::QuestFailed {
                quest_id: q,
                err: 2,
            });
            return;
        };
        let quest = self.world.quest(q).cloned().unwrap();
        let ok = row.status == quest_status::READY
            && match quest.report {
                QuestReport::Npc(t) => self.state.units.get(&npc).is_some_and(|u| u.id == t),
                QuestReport::Doodad(t) => self
                    .state
                    .doodads
                    .get(&doodad)
                    .is_some_and(|d| d.template == t),
                QuestReport::Auto | QuestReport::Journal => npc == 0 && doodad == 0,
                QuestReport::Unknown => false,
            };
        if !ok {
            self.sim
                .refused
                .push(format!("complete {q}: wrong target {npc}/{doodad}"));
            self.feed_activity(ActivityEvent::QuestFailed {
                quest_id: q,
                err: 7,
            });
            return;
        }
        self.finish(q);
        match self.sim.xp {
            Xp::Slow => {
                // a little experience: two levels for the first quests
                let lvl = 1 + (self.sim.completed.len() as u8 / 2);
                self.state.me.level = lvl.min(10);
            }
            Xp::FollowsQuests { cap } => {
                // the level of the quest, or a level per three quests done, whichever is more
                let by_count = 1 + (self.sim.completed.len() / 3).min(255) as u8;
                let level = quest.level.max(by_count).min(cap);
                if level > self.state.me.level {
                    self.state.me.level = level;
                }
            }
        }
    }

    fn on_skill(&mut self, o: &Out) {
        let mut s = PacketStream::from_bytes(o.body.clone());
        let skill = s.read_u32().unwrap();
        let caster_type = s.read_u8().unwrap();
        let _caster = s.read_bc().unwrap();
        let mut item_template = 0;
        if caster_type == 2 {
            let _item_id = s.read_u64().unwrap();
            item_template = s.read_u32().unwrap();
            let _ = s.read_u8().unwrap();
            let _ = s.read_u32().unwrap();
        }
        let target_type = s.read_u8().unwrap();
        let target = s.read_bc().unwrap();
        if caster_type == 2 {
            *self.sim.item_casts.entry(item_template).or_default() += 1;
            let target_template = self.state.units.get(&target).map_or(0, |u| u.id);
            self.sim.item_targets.push((item_template, target_template));
            if !self.item_target_ok(skill, target_template) {
                self.sim.refused.push(format!(
                    "item {item_template}: target {target_template} breaks the skill requirements"
                ));
                return;
            }
            if self.sim.deaf_items {
                return;
            }
            if self.sim.item_delay_ms > 0 {
                let due = self.now_ms + self.sim.item_delay_ms;
                self.sim.item_pending.push((due, item_template));
                return;
            }
            *self.sim.item_credited.entry(item_template).or_default() += 1;
            self.credit(
                |k| matches!(k, ObjKind::UseItem { item, .. } if *item == item_template),
                1,
                None,
            );
            return;
        }
        match target_type {
            4 => {
                if let Some(t) = self.state.doodads.get(&target).map(|d| d.template) {
                    self.credit(
                        |k| matches!(k, ObjKind::Interact { doodad, .. } if *doodad == t),
                        1,
                        None,
                    );
                    let world = self.world.clone();
                    let gathered = |item: u32| world.gather_doodads(item).contains(&t);
                    let items: Vec<u32> = world
                        .quests()
                        .flat_map(|q| q.progress.iter())
                        .flat_map(|s| s.objectives.iter())
                        .filter_map(|o| match &o.kind {
                            ObjKind::Gather { item, .. } if gathered(*item) => Some(*item),
                            _ => None,
                        })
                        .collect();
                    for item in items {
                        self.credit(
                            |k| matches!(k, ObjKind::Gather { item: i, .. } if *i == item),
                            1,
                            Some(item),
                        );
                    }
                    let _ = skill;
                }
            }
            0 => self.on_attack(target),
            _ => {}
        }
    }

    /// The server's placement rules of an item skill (`unit_reqs` 0x1B and 0x22).
    fn item_target_ok(&self, skill: u32, target: u32) -> bool {
        self.world.skill_needs(skill).iter().all(|need| match need {
            aa_world::SkillNeed::Npc(t) => target == *t,
            aa_world::SkillNeed::NpcGroup(group, negate) => {
                self.world.quest_monster_group(*group).contains(&target) == !*negate
            }
            _ => true,
        })
    }

    fn on_attack(&mut self, mob: u32) {
        let Some(unit) = self.state.units.get(&mob).cloned() else {
            return;
        };
        if unit.dead() {
            return;
        }
        let hits = self.sim.hits.entry(mob).or_insert(0);
        *hits += 1;
        if *hits < 3 {
            return;
        }
        let t = unit.id;
        self.sim.kills += 1;
        if let Xp::FollowsQuests { cap } = self.sim.xp {
            if self.sim.kills.is_multiple_of(12) && self.state.me.level < cap {
                self.state.me.level += 1;
            }
        }
        self.feed_world(WorldEvent::UnitDeath {
            obj: mob,
            killer: ME,
            respawn_wait_ms: 0,
            lost_exp: 0,
            reason: 0,
        });
        self.sim.dead.push((mob, self.now_ms + 20_000));
        // quest credit and the dropped item
        let dropped: Vec<u32> = self
            .world
            .quests()
            .flat_map(|q| q.progress.iter())
            .flat_map(|s| s.objectives.iter())
            .filter_map(|o| match &o.kind {
                ObjKind::Gather { item, droppers, .. } if droppers.contains(&t) => Some(*item),
                _ => None,
            })
            .collect();
        self.credit(
            |k| match k {
                ObjKind::Hunt { npcs, .. } => npcs.contains(&t),
                ObjKind::Gather { droppers, .. } => droppers.contains(&t),
                _ => false,
            },
            1,
            dropped.first().copied(),
        );
    }

    fn force(&mut self, kind: GoalKind) {
        self.brain.command(&BotCmd::Goal(GoalChoice::Forced(kind)));
    }
}

fn playerbots_need(kind: &ObjKind) -> i32 {
    let n = match kind {
        ObjKind::Hunt { count, .. }
        | ObjKind::Gather { count, .. }
        | ObjKind::UseItem { count, .. }
        | ObjKind::Interact { count, .. }
        | ObjKind::Craft { count, .. } => *count,
        _ => 1,
    };
    i32::try_from(n.max(1)).unwrap_or(i32::MAX)
}

fn init_logging() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_test_writer()
        .try_init();
}

fn run_for(rig: &mut Rig, minutes: u64) {
    for _ in 0..(minutes * 60 * 4) {
        rig.tick(250);
    }
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn a_nuian_bot_does_the_first_quests_of_the_start_area() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    init_logging();
    let mut rig = Rig::new(world, 1, 11);
    rig.force(GoalKind::Quest);
    run_for(&mut rig, 25);
    println!("started: {:?}", rig.sim.started);
    println!("completed: {:?}", rig.sim.completed);
    println!("refused: {:?}", rig.sim.refused);
    println!("level {}", rig.state.me.level);
    assert!(
        rig.sim.completed.contains(&6198),
        "quest 6198 not completed"
    );
    assert!(rig.sim.completed.len() >= 6, "{:?}", rig.sim.completed);
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn other_races_start_their_chains_too() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    init_logging();
    for race in [4u8, 5, 6] {
        let mut rig = Rig::new(world.clone(), race, 5 + u64::from(race));
        rig.force(GoalKind::Quest);
        run_for(&mut rig, 15);
        println!(
            "race {race}: completed {:?}, started {:?}, refused {:?}",
            rig.sim.completed, rig.sim.started, rig.sim.refused
        );
        assert!(!rig.sim.completed.is_empty(), "race {race} did nothing");
    }
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn items_the_server_does_not_consume_are_not_cast_for_ever() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let mut rig = Rig::new(world, 1, 11);
    rig.sim.deaf_items = true;
    rig.force(GoalKind::Quest);
    run_for(&mut rig, 25);
    println!("item casts: {:?}", rig.sim.item_casts);
    println!("completed: {:?}", rig.sim.completed);
    for (item, n) in &rig.sim.item_casts {
        assert!(*n <= 12, "item {item} was cast {n} times");
    }
    assert!(
        !rig.sim.completed.is_empty(),
        "the bot went on to other quests"
    );
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn a_bot_hunts_another_monster_when_somebody_else_works_on_the_first() {
    use playerbots::contest::unit_key;
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    const DECOY: u32 = 999;
    // the first monster the bot claims, with the decoy holding `decoy_claim` all along
    let first_claim = |decoy_claim: Option<u32>| -> Option<u32> {
        let mut rig = Rig::new(world.clone(), 1, 11);
        rig.force(GoalKind::Quest);
        for _ in 0..(12 * 60 * 4) {
            if let Some(obj) = decoy_claim {
                rig.hub.contest.claim(unit_key(obj), DECOY);
            }
            rig.tick(250);
            let mut held: Vec<u32> = rig
                .state
                .units
                .keys()
                .copied()
                .filter(|&obj| rig.hub.contest.others(unit_key(obj), DECOY) > 0)
                .collect();
            held.sort_unstable();
            if let Some(&obj) = held.first() {
                return Some(obj);
            }
        }
        None
    };
    let alone = first_claim(None).expect("the bot claims a monster when alone");
    let crowded = first_claim(Some(alone)).expect("the bot claims a monster when that is taken");
    assert_ne!(alone, crowded, "both bots hunted the same monster");
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn a_quest_item_is_cast_once_when_the_server_is_slow_to_credit_it() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    // the server's quest engine credits a cast about 3.5 s after it; a second cast before that
    // would find the item gone once the quest has completed
    let mut rig = Rig::new(world, 1, 11);
    rig.sim.item_delay_ms = 3_500;
    rig.force(GoalKind::Quest);
    run_for(&mut rig, 25);
    println!("item casts: {:?}", rig.sim.item_casts);
    println!("item credits: {:?}", rig.sim.item_credited);
    println!("completed: {:?}", rig.sim.completed);
    assert!(
        !rig.sim.item_casts.is_empty(),
        "no quest item was ever used"
    );
    // every one of these quest objectives wants a single use
    for (item, casts) in &rig.sim.item_casts {
        assert_eq!(*casts, 1, "item {item} was cast {casts} times for one use");
    }
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn a_group_target_item_is_cast_at_a_member_of_its_group() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    // Quest 2261 "Truth Extraction": the hypnosis wand (item 16293, skill 13886) only works on
    // an NPC of quest monster group 154, and the bot used to cast it at itself.
    let giver = *world
        .npc_spawns(10583)
        .first()
        .expect("the giver of quest 2261 spawns");
    let mut rig = Rig::new(world.clone(), 1, 11);
    rig.resume_at(giver.x, giver.y, 10, &[]);
    rig.refresh_view();
    let giver_obj = rig
        .state
        .units
        .iter()
        .find(|(_, u)| u.id == 10583)
        .map(|(&obj, _)| obj)
        .expect("the giver is in view");
    rig.on_start_quest(2261, giver_obj, 0);
    assert!(
        rig.state.quests.row(2261).is_some(),
        "the fake server started the quest"
    );
    rig.force(GoalKind::Quest);
    run_for(&mut rig, 3);
    let targets: Vec<u32> = rig
        .sim
        .item_targets
        .iter()
        .filter(|(item, _)| *item == 16293)
        .map(|(_, target)| *target)
        .collect();
    println!("wand casts at: {targets:?}; refused: {:?}", rig.sim.refused);
    let members = world.quest_monster_group(154);
    assert!(!members.is_empty(), "group 154 has members");
    assert!(!targets.is_empty(), "the wand was never cast");
    assert!(
        targets.iter().all(|t| members.contains(t)),
        "the wand was cast at {targets:?}, not at a member of {members:?}"
    );
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn a_shared_item_target_is_queued_not_crowded() {
    use playerbots::contest::unit_key;
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    // Quest 2396 "A Worthy Opponent": the tournament sword works on the single knight 7818.
    let giver = *world
        .npc_spawns(7817)
        .first()
        .expect("the giver of quest 2396 spawns");
    let mut rig = Rig::new(world.clone(), 1, 11);
    rig.resume_at(giver.x, giver.y, 10, &[]);
    rig.refresh_view();
    let giver_obj = rig
        .state
        .units
        .iter()
        .find(|(_, u)| u.id == 7817)
        .map(|(&obj, _)| obj)
        .expect("the giver is in view");
    rig.give_item(17470, 1);
    rig.on_start_quest(2396, giver_obj, 0);
    assert!(rig.state.quests.row(2396).is_some());
    // Move to a real spawn of the shared knight (the giver is 99 m from it) so the target is in
    // view, then have another bot claim it.
    let target_spawn = *world
        .npc_spawns(7818)
        .iter()
        .min_by(|a, b| {
            let da = (a.x - giver.x).powi(2) + (a.y - giver.y).powi(2);
            let db = (b.x - giver.x).powi(2) + (b.y - giver.y).powi(2);
            da.total_cmp(&db)
        })
        .expect("a spawn of the knight");
    rig.resume_at(target_spawn.x, target_spawn.y, 10, &[]);
    rig.refresh_view();
    let target_objs: Vec<u32> = rig
        .state
        .units
        .iter()
        .filter(|(_, u)| u.id == 7818)
        .map(|(&obj, _)| obj)
        .collect();
    assert!(!target_objs.is_empty(), "a knight is in view");
    for &obj in &target_objs {
        rig.hub.contest.claim(unit_key(obj), 2);
    }
    rig.force(GoalKind::Quest);
    // Shorter than CROWD_WAIT_MS, so the bot is still queueing, not resting the quest.
    for _ in 0..80 {
        rig.tick(250);
    }
    assert_eq!(
        rig.sim.item_casts.get(&17470).copied().unwrap_or(0),
        0,
        "the bot cast at a target another bot holds"
    );
    assert_eq!(
        rig.sim
            .item_targets
            .iter()
            .filter(|(i, _)| *i == 17470)
            .count(),
        0
    );
    // The other bot is done: the queued bot takes its turn.
    for &obj in &target_objs {
        rig.hub.contest.release(unit_key(obj), 2);
    }
    run_for(&mut rig, 1);
    let targets: Vec<u32> = rig
        .sim
        .item_targets
        .iter()
        .filter(|(item, _)| *item == 17470)
        .map(|(_, target)| *target)
        .collect();
    println!("sword casts at: {targets:?}");
    assert_eq!(targets, vec![7818], "the bot cast at the shared NPC");
}

// ---- progression: bots that have nothing near go where the quests are -------------------------

use playerbots::bot::goals::quest::atlas;
use playerbots::bot::goals::quest::progress::{Outcome, Why};

/// A trip as the test saw it.
#[derive(Debug, Clone)]
struct TripNote {
    start_min: u64,
    from: (f32, f32),
    to: (f32, f32),
    dist: f32,
    why: Why,
    /// `None` while it is under way.
    outcome: Option<Outcome>,
    end_min: u64,
}

/// What a bot did over a run: the zones it came to and the trips it made.
#[derive(Default)]
struct Journey {
    /// (minute, zone name) at the first visit of each zone.
    zones: Vec<(u64, String)>,
    trips: Vec<TripNote>,
    /// (minute, quest) of every accepted quest, in order.
    accepted: Vec<(u64, u32)>,
}

fn zone_of(world: &World, pos: [f32; 3]) -> String {
    world
        .zone_key_at(pos[0], pos[1])
        .and_then(|k| world.zone_name(k))
        .unwrap_or("?")
        .to_string()
}

/// Runs the rig for `minutes` and notes the zones and the trips.
fn run_journey(rig: &mut Rig, minutes: u64) -> Journey {
    let mut j = Journey::default();
    let mut seen_started = 0usize;
    for tick in 0..(minutes * 60 * 4) {
        rig.tick(250);
        let min = tick / 240;
        let zone = zone_of(&rig.world, rig.state.me.pos);
        if !j.zones.iter().any(|z| z.1 == zone) {
            j.zones.push((min, zone));
        }
        while seen_started < rig.sim.started.len() {
            j.accepted.push((min, rig.sim.started[seen_started]));
            seen_started += 1;
        }
        let progress = rig
            .state
            .quest_mem
            .as_ref()
            .and_then(|m| m.progress.as_ref());
        let now_trip = progress.and_then(|p| p.trip);
        let open = j.trips.last().is_some_and(|t| t.outcome.is_none());
        match (now_trip, open) {
            (Some(t), false) => j.trips.push(TripNote {
                start_min: min,
                from: t.from,
                to: (t.target.x, t.target.y),
                dist: t.target.dist,
                why: t.target.why,
                outcome: None,
                end_min: min,
            }),
            (None, true) => {
                let last = progress.and_then(|p| p.history.last().copied());
                let note = j.trips.last_mut().unwrap();
                note.outcome = last.map(|r| r.outcome);
                note.end_min = min;
            }
            _ => {}
        }
    }
    j
}

fn print_journey(label: &str, rig: &Rig, j: &Journey) {
    let zones: Vec<String> = j.zones.iter().map(|(m, z)| format!("{z}@{m}")).collect();
    println!(
        "{label}: {} quests done, {} accepted, level {}, {} kills, refused {}",
        rig.sim.completed.len(),
        rig.sim.started.len(),
        rig.state.me.level,
        rig.sim.kills,
        rig.sim.refused.len()
    );
    println!("  zones: {}", zones.join(", "));
    for t in &j.trips {
        println!(
            "  trip min {}..{}: ({:.0}, {:.0}) -> ({:.0}, {:.0}) {:.0} m {:?} {:?}",
            t.start_min, t.end_min, t.from.0, t.from.1, t.to.0, t.to.1, t.dist, t.why, t.outcome
        );
    }
}

/// The story line of `race` as the bots see it.
fn chain_of(world: &World, race: u8) -> Vec<u32> {
    atlas(world)
        .chain(race)
        .map(|c| c.quests.to_vec())
        .unwrap_or_default()
}

/// The quests of the chain up to `chapter`, and where the last of them is handed in.
fn chain_end(world: &World, race: u8, chapter: u16) -> (Vec<u32>, (f32, f32)) {
    let done: Vec<u32> = chain_of(world, race)
        .into_iter()
        .filter(|&q| world.quest(q).is_some_and(|q| q.chapter_idx <= chapter))
        .collect();
    let last = done
        .iter()
        .filter_map(|&q| world.quest(q))
        .max_by_key(|q| (q.chapter_idx, q.quest_idx))
        .expect("a chain");
    let spot = match last.report {
        QuestReport::Npc(t) => world.npc_spawns(t).first().copied(),
        QuestReport::Doodad(t) => world.doodad_spawns(t).first().copied(),
        _ => None,
    }
    .or_else(|| {
        last.starts.iter().find_map(|s| match s {
            QuestStart::Npc(t) => world.npc_spawns(*t).first().copied(),
            _ => None,
        })
    })
    .expect("a spot");
    (done, (spot.x, spot.y))
}

impl Rig {
    /// Puts the bot at (x, y) and level `level`, with `quests` done.
    fn resume_at(&mut self, x: f32, y: f32, level: u8, quests: &[u32]) {
        let z = self.world.ground_z(x, y).unwrap_or(0.0);
        self.motion.enter_world(ME, [x, y, z], 0.0, self.now_ms);
        self.state.me.pos = [x, y, z];
        self.state.me.level = level;
        for &q in quests {
            self.sim.completed.push(q);
            self.feed_activity(ActivityEvent::QuestCompleted {
                quest_id: q,
                component: 0,
                block_mask: 0,
            });
        }
        self.sim.xp = Xp::FollowsQuests { cap: 40 };
    }

    fn chain_started(&self, race: u8) -> Vec<u32> {
        let chain = chain_of(&self.world, race);
        self.sim
            .started
            .iter()
            .copied()
            .filter(|q| chain.contains(q))
            .collect()
    }
}

/// Each race leaves the zone it starts in and starts its second chapter in the next one.
#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data); AA_BAKED for the zones"]
fn every_race_leaves_its_start_zone_and_starts_chapter_two() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    for (race, name, minutes) in [
        (1u8, "Nuian", 240u64),
        (4, "Elf", 240),
        (5, "Harani", 240),
        (6, "Firran", 240),
    ] {
        let mut rig = Rig::new(world.clone(), race, 700 + u64::from(race));
        rig.sim.xp = Xp::FollowsQuests { cap: 30 };
        rig.force(GoalKind::Quest);
        let start_zone = zone_of(&world, rig.state.me.pos);
        let j = run_journey(&mut rig, minutes);
        print_journey(name, &rig, &j);
        let chain = chain_of(&world, race);
        assert!(
            chain.len() >= 50,
            "{name}: the chain has {} quests",
            chain.len()
        );
        let started = rig.chain_started(race);
        let in_chapter = |q: &u32, c: u16| world.quest(*q).is_some_and(|q| q.chapter_idx == c);
        assert!(
            j.zones.iter().any(|z| z.1 != start_zone),
            "{name} never left {start_zone}: {:?}",
            j.zones
        );
        assert!(
            started.iter().any(|q| in_chapter(q, 2)),
            "{name} never started chapter 2 (started {started:?})"
        );
    }
}

/// Every quest the givers within `radius` of (x, y) offer, except the ones in `keep`.
fn local_quests(world: &World, x: f32, y: f32, radius: f32, keep: &[u32]) -> Vec<u32> {
    let mut found = Vec::new();
    let center = Vec3::new(x, y, 0.0);
    world.npc_spawns_within(center, radius, |t, _| {
        found.extend(world.quests_from_npc(t))
    });
    world.doodad_spawns_within(center, radius, |t, _| {
        found.extend(world.quests_from_doodad(t));
    });
    found.sort_unstable();
    found.dedup();
    found.retain(|q| !keep.contains(q));
    found
}

/// Harani and Firran bots at the end of Villanelle with every other quest around done: the next
/// story quest is 2 km away, beyond what the quest picker searches. They go there.
#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn the_two_kilometre_hand_off_to_the_silent_forest_is_walked() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    for (race, name, first_of_chapter_6) in [(5u8, "Harani", 3558u32), (6, "Firran", 3480)] {
        let (mut done, spot) = chain_end(&world, race, 5);
        let chain = chain_of(&world, race);
        done.extend(local_quests(&world, spot.0, spot.1, 1700.0, &chain));
        done.sort_unstable();
        done.dedup();
        println!(
            "{name}: chapter 5 ends at ({:.0}, {:.0}, z {:.1}) in {}; {} quests done",
            spot.0,
            spot.1,
            world.ground_z(spot.0, spot.1).unwrap_or(0.0),
            zone_of(&world, [spot.0, spot.1, 0.0]),
            done.len()
        );
        if std::env::var_os("AA_PRINT_DONE").is_some() {
            println!("DONE {name} {done:?}");
        }
        let mut rig = Rig::new(world.clone(), race, 800 + u64::from(race));
        rig.resume_at(spot.0, spot.1, 26, &done);
        rig.force(GoalKind::Quest);
        let j = run_journey(&mut rig, 60);
        print_journey(name, &rig, &j);
        let giver = world.quest(first_of_chapter_6).unwrap();
        println!("  chapter 6 starts with {} {}", giver.id, giver.name);
        assert!(
            j.accepted.iter().any(|a| a.1 == first_of_chapter_6),
            "{name} never accepted {first_of_chapter_6}: {:?}",
            j.accepted
        );
        let trip = j
            .trips
            .iter()
            .find(|t| t.why == Why::Story(first_of_chapter_6))
            .unwrap_or_else(|| panic!("{name}: no story trip: {:?}", j.trips));
        assert!(trip.dist > 1500.0, "{name}: a trip of {} m", trip.dist);
        assert!(trip.outcome.is_some_and(Outcome::good), "{trip:?}");
    }
}

/// A Harani bot far above the quests of Villanelle does not do them: they are trivial, and the
/// story waits in the Silent Forest.
#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn a_high_level_bot_skips_the_trivial_quests_around_it() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let (done, spot) = chain_end(&world, 5, 5);
    let mut rig = Rig::new(world.clone(), 5, 802);
    rig.resume_at(spot.0, spot.1, 36, &done);
    rig.force(GoalKind::Quest);
    let j = run_journey(&mut rig, 45);
    print_journey("Harani 36", &rig, &j);
    let trip = j.trips.first().expect("a trip");
    assert_eq!(trip.why, Why::Story(3558), "{:?}", j.trips);
    // nothing it accepted before the story quest was trivial for a bot of level 36
    for (_, q) in j.accepted.iter().take_while(|a| a.1 != 3558) {
        let quest = world.quest(*q).unwrap();
        println!("  accepted {} {} L{}", q, quest.name, quest.level);
        assert!(quest.level >= 31, "trivial quest {q} L{}", quest.level);
    }
    assert!(
        j.accepted.iter().any(|a| a.1 == 3558),
        "the story quest was never accepted: {:?}",
        j.accepted
    );
}

/// A Nuian bot of level 20 at the start with the first three chapters done does not busy itself
/// with the trivial quests of the start area: it walks to the fourth chapter in White Arden.
#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn trivial_side_quests_do_not_keep_a_bot_from_its_story() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let (done, _) = chain_end(&world, 1, 3);
    let start = world.race_start(1).unwrap();
    let mut rig = Rig::new(world.clone(), 1, 900);
    rig.resume_at(start.x, start.y, 20, &done);
    rig.force(GoalKind::Quest);
    let j = run_journey(&mut rig, 90);
    print_journey("Nuian 20", &rig, &j);
    let trip = j.trips.first().expect("a trip");
    assert_eq!(trip.why, Why::Story(2498), "{:?}", j.trips);
    assert!(trip.dist > 3000.0);
    assert!(
        rig.chain_started(1).contains(&2498),
        "{:?}",
        rig.chain_started(1)
    );
}

/// When the next story quest is out of the bot's reach (its level is too low) the bot goes to the
/// zone with the most quests for its level instead.
#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn a_bot_below_its_next_story_quest_goes_to_the_best_zone() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let (mut done, _) = chain_end(&world, 1, 2);
    let start = world.race_start(1).unwrap();
    // the quests of the start area are done
    let chain = chain_of(&world, 1);
    done.extend(local_quests(&world, start.x, start.y, 1700.0, &chain));
    done.sort_unstable();
    done.dedup();
    let mut rig = Rig::new(world.clone(), 1, 901);
    // level 12: chapter 3 starts at level 15, so no story quest qualifies
    rig.resume_at(start.x, start.y, 12, &done);
    rig.force(GoalKind::Quest);
    let j = run_journey(&mut rig, 90);
    print_journey("Nuian 12", &rig, &j);
    let trip = j
        .trips
        .iter()
        .find(|t| matches!(t.why, Why::Zone(_)))
        .unwrap_or_else(|| panic!("no zone trip: {:?}", j.trips));
    assert!(trip.dist > 1500.0);
    assert!(trip.outcome.is_some_and(Outcome::good), "{trip:?}");
    assert!(rig.sim.completed.len() > done.len(), "nothing done there");
}
