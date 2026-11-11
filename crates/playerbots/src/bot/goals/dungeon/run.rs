//! One dungeon run of one bot: the facts the machine looks at, built from the tick context, and
//! the machine's intents carried out (packets, movement, the combat controller, the reports).
//!
//! The decisions are in [`super::machine`] (phases, timers, refusals) and [`super::clear`]
//! (the walk through the dungeon); this file only reads the world and does what they say.

use std::time::{Duration, Instant};

use aa_proto::cs;
use aa_world::travel::Dungeon;
use smallvec::SmallVec;

use super::clear::{self, Action, Clearing, Mob, Sight};
use super::entry::{self, Reaction};
use super::machine::{Facts, Fail, Intent, Machine, Phase, TeamFacts, Trip, MUSTER_M};
use super::report::{self, Problem, Where};
use super::route::{self, Stop};
use crate::bot::combat::{self, CombatStatus};
use crate::bot::coordinator::Dispatch;
use crate::bot::ctx::Ctx;
use crate::bot::motion::{MotionStatus, Pace};
use crate::bot::party::{self, PartyOrder, Stage, Wish, MAX_PARTY};
use crate::bot::state::{StateError, UnitKind};

/// A bot at the entrance doodad can cast on it.
pub const DOOR_RANGE: f32 = 3.5;
/// A bot walking to a doodad stops this far from it.
pub const DOOR_STOP: f32 = 2.0;
/// Monsters this far (metres) from the bot are looked at.
const VIEW_M: f32 = 70.0;
/// Monsters this far above or below the bot cannot be fought from where it stands (the melee
/// reach and the range of most skills are measured in three dimensions).
const FLOOR_REACH_M: f32 = 12.0;
/// The bot follows the height of its target when the target is this near (horizontally).
const FOLLOW_XY_M: f32 = 45.0;
/// Monsters farther than this above or below the bot are no monsters of this dungeon (a unit
/// that fell out of the world).
const FLOOR_LIMIT_M: f32 = 150.0;
/// A leg whose end is higher or lower than the bot by more than this starts at the height of its
/// end (inside an instance the bot has no ground under it, see [`match_floor`]).
const FLOOR_FIX_M: f32 = 4.0;
/// The leader's target is good for the members this long.
const BOARD_FRESH: Duration = Duration::from_secs(8);
/// No hostile unit in view this long after arriving, with spawns listed, is a finding.
const EMPTY_AFTER_MS: u64 = 25_000;
/// Hostile spawns the data must list for an empty view to be a finding.
const EMPTY_LISTED_MIN: usize = 20;
/// How long a dungeon the server turned away is left alone when it gave no reason to wait longer.
const SOON_BACK_OFF_MS: u64 = 15 * 60_000;
/// Gap between two answers to an invitation.
const ACCEPT_GAP_MS: u64 = 3_000;
/// A load the server never finished (it said "processing" and then nothing) is dropped after this.
const STALE_LOAD_MS: u64 = 45_000;
/// Reports of one run, one bit each.
const T_NO_LOAD: u16 = 1;
const T_WRONG_WORLD: u16 = 1 << 1;
const T_NO_MONSTERS: u16 = 1 << 2;
const T_EXIT: u16 = 1 << 3;
const T_EXPELLED: u16 = 1 << 4;
const T_CAST: u16 = 1 << 5;
const T_NO_ENTRANCE: u16 = 1 << 6;
const T_REFUSED: u16 = 1 << 7;
const T_ERROR: u16 = 1 << 8;
const T_BOSS: u16 = 1 << 9;
const T_MONSTER_Z: u16 = 1 << 10;

/// A doodad the run uses: the entrance or an exit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Door {
    pub(crate) template: u32,
    pub(crate) skill: u32,
    pub(crate) pos: [f32; 3],
}

/// A party member as the hub shows it.
#[derive(Debug, Clone, Copy)]
struct Mate {
    bot: u32,
    rank: u8,
    char_id: u32,
    online: bool,
    pos: [f32; 2],
    hp: u8,
}

fn flat(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt()
}

/// A distance that weighs the height heavily: inside an instance the bot keeps the height it
/// arrived at (see `Place::ground`), so a doodad on another floor is out of reach however near it
/// is on the map, and the exit to use is the one on the bot's own floor.
fn floor_dist(a: [f32; 3], b: [f32; 3]) -> f32 {
    flat(a, b) + 3.0 * (a[2] - b[2]).abs()
}

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The party came out again.
    Done,
    /// The run was given up.
    Failed(Fail),
}

/// The state of one run. See the module docs.
#[derive(Debug)]
pub struct Run {
    machine: Machine,
    /// Index into `World::dungeons()`.
    dungeon: Option<u16>,
    clearing: Clearing,
    route: Vec<Stop>,
    door: Option<Door>,
    deaths: u8,
    dead_noted: bool,
    kills: u16,
    boss_kills: u8,
    accept_ms: u64,
    route_done: bool,
    told: u16,
    hostile_seen: bool,
    listed: usize,
    unknown_error_ms: u64,
    last_phase: Phase,
    began: Instant,
    last_cast_skill: u32,
    cast_result: u8,
    finished: bool,
    /// The exit skill of the last exit cast and the bot clock of that cast.
    exit_cast: (u32, u64),
    /// Refusals of the exit skill seen, and the bot clock of the last one.
    exit_refused: (u8, u64),
    exit_noted: bool,
    /// How far above (+) or below (-) the spawn points the server keeps the monsters of this
    /// dungeon (0 until a monster was seen off its spawn height), see
    /// [`Run::follow_target_floor`].
    mob_dz: f32,
}

impl Run {
    /// A run for the party `order` (a wish, when there is none yet).
    pub fn new(ctx: &Ctx<'_>, order: Option<&PartyOrder>) -> Run {
        let dungeon = order
            .and_then(|o| party::dungeon_index(ctx.world, o.zone_group()))
            .and_then(|i| u16::try_from(i).ok());
        let door = dungeon.and_then(|i| door_of(ctx.world.dungeons().get(usize::from(i))?));
        Run {
            machine: Machine::new(order.is_some(), ctx.now_ms),
            dungeon,
            clearing: Clearing::default(),
            route: Vec::new(),
            door,
            deaths: 0,
            dead_noted: false,
            kills: 0,
            boss_kills: 0,
            accept_ms: 0,
            route_done: false,
            told: 0,
            hostile_seen: false,
            listed: 0,
            unknown_error_ms: 0,
            last_phase: Phase::Gather,
            began: Instant::now(),
            last_cast_skill: 0,
            cast_result: 0,
            finished: false,
            exit_cast: (0, 0),
            exit_refused: (0, 0),
            exit_noted: false,
            mob_dz: 0.0,
        }
    }

    /// Whether the goal drives the combat controller: inside, and on the way out. The load
    /// counts: a bot that arrives among monsters must still see its arrival.
    pub fn owns_combat(&self) -> bool {
        matches!(
            self.machine.phase(),
            Phase::Load | Phase::Clear | Phase::Leave
        )
    }

    /// Whether the run goes on after a death of the bot.
    pub fn survives_death(&self) -> bool {
        self.machine.phase().survives_death()
    }

    /// The bot died.
    pub fn on_dead(&mut self, ctx: &mut Ctx<'_>) {
        if !self.dead_noted {
            self.dead_noted = true;
            self.deaths = self.deaths.saturating_add(1);
            ctx.log
                .say(format!("dungeon: died inside ({} so far)", self.deaths));
        }
    }

    /// The name of the dungeon, for logs.
    pub fn name<'a>(&self, ctx: &'a Ctx<'_>) -> &'a str {
        self.dungeon_ref(ctx).map_or("?", |d| &*d.name)
    }

    /// Appends what the run is doing to `out` (the web UI's detail line).
    pub fn describe(&self, out: &mut String) {
        use std::fmt::Write;
        let _ = write!(out, "{}", self.machine.phase().word());
        if self.machine.phase() == Phase::Clear && !self.route.is_empty() {
            let _ = write!(
                out,
                " ({} of {} stops left, {} kills, {} deaths)",
                self.clearing.cursor.left(self.route.len()),
                self.route.len(),
                self.kills,
                self.deaths
            );
        }
        if self.machine.exit_failed() {
            out.push_str(" (the exit does nothing)");
        }
    }

    /// Run statistics for the one-line summary: kills, boss kills, deaths.
    pub fn stats(&self) -> (u16, u8, u8) {
        (self.kills, self.boss_kills, self.deaths)
    }

    fn dungeon_ref<'w>(&self, ctx: &Ctx<'w>) -> Option<&'w Dungeon> {
        ctx.world.dungeons().get(usize::from(self.dungeon?))
    }

    fn here<'w>(&self, ctx: &Ctx<'w>) -> Option<(&'w Dungeon, Where<'w>)> {
        let d = self.dungeon_ref(ctx)?;
        Some((
            d,
            Where {
                name: &d.name,
                zone_group: d.zone_group,
            },
        ))
    }

    /// Files `problem` once per run.
    fn tell(&mut self, ctx: &Ctx<'_>, bit: u16, problem: Problem<'_>) {
        if self.told & bit != 0 {
            return;
        }
        self.told |= bit;
        if let Some((_, at)) = self.here(ctx) {
            ctx.report_issue(report::build(at, problem));
        }
    }

    // ---- the party as the hub shows it ----------------------------------------------------

    fn crew(&self, ctx: &Ctx<'_>, order: &PartyOrder) -> SmallVec<[Mate; MAX_PARTY]> {
        let me = ctx.persona.id;
        order
            .members
            .iter()
            .enumerate()
            .map(|(rank, &bot)| {
                if bot == me {
                    return Mate {
                        bot,
                        rank: rank as u8,
                        char_id: ctx.state.me.char_id,
                        online: true,
                        pos: [ctx.state.me.pos[0], ctx.state.me.pos[1]],
                        hp: ctx.state.me.hp_pct(),
                    };
                }
                match ctx.hub.slot(bot) {
                    Some(s) => {
                        let online = s.obj_id.load(std::sync::atomic::Ordering::Relaxed) != 0;
                        let char_id = s.char_id.load(std::sync::atomic::Ordering::Relaxed);
                        let (x, y, hp) = s.with(|g| (g.snap.x, g.snap.y, g.snap.hp_pct));
                        Mate {
                            bot,
                            rank: rank as u8,
                            char_id,
                            online,
                            pos: [x, y],
                            hp,
                        }
                    }
                    None => Mate {
                        bot,
                        rank: rank as u8,
                        char_id: 0,
                        online: false,
                        pos: [0.0; 2],
                        hp: 100,
                    },
                }
            })
            .collect()
    }

    // ---- facts -----------------------------------------------------------------------------

    fn facts(&mut self, ctx: &Ctx<'_>, order: Option<&PartyOrder>) -> Facts {
        let now = ctx.now_ms;
        let mut f = Facts {
            now_ms: now,
            ..Facts::default()
        };
        let (Some(order), Some(d)) = (order, self.dungeon_ref(ctx)) else {
            return f;
        };
        let me = ctx.state.me.pos;
        let crew = self.crew(ctx, order);
        let myself = ctx.persona.id;
        let leader_mate = crew.iter().find(|m| m.bot == order.leader).copied();
        let leader_char = leader_mate.map_or(0, |m| m.char_id);
        f.leader = order.is_leader(myself);
        f.rank = crew.iter().find(|m| m.bot == myself).map_or(0, |m| m.rank);
        f.expected = crew.iter().filter(|m| m.online).count().clamp(1, MAX_PARTY) as u8;
        f.level = ctx.state.me.level;
        f.level_min = d.level_min;
        f.level_max = d.level_max;
        f.raise_ok = ctx.gm_allowed() && ctx.cfg.behavior.dungeon_level_up;
        f.deaths = self.deaths;
        let place = &ctx.state.place;
        f.in_instance = place.in_instance();
        f.loading = place.loading();
        f.world_ok = !place.in_instance() || self.world_matches(ctx, d);
        // the team
        let team = ctx.state.team().filter(|t| t.in_team());
        let owner = team.map_or(0, |t| t.owner);
        let mut missing = 0u8;
        for m in crew.iter().filter(|m| m.bot != myself && m.online) {
            if !team.is_some_and(|t| m.char_id != 0 && t.has(m.char_id)) && m.rank < 8 {
                missing |= 1 << m.rank;
            }
        }
        f.team = TeamFacts {
            in_team: team.is_some(),
            led_by_leader: team.is_some()
                && (owner == 0 || leader_char == 0 || owner == leader_char),
            size: team.map_or(0, |t| t.size().min(255) as u8),
            missing,
            invite: ctx
                .state
                .team()
                .and_then(|t| t.invite.as_ref())
                .is_some_and(|i| leader_char == 0 || i.from_char == leader_char),
        };
        // the entrance
        if let Some(door) = self.door {
            f.door_dist = flat(me, door.pos);
            f.at_door = f.door_dist <= DOOR_RANGE;
            f.door_in_view = find_doodad(ctx, door.template, door.pos).is_some();
            let near = crew
                .iter()
                .filter(|m| m.online)
                .filter(|m| {
                    flat([m.pos[0], m.pos[1], 0.0], [door.pos[0], door.pos[1], 0.0]) <= MUSTER_M
                })
                .count();
            f.muster_near = near.min(255) as u8;
            f.muster_far = crew
                .iter()
                .filter(|m| m.online)
                .map(|m| flat([m.pos[0], m.pos[1], 0.0], [door.pos[0], door.pos[1], 0.0]))
                .fold(0.0, f32::max);
            f.muster_ready = crew.iter().filter(|m| m.online).all(|m| {
                flat([m.pos[0], m.pos[1], 0.0], [door.pos[0], door.pos[1], 0.0]) <= MUSTER_M
            });
        }
        f.trip = match ctx.motion.status() {
            MotionStatus::Moving => Trip::Moving,
            MotionStatus::Failed(_) => Trip::Failed,
            _ => Trip::Idle,
        };
        // what the server said to the entry cast
        if matches!(self.machine.phase(), Phase::Enter | Phase::Muster) {
            self.read_errors(ctx, &mut f);
        }
        // the end of the clear
        f.route_done = self.route_done;
        if !f.leader {
            let stage = ctx.hub.board.stage(order.party_id);
            f.party_leaving = stage >= Stage::Leaving;
            // (a member that comes to the door after the leader went in is not kept waiting)
            f.muster_ready |= stage >= Stage::Clearing;
            f.leader_gone = leader_mate.is_none_or(|m| !m.online);
        }
        // the way out
        if matches!(self.machine.phase(), Phase::Leave) {
            if let Some((door, in_view)) = self.exit_door(ctx, d) {
                f.exit_dist = flat(me, door.pos);
                f.exit_in_view = in_view;
            } else {
                f.exit_dist = 0.0;
                f.exit_in_view = false;
            }
        }
        f
    }

    /// Whether the world the bot was loaded into is the dungeon's.
    fn world_matches(&self, ctx: &Ctx<'_>, d: &Dungeon) -> bool {
        let place = &ctx.state.place;
        if place.zone_key() != u32::from(d.zone_key) {
            return false;
        }
        match (place.world_name(), ctx.world.dungeon_world(d)) {
            (Some(got), Some(w)) => got == &*w.name,
            _ => true,
        }
    }

    /// Follows the exit casts: a refusal of the exit skill is the bot's doing (out of range, on
    /// another floor), a cast the server accepted and did nothing about is the server's, and
    /// reported when the bot has to fall back on leaving the team.
    fn watch_exit(&mut self, ctx: &mut Ctx<'_>) {
        let (skill, cast_ms) = self.exit_cast;
        if skill != 0 {
            let seen = self.exit_refused.1.max(cast_ms.saturating_sub(1));
            for &(t, e) in ctx.state.errors.iter() {
                if t > seen {
                    if let StateError::Skill { skill: s, result } = e {
                        if s == skill && result != 0 {
                            self.exit_refused = (self.exit_refused.0.saturating_add(1), t);
                        }
                    }
                }
            }
        }
        if !self.machine.exit_failed() || self.exit_noted {
            return;
        }
        self.exit_noted = true;
        let refused = self.exit_refused.0;
        let Some(d) = self.dungeon_ref(ctx) else {
            return;
        };
        let exit = self.exit_door(ctx, d).map(|x| x.0);
        if refused == 0 {
            self.tell(
                ctx,
                T_EXIT,
                Problem::ExitStuck {
                    doodad: exit.map_or(0, |x| x.template),
                    skill: exit.map_or(skill, |x| x.skill),
                },
            );
        } else {
            ctx.log.say(format!(
                "dungeon: the server refused the exit skill {refused} times (out of range or another floor); the bot leaves the team instead"
            ));
        }
    }

    /// Reads the refusals the server sent since the last cast.
    fn read_errors(&mut self, ctx: &Ctx<'_>, f: &mut Facts) {
        let watermark = self.machine.error_watermark();
        let skill = self.last_cast_skill;
        for &(t, e) in ctx.state.errors.iter() {
            if t <= watermark {
                continue;
            }
            match e {
                StateError::Msg(code) => match entry::classify(code) {
                    Some(x) => f.error = Some(x),
                    None => {
                        if t > self.unknown_error_ms
                            && self.machine.cast_age_ms(ctx.now_ms).is_some()
                        {
                            self.unknown_error_ms = t;
                            self.tell(ctx, T_ERROR, Problem::ErrorCode { code });
                        }
                    }
                },
                StateError::Skill { skill: s, result }
                    if s == skill && skill != 0 && result != 0 =>
                {
                    f.cast_refused = true;
                    self.cast_result = result;
                }
                _ => {}
            }
        }
    }

    /// The exit to use, see [`exit_door_of`].
    fn exit_door(&self, ctx: &Ctx<'_>, d: &Dungeon) -> Option<(Door, bool)> {
        exit_door_of(ctx, d)
    }

    // ---- one tick --------------------------------------------------------------------------

    /// One tick of the run: returns whether it goes on.
    pub fn tick(&mut self, ctx: &mut Ctx<'_>, order: Option<&PartyOrder>) -> Option<Outcome> {
        self.dead_noted = false;
        if order.is_some() && self.dungeon.is_none() {
            ctx.log
                .say("dungeon: the party's dungeon is not known to this bot");
            return Some(Outcome::Failed(Fail::Team));
        }
        let leader_char = order.and_then(|o| {
            let c = Dispatch::who(ctx.hub, o.leader).0;
            (c != 0).then_some(c)
        });
        reject_strays(ctx, leader_char);
        drop_stale_load(ctx);
        let facts = self.facts(ctx, order);
        let before = self.machine.phase();
        let intent = self.machine.step(&facts);
        let after = self.machine.phase();
        if before != after {
            self.on_phase(ctx, order, before, after);
        }
        self.last_phase = after;
        self.watch_exit(ctx);
        self.carry_out(ctx, order, &facts, intent)
    }

    fn on_phase(&mut self, ctx: &mut Ctx<'_>, order: Option<&PartyOrder>, from: Phase, to: Phase) {
        let name = self.name(ctx).to_owned();
        let note = phase_note(from, to);
        ctx.log.say(if note.is_empty() {
            format!("dungeon: {} ({name})", to.word())
        } else {
            format!("dungeon: {} ({name}): {note}", to.word())
        });
        let leader = order.is_some_and(|o| o.is_leader(ctx.persona.id));
        let party_id = order.map_or(0, |o| o.party_id);
        match to {
            Phase::Clear => self.on_inside(ctx, order),
            Phase::Leave if leader => ctx.hub.board.set_stage(party_id, Stage::Leaving),
            Phase::Out if self.machine.expelled() => {
                let inside_s = match self.machine.entered_ms() {
                    0 => 0,
                    e => ctx.now_ms.saturating_sub(e) / 1000,
                };
                self.tell(ctx, T_EXPELLED, Problem::Expelled { inside_s });
            }
            _ => {}
        }
    }

    /// The bot just arrived inside: note the entry, plan the route.
    fn on_inside(&mut self, ctx: &mut Ctx<'_>, order: Option<&PartyOrder>) {
        let Some((d, _)) = self.here(ctx) else { return };
        let (zone_group, now) = (d.zone_group, ctx.now_ms);
        ctx.state.party_mem().note_entry(zone_group, now);
        let iw = ctx.world.dungeon_world(d);
        self.listed = iw.map_or(0, |w| {
            w.npcs()
                .iter()
                .filter(|n| hostile_template(ctx, n.template))
                .count()
        });
        let leader = order.is_some_and(|o| o.is_leader(ctx.persona.id));
        if leader {
            let me = ctx.state.me.pos;
            if let Some(w) = iw {
                let bosses = route::bosses_of(&w.name);
                self.route = route::plan(w.npcs(), me, bosses, |t| hostile_template(ctx, t));
            }
            let bosses = self.route.iter().filter(|s| s.is_boss()).count();
            ctx.log.say(format!(
                "dungeon: inside {} (instance {}), route of {} stops, {} bosses, {} hostile spawns",
                d.name,
                ctx.state.place.instance_id(),
                self.route.len(),
                bosses,
                self.listed
            ));
            if let Some(o) = order {
                ctx.hub.board.set_stage(o.party_id, Stage::Clearing);
            }
        } else {
            ctx.log.say(format!(
                "dungeon: inside {} (instance {})",
                d.name,
                ctx.state.place.instance_id()
            ));
        }
    }

    fn carry_out(
        &mut self,
        ctx: &mut Ctx<'_>,
        order: Option<&PartyOrder>,
        facts: &Facts,
        intent: Intent,
    ) -> Option<Outcome> {
        match intent {
            Intent::Wait => {}
            Intent::PostWish => self.post_wish(ctx),
            Intent::RaiseLevel => {
                if let Some(d) = self.dungeon_ref(ctx) {
                    let (from, to) = (ctx.state.me.level, d.level_min);
                    if ctx.gm(&format!("/level {to}")) {
                        ctx.log.say(format!(
                            "dungeon: raised the level {from} -> {to} (GM) for {}",
                            d.name
                        ));
                    }
                }
            }
            Intent::Invite(rank) => self.invite(ctx, order, rank),
            Intent::Accept => self.accept(ctx),
            Intent::LeaveTeam => self.leave_team(ctx),
            Intent::DismissTeam => self.dismiss_team(ctx),
            Intent::GoToDoor => {
                if let Some(door) = self.door {
                    ctx.combat.note_travelling(ctx.now_ms);
                    ctx.travel(door.pos[0], door.pos[1], DOOR_STOP, Pace::Run);
                }
            }
            Intent::Stand => {
                if ctx.motion.is_moving() {
                    ctx.motion.stop();
                }
            }
            Intent::CastEntry => self.cast_entry(ctx),
            Intent::Clear => self.clear_step(ctx, order),
            Intent::GoToExit => self.go_to_exit(ctx),
            Intent::CastExit => self.cast_exit(ctx),
            Intent::Done => return Some(Outcome::Done),
            Intent::Fail(f) => {
                self.on_fail(ctx, f);
                return Some(Outcome::Failed(f));
            }
        }
        let _ = facts;
        self.watch_empty(ctx);
        None
    }

    // ---- intents ---------------------------------------------------------------------------

    fn post_wish(&mut self, ctx: &mut Ctx<'_>) {
        let now = ctx.now_ms;
        let me = &ctx.state.me;
        let entry_limit = ctx.cfg.dungeons.entry_limit;
        let mut skip = 0u32;
        if let Some(mem) = ctx.state.party.as_deref() {
            for s in party::sites(ctx.world) {
                let count = ctx
                    .world
                    .dungeons()
                    .get(usize::from(s.idx))
                    .map_or(3, |d| d.enter_count);
                if mem.blocked(s.zone_group, entry_limit.then_some(count), now) {
                    skip |= 1 << s.idx;
                }
            }
        }
        let mother = ctx.world.mother_faction(me.faction);
        let wish = Wish {
            bot: ctx.persona.id,
            at: (me.pos[0], me.pos[1]),
            level: me.level,
            alliance: if mother != 0 { mother } else { me.faction },
            skip,
            raise: ctx.gm_allowed() && ctx.cfg.behavior.dungeon_level_up,
            jump: ctx.gm_allowed() && ctx.cfg.bots.long_travel_teleport_m > 0.0,
        };
        let sent = ctx.coord.request_party(wish);
        ctx.log.say(format!(
            "dungeon: looking for a party (level {}, {})",
            wish.level,
            if sent {
                "asked"
            } else {
                "the coordinator is busy"
            }
        ));
    }

    fn invite(&mut self, ctx: &mut Ctx<'_>, order: Option<&PartyOrder>, rank: u8) {
        let Some(bot) = order.and_then(|o| o.members.get(usize::from(rank)).copied()) else {
            return;
        };
        let (_, name) = Dispatch::who(ctx.hub, bot);
        if name.is_empty() {
            return;
        }
        let team_id = ctx.state.team().map_or(0, |t| t.team_id);
        ctx.send(cs::invite_to_team(team_id, true, &name));
        ctx.log.say(format!("dungeon: invited {name}"));
    }

    fn accept(&mut self, ctx: &mut Ctx<'_>) {
        if self.accept_ms != 0 && ctx.now_ms.saturating_sub(self.accept_ms) < ACCEPT_GAP_MS {
            return;
        }
        let Some(inv) = ctx.state.team().and_then(|t| t.invite.clone()) else {
            return;
        };
        self.accept_ms = ctx.now_ms.max(1);
        ctx.send(cs::reply_to_join_team(
            inv.team_id,
            inv.is_party,
            inv.from_char,
            false,
            &inv.from_name,
            false,
        ));
        ctx.log
            .say(format!("dungeon: joining the team of {}", inv.from_name));
    }

    fn leave_team(&mut self, ctx: &mut Ctx<'_>) {
        let (team_id, me) = (
            ctx.state.team().map_or(0, |t| t.team_id),
            ctx.state.me.char_id,
        );
        if team_id != 0 {
            ctx.send(cs::ask_risky_team_action(team_id, me, cs::RISKY_LEAVE));
            ctx.log.say("dungeon: leaving the team");
        }
    }

    fn dismiss_team(&mut self, ctx: &mut Ctx<'_>) {
        let (team_id, me) = (
            ctx.state.team().map_or(0, |t| t.team_id),
            ctx.state.me.char_id,
        );
        if team_id != 0 {
            ctx.send(cs::ask_risky_team_action(team_id, me, cs::RISKY_DISMISS));
            ctx.log.say("dungeon: dismissing the team");
        }
    }

    fn cast_entry(&mut self, ctx: &mut Ctx<'_>) {
        let Some(door) = self.door else { return };
        let Some((obj, pos)) = find_doodad(ctx, door.template, door.pos) else {
            return;
        };
        if ctx.motion.is_moving() {
            ctx.motion.stop();
        }
        ctx.motion.face(pos[0], pos[1]);
        let skill = if door.skill != 0 {
            door.skill
        } else {
            ctx.world.doodad_use_skill(door.template)
        };
        self.last_cast_skill = skill;
        let me = ctx.state.me.obj;
        ctx.send(cs::start_skill_doodad(skill, me, obj));
        ctx.log.say(format!(
            "dungeon: casting the entrance skill {skill} on doodad {obj}"
        ));
    }

    fn go_to_exit(&mut self, ctx: &mut Ctx<'_>) {
        let Some(d) = self.dungeon_ref(ctx) else {
            return;
        };
        let Some((door, _)) = self.exit_door(ctx, d) else {
            return;
        };
        if ctx.combat.busy() {
            return;
        }
        let now = ctx.now_ms;
        let moved = ctx
            .motion
            .destination()
            .is_none_or(|(x, y)| flat([x, y, 0.0], [door.pos[0], door.pos[1], 0.0]) > 3.0);
        if !ctx.motion.is_moving() || moved {
            match_floor(ctx, door.pos);
            ctx.motion
                .go_to(door.pos[0], door.pos[1], DOOR_STOP, Pace::Run, now);
        }
    }

    fn cast_exit(&mut self, ctx: &mut Ctx<'_>) {
        let Some(d) = self.dungeon_ref(ctx) else {
            return;
        };
        let Some((door, true)) = self.exit_door(ctx, d) else {
            return;
        };
        let Some((obj, pos)) = find_doodad(ctx, door.template, door.pos) else {
            return;
        };
        if ctx.motion.is_moving() {
            ctx.motion.stop();
        }
        ctx.motion.face(pos[0], pos[1]);
        let skill = if door.skill != 0 {
            door.skill
        } else {
            ctx.world.doodad_use_skill(door.template)
        };
        let me = ctx.state.me.obj;
        ctx.send(cs::start_skill_doodad(skill, me, obj));
        self.exit_cast = (skill, ctx.now_ms.max(1));
        ctx.log.say(format!(
            "dungeon: casting the exit skill {skill} on doodad {obj}"
        ));
    }

    // ---- clearing ---------------------------------------------------------------------------

    fn clear_step(&mut self, ctx: &mut Ctx<'_>, order: Option<&PartyOrder>) {
        let Some(order) = order else { return };
        self.follow_target_floor(ctx);
        let leader = order.is_leader(ctx.persona.id);
        // what the fight that just ended came to
        if let CombatStatus::TargetDead { obj } = ctx.combat.status() {
            self.note_kill(ctx, obj);
        }
        if leader {
            if let Some(t) = ctx.combat.target() {
                ctx.hub.board.set_target(order.party_id, t, Instant::now());
            }
        }
        if ctx.combat.busy() {
            return;
        }
        let crew = self.crew(ctx, order);
        let mut mobs: SmallVec<[Mob; 24]> = SmallVec::new();
        self.collect_mobs(ctx, &mut mobs);
        if !mobs.is_empty() {
            self.hostile_seen = true;
        }
        let me = ctx.state.me.pos;
        let myself = ctx.persona.id;
        let others = || crew.iter().filter(|m| m.bot != myself && m.online);
        let spread = clear::spread(me, others().map(|m| [m.pos[0], m.pos[1], me[2]]));
        let party_hp = others().map(|m| m.hp).min().unwrap_or(100);
        let leader_pos = if leader {
            None
        } else {
            self.leader_pos(ctx, order, &crew)
        };
        let board_target = if leader {
            None
        } else {
            ctx.hub
                .board
                .target(order.party_id, Instant::now(), BOARD_FRESH)
        };
        let sight = Sight {
            now_ms: ctx.now_ms,
            me,
            hp_pct: ctx.state.me.hp_pct(),
            mp_pct: ctx.state.me.mp_pct(),
            has_mana: ctx.state.me.mp_max > 0,
            mobs: &mobs,
            spread: if leader { spread } else { 0.0 },
            party_hp: if leader { party_hp } else { 100 },
            leader: leader_pos,
            board_target,
        };
        let action = if leader {
            self.clearing.leader(&self.route, &sight)
        } else {
            self.clearing.member(&sight)
        };
        let missing = self.clearing.take_missing_boss();
        if missing != 0 {
            ctx.log
                .say(format!("dungeon: boss {missing} did not show at its stop"));
            self.tell(ctx, T_BOSS, Problem::BossMissing { template: missing });
        }
        match action {
            Action::Hold | Action::Rest => {
                if ctx.motion.is_moving() {
                    ctx.motion.stop();
                }
            }
            Action::Walk { x, y, radius } => {
                let now = ctx.now_ms;
                let moved = ctx
                    .motion
                    .destination()
                    .is_none_or(|(dx, dy)| flat([dx, dy, 0.0], [x, y, 0.0]) > 4.0);
                if !ctx.motion.is_moving() || moved {
                    // the floor the leg ends on: the stop's for the leader, the leader's own for
                    // the others
                    let floor = if leader {
                        self.route
                            .iter()
                            .find(|s| flat([s.x, s.y, 0.0], [x, y, 0.0]) < 1.0)
                            .map(|s| s.z + self.mob_dz)
                    } else {
                        leader_pos.map(|p| p[2])
                    };
                    if let Some(z) = floor {
                        match_floor(ctx, [x, y, z]);
                    }
                    ctx.motion.go_to(x, y, radius, Pace::Run, now);
                }
                ctx.combat.note_travelling(now);
            }
            Action::Engage(obj) => {
                ctx.combat.engage(obj);
                if leader {
                    ctx.hub
                        .board
                        .set_target(order.party_id, obj, Instant::now());
                }
            }
            Action::Done => {
                if !self.route_done {
                    self.route_done = true;
                    let (kills, bosses, deaths) = self.stats();
                    ctx.log.say(format!(
                        "dungeon: the route is done ({kills} kills, {bosses} bosses, {deaths} deaths, {} stops skipped)",
                        self.clearing.cursor.skipped
                    ));
                }
            }
        }
    }

    /// Puts the bot at the height of the monster it fights when that stands too far above or below
    /// for a skill to reach it. The server takes the client's position at its word, and the 1.2
    /// server of the first live runs keeps the monsters of an instance on another height than
    /// their spawn points (Sharpwind Mines: spawns at 167 m, monsters at 109.9 m, all of them),
    /// which left the whole party "fighting" 57 m above its targets for minutes. The offset is
    /// remembered for the legs to the next stops and filed once as a server finding.
    fn follow_target_floor(&mut self, ctx: &mut Ctx<'_>) {
        let Some(obj) = ctx.combat.target() else {
            return;
        };
        let Some(u) = ctx.state.units.get(&obj) else {
            return;
        };
        if u.kind != UnitKind::Npc {
            return;
        }
        let (me, upos, template) = (ctx.state.me.pos, u.pos, u.id);
        let Some(z) = floor_to_follow(me, upos) else {
            return;
        };
        if let Some(spawn_z) = self.spawn_height(ctx, template, upos) {
            let off = upos[2] - spawn_z;
            if off.abs() > FLOOR_REACH_M {
                self.mob_dz = off;
                self.tell(
                    ctx,
                    T_MONSTER_Z,
                    Problem::MonsterHeight {
                        template,
                        spawn_z: spawn_z.round() as i32,
                        seen_z: upos[2].round() as i32,
                    },
                );
            }
        }
        // Only the server's misplaced monsters are followed up or down: a monster that stands at
        // its own spawn height on another floor is reached on foot (the instance navmesh routes
        // over its stairs, with their heights), never by a jump through the floor.
        if self.mob_dz == 0.0 {
            return;
        }
        ctx.motion.set_height(z);
        ctx.state.me.pos[2] = z;
    }

    /// The height of the spawn point of `template` nearest to `pos` (within 12 m, horizontally).
    fn spawn_height(&self, ctx: &Ctx<'_>, template: u32, pos: [f32; 3]) -> Option<f32> {
        let w = self
            .dungeon_ref(ctx)
            .and_then(|d| ctx.world.dungeon_world(d))?;
        w.npcs()
            .iter()
            .filter(|n| n.template == template)
            .map(|n| (flat(pos, [n.pos.x, n.pos.y, 0.0]), n.pos.z))
            .filter(|(d, _)| *d <= 12.0)
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, z)| z)
    }

    fn note_kill(&mut self, ctx: &mut Ctx<'_>, obj: u32) {
        self.kills = self.kills.saturating_add(1);
        let template = ctx.state.units.get(&obj).map(|u| u.id);
        let boss = template.is_some_and(|t| self.is_boss(ctx, t));
        if boss {
            self.boss_kills = self.boss_kills.saturating_add(1);
            ctx.log.say(format!(
                "dungeon: boss {} killed",
                template.unwrap_or_default()
            ));
        }
    }

    fn is_boss(&self, ctx: &Ctx<'_>, template: u32) -> bool {
        self.dungeon_ref(ctx)
            .and_then(|d| ctx.world.dungeon_world(d))
            .is_some_and(|w| route::bosses_of(&w.name).contains(&template))
    }

    /// The monsters in view the bot may fight.
    fn collect_mobs(&self, ctx: &Ctx<'_>, out: &mut SmallVec<[Mob; 24]>) {
        let me = ctx.state.me.pos;
        let (faction, level) = (ctx.state.me.faction, ctx.state.me.level);
        for (&obj, u) in &ctx.state.units {
            if u.kind != UnitKind::Npc || u.dist2d(me) > VIEW_M {
                continue;
            }
            // (the monsters of another floor are fought too: the bot follows its target's height,
            // see `follow_target_floor`)
            if (u.pos[2] - me[2]).abs() > FLOOR_LIMIT_M {
                continue;
            }
            let Some(t) = combat::attackable(ctx.world, faction, u) else {
                continue;
            };
            if t.level > level.saturating_add(clear::LEVEL_CEILING) && !self.is_boss(ctx, u.id) {
                continue;
            }
            if ctx.state.avoids(obj, u.id, ctx.now_ms) {
                continue;
            }
            out.push(Mob {
                obj,
                template: u.id,
                pos: u.pos,
            });
        }
    }

    /// Where the leader is: in view, else as the hub last saw it.
    fn leader_pos(&self, ctx: &Ctx<'_>, order: &PartyOrder, crew: &[Mate]) -> Option<[f32; 3]> {
        let leader = crew.iter().find(|m| m.bot == order.leader)?;
        if !leader.online {
            return None;
        }
        let in_view = ctx
            .state
            .units
            .values()
            .find(|u| {
                u.kind == UnitKind::Character && leader.char_id != 0 && u.id == leader.char_id
            })
            .map(|u| u.pos);
        in_view.or(Some([leader.pos[0], leader.pos[1], ctx.state.me.pos[2]]))
    }

    /// No hostile unit at all in view for a while, with many listed: the server shows nothing.
    fn watch_empty(&mut self, ctx: &mut Ctx<'_>) {
        if self.machine.phase() != Phase::Clear || self.hostile_seen {
            return;
        }
        let entered = self.machine.entered_ms();
        if entered == 0 || ctx.now_ms.saturating_sub(entered) < EMPTY_AFTER_MS {
            return;
        }
        if self.listed >= EMPTY_LISTED_MIN && self.told & T_NO_MONSTERS == 0 {
            let listed = self.listed;
            let count = |k: UnitKind| ctx.state.units.values().filter(|u| u.kind == k).count();
            let (seen_npcs, seen_characters) = (count(UnitKind::Npc), count(UnitKind::Character));
            let waited_s = ctx.now_ms.saturating_sub(entered) / 1000;
            ctx.log.say(format!(
                "dungeon: nothing hostile in view {waited_s} s after arriving ({seen_npcs} NPC units, {seen_characters} characters, {} doodads in view; the data lists {listed} hostile spawns)",
                ctx.state.doodads.len()
            ));
            self.sample_npcs(ctx);
            // Monsters on other floors are the bot's doing (it has no ground under it), no
            // finding about the server.
            let off_floor = self.hostile_off_floor(ctx);
            if off_floor > 0 {
                self.told |= T_NO_MONSTERS;
                ctx.log.say(format!(
                    "dungeon: {off_floor} hostile monsters in view stand on other floors"
                ));
                return;
            }
            self.tell(
                ctx,
                T_NO_MONSTERS,
                Problem::NoMonsters {
                    listed,
                    seen_npcs,
                    seen_characters,
                    waited_s,
                },
            );
        }
    }

    /// How many monsters in view the bot would fight if they stood on its floor.
    fn hostile_off_floor(&self, ctx: &Ctx<'_>) -> usize {
        let me = ctx.state.me.pos;
        let faction = ctx.state.me.faction;
        ctx.state
            .units
            .values()
            .filter(|u| u.kind == UnitKind::Npc && u.dist2d(me) <= VIEW_M)
            .filter(|u| combat::attackable(ctx.world, faction, u).is_some())
            .count()
    }

    /// Writes the nearest few NPC units in view to the bot log: what they are to this bot
    /// (the reason a unit is no monster is in these numbers).
    fn sample_npcs(&self, ctx: &mut Ctx<'_>) {
        let me = ctx.state.me.pos;
        let own = ctx.state.me.faction;
        let mut near: SmallVec<[(f32, u32); 5]> = SmallVec::new();
        let mut all: Vec<(f32, u32)> = ctx
            .state
            .units
            .iter()
            .filter(|(_, u)| u.kind == UnitKind::Npc)
            .map(|(&o, u)| (u.dist2d(me), o))
            .collect();
        all.sort_by(|a, b| a.0.total_cmp(&b.0));
        near.extend(all.into_iter().take(5));
        for (d, o) in near {
            let Some(u) = ctx.state.units.get(&o) else {
                continue;
            };
            let template = ctx.world.npc(u.id);
            let faction = if u.faction != 0 {
                u.faction
            } else {
                template.map_or(0, |t| t.faction_id)
            };
            let line = format!(
                "dungeon: sample npc {} lv{} faction {} (own {}): {:?}, {:.0} m away, dz {:.1}, flags {:#x}, template {}",
                u.id,
                u.level,
                faction,
                own,
                ctx.world.relation(own, faction),
                d,
                u.pos[2] - me[2],
                u.flags,
                if template.is_some() { "known" } else { "unknown" }
            );
            ctx.log.say(line);
        }
    }

    // ---- ends ---------------------------------------------------------------------------------

    /// Files the report for a failed run and remembers what the server told.
    fn on_fail(&mut self, ctx: &mut Ctx<'_>, f: Fail) {
        let Some((d, _)) = self.here(ctx) else {
            ctx.log.say(format!("dungeon: failed ({})", f.word()));
            return;
        };
        let (zone_group, now) = (d.zone_group, ctx.now_ms);
        ctx.log
            .say(format!("dungeon: failed ({}) in {}", f.word(), d.name));
        let back_off = match f {
            Fail::Refused(e) => match entry::reaction(e) {
                Reaction::GiveUp { back_off_ms } => Some(back_off_ms),
                Reaction::Retry | Reaction::RaiseLevel => Some(SOON_BACK_OFF_MS),
            },
            Fail::NoLoad | Fail::WrongWorld | Fail::CastRefused | Fail::NoDoor => {
                Some(SOON_BACK_OFF_MS)
            }
            _ => None,
        };
        if let Some(ms) = back_off {
            ctx.state
                .party_mem()
                .back_off(zone_group, now.saturating_add(ms));
        }
        let skill = self.last_cast_skill;
        let door = self.door;
        match f {
            Fail::NoLoad => {
                let waited_s = self.machine.cast_age_ms(now).unwrap_or(0) / 1000;
                self.tell(
                    ctx,
                    T_NO_LOAD,
                    Problem::NoLoad {
                        skill,
                        doodad: door.map_or(0, |x| x.template),
                        waited_s,
                    },
                );
            }
            Fail::WrongWorld => {
                let place = &ctx.state.place;
                let (want, got) = (d.zone_key, place.zone_key());
                let world = place.world_name().map(str::to_owned);
                self.tell(
                    ctx,
                    T_WRONG_WORLD,
                    Problem::WrongWorld {
                        want_zone_key: want,
                        got_zone_key: got,
                        got_world: world.as_deref(),
                    },
                );
            }
            Fail::NoDoor => {
                if let Some(door) = door {
                    self.tell(
                        ctx,
                        T_NO_ENTRANCE,
                        Problem::NoEntrance {
                            doodad: door.template,
                        },
                    );
                }
            }
            Fail::ExitStuck => {
                let exit = self.exit_door(ctx, d).map(|x| x.0);
                self.tell(
                    ctx,
                    T_EXIT,
                    Problem::ExitStuck {
                        doodad: exit.map_or(0, |x| x.template),
                        skill: exit.map_or(0, |x| x.skill),
                    },
                );
            }
            Fail::CastRefused => {
                let result = self.cast_result;
                self.tell(ctx, T_CAST, Problem::CastRefused { skill, result });
            }
            Fail::Refused(e) => {
                self.tell(ctx, T_REFUSED, Problem::Refused(e));
            }
            _ => {}
        }
    }

    /// The goal is over (any way): the leader tells the members, the bot notes the run.
    pub fn finish(
        &mut self,
        ctx: &mut Ctx<'_>,
        order: Option<&PartyOrder>,
        outcome: Option<Outcome>,
    ) {
        if self.finished {
            return;
        }
        self.finished = true;
        let now = ctx.now_ms;
        if let Some(o) = order {
            if o.is_leader(ctx.persona.id) {
                ctx.hub.board.set_stage(o.party_id, Stage::Done);
            }
        } else if self.machine.wish_posted() && self.machine.phase() == Phase::Gather {
            ctx.coord.cancel(ctx.persona.id);
        }
        if order.is_none() {
            return;
        }
        let (kills, bosses, deaths) = self.stats();
        let took = self.began.elapsed().as_secs();
        let name = self.name(ctx).to_owned();
        let inside_s = match self.machine.entered_ms() {
            0 => 0,
            e => now.saturating_sub(e) / 1000,
        };
        let mem = ctx.state.party_mem();
        mem.last_run_ms = now.max(1);
        match outcome {
            Some(Outcome::Done) => mem.runs = mem.runs.saturating_add(1),
            _ => mem.failed_runs = mem.failed_runs.saturating_add(1),
        }
        ctx.log.say(format!(
            "dungeon: run over ({name}): {}, {kills} kills, {bosses} bosses, {deaths} deaths, {inside_s} s inside, {took} s in all",
            match outcome {
                Some(Outcome::Done) => "completed".to_owned(),
                Some(Outcome::Failed(f)) => format!("failed: {}", f.word()),
                None => "interrupted".to_owned(),
            },
        ));
        // a bot that is still in the team (the goal was cut short) leaves it
        if matches!(
            self.machine.phase(),
            Phase::Gather
                | Phase::Prep
                | Phase::Team
                | Phase::Travel
                | Phase::Muster
                | Phase::Enter
        ) && ctx.state.team().is_some_and(|t| t.in_team())
        {
            self.leave_team(ctx);
        }
    }
}

fn phase_note(from: Phase, to: Phase) -> &'static str {
    match (from, to) {
        (Phase::Gather, _) => "the party is complete",
        (_, Phase::Clear) => "loaded",
        (_, Phase::Leave) => "the clear is over",
        (_, Phase::Out) => "back outside",
        _ => "",
    }
}

/// The exit to use: the doodad in view nearest to the bot, else the nearest the data lists. The
/// flag says whether the doodad is in view.
pub(crate) fn exit_door_of(ctx: &Ctx<'_>, d: &Dungeon) -> Option<(Door, bool)> {
    let iw = ctx.world.dungeon_world(d)?;
    let me = ctx.state.me.pos;
    let mut best: Option<(Door, f32)> = None;
    for x in iw.exits() {
        if let Some((_, pos)) = find_doodad(ctx, x.template, [x.pos.x, x.pos.y, x.pos.z]) {
            let dist = floor_dist(me, pos);
            if best.is_none_or(|b| dist < b.1) {
                best = Some((
                    Door {
                        template: x.template,
                        skill: x.skill,
                        pos,
                    },
                    dist,
                ));
            }
        }
    }
    if let Some((door, _)) = best {
        return Some((door, true));
    }
    let x = iw.exits().iter().min_by(|a, b| {
        floor_dist(me, [a.pos.x, a.pos.y, a.pos.z])
            .total_cmp(&floor_dist(me, [b.pos.x, b.pos.y, b.pos.z]))
    })?;
    Some((
        Door {
            template: x.template,
            skill: x.skill,
            pos: [x.pos.x, x.pos.y, x.pos.z],
        },
        false,
    ))
}

/// The entrance the party uses: the first one the data lists.
pub(crate) fn door_of(d: &Dungeon) -> Option<Door> {
    let e = d.entrances.first()?;
    let p = e.spawns.first()?;
    Some(Door {
        template: e.doodad,
        skill: e.skill,
        pos: [p.x, p.y, p.z],
    })
}

/// The doodad of `template` in view nearest to `near`: object id and position.
pub(crate) fn find_doodad(ctx: &Ctx<'_>, template: u32, near: [f32; 3]) -> Option<(u32, [f32; 3])> {
    ctx.state
        .doodads
        .iter()
        .filter(|(_, d)| d.template == template)
        .map(|(&obj, d)| (obj, d.pos))
        .min_by(|a, b| flat(a.1, near).total_cmp(&flat(b.1, near)))
}

/// Whether NPC `template` is an enemy of the bot's faction.
fn hostile_template(ctx: &Ctx<'_>, template: u32) -> bool {
    ctx.world.npc(template).is_some_and(|t| {
        ctx.world.relation(ctx.state.me.faction, t.faction_id) == aa_world::Relation::Hostile
    })
}

/// A load that never ended (the server sent `SCProcessingInstance` for an entry it then refused
/// without a word) would keep the next entry from being cast: forget it.
fn drop_stale_load(ctx: &mut Ctx<'_>) {
    let place = &ctx.state.place;
    if place.in_instance() {
        return;
    }
    if place
        .loading_since_ms()
        .is_some_and(|t| ctx.now_ms.saturating_sub(t) > STALE_LOAD_MS)
    {
        ctx.state.place.loaded();
        ctx.log
            .say("dungeon: dropped a load the server never finished");
    }
}

/// The height the bot takes to fight a monster at `target` from `me`: the monster's, when that is
/// more than [`FLOOR_REACH_M`] above or below and the monster is within [`FOLLOW_XY_M`] on the map.
pub(crate) fn floor_to_follow(me: [f32; 3], target: [f32; 3]) -> Option<f32> {
    let off_floor = (target[2] - me[2]).abs() > FLOOR_REACH_M;
    (off_floor && target[2].is_finite() && flat(me, target) <= FOLLOW_XY_M).then_some(target[2])
}

/// Inside an instance the bot has no ground under it: it keeps the height it arrived at, so the
/// mobs of another floor stay out of reach. A leg to another floor therefore starts at the height
/// of its end (the server takes the position the client reports).
pub(crate) fn match_floor(ctx: &mut Ctx<'_>, dest: [f32; 3]) {
    let me = ctx.state.me.pos;
    let z = dest[2];
    if (z - me[2]).abs() <= FLOOR_FIX_M || routed_on_mesh(ctx, me, dest) {
        return;
    }
    ctx.motion.teleported([me[0], me[1], z]);
    ctx.state.me.pos[2] = z;
}

/// Whether the walk from `from` to `to` goes over the instance's navmesh: both ends lie in the
/// same connected part of it, so the route carries the heights of its stairs and ramps and the
/// bot walks up or down them instead of changing floors on the spot.
fn routed_on_mesh(ctx: &Ctx<'_>, from: [f32; 3], to: [f32; 3]) -> bool {
    if !ctx.cfg.behavior.navmesh {
        return false;
    }
    let Some(mesh) = ctx
        .state
        .place
        .world_name()
        .and_then(|w| ctx.world.nav_of(w))
    else {
        return false;
    };
    let at = |p: [f32; 3]| mesh.locate(aa_world::Vec3::new(p[0], p[1], p[2]), 8.0);
    match (at(from), at(to)) {
        (Some(a), Some(b)) => mesh.same_component(a, b),
        _ => false,
    }
}

/// Turns down an invitation that is not the leader's.
pub fn reject_strays(ctx: &mut Ctx<'_>, leader_char: Option<u32>) {
    let Some(inv) = ctx.state.team().and_then(|t| t.invite.clone()) else {
        return;
    };
    if leader_char == Some(inv.from_char) {
        return;
    }
    ctx.send(cs::reply_to_join_team(
        inv.team_id,
        inv.is_party,
        inv.from_char,
        true,
        &inv.from_name,
        false,
    ));
    ctx.state.party_mem().team.invite = None;
    ctx.log.say(format!(
        "dungeon: turned down the team invitation of {}",
        inv.from_name
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bot_takes_the_height_of_a_monster_it_cannot_reach_from_its_own() {
        let me = [450.0, 298.0, 150.0];
        // Palace Cellar: the monsters 29.7 m below the entrance, 4 m away on the map
        assert_eq!(floor_to_follow(me, [454.0, 298.0, 120.3]), Some(120.3));
        // Sharpwind Mines: spawn points at 167 m, monsters at 109.9 m
        let me = [628.0, 324.0, 167.5];
        assert_eq!(floor_to_follow(me, [613.4, 324.4, 109.9]), Some(109.9));
        // the same floor (stairs, a slope) is walked
        assert_eq!(floor_to_follow(me, [613.4, 324.4, 160.0]), None);
        assert_eq!(floor_to_follow(me, [613.4, 324.4, 175.0]), None);
        // a monster far away on the map is not followed yet
        assert_eq!(floor_to_follow(me, [700.0, 324.0, 109.9]), None);
        // a height that is no number is ignored
        assert_eq!(floor_to_follow(me, [613.4, 324.4, f32::NAN]), None);
        assert_eq!(floor_to_follow(me, [613.4, 324.4, f32::INFINITY]), None);
    }
}
