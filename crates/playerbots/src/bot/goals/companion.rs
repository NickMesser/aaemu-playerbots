//! Companion: a bot in the party of a real player follows the leader, helps in its fights and,
//! with `party.follow_into_dungeons`, goes through a dungeon door behind it (owned by the party
//! feature, see [`crate::bot::companion`]).
//!
//! The brain starts the goal when the bot is in a team with a real player and runs nothing it must
//! not drop ([`crate::bot::companion::duty`]); the goal ends when the bot is out of the party.
//! What it does each tick:
//!
//! * **Follow.** The leader is the owner of the team when that is a real player, else the first
//!   real player online. The bot walks to its unit in view, else to the position the server sends
//!   with every move of a member (`SCTeamRemoteMembersEx`), stops a few metres short
//!   ([`follow::stop_radius`], a different distance for each bot of the party) and starts again
//!   when the leader is more than [`follow::START_M`] away. Far trips go by [`Ctx::travel`] (portals,
//!   GM `/move`), and while the bot catches up the combat controller only turns on what attacks it
//!   ([`crate::bot::combat::Combat::note_travelling`]). A bot below 40 % hit points rests next to
//!   the leader until 70 %.
//! * **Fights.** The combat controller already helps with any team member's fight (the monsters
//!   that attack a member, then the member's target); `assist` points the bot at the leader's
//!   target for a while. The goal does not own the controller: a fight pauses it.
//! * **Orders.** `stay` stands still (the controller then helps only close by), `follow` ends
//!   it, `come` walks to the speaker, `dungeon <name>` walks to the entrance of a dungeon and
//!   waits there ([`crate::bot::companion::command`]).
//! * **Dungeons.** [`crate::bot::companion::lead::leader_state`] tells from the position records
//!   when the leader went into an instance (or that it vanished at an entrance); the machine
//!   [`enter::Machine`] then sends the bot to the entrance, casts the entrance skill (the server
//!   puts a team member into the instance the team opened), follows and assists inside, and walks
//!   to an exit and casts it when the leader is out again. A dungeon the bot cannot enter (level,
//!   entry limit, refusals) is left alone and the leader is told once.
//!
//! The pure parts are in the sibling modules (and tested there); this file is the glue to the
//! tick context.

use pb_web::Activity;

use aa_proto::cs;
use aa_world::travel::Dungeon;
use aa_world::Vec3;

use crate::bot::companion::command::Say;
use crate::bot::companion::enter::{Facts, Intent, Leader, Machine, Phase};
use crate::bot::companion::follow::{self, Action, Inputs};
use crate::bot::companion::lead::{self, Rec, Sight, ENTRANCE_M};
use crate::bot::companion::{self as comp, Mode};
use crate::bot::ctx::Ctx;
use crate::bot::manner;
use crate::bot::motion::{MotionStatus, Pace};
use crate::bot::state::{StateError, UnitKind};
use crate::config::BehaviorCfg;
use crate::roster::Temperament;

use super::dungeon::entry;
use super::dungeon::run::{self as dun, Door, DOOR_RANGE, DOOR_STOP};
use super::GoalStatus;

/// A monster that targeted the bot this recently threatens it (it does not rest).
const THREAT_MS: u64 = 4_000;
/// A far trip is aimed again when the leader is this far from its destination.
const FAR_RETARGET_M: f32 = 25.0;
/// A trip to a door is aimed again when the door is this far from its destination.
const DOOR_RETARGET_M: f32 = 3.0;
/// The leader stood this close (metres) to an entrance and left the view: it went in.
/// How often the glue looks for the entrances near the leader (bot clock, milliseconds).
const SIGHT_GAP_MS: u64 = 1_000;

fn flat(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt()
}

/// Companion: follow a real player's party, e.g. into a dungeon. See the module docs.
#[derive(Debug, Default)]
pub struct Companion {
    run: Option<Box<Run>>,
}

/// What the goal remembers while it runs.
#[derive(Debug)]
struct Run {
    machine: Machine,
    /// The bot rests (the follow rule said so on the last decision).
    resting: bool,
    /// Where the leader was last in view.
    sight: Sight,
    /// Where the leader last stood in the main world (to pick the entrance it used).
    main_pos: Option<[f32; 3]>,
    /// The dungeon the machine works on: index into `World::dungeons()`.
    dungeon: Option<usize>,
    /// The entrance the machine walks to and casts on.
    door: Option<Door>,
    /// The skill of the last entry cast and the bot clock after which errors are about it.
    last_cast_skill: u32,
    err_mark: u64,
    /// The exit cast: skill, bot clock, refusals so far.
    exit_cast: (u32, u64, u8),
    /// Bot clock of the last look for entrances near the leader.
    looked_ms: u64,
    /// The last phase the glue logged.
    last_phase: Phase,
    /// The leader was told that the bot cannot go in.
    away_told: bool,
    /// The leader's last known state (for the log).
    state: Leader,
    /// A short description for the web UI.
    word: &'static str,
}

impl Run {
    fn new() -> Run {
        Run {
            machine: Machine::new(),
            resting: false,
            sight: Sight::default(),
            main_pos: None,
            dungeon: None,
            door: None,
            last_cast_skill: 0,
            err_mark: 0,
            exit_cast: (0, 0, 0),
            looked_ms: 0,
            last_phase: Phase::Outside,
            away_told: false,
            state: Leader::Unknown,
            word: "starting",
        }
    }
}

/// What the bot knows of its leader this tick.
#[derive(Debug, Clone, Copy)]
struct Lead {
    char_id: u32,
    online: bool,
    view: Option<[f32; 3]>,
    rec: Option<Rec>,
}

fn lead_of(ctx: &Ctx<'_>, char_id: u32) -> Lead {
    let rec = ctx.state.team().and_then(|t| t.remote_of(char_id));
    Lead {
        char_id,
        online: rec.is_none_or(|r| r.online),
        view: view_of(ctx, char_id),
        rec: rec.filter(|r| r.located()).map(|r| Rec {
            seen_ms: r.seen_ms,
            zone_id: r.zone_id,
            instance_id: r.instance_id,
            pos: r.pos,
        }),
    }
}

/// The position of character `char_id` when its unit is in view.
fn view_of(ctx: &Ctx<'_>, char_id: u32) -> Option<[f32; 3]> {
    ctx.state
        .units
        .values()
        .find(|u| u.kind == UnitKind::Character && u.id == char_id)
        .map(|u| u.pos)
}

/// The entrance of `d` (and its spawn) nearest to `near`.
fn pick_door(d: &Dungeon, near: [f32; 3]) -> Option<Door> {
    let (e, p) = d.nearest_entrance(Vec3::new(near[0], near[1], near[2]))?;
    Some(Door {
        template: e.doodad,
        skill: e.skill,
        pos: [p.x, p.y, p.z],
    })
}

/// The dungeon with an entrance within [`ENTRANCE_M`] of `pos`.
fn dungeon_at(ctx: &Ctx<'_>, pos: [f32; 3]) -> Option<usize> {
    let at = Vec3::new(pos[0], pos[1], pos[2]);
    ctx.world
        .dungeons()
        .iter()
        .enumerate()
        .filter_map(|(i, d)| {
            let (_, p) = d.nearest_entrance(at)?;
            let dist = p.dist2d_sq(at).sqrt();
            (dist <= ENTRANCE_M).then_some((i, dist))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _)| i)
}

/// The dungeon the bot stands in (it is in an instance).
fn dungeon_in(ctx: &Ctx<'_>) -> Option<usize> {
    let key = ctx.state.place.zone_key();
    if key == 0 {
        return None;
    }
    ctx.world
        .dungeons()
        .iter()
        .position(|d| u32::from(d.zone_key) == key)
}

impl Companion {
    /// A fresh goal (the brain starts it with [`Goal::new`](super::Goal::new)).
    pub fn new() -> Companion {
        Companion::default()
    }

    /// Called once when the brain picks the goal.
    pub fn start(&mut self, ctx: &mut Ctx<'_>) {
        let run = self.run.insert(Box::new(Run::new()));
        run.looked_ms = ctx.now_ms;
        ctx.log.say("companion: following the party");
    }

    /// One tick.
    pub fn tick(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        match self.run.as_deref_mut() {
            Some(run) => run.tick(ctx),
            None => GoalStatus::Done,
        }
    }

    /// Called when the goal ends, fails or is interrupted (stop motion, close windows).
    pub fn stop(&mut self, ctx: &mut Ctx<'_>) {
        if let Some(run) = self.run.as_deref_mut() {
            if run.resting {
                run.resting = false;
                manner::rest_end(ctx);
            }
        }
        if ctx.motion.is_moving() {
            ctx.motion.stop();
        }
    }

    /// Whether the goal drives the combat controller itself (see [`super::Goal::owns_combat`]):
    /// never, the controller helps the party on its own and pauses the goal while it fights.
    pub fn owns_combat(&self) -> bool {
        false
    }

    /// The activity the web UI shows while the goal runs.
    pub fn activity(&self, moving: bool, fighting: bool) -> Activity {
        if fighting {
            Activity::Fighting
        } else if moving {
            Activity::Traveling
        } else {
            Activity::Idle
        }
    }

    /// Appends a one-line description of what the goal is doing to `out`.
    pub fn describe(&self, out: &mut String) {
        out.push_str("companion: ");
        out.push_str(self.run.as_deref().map_or("not started", |r| r.word));
    }
}

impl Run {
    fn tick(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let now = ctx.now_ms;
        if !ctx.cfg.party.join_player_parties || !comp::in_player_party(ctx) {
            return GoalStatus::Done;
        }
        let (mode, come, ask, leave, assist_until) = {
            let m = comp::mem_of(ctx.state);
            (m.mode, m.come, m.dungeon, m.leave, m.assist_until_ms)
        };
        if leave {
            self.word = "leaving the party";
            self.hold(ctx);
            return GoalStatus::Running;
        }
        let Some(leader) = comp::leader_char(ctx) else {
            return GoalStatus::Done;
        };
        let lead = lead_of(ctx, leader);
        self.note_sight(ctx, &lead);
        let state = lead::leader_state(&lead::In {
            now_ms: now,
            online: lead.online,
            in_view: lead.view.is_some(),
            rec: lead.rec,
            in_instance: ctx.state.place.in_instance(),
            my_instance: ctx.state.place.instance_id(),
            sight: self.sight,
        });
        if state != self.state {
            ctx.log.debug(format!("companion: the leader is {state:?}"));
            self.state = state;
        }
        // the machine
        let enabled = ctx.cfg.party.follow_into_dungeons && mode == Mode::Follow;
        let facts = self.facts(ctx, &lead, state, enabled);
        let intent = self.machine.step(&facts);
        let phase = self.machine.phase();
        if phase != self.last_phase {
            self.on_phase(ctx, self.last_phase, phase, leader);
            self.last_phase = phase;
        }
        if phase != Phase::Outside && ask.is_some() {
            comp::mem_of(ctx.state).dungeon = None;
        }
        self.word = phase_word(phase, mode);
        let following = mode == Mode::Follow
            && come == 0
            && ask.is_none()
            && matches!(phase, Phase::Outside | Phase::Inside);
        self.assist(ctx, &lead, now < assist_until, following);
        let walk_to = self.target(ctx, &lead, mode, come, ask);
        self.carry_out(ctx, intent, walk_to, &lead, ask);
        GoalStatus::Running
    }

    /// Notes where the leader is while it is in view.
    fn note_sight(&mut self, ctx: &Ctx<'_>, lead: &Lead) {
        let now = ctx.now_ms;
        if let Some(pos) = lead.view {
            let near =
                if now.saturating_sub(self.looked_ms) >= SIGHT_GAP_MS || self.sight.at_ms == 0 {
                    self.looked_ms = now;
                    Some(!ctx.state.place.in_instance() && dungeon_at(ctx, pos).is_some())
                } else {
                    None
                };
            self.sight = Sight {
                at_ms: now,
                pos,
                near_entrance: near.unwrap_or(self.sight.near_entrance),
            };
            if !ctx.state.place.in_instance() {
                self.main_pos = Some(pos);
            }
        } else if let Some(r) = lead.rec.filter(|r| r.instance_id == 0) {
            self.main_pos = Some(r.pos);
        }
    }

    // ---- the machine's facts -----------------------------------------------------------------

    fn facts(&mut self, ctx: &Ctx<'_>, lead: &Lead, state: Leader, enabled: bool) -> Facts {
        let now = ctx.now_ms;
        let place = &ctx.state.place;
        let mut f = Facts {
            now_ms: now,
            enabled,
            leader: state,
            in_instance: place.in_instance(),
            my_instance: place.instance_id(),
            loading: place.loading(),
            door_known: false,
            ..Facts::default()
        };
        // which dungeon, and its entrance
        let phase = self.machine.phase();
        let found = if place.in_instance() {
            dungeon_in(ctx).or(self.dungeon)
        } else if matches!(phase, Phase::Outside | Phase::Away) {
            match state {
                Leader::Dungeon(_) => lead
                    .rec
                    .and_then(|r| {
                        ctx.world
                            .dungeons()
                            .iter()
                            .position(|d| u32::from(d.zone_key) == r.zone_id)
                    })
                    // (the entrance the leader was last seen at, when the zone is not a dungeon's)
                    .or_else(|| {
                        self.sight
                            .near_entrance
                            .then(|| dungeon_at(ctx, self.sight.pos))
                            .flatten()
                    }),
                Leader::Vanished => dungeon_at(ctx, self.sight.pos),
                _ => self.dungeon,
            }
        } else {
            self.dungeon
        };
        if found != self.dungeon {
            self.dungeon = found;
            self.door = None;
        }
        let me = ctx.state.me.pos;
        let dungeon = self.dungeon.and_then(|i| ctx.world.dungeons().get(i));
        if let Some(d) = dungeon {
            if self.door.is_none() && !place.in_instance() {
                let near = if state == Leader::Vanished {
                    self.sight.pos
                } else {
                    self.main_pos.unwrap_or(me)
                };
                self.door = pick_door(d, near);
            }
            let entry_limit = ctx.cfg.dungeons.entry_limit.then_some(d.enter_count);
            let blocked = ctx
                .state
                .party
                .as_deref()
                .is_some_and(|p| p.blocked(d.zone_group, entry_limit, now));
            f.door_known = self.door.is_some() && !blocked;
            let can_raise = ctx.gm_allowed() && ctx.cfg.behavior.dungeon_level_up;
            f.can_raise = can_raise;
            f.level_low = ctx.state.me.level < d.level_min && !can_raise;
            if let Some(door) = self.door {
                f.door_dist = flat(me, door.pos);
                f.at_door = f.door_dist <= DOOR_RANGE;
                f.door_in_view = dun::find_doodad(ctx, door.template, door.pos).is_some();
            }
            if place.in_instance() {
                if let Some((door, in_view)) = dun::exit_door_of(ctx, d) {
                    f.at_exit = flat(me, door.pos) <= DOOR_RANGE;
                    f.exit_in_view = in_view;
                }
            }
        }
        f.trip_failed = matches!(self.machine.phase(), Phase::Door)
            && matches!(ctx.motion.status(), MotionStatus::Failed(_));
        // what the server said to the casts
        if matches!(phase, Phase::Cast | Phase::Door) {
            self.read_errors(ctx, &mut f);
        }
        if self.exit_cast.0 != 0 {
            f.exit_refused = self.exit_refusals(ctx) > 0;
        }
        f
    }

    /// Reads the refusals the server sent since the last entry cast.
    fn read_errors(&self, ctx: &Ctx<'_>, f: &mut Facts) {
        for &(t, e) in ctx.state.errors.iter() {
            if t <= self.err_mark {
                continue;
            }
            match e {
                StateError::Msg(code) => {
                    if let Some(x) = entry::classify(code) {
                        f.refused = Some(x);
                    }
                }
                StateError::Skill { skill, result }
                    if skill == self.last_cast_skill && skill != 0 && result != 0 =>
                {
                    f.cast_refused = true;
                }
                _ => {}
            }
        }
    }

    /// How many times the server refused the exit skill since it was cast.
    fn exit_refusals(&self, ctx: &Ctx<'_>) -> u8 {
        let (skill, at, _) = self.exit_cast;
        ctx.state
            .errors
            .iter()
            .filter(|(t, e)| {
                *t > at.saturating_sub(1)
                    && matches!(e, StateError::Skill { skill: s, result } if *s == skill && *result != 0)
            })
            .count()
            .min(255) as u8
    }

    fn on_phase(&mut self, ctx: &mut Ctx<'_>, from: Phase, to: Phase, leader: u32) {
        let name = self
            .dungeon
            .and_then(|i| ctx.world.dungeons().get(i))
            .map_or_else(String::new, |d| d.name.to_string());
        let why = self
            .machine
            .why()
            .filter(|_| to == Phase::Away)
            .map_or(String::new(), |w| format!(": {}", w.word()));
        ctx.log.say(format!(
            "companion: {} ({name}){why}",
            phase_word(to, Mode::Follow)
        ));
        // inside for the first time: the entry counts against the limit
        if to == Phase::Inside && !matches!(from, Phase::Exit | Phase::Inside) {
            if let Some(d) = self.dungeon.and_then(|i| ctx.world.dungeons().get(i)) {
                let (group, now) = (d.zone_group, ctx.now_ms);
                ctx.state.party_mem().note_entry(group, now);
                ctx.log.say(format!(
                    "companion: inside {} (instance {})",
                    d.name,
                    ctx.state.place.instance_id()
                ));
            }
        }
        if to == Phase::Away {
            self.away_told = false;
        }
        if to == Phase::Away && !self.away_told && !name.is_empty() {
            self.away_told = true;
            if let Some(who) = leader_name(ctx, leader) {
                comp::say_to(ctx, &who, Say::CannotEnter, &name);
            }
        }
    }

    // ---- what to walk to --------------------------------------------------------------------

    /// The place the bot walks to when nothing else is going on: a speaker who said `come`, the
    /// entrance of a dungeon that was asked for, or the leader; `None` to stand.
    fn target(
        &mut self,
        ctx: &mut Ctx<'_>,
        lead: &Lead,
        mode: Mode,
        come: u32,
        ask: Option<u32>,
    ) -> Option<[f32; 3]> {
        let my = ctx.state.place.instance_id();
        let now = ctx.now_ms;
        if come != 0 {
            let who = lead_of(ctx, come);
            match lead::position(who.view, who.rec, my, now) {
                Some(p) => {
                    if flat(ctx.state.me.pos, p) <= follow::START_M && !ctx.motion.is_moving() {
                        comp::mem_of(ctx.state).come = 0;
                    }
                    return Some(p);
                }
                None => comp::mem_of(ctx.state).come = 0,
            }
        }
        if mode == Mode::Stay {
            return None;
        }
        if let Some(group) = ask {
            if self.machine.phase() == Phase::Outside {
                let door = ctx
                    .world
                    .dungeons()
                    .iter()
                    .find(|d| d.zone_group == group)
                    .and_then(|d| pick_door(d, ctx.state.me.pos));
                if let Some(door) = door {
                    self.arrived_at(ctx, door.pos, lead.char_id, group);
                    return Some(door.pos);
                }
            }
        }
        lead::position(lead.view, lead.rec, my, now)
    }

    /// The bot stands at the entrance it was sent to: tell the leader once.
    fn arrived_at(&mut self, ctx: &mut Ctx<'_>, door: [f32; 3], leader: u32, group: u32) {
        if flat(ctx.state.me.pos, door) > follow::START_M || ctx.motion.is_moving() {
            return;
        }
        if std::mem::replace(&mut comp::mem_of(ctx.state).dungeon_told, true) {
            return;
        }
        let name = ctx
            .world
            .dungeons()
            .iter()
            .find(|d| d.zone_group == group)
            .map_or_else(String::new, |d| d.name.to_string());
        if let Some(who) = leader_name(ctx, leader) {
            comp::say_to(ctx, &who, Say::AtDoor, &name);
        }
        ctx.log.say(format!("companion: at the entrance of {name}"));
    }

    // ---- fights --------------------------------------------------------------------------------

    /// Takes up the leader's target: always after an `assist` order, else while the bot follows
    /// (see [`follow::should_assist`]).
    fn assist(&mut self, ctx: &mut Ctx<'_>, lead: &Lead, ordered: bool, following: bool) {
        let now = ctx.now_ms;
        if ctx.combat.target().is_some() || !(ordered || following) {
            return;
        }
        let Some(team) = ctx.state.team() else { return };
        let Some(obj) = team
            .remote_of(lead.char_id)
            .map(|r| r.obj_id)
            .filter(|&o| o != 0)
        else {
            return;
        };
        let Some((target, at_ms)) = team.target_of(obj) else {
            return;
        };
        let Some(u) = ctx.state.units.get(&target) else {
            return;
        };
        let faction = ctx.state.me.faction;
        let fair_game = !u.dead()
            && crate::bot::combat::attackable(ctx.world, faction, u).is_some()
            && !ctx.state.avoids(target, u.id, now);
        let leader = lead::position(lead.view, lead.rec, ctx.state.place.instance_id(), now);
        let take = follow::should_assist(&follow::AssistIn {
            now_ms: now,
            ordered,
            following,
            target_at_ms: at_ms,
            fair_game,
            target: u.pos,
            leader,
            me: ctx.state.me.pos,
        });
        if take {
            ctx.combat.engage_assist(target);
            ctx.log
                .debug(format!("companion: assisting against unit {target}"));
        }
    }

    // ---- carrying out the machine's intent ---------------------------------------------------

    fn carry_out(
        &mut self,
        ctx: &mut Ctx<'_>,
        intent: Intent,
        walk_to: Option<[f32; 3]>,
        lead: &Lead,
        ask: Option<u32>,
    ) {
        let _ = (lead, ask);
        match intent {
            Intent::Follow => self.follow_step(ctx, walk_to),
            Intent::Stand => self.hold(ctx),
            Intent::GoToDoor => self.go_to_door(ctx),
            Intent::CastEntry => self.cast_entry(ctx),
            Intent::RaiseLevel => self.raise_level(ctx),
            Intent::GoToExit => self.go_to_exit(ctx),
            Intent::CastExit => self.cast_exit(ctx),
            Intent::LeaveTeam => self.leave_team(ctx),
        }
    }

    fn hold(&mut self, ctx: &mut Ctx<'_>) {
        if ctx.motion.is_moving() {
            ctx.motion.stop();
        }
    }

    /// Follows the target: walk when it is far, stand when near, rest when hurt.
    fn follow_step(&mut self, ctx: &mut Ctx<'_>, target: Option<[f32; 3]>) {
        let now = ctx.now_ms;
        let Some(t) = target else {
            self.stop_resting(ctx);
            self.hold(ctx);
            return;
        };
        let me = ctx.state.me.pos;
        let threatened = ctx
            .state
            .threats
            .iter()
            .any(|&(_, at)| now.saturating_sub(at) < THREAT_MS);
        let action = follow::decide(&Inputs {
            me,
            leader: t,
            moving: ctx.motion.is_moving(),
            dest: ctx.motion.destination(),
            hp_pct: ctx.state.me.hp_pct(),
            resting: self.resting,
            threatened,
            slot: self.slot(ctx),
        });
        if action == Action::Rest {
            if !self.resting {
                self.resting = true;
                manner::rest_begin(ctx);
            }
            self.word = "resting";
            self.hold(ctx);
            manner::rest_tick(ctx);
            return;
        }
        self.stop_resting(ctx);
        if flat(me, t) > follow::CATCH_UP_M {
            ctx.combat.note_travelling(now);
        }
        match action {
            Action::Hold => self.hold(ctx),
            Action::Rest | Action::Keep => {}
            Action::Walk {
                x,
                y,
                radius,
                pace,
                far,
                ..
            } => {
                if far {
                    let aimed = ctx.motion.is_moving()
                        && ctx.motion.destination().is_some_and(|(dx, dy)| {
                            flat([dx, dy, 0.0], [x, y, 0.0]) < FAR_RETARGET_M
                        });
                    if !aimed {
                        ctx.travel(x, y, radius, pace);
                    }
                } else {
                    if ctx.state.place.in_instance() {
                        dun::match_floor(ctx, [x, y, t[2]]);
                    }
                    ctx.motion.chase_to(x, y, radius, pace, now);
                }
            }
        }
    }

    fn stop_resting(&mut self, ctx: &mut Ctx<'_>) {
        if std::mem::take(&mut self.resting) {
            manner::rest_end(ctx);
        }
    }

    /// The number of this bot among the bots of the party (0, 1, ...): the stop distance differs.
    fn slot(&self, ctx: &Ctx<'_>) -> u8 {
        let me = ctx.state.me.char_id;
        ctx.state.team().map_or(0, |t| {
            t.remote
                .iter()
                .filter(|r| r.char_id < me && ctx.hub.is_bot_char(r.char_id))
                .count()
                .min(255) as u8
        })
    }

    fn go_to_door(&mut self, ctx: &mut Ctx<'_>) {
        let Some(door) = self.door else { return };
        self.stop_resting(ctx);
        let moved = ctx
            .motion
            .destination()
            .is_none_or(|(x, y)| flat([x, y, 0.0], door.pos) > DOOR_RETARGET_M);
        ctx.combat.note_travelling(ctx.now_ms);
        if !ctx.motion.is_moving() || moved {
            ctx.travel(door.pos[0], door.pos[1], DOOR_STOP, Pace::Run);
        }
    }

    fn cast_entry(&mut self, ctx: &mut Ctx<'_>) {
        let Some(door) = self.door else { return };
        let Some((obj, pos)) = dun::find_doodad(ctx, door.template, door.pos) else {
            return;
        };
        self.hold(ctx);
        ctx.motion.face(pos[0], pos[1]);
        let skill = if door.skill != 0 {
            door.skill
        } else {
            ctx.world.doodad_use_skill(door.template)
        };
        self.last_cast_skill = skill;
        self.err_mark = ctx.now_ms;
        let me = ctx.state.me.obj;
        ctx.send(cs::start_skill_doodad(skill, me, obj));
        ctx.log.say(format!(
            "companion: casting the entrance skill {skill} on doodad {obj}"
        ));
    }

    fn raise_level(&mut self, ctx: &mut Ctx<'_>) {
        // (the refusal that sent us here is answered: only later ones count)
        self.err_mark = ctx.now_ms;
        let Some(d) = self.dungeon.and_then(|i| ctx.world.dungeons().get(i)) else {
            return;
        };
        let (from, to) = (ctx.state.me.level, d.level_min);
        if from < to && ctx.gm(&format!("/level {to}")) {
            ctx.log.say(format!(
                "companion: raised the level {from} -> {to} (GM) for {}",
                d.name
            ));
        }
    }

    fn go_to_exit(&mut self, ctx: &mut Ctx<'_>) {
        let Some(d) = self.dungeon.and_then(|i| ctx.world.dungeons().get(i)) else {
            return;
        };
        let Some((door, _)) = dun::exit_door_of(ctx, d) else {
            return;
        };
        let now = ctx.now_ms;
        let moved = ctx
            .motion
            .destination()
            .is_none_or(|(x, y)| flat([x, y, 0.0], door.pos) > DOOR_RETARGET_M);
        if !ctx.motion.is_moving() || moved {
            dun::match_floor(ctx, door.pos);
            ctx.motion
                .go_to(door.pos[0], door.pos[1], DOOR_STOP, Pace::Run, now);
        }
    }

    fn cast_exit(&mut self, ctx: &mut Ctx<'_>) {
        let Some(d) = self.dungeon.and_then(|i| ctx.world.dungeons().get(i)) else {
            return;
        };
        let Some((door, true)) = dun::exit_door_of(ctx, d) else {
            return;
        };
        let Some((obj, pos)) = dun::find_doodad(ctx, door.template, door.pos) else {
            return;
        };
        self.hold(ctx);
        ctx.motion.face(pos[0], pos[1]);
        let skill = if door.skill != 0 {
            door.skill
        } else {
            ctx.world.doodad_use_skill(door.template)
        };
        self.exit_cast = (skill, ctx.now_ms.max(1), 0);
        let me = ctx.state.me.obj;
        ctx.send(cs::start_skill_doodad(skill, me, obj));
        ctx.log.say(format!(
            "companion: casting the exit skill {skill} on doodad {obj}"
        ));
    }

    /// The exit does nothing: leave the team, the server moves a member that left after a while.
    fn leave_team(&mut self, ctx: &mut Ctx<'_>) {
        let (team_id, me) = (
            ctx.state.team().map_or(0, |t| t.team_id),
            ctx.state.me.char_id,
        );
        if team_id != 0 {
            ctx.send(cs::ask_risky_team_action(team_id, me, cs::RISKY_LEAVE));
            ctx.log
                .say("companion: the exit does nothing; leaving the party");
        }
    }
}

/// The name of character `char_id` in the bot's team.
fn leader_name(ctx: &Ctx<'_>, char_id: u32) -> Option<String> {
    ctx.state
        .team()?
        .remote_of(char_id)
        .map(|r| r.name.to_string())
}

/// A word for the phase, for the web UI and the log.
fn phase_word(phase: Phase, mode: Mode) -> &'static str {
    match (phase, mode) {
        (Phase::Outside, Mode::Stay) => "staying",
        (Phase::Outside, _) => "following",
        (p, _) => p.word(),
    }
}

/// The goal's weight before the situation is looked at (`brain::base_weight` calls this): none,
/// the goal is not drawn by weight but started by the party duty and scored by [`score`].
pub fn base_weight(_behavior: &BehaviorCfg, _temper: &Temperament) -> f32 {
    0.0
}

/// The score of the goal for this tick: high while the bot is in the party of a real player (the
/// duty starts it anyway; this keeps the choice on it after another goal ended), 0 otherwise.
pub fn score(ctx: &Ctx<'_>, _base: f32) -> f32 {
    if ctx.cfg.party.join_player_parties
        && comp::in_player_party(ctx)
        && !comp::peek(ctx.state).is_some_and(|m| m.leave)
    {
        1000.0
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_goal_shows_as_idle_or_traveling_or_fighting() {
        let g = Companion::new();
        assert_eq!(g.activity(false, false), Activity::Idle);
        assert_eq!(g.activity(true, false), Activity::Traveling);
        assert_eq!(g.activity(true, true), Activity::Fighting);
        assert!(!g.owns_combat());
        let mut text = String::new();
        g.describe(&mut text);
        assert!(text.starts_with("companion"), "{text}");
    }

    #[test]
    fn the_goal_is_not_drawn_by_weight() {
        let b = BehaviorCfg::default();
        assert_eq!(base_weight(&b, &Temperament::NEUTRAL), 0.0);
    }

    #[test]
    fn a_goal_that_was_not_started_is_one_pointer() {
        assert_eq!(
            std::mem::size_of::<Companion>(),
            std::mem::size_of::<usize>()
        );
    }

    #[test]
    fn the_phases_have_words_and_a_staying_bot_says_so() {
        assert_eq!(phase_word(Phase::Outside, Mode::Follow), "following");
        assert_eq!(phase_word(Phase::Outside, Mode::Stay), "staying");
        assert_eq!(phase_word(Phase::Door, Mode::Follow), Phase::Door.word());
    }

    #[test]
    fn the_flat_distance_ignores_the_height() {
        assert_eq!(flat([0.0, 0.0, 0.0], [3.0, 4.0, 100.0]), 5.0);
    }
}
