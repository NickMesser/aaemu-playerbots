//! The guild duty and the guild goal through the real brain and the real `Ctx`, against the real
//! `main_world` data, with the server's answers played by the test:
//!
//! ```text
//! AA_COMPACT=<.../app/Data/compact.sqlite3> AA_SERVER_DATA=<.../app/Data> [AA_BAKED=<data dir>] \
//!   cargo test -p playerbots --test guild_real_data -- --ignored --nocapture
//! ```
//!
//! Each test builds one bot (a `Rig`), puts it in the situation the server's packets would, runs
//! the brain in steps of simulated time and reads the packets the bot sent.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};

use aa_proto::cs::Out;
use aa_proto::guild::{self as wire, FactionBrief, GuildEvent, MemberBrief};
use aa_proto::opcodes::cs as op;
use aa_proto::sc::ServerEvent;
use aa_proto::sc_activity::{ActivityEvent, TeamMemberBrief};
use aa_proto::sc_world::WorldEvent;
use aa_world::{World, WorldPaths};
use playerbots::archetype::Archetypes;
use playerbots::bot::brain::Brain;
use playerbots::bot::cmd::{BotCmd, GoalChoice, GoalKind};
use playerbots::bot::combat::Combat;
use playerbots::bot::coordinator::CoordHandle;
use playerbots::bot::ctx::{Ctx, Persona};
use playerbots::bot::guild::book::{Card, CAN_FOUND, CAN_JOIN};
use playerbots::bot::motion::{Motion, MotionCfg, MotionEnv};
use playerbots::bot::party::{PartyOrder, PartyPurpose};
use playerbots::bot::state::BotState;
use playerbots::config::Config;
use playerbots::hub::{unix_ms, Hub, HubParams, Logger};
use playerbots::roster::Temperament;

fn world() -> Option<Arc<World>> {
    static WORLD: OnceLock<Option<Arc<World>>> = OnceLock::new();
    WORLD
        .get_or_init(|| {
            let paths = WorldPaths {
                compact: PathBuf::from(std::env::var_os("AA_COMPACT")?),
                server_data: PathBuf::from(std::env::var_os("AA_SERVER_DATA")?),
                baked: std::env::var_os("AA_BAKED").map(PathBuf::from),
                world: "main_world".into(),
            };
            Some(Arc::new(World::load(&paths).expect("load main_world")))
        })
        .clone()
}

const ME: u32 = 0x400;
const ME_CHAR: u32 = 500;

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
    /// Everything the bot sent so far.
    sent: Vec<Out>,
    rng: fastrand::Rng,
    now_ms: u64,
    mother: u32,
}

fn guild_config() -> Config {
    let mut cfg = Config::default();
    cfg.guild.enabled = true;
    cfg.guild.churn_per_hour = 0.0;
    cfg.social.chatter_needs_player = false;
    cfg
}

impl Rig {
    fn new(world: Arc<World>, cfg: Config) -> Rig {
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
        slot.char_id.store(ME_CHAR, Ordering::Relaxed);
        hub.mark_bot_char(ME_CHAR);
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
        state.me.char_id = ME_CHAR;
        state.me.name = "Tester".into();
        state.me.obj = ME;
        state.me.spawned = true;
        state.me.level = 12;
        state.me.hp = 1000;
        state.me.hp_max = 1000;
        state.me.mp = 400;
        state.me.mp_max = 400;
        state.me.money = 50_000;
        state.me.faction = world.race_faction(1).expect("Nuian faction");
        state.me.pos = [start.x, start.y, z];
        state.skills = vec![(persona.archetype.starter_skill, 1)];
        let mother = world.mother_faction(state.me.faction);
        let mut motion = Motion::new(MotionCfg::default());
        motion.enter_world(ME, state.me.pos, 0.0, 1);
        let mut brain = Brain::new(&cfg.behavior, &persona.temper);
        // (a quiet goal, so only the guild duty acts)
        brain.command(&BotCmd::Goal(GoalChoice::Forced(GoalKind::Rest)));
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
            sent: Vec::new(),
            rng: fastrand::Rng::with_seed(11),
            now_ms: 1,
            world,
            mother,
        }
    }

    /// One brain tick `dt` milliseconds later; returns the packets it sent.
    fn tick(&mut self, dt: u64) -> Vec<Out> {
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
                real_player_near: true,
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
        let new = std::mem::take(&mut self.out);
        self.sent.extend(new.iter().cloned());
        new
    }

    /// Applies a guild packet as the client would.
    fn guild(&mut self, ev: GuildEvent) {
        self.state
            .apply(&ServerEvent::Activity(ActivityEvent::Guild(Box::new(ev))));
    }

    fn activity(&mut self, ev: ActivityEvent) {
        self.state.apply(&ServerEvent::Activity(ev));
    }

    fn chat(&mut self, kind: i16, name: &str, msg: &str) {
        self.state.apply(&ServerEvent::World(WorldEvent::Chat {
            kind,
            sender_obj: 0,
            sender_id: 900,
            name: name.into(),
            msg: msg.into(),
        }));
    }

    /// Whether the bot sent exactly this packet so far.
    fn sent_has(&self, p: &Out) -> bool {
        self.sent.contains(p)
    }

    /// The packets of one opcode the bot sent so far.
    fn sent_of(&self, opcode: u16) -> Vec<&Out> {
        self.sent.iter().filter(|o| o.opcode == opcode).collect()
    }
}

// ---- helpers ----------------------------------------------------------------------------------

fn member(char_id: u32, name: &str, level: u8, role: u8) -> MemberBrief {
    MemberBrief {
        guild: 5,
        char_id,
        online: true,
        name: name.into(),
        level,
        role,
    }
}

fn faction(id: u32, name: &str, owner: u32) -> FactionBrief {
    FactionBrief {
        id,
        mother_id: 1,
        name: name.into(),
        owner_id: owner,
        owner_name: "Owner".into(),
        created: 0,
    }
}

/// A bot of the book that is not in a guild.
fn card(bot: u32, name: &str, mother: u32, level: u8, flags: u8) -> Card {
    Card {
        bot,
        char_id: 2_000 + bot,
        name: name.into(),
        mother,
        level,
        guild: 0,
        flags,
        seen_ms: 0,
    }
}

/// Puts the bot in guild 5 as `role`, with `others` (`char id, name, level, role`) besides it.
/// Every character below 900 is one of our bots.
fn join_guild(rig: &mut Rig, role: u8, others: &[(u32, &str, u8, u8)]) {
    let owner = if role == wire::role::OWNER {
        ME_CHAR
    } else {
        others
            .iter()
            .find(|o| o.3 == wire::role::OWNER)
            .map_or(0, |o| o.0)
    };
    for o in others {
        if o.0 < 900 {
            rig.hub.mark_bot_char(o.0);
        }
    }
    rig.state.guild.on_own_guild(5, rig.now_ms);
    rig.guild(GuildEvent::Factions(
        vec![faction(5, "Iron Pilgrims", owner)].into_boxed_slice(),
    ));
    let mut members = vec![member(ME_CHAR, "Tester", 12, role)];
    members.extend(others.iter().map(|o| member(o.0, o.1, o.2, o.3)));
    rig.guild(GuildEvent::MemberList {
        total: members.len() as u32,
        guild: 5,
        members: members.into_boxed_slice(),
    });
}

fn whisper_to(packet: &Out, name: &str) -> bool {
    packet.opcode == op::CSSendChatMessagePacket
        && i16::from_le_bytes([packet.body[0], packet.body[1]]) == -3
        && String::from_utf8_lossy(&packet.body).contains(name)
}

fn peers(rig: &Rig) -> Vec<(u32, u32, String)> {
    let mut v = Vec::new();
    for bot in 2..=5u32 {
        let name = format!("Peer{bot}");
        let slot = rig.hub.register(pb_web::BotSnapshot {
            id: bot,
            name: name.clone(),
            ..pb_web::BotSnapshot::default()
        });
        slot.char_id.store(ME_CHAR + bot, Ordering::Relaxed);
        v.push((bot, ME_CHAR + bot, name));
    }
    v
}

fn team_member(char_id: u32, name: &str) -> TeamMemberBrief {
    TeamMemberBrief {
        char_id,
        name: name.into(),
        level: 12,
        role: 0,
        obj_id: 0x500 + char_id,
    }
}

/// The name in a `CSCreateExpedition` packet.
fn create_name(p: &Out) -> String {
    let n = usize::from(u16::from_le_bytes([p.body[0], p.body[1]]));
    String::from_utf8(p.body[2..2 + n].to_vec()).unwrap()
}

// ---- the tests --------------------------------------------------------------------------------

#[tokio::test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
async fn an_owner_appoints_officers_and_recruits_only_its_own_faction() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let mut rig = Rig::new(world, guild_config());
    // seven bots and a real player
    let others = [
        (601, "Anna", 10, wire::role::MEMBER),
        (602, "Berta", 11, wire::role::MEMBER),
        (603, "Cora", 12, wire::role::MEMBER),
        (604, "Dora", 13, wire::role::MEMBER),
        (605, "Emma", 14, wire::role::MEMBER),
        (606, "Fiona", 15, wire::role::MEMBER),
        (607, "Greta", 16, wire::role::MEMBER),
        (900, "Realo", 40, wire::role::MEMBER),
    ];
    join_guild(&mut rig, wire::role::OWNER, &others);
    // candidates: one of the faction, one of another faction, one below the minimum level
    let (m, now) = (rig.mother, unix_ms());
    for c in [
        card(21, "Recruit", m, 12, CAN_JOIN),
        card(22, "Stranger", m + 1000, 12, CAN_JOIN),
        card(23, "Newbie", m, 3, CAN_JOIN),
    ] {
        rig.hub.guilds.update_card(c, now);
    }
    for _ in 0..150 {
        let new = rig.tick(1_000);
        // the server confirms an appointment
        for p in &new {
            for o in &others {
                if *p == wire::set_member_role(wire::role::OFFICER, o.0) {
                    rig.guild(GuildEvent::RoleChanged {
                        char_id: o.0,
                        role: wire::role::OFFICER,
                        name: o.1.into(),
                    });
                }
            }
        }
    }
    let appointed: Vec<u32> = others
        .iter()
        .filter(|o| rig.sent_has(&wire::set_member_role(wire::role::OFFICER, o.0)))
        .map(|o| o.0)
        .collect();
    assert_eq!(appointed, [606, 607], "the two best bots are made officers");
    assert!(
        rig.sent_has(&wire::invite("Recruit")),
        "an unguilded bot is invited"
    );
    assert!(!rig.sent_has(&wire::invite("Stranger")), "another faction");
    assert!(
        !rig.sent_has(&wire::invite("Newbie")),
        "below the minimum level"
    );
    assert!(
        rig.sent_of(op::CSKickFromExpeditionPacket).is_empty(),
        "nobody is expelled"
    );
    assert!(rig.state.guild.in_guild());
}

#[tokio::test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
async fn invitations_are_answered_by_who_sends_them() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let invite = |inviter: u32| GuildEvent::Invitation {
        inviter,
        inviter_name: "Boss".into(),
        guild: 9,
        guild_name: "Hollow Order".into(),
    };
    // a bot's invitation is accepted ...
    let mut rig = Rig::new(world.clone(), guild_config());
    rig.hub.mark_bot_char(701);
    rig.tick(1_000);
    rig.guild(invite(701));
    for _ in 0..12 {
        rig.tick(1_000);
    }
    assert!(rig.sent_has(&wire::reply_invite(9, 701, true)));
    // ... a real player's is declined unless the switch is on
    let mut rig = Rig::new(world.clone(), guild_config());
    rig.tick(1_000);
    rig.guild(invite(901));
    for _ in 0..12 {
        rig.tick(1_000);
    }
    assert!(rig.sent_has(&wire::reply_invite(9, 901, false)));
    assert!(!rig.sent_has(&wire::reply_invite(9, 901, true)));
    let mut cfg = guild_config();
    cfg.guild.accept_player_invites = true;
    let mut rig = Rig::new(world.clone(), cfg);
    rig.tick(1_000);
    rig.guild(invite(901));
    for _ in 0..12 {
        rig.tick(1_000);
    }
    assert!(rig.sent_has(&wire::reply_invite(9, 901, true)));
    // a bot in a guild declines every invitation
    let mut rig = Rig::new(world, guild_config());
    join_guild(
        &mut rig,
        wire::role::MEMBER,
        &[(601, "Boss", 20, wire::role::OWNER)],
    );
    rig.tick(1_000);
    rig.hub.mark_bot_char(701);
    rig.guild(invite(701));
    for _ in 0..12 {
        rig.tick(1_000);
    }
    assert!(rig.sent_has(&wire::reply_invite(9, 701, false)));
}

#[tokio::test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
async fn a_whisper_asking_for_an_invitation_is_answered_by_an_officer_only() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    // an officer of a bot guild invites the player and tells him so
    let mut rig = Rig::new(world.clone(), guild_config());
    join_guild(
        &mut rig,
        wire::role::OFFICER,
        &[
            (601, "Boss", 30, wire::role::OWNER),
            (602, "Mate", 12, wire::role::MEMBER),
        ],
    );
    rig.tick(1_000);
    rig.chat(-3, "Wanderer", "hi, ginv please?");
    for _ in 0..12 {
        rig.tick(1_000);
    }
    assert!(rig.sent_has(&wire::invite("Wanderer")));
    let answers = rig
        .sent
        .iter()
        .filter(|p| whisper_to(p, "Wanderer"))
        .count();
    assert_eq!(answers, 1, "one whisper back, not two");
    // asking again at once changes nothing
    rig.chat(-3, "Wanderer", "ginv");
    for _ in 0..12 {
        rig.tick(1_000);
    }
    assert_eq!(rig.sent_of(op::CSInviteToExpeditionPacket).len(), 1);
    // a plain member cannot invite and points at the officer
    let mut rig = Rig::new(world, guild_config());
    join_guild(
        &mut rig,
        wire::role::MEMBER,
        &[
            (601, "Boss", 30, wire::role::OWNER),
            (602, "Mate", 12, wire::role::OFFICER),
        ],
    );
    rig.tick(1_000);
    rig.chat(-3, "Wanderer", "can i join your guild?");
    for _ in 0..12 {
        rig.tick(1_000);
    }
    assert!(rig.sent_of(op::CSInviteToExpeditionPacket).is_empty());
    let pointer = rig.sent.iter().find(|p| whisper_to(p, "Wanderer"));
    assert!(
        pointer.is_some_and(|p| String::from_utf8_lossy(&p.body).contains("Boss")),
        "{:?}",
        rig.sent
            .iter()
            .map(|p| String::from_utf8_lossy(&p.body).into_owned())
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
async fn a_leaving_owner_hands_the_guild_over_first_and_rests_before_the_next_one() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let mut cfg = guild_config();
    cfg.guild.churn_per_hour = 60.0;
    let mut rig = Rig::new(world, cfg);
    join_guild(
        &mut rig,
        wire::role::OWNER,
        &[
            (601, "Anna", 10, wire::role::MEMBER),
            (602, "Berta", 20, wire::role::MEMBER),
            (603, "Cora", 14, wire::role::OFFICER),
            (900, "Realo", 50, wire::role::MEMBER),
        ],
    );
    // a quarter of an hour in the guild first
    let mut new = rig.tick(16 * 60_000);
    for _ in 0..400 {
        if new.contains(&wire::change_owner(603)) {
            // (an officer is preferred to the higher level, a bot to a real player)
            rig.guild(GuildEvent::OwnerChanged {
                from: ME_CHAR,
                to: 603,
                name: "Cora".into(),
            });
        }
        if new.contains(&wire::leave()) {
            rig.guild(GuildEvent::UnitChanged {
                obj: ME,
                char_id: ME_CHAR,
                kicker: "".into(),
                name: "Tester".into(),
                from: 5,
                to: 0,
                expel: false,
            });
            break;
        }
        new = rig.tick(1_000);
    }
    let hand = rig.sent.iter().position(|p| *p == wire::change_owner(603));
    let left = rig.sent.iter().position(|p| *p == wire::leave());
    assert!(hand.is_some(), "the guild is handed to the officer");
    assert!(left > hand, "and only then left");
    assert!(!rig.state.guild.in_guild());
    // the card says "not available" for a while, then "available" again
    rig.tick(1_000);
    let card = rig.hub.guilds.card(1, unix_ms()).expect("a card");
    assert!(!card.can_join(), "a bot that just left rests");
    rig.tick(100_000);
    rig.tick(1_000);
    let card = rig.hub.guilds.card(1, unix_ms()).expect("a card");
    assert!(card.can_join(), "and can be invited again afterwards");
}

#[tokio::test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
async fn a_founder_gathers_the_team_creates_the_guild_and_dismisses_the_team() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let mut rig = Rig::new(world, guild_config());
    let peers = peers(&rig);
    let order = PartyOrder {
        party_id: 3,
        leader: 1,
        members: [1, 2, 3, 4, 5].into_iter().collect(),
        roles: Default::default(),
        purpose: PartyPurpose::Guild,
    };
    // the coordinator reserved the place
    assert!(rig.hub.guilds.reserve(3, 3, unix_ms()));
    rig.brain.command(&BotCmd::Party(Box::new(order)));
    let mut joined: Vec<u32> = Vec::new();
    let mut created = None;
    let mut dismissed = false;
    for _ in 0..200 {
        let new = rig.tick(1_000);
        for (_, char_id, name) in &peers {
            let team_id = if joined.is_empty() { 0 } else { 77 };
            if new.contains(&aa_proto::cs::invite_to_team(team_id, true, name))
                && !joined.contains(char_id)
            {
                // the invited bot accepts: the server tells the founder
                if joined.is_empty() {
                    rig.activity(ActivityEvent::JoinedTeam {
                        team_id: 77,
                        owner_id: ME_CHAR,
                        is_party: true,
                        members: vec![team_member(ME_CHAR, "Tester"), team_member(*char_id, name)]
                            .into_boxed_slice(),
                    });
                } else {
                    rig.activity(ActivityEvent::TeamMemberJoined {
                        team_id: 77,
                        member: team_member(*char_id, name),
                        party: 0,
                    });
                }
                joined.push(*char_id);
            }
        }
        if let Some(p) = new
            .iter()
            .find(|p| p.opcode == op::CSCreateExpeditionPacket)
        {
            let name = create_name(p);
            assert_eq!(*p, wire::create(&name, 0));
            assert!(playerbots::bot::guild::names::valid(&name), "{name}");
            // the server answers: the guild exists
            rig.guild(GuildEvent::Created {
                faction: Box::new(faction(5, &name, ME_CHAR)),
                owner_obj: ME,
            });
            created = Some(name);
        }
        if new
            .iter()
            .any(|p| p.opcode == op::CSAskRiskyTeamActionPacket)
            && created.is_some()
        {
            dismissed = true;
            rig.activity(ActivityEvent::TeamDismissed { team_id: 77 });
        }
        if dismissed && rig.brain.goal().is_none() {
            break;
        }
    }
    assert_eq!(joined.len(), 4, "everybody was invited, in order");
    assert!(created.is_some(), "the guild was created");
    assert!(dismissed, "the team was dismissed");
    assert!(rig.state.guild.in_guild(), "the founder is in its guild");
    assert_eq!(
        rig.hub.guilds.foundings(),
        0,
        "the place the founding held is free"
    );
    assert!(
        !rig.brain
            .goal()
            .is_some_and(|g| g.kind() == GoalKind::Guild),
        "the goal is over"
    );
    // the name is taken for the other bots
    assert!(rig.hub.guilds.name_taken(created.as_deref().unwrap()));
}

/// A coordinator sink that keeps the orders and holds a book of its own.
#[derive(Default)]
struct Sink {
    sent: parking_lot::Mutex<Vec<(u32, BotCmd)>>,
    book: playerbots::bot::guild::GuildBook,
}

impl playerbots::bot::coordinator::Dispatch for Sink {
    fn send(&self, bot: u32, cmd: BotCmd) -> Result<(), String> {
        self.sent.lock().push((bot, cmd));
        Ok(())
    }
    fn who(&self, bot: u32) -> (u32, String) {
        (ME_CHAR + bot, format!("Bot{bot}"))
    }
    fn guild_book(&self) -> Option<&playerbots::bot::guild::GuildBook> {
        Some(&self.book)
    }
}

#[tokio::test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
async fn a_bot_that_can_sign_a_charter_wishes_and_the_coordinator_orders_the_team() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let mut cfg = guild_config();
    cfg.guild.party_size = 1;
    let mut rig = Rig::new(world, cfg);
    let sink = Arc::new(Sink::default());
    let (handle, coordinator) = playerbots::bot::coordinator::channel(sink.clone());
    let cancel = tokio_util::sync::CancellationToken::new();
    tokio::spawn(coordinator.with_guilds(1, 3).run(cancel.clone()));
    rig.coord = handle;
    // the bots have been reporting for a while (the founding grace is over) ...
    let old = unix_ms() - 200_000;
    let mother = rig.mother;
    let ready = |flags| card(1, "Tester", mother, 12, flags);
    rig.hub.guilds.update_card(ready(CAN_JOIN | CAN_FOUND), old);
    // ... the coordinator knows the bot can sign (its card is in the sink's book as well) ...
    sink.book
        .update_card(ready(CAN_JOIN | CAN_FOUND), unix_ms());
    // ... and the bot asks for a team on its own, once, whatever it does (it rests here)
    for _ in 0..10 {
        rig.tick(1_000);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    cancel.cancel();
    let sent = sink.sent.lock();
    let (bot, cmd) = sent.first().expect("the coordinator ordered a team");
    let BotCmd::Party(order) = cmd else {
        panic!("{cmd:?}")
    };
    assert_eq!(
        (*bot, order.leader, order.purpose),
        (1, 1, PartyPurpose::Guild)
    );
    assert_eq!(order.members.as_slice(), [1]);
    assert_eq!(sink.book.foundings(), 1, "the place is reserved");
    assert!(
        rig.state.guild.get().is_some_and(|m| m.t.wished_ms != 0),
        "the bot remembers its wish"
    );
}
