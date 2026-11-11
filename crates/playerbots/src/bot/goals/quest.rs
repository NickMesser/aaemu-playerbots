//! The quest goal: pick a quest, accept it from its giver, do its objectives, turn it in.
//!
//! A port of the decision logic of the original bots' quest controllers (intake and lifecycle):
//!
//! * **Pick.** A quest already in the journal that the bot can work on comes first (ready quests
//!   are turned in, then the story line, then the others); otherwise the givers near the bot
//!   offer quests ([`select`]): story first, then the nearest, within 300 m and, if nothing is
//!   there, within 1500 m. The bot keeps to one quest at a time.
//! * **Accept.** Walk to the giver (an NPC or a doodad, found in the entity view or at its static
//!   spawns), send `CSStartQuestContext`, wait for the server's `SCQuestContextStarted`.
//! * **Objectives.** One objective at a time, the first whose server counter is not met
//!   ([`objective`]): talk to an NPC, kill monsters (the combat controller fights and loots),
//!   gather an item from droppers or doodads, use a doodad or an item with the skill the client
//!   would cast, stand in a sphere. Level, craft and the other objective kinds are unsupported:
//!   the quest is quarantined and dropped from the journal.
//! * **Turn in.** Walk to the report NPC or doodad and send `CSCompleteQuestContext`; quests that
//!   complete by themselves are waited for.
//! * **Progression.** A bot that has found nothing within 1500 m for several scans in a row does
//!   not stay where it is: it plans a trip ([`progress`]) to the giver of the next quest of its
//!   race's story line, else to the zone with the most quests for its level, else to monsters of
//!   its level, and walks there until quests come within reach. Trivial side quests (far below the
//!   bot's level) give way to such a trip.
//! * **Failures** are typed ([`Failure`]) and remembered in [`Memory`]: a failed quest backs off
//!   for 30 s, 2 min and then 8 min, and the fourth failure in a row quarantines a side quest
//!   for the session (a story quest rests instead), an objective without progress for 4 minutes
//!   is skipped or fails its quest.
//!
//! The goal owns the combat controller: while a fight or a loot job is on, the goal waits and
//! re-issues its walk afterwards. All state of a run is boxed so the goal enum stays small.

mod memory;
mod objective;
pub mod progress;
mod select;

use std::fmt::Write;

use aa_proto::cs;
use aa_proto::sc_activity::quest_status;
use aa_world::{
    AtlasPoint, ObjKind, Quest as QuestData, QuestAtlas, QuestReport, SkillNeed, Vec3, World,
};
use smallvec::SmallVec;

use crate::bot::avoid::UNREACHABLE_MS;
use crate::bot::combat::{CombatStatus, Loss};
use crate::bot::ctx::Ctx;
use crate::bot::motion::{MotionStatus, MoveFailure, Pace};
use crate::bot::state::{dist2d, dist3d, BotState, StateError, TagOwner, UnitKind};
use crate::bot::watch::classify::{CreditNote, CreditStep};
use crate::bot::watch::{Finding, Role};
use crate::contest::{doodad_key, spot_key, unit_key};
use crate::events::EventBoard;

use objective::{Env, Plan, Slot};
use progress::{Dest, Outcome, Progress, Trip, Why};
use select::{Candidate, Giver, Judge};

pub use memory::{Failure, Memory, Verdict};

use super::{jitter_ms, FailReason, GoalStatus};

/// Length of a quest run in milliseconds; the goal ends and the brain chooses again.
const RUN_MS: (u64, u64) = (480_000, 900_000);
/// Below this many percent of hit points the goal ends (the brain rests).
const LOW_HP_PCT: u8 = 35;
/// The server has this long to answer an accept or a turn-in.
const ACK_MS: u64 = 5_000;
/// Gap between two talk packets for the same objective.
const TALK_GAP_MS: u64 = 3_000;
/// Minimum gap between two casts, on top of the skill's cast time.
const CAST_GAP_MS: u64 = 2_000;
/// Minimum gap between two casts of a quest item. The server needs a few seconds to credit the
/// objective (its quest engine evaluates in batches) and to finish the cast: a second cast after
/// the usual 2 s found the quest item gone once the quest had turned ready and completed, and
/// the server logged a cancelled cast for each of them.
const ITEM_CAST_GAP_MS: u64 = 4_500;
/// An objective without a counter change for this long is skipped or fails its quest.
const NO_PROGRESS_MS: u64 = 240_000;
/// How long a quest with all objectives met may stay unready before the bot tries to complete it.
const READY_WAIT_MS: u64 = 8_000;
/// How long an automatic completion is waited for before the bot asks for it.
const AUTO_WAIT_MS: u64 = 6_000;
/// Distance at which an NPC can be talked to / handed a quest.
const NPC_RANGE: f32 = 4.5;
/// The server's reach for a quest request at an NPC: 3 m interaction + 2 m tolerance + 10 m
/// slack (`npc_out_of_quest_reach`). Measured in 3D, so a bot under a floor is out of reach.
const QUEST_NPC_REACH: f32 = 15.0;
/// The server's reach for a quest request at a doodad (5 m use range + 15 m allowance).
const QUEST_DOODAD_REACH: f32 = 20.0;
/// The bot stops this far from an NPC.
const NPC_STOP: f32 = 3.0;
/// Distance at which a doodad is used.
const DOODAD_RANGE: f32 = 3.5;
/// The bot stops this far from a doodad.
const DOODAD_STOP: f32 = 2.0;
/// Monsters further away than this (metres) are not picked from the view.
const VIEW_RADIUS: f32 = 70.0;
/// Spawns closer than this are in view already; if nothing is there, they are empty.
const SPAWN_MIN_M: f32 = 10.0;
/// The bot stops this far from a spawn it searches.
const SPAWN_STOP_M: f32 = 6.0;
/// Searched spots are remembered in cells of this size.
const CELL_M: f32 = 20.0;
/// A monster that was lost is not picked again for this long.
const IGNORE_MS: u64 = 30_000;
/// Spots tried for a missing NPC or doodad before the bot gives up.
const MAX_SPOTS: u8 = 3;
/// Casts or packets without progress after which an objective fails.
const MAX_TRIES: u8 = 6;
/// Other characters within this many metres make a spawn "crowded".
const CROWD_RADIUS_M: f32 = 60.0;
/// Another character within this many metres of a kill may have tagged the monster first, which
/// takes the quest credit away from the bot that strikes last.
const TAG_SHARE_RADIUS_M: f32 = 40.0;
/// A crowded spawn is given this long between two rounds of searching it (the respawns).
const CROWD_ROUND_GAP_MS: u64 = 30_000;
/// Rounds of searching a crowded spawn before the quest rests (an uncrowded one gets two).
const CROWD_ROUNDS: u8 = 5;
/// A claim on a target or spot is renewed this often.
const CLAIM_RENEW_MS: u64 = 10_000;
/// How long a bot queues for a single item-use target another bot holds before it rests the
/// quest (the server's duel dummy, quest 2396).
const CROWD_WAIT_MS: u64 = 30_000;
/// The same item cast this many times with nothing changing (not the objective counter, not the
/// stack) is not in the server's bag any more: the bot's picture is out of date.
const STALE_CASTS: u8 = 2;
/// A giver or target must have shown up this long after the bot arrived at its spawn.
const ARRIVE_GRACE_MS: u64 = 2_500;
/// How long the counter of an item the quest supplies may lag behind the quest start.
const SUPPLY_WAIT_MS: u64 = 20_000;
/// A fight against one monster that lasts longer is given up (the monster is out of reach, behind
/// an obstacle or too tough) and the monster is left alone for [`GIVE_UP_IGNORE_MS`].
const FIGHT_BUDGET_MS: u64 = 45_000;
/// How long a monster that took too long is left alone.
const GIVE_UP_IGNORE_MS: u64 = 90_000;
/// Gap between two walk commands towards the same spot.
const GO_GAP_MS: u64 = 1_000;
/// Spawns of an objective's targets farther away than this are not searched (the bot does not
/// cross the continent for a monster or a plant).
const SEARCH_RADIUS_M: f32 = 1_500.0;
/// A round of searched spots counts as empty only this long after the previous one (monsters
/// and plants respawn).
const ROUND_GAP_MS: u64 = 20_000;
/// A trip the motion controller cuts off at its time limit is continued, at most this many legs.
const MAX_LEGS: u8 = 6;
/// A leg must have brought the bot at least this much closer to count as progress.
const LEG_MIN_GAIN_M: f32 = 30.0;
/// Gap between two scans for a quest, and between two looks for quests on a trip.
const SCAN_GAP_MS: u64 = 5_000;
/// A trip that got this much closer to its destination has the whole of its legs again.
const TRIP_CHECKPOINT_M: f32 = 250.0;

/// What the goal is working on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Choose the next piece of work.
    Pick,
    /// Walk to a giver and accept a quest.
    Accept,
    /// Do the objectives of an active quest.
    Work,
    /// Walk to the report target and hand the quest in.
    TurnIn,
    /// Walk to a place with quests: nothing is near ([`progress`]).
    Travel,
}

/// Where the current piece of work is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Getting to the target.
    Walk,
    /// A packet is out, waiting for the server.
    Wait,
}

/// What a step of a walk came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Walk {
    Moving,
    Arrived,
    Failed,
}

/// Something the bot walks to and acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Npc(u32),
    Doodad(u32),
}

/// What reaching a target came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// The target's object id; the bot is next to it.
    Here(u32),
    Walking,
    /// Not at any of its spawns.
    Missing,
    /// The way is blocked.
    Blocked,
}

/// Doing quests. See the module docs.
#[derive(Debug, Default)]
pub struct Quest(Box<Inner>);

impl Quest {
    /// Called once when the brain picks the goal.
    pub fn start(&mut self, ctx: &mut Ctx<'_>) {
        self.0.start(ctx);
    }

    /// One tick.
    pub fn tick(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let status = self.0.tick(ctx);
        if ctx.motion.is_moving() && ctx.combat.target().is_none() {
            // Walking to an objective: the combat controller only fights back against close
            // attackers (a monster the quest wants is engaged by the goal itself).
            ctx.combat.note_travelling(ctx.now_ms);
        }
        status
    }

    /// Called when the goal ends.
    pub fn stop(&mut self, ctx: &mut Ctx<'_>) {
        self.0.stop(ctx);
    }

    /// Appends what the bot is doing.
    pub fn describe(&self, out: &mut String) {
        if self.0.detail.is_empty() {
            out.push_str("quest");
        } else {
            out.push_str(&self.0.detail);
        }
    }
}

#[derive(Debug)]
struct Inner {
    mode: Mode,
    stage: Stage,
    quest: u32,
    giver: Option<Giver>,
    /// Counter index of the objective being worked, `u8::MAX` for none.
    obj: u8,
    /// Counter indexes the bot gave up on (bit per index).
    skip: u8,
    tries: u8,
    spots_tried: u8,
    spot: Option<(f32, f32)>,
    go_ms: u64,
    /// Distance to the spot when the current leg of the trip started.
    leg_from: f32,
    /// Legs of the current trip done after a time-out.
    legs: u8,
    /// When the last round of searched spots ended.
    round_ms: u64,
    since_ms: u64,
    sent_ms: u64,
    arrived_ms: u64,
    end_ms: u64,
    best_have: i32,
    progress_ms: u64,
    fighting: Option<u32>,
    engaged_ms: u64,
    visited: SmallVec<[(i32, i32); 4]>,
    ignore: SmallVec<[(u32, u64); 4]>,
    /// The claim on the monster, doodad or spot in work: its key and the bot clock of its last
    /// renewal.
    claim_key: u64,
    claim_ms: u64,
    /// Item instances that were cast without effect (see [`STALE_CASTS`]).
    stale: SmallVec<[u64; 2]>,
    /// The last item cast `(instance, stack count, objective counter, casts in a row)`.
    use_note: (u64, i32, i32, u8),
    /// Bot clock at which the bot started waiting for a single item-use target another bot
    /// holds, 0 when it is not waiting.
    use_wait_ms: u64,
    /// The kill or cast the server confirmed that has not been credited yet.
    credit: Option<CreditNote>,
    /// The skill of the cast that is the objective itself (an item or a doodad use), 0 for none;
    /// its confirmation is the start of a [`CreditNote`].
    cast_skill: u32,
    /// Spawn points of the target in work where the bot stood and found nothing.
    stood: u8,
    /// Bot clock of the next look for quests on a trip.
    trip_scan_ms: u64,
    /// Distance to the destination at the last checkpoint of the trip.
    cp_dist: f32,
    detail: String,
    detail_key: u64,
}

impl Default for Inner {
    fn default() -> Self {
        Inner {
            mode: Mode::Pick,
            stage: Stage::Walk,
            quest: 0,
            giver: None,
            obj: u8::MAX,
            skip: 0,
            tries: 0,
            spots_tried: 0,
            spot: None,
            go_ms: 0,
            leg_from: 0.0,
            legs: 0,
            round_ms: 0,
            since_ms: 0,
            sent_ms: 0,
            arrived_ms: 0,
            end_ms: 0,
            best_have: -1,
            progress_ms: 0,
            fighting: None,
            engaged_ms: 0,
            visited: SmallVec::new(),
            ignore: SmallVec::new(),
            claim_key: 0,
            claim_ms: 0,
            stale: SmallVec::new(),
            use_note: (0, 0, 0, 0),
            use_wait_ms: 0,
            credit: None,
            cast_skill: 0,
            stood: 0,
            trip_scan_ms: 0,
            cp_dist: f32::MAX,
            detail: String::new(),
            detail_key: 0,
        }
    }
}

fn cell(x: f32, y: f32) -> (i32, i32) {
    ((x / CELL_M).floor() as i32, (y / CELL_M).floor() as i32)
}

/// The quest memory of the bot.
fn mem(state: &mut BotState) -> &mut Memory {
    state.quest_mem.get_or_insert_with(Default::default)
}

fn usable(state: &BotState, quest: u32, now_ms: u64) -> bool {
    state
        .quest_mem
        .as_ref()
        .is_none_or(|m| m.usable(quest, now_ms))
}

/// The progression memory of the bot.
fn prog(state: &mut BotState) -> &mut Progress {
    mem(state).progress.get_or_insert_with(Default::default)
}

/// The trip under way, if any (does not allocate the memory).
fn current_trip(state: &BotState) -> Option<Trip> {
    state
        .quest_mem
        .as_ref()
        .and_then(|m| m.progress.as_ref())
        .and_then(|p| p.trip)
}

/// The table of where the quests are, built by the first bot that asks. The quests in it are the
/// ones a bot can do at all (every objective and the turn-in have a plan); whether a bot may do
/// one now is judged per bot by [`Eligibility`].
pub fn atlas(world: &World) -> &QuestAtlas {
    world.quest_atlas(&|q| {
        objective::supported(
            &Env {
                world,
                from: None,
                navmesh: false,
            },
            q,
            u8::MAX,
        )
        .is_ok()
    })
}

/// What the bot's data says about a quest, in one place for the picker (`find_candidate`) and for
/// the choice of a place to go to ([`plan_target`]).
struct Eligibility<'a> {
    world: &'a World,
    facts: Env<'a>,
    level: u8,
    faction: u32,
    who: aa_world::Who<'a>,
    judge: Judge<'a>,
    /// The running world events: a quest whose targets only exist during an event waits.
    events: &'a EventBoard,
}

impl Eligibility<'_> {
    /// Whether the bot may start `quest` at `giver` now: level, done, in the journal, resting,
    /// prerequisites, the race and quest gates of the start, a giver that is not hostile and every
    /// objective within the bot's ability.
    fn check(&self, quest: &QuestData, giver: Giver) -> bool {
        self.explain(quest, giver).is_ok()
    }

    /// Like [`Eligibility::check`], with the reason for a no.
    fn explain(&self, quest: &QuestData, giver: Giver) -> Result<(), &'static str> {
        match self.judge.check(quest) {
            Ok(()) => {}
            Err(select::Skip::Level) => return Err("level too low"),
            Err(select::Skip::Done) => return Err("done"),
            Err(select::Skip::Active) => return Err("in the journal"),
            Err(select::Skip::Prerequisite) => return Err("prerequisite not done"),
            Err(select::Skip::Resting) => return Err("resting after a failure"),
            Err(select::Skip::NoGiver) => return Err("no giver"),
        }
        if self
            .world
            .zone_key_for_id(quest.zone_id)
            .is_some_and(|z| self.world.zone_closed(z))
        {
            return Err("closed zone");
        }
        if !self.world.start_gates().open(quest.id, &self.who) {
            return Err("start requirements not met");
        }
        if let Giver::Npc(t) = giver {
            let hostile = self.world.npc(t).is_some_and(|n| {
                self.world.relation(self.faction, n.faction_id) == aa_world::Relation::Hostile
            });
            if hostile {
                return Err("giver is hostile");
            }
        }
        if let Some(targets) = event_targets(self.world, quest) {
            // An event quest is only taken while the event that spawns its targets runs.
            if !targets.iter().any(|t| self.events.runs_kill_target(*t)) {
                return Err("event not running");
            }
        }
        if objective::supported(&self.facts, quest, self.level).is_err() {
            return Err("objectives not supported");
        }
        Ok(())
    }
}

/// The NPC templates that make `quest` event-gated, or `None` when its targets exist all the
/// time. A seasonal event quest (category 55) is always gated, and so is a hunt that names a
/// wave NPC of a tower defence (the Rift quests 6209/6211: their groups mix wave kills with
/// mobs the event spawns).
fn event_targets(world: &World, quest: &QuestData) -> Option<Vec<u32>> {
    let mut targets: Vec<u32> = Vec::new();
    let mut hunts = 0usize;
    for step in quest.progress.iter() {
        for obj in step.objectives.iter() {
            if let ObjKind::Hunt { npcs, .. } = &obj.kind {
                hunts += 1;
                targets.extend_from_slice(npcs);
            }
        }
    }
    targets.sort_unstable();
    targets.dedup();
    if quest.category == select::EVENT_CATEGORY {
        return Some(targets);
    }
    let any_event = hunts > 0 && targets.iter().any(|t| world.events().is_wave_npc(*t));
    any_event.then_some(targets)
}

/// Runs `f` with the [`Eligibility`] of the bot at bot-clock `now_ms`.
fn with_eligibility<R>(
    world: &World,
    state: &BotState,
    now_ms: u64,
    navmesh: bool,
    events: &EventBoard,
    f: impl FnOnce(&Eligibility<'_>) -> R,
) -> R {
    let (level, faction) = (state.me.level, state.me.faction);
    let from = Vec3::new(state.me.pos[0], state.me.pos[1], state.me.pos[2]);
    let done = |q: u32| state.quests.is_done(q);
    let owns = |item: u32| state.find_item(item).is_some();
    let active = |q: u32| state.quests.row(q).is_some();
    let usable_now = |q: u32| usable(state, q, now_ms);
    f(&Eligibility {
        world,
        facts: Env {
            world,
            from: Some(from),
            navmesh,
        },
        level,
        faction,
        who: aa_world::Who {
            level,
            race: state.me.race,
            gender: state.me.gender,
            mother_faction: world.mother_faction(faction),
            done: &done,
            in_journal: &active,
            owns: &owns,
        },
        judge: Judge {
            level,
            done: &done,
            active: &active,
            usable: &usable_now,
        },
        events,
    })
}

/// Where to go when no quest is near: the giver of the next story quest, else the best zone, else
/// monsters of the bot's level. See [`progress`].
fn plan_target(ctx: &Ctx<'_>) -> Option<Dest> {
    let world = ctx.world;
    let state = &*ctx.state;
    let now = ctx.now_ms;
    let (level, faction, race) = (state.me.level, state.me.faction, state.me.race);
    let me = (state.me.pos[0], state.me.pos[1]);
    let atlas = atlas(world);
    let memory = state.quest_mem.as_ref().and_then(|m| m.progress.as_deref());
    // (a place in a zone group at war is not a destination either)
    let blocked =
        |x: f32, y: f32| memory.is_some_and(|p| p.blocked(x, y, now)) || ctx.in_war_zone(x, y);
    let risky = |from: (f32, f32), to: (f32, f32)| {
        progress::deadly(world, faction, level, from, to) || progress::cut_off(world, from, to)
    };
    let settled = memory.is_some_and(|p| p.settled(now));
    with_eligibility(
        world,
        state,
        now,
        ctx.motion.uses_navmesh(),
        &ctx.hub.events,
        |elig| {
            let eligible = |pt: &AtlasPoint| {
                pt.level <= level.saturating_add(select::LEVEL_SLACK)
                    && !world.zone_closed(pt.zone)
                    && world.quest(pt.quest).is_some_and(|q| {
                        let giver = if pt.doodad {
                            Giver::Doodad(pt.giver)
                        } else {
                            Giver::Npc(pt.giver)
                        };
                        elig.check(q, giver)
                    })
            };
            let weight = |pt: &AtlasPoint| {
                if eligible(pt) {
                    progress::level_weight(level, pt.level)
                } else {
                    0.0
                }
            };
            if let Some(chain) = atlas.chain(race) {
                if tracing::enabled!(tracing::Level::DEBUG) {
                    // the first quest of the story line that is not done, and what is wrong with it
                    if let Some(q) = chain.quests.iter().find(|q| !state.quests.is_done(**q)) {
                        let verdict = world.quest(*q).map(|quest| {
                            let giver = quest
                                .starts
                                .iter()
                                .find_map(|s| match s {
                                    aa_world::QuestStart::Npc(t) => Some(Giver::Npc(*t)),
                                    aa_world::QuestStart::Doodad(t) => Some(Giver::Doodad(*t)),
                                    _ => None,
                                })
                                .unwrap_or(Giver::Npc(0));
                            elig.explain(quest, giver)
                        });
                        ctx.log
                            .debug(format!("progress: next story quest {q}: {verdict:?}"));
                    }
                }
                if let Some(t) =
                    progress::story_target(atlas, &chain.quests, me, &eligible, &blocked, &risky)
                {
                    return Some(t);
                }
            }
            if let Some(t) = progress::zone_target(atlas, me, &weight, &blocked, &risky) {
                return Some(t);
            }
            if settled {
                return None;
            }
            progress::grind_target(atlas, me, level, &blocked, &risky, &|hub| {
                progress::grind_spot(world, faction, level, hub)
            })
        },
    )
}

/// "Zone name (x, y)" for the logs.
fn place(world: &World, x: f32, y: f32) -> String {
    let zone = world
        .zone_key_at(x, y)
        .and_then(|k| world.zone_name(k))
        .unwrap_or("?");
    format!("{zone} ({x:.0}, {y:.0})")
}

impl Inner {
    fn start(&mut self, ctx: &mut Ctx<'_>) {
        *self = Inner::default();
        self.end_ms = ctx.now_ms + jitter_ms(ctx.rng, RUN_MS.0, RUN_MS.1);
        self.since_ms = ctx.now_ms;
        mem(ctx.state);
    }

    fn stop(&mut self, ctx: &mut Ctx<'_>) {
        ctx.combat.disengage();
        if ctx.motion.is_moving() {
            ctx.motion.stop();
        }
        self.fighting = None;
        ctx.hub.contest.release_bot(ctx.persona.id);
        // A death on the way to a far place counts against the trip: a road that kills twice is
        // not walked a third time.
        if ctx.state.me.dead && self.mode == Mode::Travel {
            let now = ctx.now_ms;
            if prog(ctx.state).died(now) {
                ctx.log
                    .say("progress: died twice on the way; giving the trip up");
            }
        }
    }

    fn tick(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let now = ctx.now_ms;
        match ctx.combat.status() {
            CombatStatus::TargetDead { .. } => {
                if let Some(obj) = self.fighting.take() {
                    ctx.hub.contest.release(unit_key(obj), ctx.persona.id);
                    self.note_kill(ctx, obj);
                }
            }
            CombatStatus::Lost => {
                if let Some(obj) = self.fighting.take() {
                    let unreachable = ctx.combat.last_loss().is_some_and(Loss::unreachable);
                    self.give_up(ctx, obj, IGNORE_MS, unreachable);
                }
            }
            _ => {}
        }
        if let (Some(obj), Some(target)) = (self.fighting, ctx.combat.target()) {
            if target == obj && now.saturating_sub(self.engaged_ms) > FIGHT_BUDGET_MS {
                ctx.log
                    .debug(format!("quest {}: giving up on monster {obj}", self.quest));
                // a monster that never lost a hit point in all that time is out of reach
                let unreachable = !ctx.combat.dealt_damage();
                ctx.combat.disengage();
                ctx.motion.stop();
                self.fighting = None;
                self.give_up(ctx, obj, GIVE_UP_IGNORE_MS, unreachable);
            }
        }
        if let Some(obj) = self.fighting {
            self.renew_claim(ctx, unit_key(obj));
        }
        if ctx.combat.busy() {
            // The controller defends the bot or loots; the walk is re-issued afterwards.
            return GoalStatus::Running;
        }
        // (a trip is not cut short by the end of the run: it has a time limit of its own)
        let travelling = self.mode == Mode::Travel;
        if (now >= self.end_ms && !travelling) || ctx.state.me.hp_pct() < LOW_HP_PCT {
            return GoalStatus::Done;
        }
        let status = match self.mode {
            Mode::Pick => self.pick(ctx),
            Mode::Accept => self.accept(ctx),
            Mode::Work => self.work(ctx),
            Mode::TurnIn => self.turn_in(ctx),
            Mode::Travel => self.travel(ctx),
        };
        self.refresh_detail(ctx);
        status
    }

    // ---- choosing -----------------------------------------------------------------------

    fn pick(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let now = ctx.now_ms;
        if let Some((quest, mode)) = self.best_active(ctx) {
            // (work in the journal is not an empty scan)
            prog(ctx.state).fruitful();
            self.begin(ctx, quest, mode, None);
            return GoalStatus::Running;
        }
        // A trip that was under way when the last run ended goes on.
        if self.resume_trip(ctx) {
            return GoalStatus::Running;
        }
        if now < mem(ctx.state).next_scan_ms {
            return GoalStatus::Running;
        }
        match self.scan(ctx) {
            Some(c) if !c.trivial => {
                prog(ctx.state).fruitful();
                self.accept_candidate(ctx, c);
                GoalStatus::Running
            }
            Some(c) => {
                // Only quests far below the bot's level are near: a place with better ones is
                // worth the walk, else they are better than nothing.
                prog(ctx.state).fruitful();
                if self.retry_soon(ctx) {
                    // a quest in the journal comes back soon: neither a trip nor a trivial quest,
                    // the brain fills the time
                    mem(ctx.state).next_scan_ms = now + SCAN_GAP_MS;
                    return GoalStatus::Failed(FailReason::NoTarget);
                }
                if !self.start_trip(ctx) {
                    self.accept_candidate(ctx, c);
                }
                GoalStatus::Running
            }
            None => self.nothing_near(ctx),
        }
    }

    /// The best acceptable quest within reach: within 300 m, else within 1500 m (a trivial one
    /// near the bot gives way to a worthwhile one farther away).
    fn scan(&self, ctx: &mut Ctx<'_>) -> Option<Candidate> {
        let near = self.find_candidate(ctx, select::NEAR_RADIUS);
        if near.as_ref().is_some_and(|c| !c.trivial) {
            return near;
        }
        self.find_candidate(ctx, select::FAR_RADIUS).or(near)
    }

    fn accept_candidate(&mut self, ctx: &mut Ctx<'_>, c: Candidate) {
        ctx.log.debug(format!(
            "quest {}: giver {:?} at {:.0} m",
            c.quest, c.giver, c.dist
        ));
        self.begin(ctx, c.quest, Mode::Accept, Some(c.giver));
    }

    /// A scan found nothing. After [`progress::EMPTY_SCANS`] of them in a row the bot goes where
    /// quests are; until then it looks again in a few seconds, and when there is nowhere to go the
    /// goal fails (the brain tries grinding and wandering).
    fn nothing_near(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let now = ctx.now_ms;
        let (empty, barren) = {
            let p = prog(ctx.state);
            let barren = p.barren(now);
            p.empty_scans = p.empty_scans.saturating_add(1);
            (p.empty_scans, barren)
        };
        if barren {
            ctx.log.say(
                "progress: no quest for this bot where the trip went; leaving that place alone for a while",
            );
        }
        mem(ctx.state).next_scan_ms = now + SCAN_GAP_MS;
        if empty < progress::EMPTY_SCANS {
            return GoalStatus::Running;
        }
        if !self.retry_soon(ctx) && self.start_trip(ctx) {
            return GoalStatus::Running;
        }
        GoalStatus::Failed(FailReason::NoTarget)
    }

    /// Whether a quest in the journal is only resting after a failure and may be tried again
    /// within [`progress::RETRY_WAIT_MS`]: the bot stays near it instead of going on a trip.
    fn retry_soon(&self, ctx: &Ctx<'_>) -> bool {
        let now = ctx.now_ms;
        let Some(m) = ctx.state.quest_mem.as_ref() else {
            return false;
        };
        ctx.state
            .quests
            .active
            .iter()
            .filter(|row| row.status != quest_status::FAILED)
            .any(|row| {
                m.retry_in(row.template, now)
                    .is_some_and(|t| t <= progress::RETRY_WAIT_MS)
            })
    }

    // ---- trips --------------------------------------------------------------------------

    /// Plans a trip and starts it. `false` when there is nowhere better to go (that is remembered
    /// for a while, so the planning does not run on every scan).
    fn start_trip(&mut self, ctx: &mut Ctx<'_>) -> bool {
        let now = ctx.now_ms;
        if prog(ctx.state).searched_in_vain(now) {
            return false;
        }
        let Some(target) = plan_target(ctx) else {
            prog(ctx.state).found_nothing(now);
            ctx.log
                .debug("progress: nowhere better to go; trying again later");
            return false;
        };
        let me = (ctx.state.me.pos[0], ctx.state.me.pos[1]);
        let trip = prog(ctx.state).begin(target, me, now);
        let world = ctx.world;
        let what = match target.why {
            Why::Story(q) => format!("for story quest {q} {}", quest_name(world, q)),
            Why::Zone(_) => format!("for quests suited to level {}", ctx.state.me.level),
            Why::Grind(_) => format!("to hunt monsters of level {}", ctx.state.me.level),
        };
        ctx.log.say(format!(
            "progress: heading to {} {what}: {:.0} m from {}, {} s allowed",
            place(world, target.x, target.y),
            target.dist,
            place(world, me.0, me.1),
            (trip.deadline_ms - now) / 1000
        ));
        self.enter_travel(ctx);
        true
    }

    /// Switches to walking the trip under way.
    fn enter_travel(&mut self, ctx: &mut Ctx<'_>) {
        let now = ctx.now_ms;
        self.mode = Mode::Travel;
        self.giver = None;
        self.obj = u8::MAX;
        self.quest = 0;
        self.reset_task(now);
        self.trip_scan_ms = now + SCAN_GAP_MS;
        self.cp_dist = f32::MAX;
    }

    /// Takes up the trip the last run left unfinished, or ends it when its time is up.
    fn resume_trip(&mut self, ctx: &mut Ctx<'_>) -> bool {
        let Some(trip) = current_trip(ctx.state) else {
            return false;
        };
        if ctx.now_ms >= trip.deadline_ms {
            self.finish_trip(ctx, Outcome::TimedOut);
            return false;
        }
        self.enter_travel(ctx);
        true
    }

    /// Records the end of the trip under way and logs it.
    fn finish_trip(&mut self, ctx: &mut Ctx<'_>, outcome: Outcome) {
        let now = ctx.now_ms;
        let me = (ctx.state.me.pos[0], ctx.state.me.pos[1]);
        let Some(trip) = prog(ctx.state).end(outcome, now) else {
            return;
        };
        let t = trip.target;
        let left = ((t.x - me.0).powi(2) + (t.y - me.1).powi(2)).sqrt();
        ctx.log.say(format!(
            "progress: trip to {} {} after {} s ({:.0} m of {:.0} m to go)",
            place(ctx.world, t.x, t.y),
            outcome.text(),
            now.saturating_sub(trip.started_ms) / 1000,
            left,
            t.dist
        ));
    }

    /// Ends the trip for good and fails the goal with the matching reason.
    fn end_trip(&mut self, ctx: &mut Ctx<'_>, outcome: Outcome) -> GoalStatus {
        ctx.motion.stop();
        self.finish_trip(ctx, outcome);
        self.back_to_pick(ctx);
        GoalStatus::Failed(match outcome {
            Outcome::TimedOut => FailReason::Timeout,
            _ => FailReason::Stuck,
        })
    }

    /// One tick of a trip: quests that come within reach end it, otherwise the bot walks on.
    fn travel(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let now = ctx.now_ms;
        let Some(trip) = current_trip(ctx.state) else {
            self.back_to_pick(ctx);
            return GoalStatus::Running;
        };
        if now >= trip.deadline_ms {
            return self.end_trip(ctx, Outcome::TimedOut);
        }
        if now >= self.trip_scan_ms {
            self.trip_scan_ms = now + SCAN_GAP_MS;
            if let Some(c) = self.scan(ctx).filter(|c| !c.trivial) {
                // The way led within reach of something the picker takes: its job is done.
                ctx.motion.stop();
                self.finish_trip(ctx, Outcome::InRange);
                prog(ctx.state).fruitful();
                self.accept_candidate(ctx, c);
                return GoalStatus::Running;
            }
        }
        let (x, y) = (trip.target.x, trip.target.y);
        let left = dist2d(ctx.state.me.pos, [x, y, 0.0]);
        if left + TRIP_CHECKPOINT_M < self.cp_dist {
            // Good progress renews the legs the walk may be cut into.
            self.cp_dist = left;
            self.legs = 0;
        }
        match self.walk(ctx, x, y, progress::ARRIVE_M) {
            Walk::Moving => GoalStatus::Running,
            Walk::Arrived => {
                self.finish_trip(ctx, Outcome::Arrived);
                self.back_to_pick(ctx);
                mem(ctx.state).next_scan_ms = 0;
                if matches!(trip.target.why, Why::Grind(_)) {
                    // monsters are what this place is for: the brain's grinding takes over
                    return GoalStatus::Failed(FailReason::NoTarget);
                }
                GoalStatus::Running
            }
            Walk::Failed => self.end_trip(ctx, Outcome::Blocked),
        }
    }

    // ---- choosing ------------------------------------------------------------------------

    /// The quest in the journal to work on next, and what to do with it.
    fn best_active(&mut self, ctx: &mut Ctx<'_>) -> Option<(u32, Mode)> {
        let now = ctx.now_ms;
        let world = ctx.world;
        let me = ctx.state.me.pos;
        let facts = Env {
            world,
            from: Some(Vec3::new(me[0], me[1], me[2])),
            navmesh: ctx.motion.uses_navmesh(),
        };
        let level = ctx.state.me.level;
        // (rank, chapter, idx, quest) of the best so far; rank 0 = focus, 1 = ready, 2 = other.
        let mut best: Option<((u8, u16, u16, u32), Mode)> = None;
        let mut failures: SmallVec<[(u32, Failure); 2]> = SmallVec::new();
        let mut drops: SmallVec<[u32; 2]> = SmallVec::new();
        for row in &ctx.state.quests.active {
            let Some(quest) = world.quest(row.template) else {
                continue;
            };
            if row.status == quest_status::FAILED {
                drops.push(row.template);
                continue;
            }
            if !usable(ctx.state, row.template, now) {
                continue;
            }
            let mode = if row.status == quest_status::READY {
                if let Err(why) = objective::report_supported(&facts, quest) {
                    failures.push((row.template, why));
                    continue;
                }
                Mode::TurnIn
            } else {
                match supported_from(&facts, quest, row, level) {
                    Ok(()) => Mode::Work,
                    Err(why) => {
                        failures.push((row.template, why));
                        continue;
                    }
                }
            };
            let rank = if row.template == self.quest {
                0
            } else if mode == Mode::TurnIn {
                1
            } else {
                2
            };
            let (chapter, idx) = if quest.story {
                (quest.chapter_idx, quest.quest_idx)
            } else {
                (u16::MAX, u16::MAX)
            };
            let key = (rank, chapter, idx, row.template);
            if best.as_ref().is_none_or(|b| key < b.0) {
                best = Some((key, mode));
            }
        }
        for q in drops {
            ctx.log
                .say(format!("quest {q}: failed on the server; dropping it"));
            ctx.send(cs::drop_quest_context(q));
            ctx.state.quests.active.retain(|r| r.template != q);
        }
        for (q, why) in failures {
            self.quest = q;
            self.record_failure(ctx, why);
        }
        best.map(|(key, mode)| (key.3, mode))
    }

    /// The best acceptable quest the givers within `radius` offer.
    fn find_candidate(&self, ctx: &mut Ctx<'_>, radius: f32) -> Option<Candidate> {
        let world = ctx.world;
        let state = &*ctx.state;
        let level = state.me.level;
        let me = state.me.pos;
        let center = Vec3::new(me[0], me[1], me[2]);
        with_eligibility(
            world,
            state,
            ctx.now_ms,
            ctx.motion.uses_navmesh(),
            &ctx.hub.events,
            |elig| {
                let mut best: Option<Candidate> = None;
                let mut consider = |quest_id: u32, giver: Giver, p: Vec3| {
                    let Some(quest) = world.quest(quest_id) else {
                        return;
                    };
                    if ctx.in_war_zone(p.x, p.y) || world.zone_closed_at(p.x, p.y) {
                        return;
                    }
                    let dist = ((p.x - me[0]).powi(2) + (p.y - me[1]).powi(2)).sqrt();
                    // Cheap checks first; the data checks run only for a quest that would win.
                    let cand = Candidate {
                        quest: quest_id,
                        giver,
                        at: (p.x, p.y),
                        dist,
                        event: quest.category == select::EVENT_CATEGORY,
                        story: quest.story,
                        trivial: progress::trivial(level, quest.level, quest.story),
                        chapter: quest.chapter_idx,
                        idx: quest.quest_idx,
                    };
                    if best
                        .as_ref()
                        .is_some_and(|b| select::order(&cand, b) != std::cmp::Ordering::Less)
                    {
                        return;
                    }
                    if elig.check(quest, giver) {
                        best = Some(cand);
                    }
                };
                world.npc_spawns_within(center, radius, |t, p| {
                    for &q in world.quests_from_npc(t) {
                        consider(q, Giver::Npc(t), p);
                    }
                });
                world.doodad_spawns_within(center, radius, |t, p| {
                    for &q in world.quests_from_doodad(t) {
                        consider(q, Giver::Doodad(t), p);
                    }
                });
                best
            },
        )
    }

    /// Starts working on `quest`.
    fn begin(&mut self, ctx: &mut Ctx<'_>, quest: u32, mode: Mode, giver: Option<Giver>) {
        if self.quest != quest {
            self.skip = 0;
        }
        self.quest = quest;
        self.mode = mode;
        self.giver = giver;
        self.reset_task(ctx.now_ms);
        self.obj = u8::MAX;
        ctx.log.debug(format!("quest {quest}: {mode:?}"));
    }

    fn reset_task(&mut self, now: u64) {
        self.stage = Stage::Walk;
        self.tries = 0;
        self.spots_tried = 0;
        self.spot = None;
        self.go_ms = 0;
        self.legs = 0;
        self.round_ms = 0;
        self.since_ms = now;
        self.sent_ms = 0;
        self.arrived_ms = 0;
        self.best_have = -1;
        self.progress_ms = now;
        self.fighting = None;
        self.visited.clear();
        self.credit = None;
        self.cast_skill = 0;
        self.use_wait_ms = 0;
        self.stood = 0;
    }

    fn back_to_pick(&mut self, ctx: &mut Ctx<'_>) {
        self.mode = Mode::Pick;
        self.giver = None;
        self.obj = u8::MAX;
        self.reset_task(ctx.now_ms);
    }

    // ---- accepting ----------------------------------------------------------------------

    fn accept(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let (q, now) = (self.quest, ctx.now_ms);
        if ctx.state.quests.row(q).is_some() {
            mem(ctx.state).accepted(q);
            let name = quest_name(ctx.world, q);
            ctx.log.say(format!("accepted quest {q} {name}"));
            self.begin(ctx, q, Mode::Work, None);
            return GoalStatus::Running;
        }
        let Some(giver) = self.giver else {
            self.back_to_pick(ctx);
            return GoalStatus::Running;
        };
        match self.stage {
            Stage::Walk => {
                let target = match giver {
                    Giver::Npc(t) => Target::Npc(t),
                    Giver::Doodad(t) => Target::Doodad(t),
                };
                match self.reach(ctx, target, NPC_RANGE) {
                    Reach::Here(obj) => {
                        let out = match giver {
                            Giver::Npc(_) => cs::start_quest_context(q, obj, 0, 0),
                            Giver::Doodad(_) => cs::start_quest_context(q, 0, obj, 0),
                        };
                        ctx.send(out);
                        self.stage = Stage::Wait;
                        self.sent_ms = now;
                        GoalStatus::Running
                    }
                    Reach::Walking => GoalStatus::Running,
                    Reach::Missing => self.fail(ctx, Failure::GiverMissing),
                    Reach::Blocked => self.fail(ctx, Failure::Unreachable),
                }
            }
            Stage::Wait => {
                if let Some(err) = server_error(ctx.state, q, self.sent_ms) {
                    // the bot's gates (level, race, prior quests, items) let this quest through
                    ctx.find(Finding::QuestRefused {
                        quest: q,
                        code: err,
                        turn_in: false,
                    });
                    return self.fail(ctx, Failure::Server(err));
                }
                if let Some((kind, value)) = server_unit_req(ctx.state, q, self.sent_ms) {
                    // (kind 0x25 is "this quest must be in progress first": the picker takes the
                    // prerequisite once the start gate below lets it through)
                    ctx.find(Finding::QuestUnitReq {
                        quest: q,
                        kind,
                        value,
                    });
                    return self.fail(ctx, Failure::UnitReq { kind, value });
                }
                if now.saturating_sub(self.sent_ms) > ACK_MS {
                    self.tries += 1;
                    if self.tries >= 2 {
                        return self.fail(ctx, Failure::AcceptRefused);
                    }
                    self.stage = Stage::Walk;
                }
                GoalStatus::Running
            }
        }
    }

    // ---- objectives ---------------------------------------------------------------------

    fn work(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let (q, now) = (self.quest, ctx.now_ms);
        let world = ctx.world;
        let Some(quest) = world.quest(q) else {
            self.back_to_pick(ctx);
            return GoalStatus::Running;
        };
        let Some(row) = ctx.state.quests.row(q).copied() else {
            self.gone(ctx);
            return GoalStatus::Running;
        };
        if row.status == quest_status::READY {
            self.begin(ctx, q, Mode::TurnIn, None);
            return GoalStatus::Running;
        }
        if row.status == quest_status::FAILED {
            return self.fail(ctx, Failure::Server(0));
        }
        let Some(slot) = objective::next_unmet(quest, &row, self.skip) else {
            // Everything is met; the server turns the quest ready by itself.
            if now.saturating_sub(self.since_ms) > READY_WAIT_MS {
                if self.tries == 0 {
                    self.tries = 1;
                    let selected =
                        crate::bot::gear::reward_choice(ctx, q, objective::reward_choice(quest));
                    ctx.send(cs::complete_quest_context(q, 0, 0, selected));
                    self.since_ms = now;
                } else {
                    return self.fail(ctx, Failure::NoProgress);
                }
            }
            return GoalStatus::Running;
        };
        if usize::from(self.obj) != slot.idx {
            self.start_objective(ctx, &slot);
        }
        if slot.have > self.best_have {
            self.best_have = slot.have;
            self.progress_ms = now;
            self.tries = 0;
            self.sent_ms = 0;
            // The objective advanced: a target or spot this objective claimed is free again.
            if self.claim_key != 0 {
                ctx.hub.contest.release(self.claim_key, ctx.persona.id);
                self.claim_key = 0;
                self.claim_ms = 0;
            }
        } else if now.saturating_sub(self.progress_ms) > NO_PROGRESS_MS {
            return self.stall(ctx, quest, &row, &slot);
        }
        self.track_credit(ctx, &slot);
        let me = ctx.state.me.pos;
        let facts = Env {
            world,
            from: Some(Vec3::new(me[0], me[1], me[2])),
            navmesh: ctx.motion.uses_navmesh(),
        };
        match objective::plan(&facts, quest, slot.obj, ctx.state.me.level) {
            Ok(plan) => self.exec(ctx, quest, &slot, plan),
            Err(why) => self.fail(ctx, why),
        }
    }

    /// The monster the bot was fighting for the quest died by the bot's hand: if killing it is the
    /// objective, the server must credit it.
    fn note_kill(&mut self, ctx: &mut Ctx<'_>, obj: u32) {
        if self.credit.is_some() || self.mode != Mode::Work {
            return;
        }
        let state = &*ctx.state;
        let Some(template) = state.units.get(&obj).map(|u| u.id) else {
            return;
        };
        // (only a kill the server named the bot as the killer of counts)
        if !state.kills.iter().any(|k| k.0 == obj) {
            return;
        }
        // The server credits a kill to whoever tagged the monster first (the tag team when the
        // party did more than half the damage). A killing blow on a monster somebody else tagged
        // gets nothing, so it is not a credit that can go missing. (Seen live: two bots on the
        // same boss, the one that struck last got no credit and the report was a false alarm.)
        let tag = state.units.get(&obj).and_then(|u| u.tag());
        if !credit_expected(tag, characters_near(state, TAG_SHARE_RADIUS_M)) {
            return;
        }
        let (Some(quest), Some(row)) = (ctx.world.quest(self.quest), state.quests.row(self.quest))
        else {
            return;
        };
        let Some(slot) = objective::unmet(quest, row).find(|s| s.idx == usize::from(self.obj))
        else {
            return;
        };
        let me = state.me.pos;
        let facts = Env {
            world: ctx.world,
            from: Some(Vec3::new(me[0], me[1], me[2])),
            navmesh: ctx.motion.uses_navmesh(),
        };
        if let Ok(Plan::Kill { templates }) =
            objective::plan(&facts, quest, slot.obj, state.me.level)
        {
            if templates.contains(&template) {
                self.credit = Some(CreditNote {
                    by_skill: false,
                    source: template,
                    idx: self.obj,
                    have: slot.have,
                    since_ms: ctx.now_ms,
                });
            }
        }
    }

    /// Follows the action the server confirmed (a kill, an item or doodad cast) until the counter
    /// of the objective moves; reports it when it does not.
    fn track_credit(&mut self, ctx: &mut Ctx<'_>, slot: &Slot<'_>) {
        let now = ctx.now_ms;
        let Some(note) = self.credit else {
            // an item or doodad cast the server accepted: the credit is due after the cast
            let (skill, at) = ctx.state.watch.cast_ok;
            if self.cast_skill != 0
                && skill == self.cast_skill
                && self.sent_ms != 0
                && at >= self.sent_ms
            {
                let cast_ms = ctx
                    .world
                    .skill(skill)
                    .map_or(0, |s| u64::from(s.casting_time));
                self.credit = Some(CreditNote {
                    by_skill: true,
                    source: skill,
                    idx: slot.idx.min(usize::from(u8::MAX)) as u8,
                    have: slot.have,
                    since_ms: at + cast_ms,
                });
                self.cast_skill = 0;
            }
            return;
        };
        let damaged = ctx.state.last_damage_ms > note.since_ms.saturating_sub(10_000);
        match note.step(slot.idx, slot.have, now, damaged) {
            CreditStep::Keep => {}
            CreditStep::Drop => self.credit = None,
            CreditStep::Report => {
                self.credit = None;
                ctx.find(Finding::QuestCreditMissing {
                    quest: self.quest,
                    component: slot.obj.component,
                    source: note.source,
                    by_skill: note.by_skill,
                    have: slot.have,
                    need: slot.need,
                });
            }
        }
    }

    /// The NPC or doodad is there but the bot could not get close enough for the server: a
    /// house floor or a cave the navigation mesh has no way to, reported once per target.
    fn role_unreachable(&self, ctx: &mut Ctx<'_>, target: Target, dist_m: f32) {
        let (template, doodad) = match target {
            Target::Npc(t) => (t, false),
            Target::Doodad(t) => (t, true),
        };
        let role = match self.mode {
            Mode::Accept => Role::Giver,
            Mode::TurnIn => Role::TurnIn,
            _ => Role::Objective,
        };
        ctx.find(Finding::RoleUnreachable {
            template,
            doodad,
            role,
            dist_m,
        });
    }

    /// The giver, turn-in NPC or doodad of the quest is not at any of its spawns that the bot
    /// stood at: a finding when it really stood at one, saw nothing and no corpse of the NPC is
    /// around (a dead NPC is waiting to respawn, not missing).
    fn role_missing(&self, ctx: &mut Ctx<'_>, target: Target) {
        if self.stood == 0 {
            return;
        }
        let (template, doodad) = match target {
            Target::Npc(t) => (t, false),
            Target::Doodad(t) => (t, true),
        };
        if !doodad
            && ctx
                .state
                .units
                .values()
                .any(|u| u.kind == UnitKind::Npc && u.id == template)
        {
            return;
        }
        let role = match self.mode {
            Mode::Accept => Role::Giver,
            Mode::TurnIn => Role::TurnIn,
            _ => Role::Objective,
        };
        ctx.find(Finding::RoleMissing {
            template,
            doodad,
            role,
            spots: self.stood,
        });
    }

    /// The quest left the journal: completed (automatic report), dropped, or never there.
    fn gone(&mut self, ctx: &mut Ctx<'_>) {
        let q = self.quest;
        if ctx.state.quests.is_done(q) {
            self.completed(ctx, q);
        } else {
            ctx.log.debug(format!("quest {q}: left the journal"));
        }
        self.back_to_pick(ctx);
    }

    fn completed(&mut self, ctx: &mut Ctx<'_>, q: u32) {
        mem(ctx.state).success(q);
        let name = quest_name(ctx.world, q);
        let level = ctx.state.me.level;
        ctx.log
            .say(format!("completed quest {q} {name} (level {level})"));
    }

    fn start_objective(&mut self, ctx: &mut Ctx<'_>, slot: &Slot<'_>) {
        let now = ctx.now_ms;
        self.obj = slot.idx.min(usize::from(u8::MAX - 1)) as u8;
        self.reset_task(now);
        self.best_have = slot.have;
        ctx.motion.stop();
        ctx.combat.disengage();
    }

    /// The objective made no progress for too long: skip it if another one remains, else give up
    /// on the quest.
    fn stall(
        &mut self,
        ctx: &mut Ctx<'_>,
        quest: &QuestData,
        row: &aa_proto::sc_activity::QuestRow,
        slot: &Slot<'_>,
    ) -> GoalStatus {
        let others = objective::unmet(quest, row)
            .filter(|s| s.idx != slot.idx && (s.idx >= objective::MAX_SLOTS))
            .count()
            + objective::unmet(quest, row)
                .filter(|s| {
                    s.idx != slot.idx
                        && s.idx < objective::MAX_SLOTS
                        && self.skip & (1 << s.idx) == 0
                })
                .count();
        if others > 0 && slot.idx < objective::MAX_SLOTS {
            self.skip |= 1 << slot.idx;
            ctx.log.say(format!(
                "quest {}: objective {} made no progress; trying another",
                quest.id, slot.idx
            ));
            self.obj = u8::MAX;
            self.reset_task(ctx.now_ms);
            return GoalStatus::Running;
        }
        self.fail(ctx, Failure::NoProgress)
    }

    fn exec(
        &mut self,
        ctx: &mut Ctx<'_>,
        quest: &QuestData,
        slot: &Slot<'_>,
        plan: Plan<'_>,
    ) -> GoalStatus {
        match plan {
            Plan::Talk { npc } => self.do_talk(ctx, quest, slot, npc),
            Plan::Kill { templates } | Plan::Drops { templates, .. } => {
                self.do_kill(ctx, templates)
            }
            Plan::Harvest { doodads, .. } => self.do_doodad(ctx, slot, doodads, 0),
            Plan::Interact { doodad, skill } => {
                self.do_doodad(ctx, slot, std::slice::from_ref(&doodad), skill)
            }
            Plan::UseItem { item, skill } => self.do_use_item(ctx, quest, slot, item, skill),
            Plan::Sphere { x, y, radius } => self.do_sphere(ctx, x, y, radius),
            Plan::Supplied => {
                if ctx.now_ms.saturating_sub(self.since_ms) > SUPPLY_WAIT_MS {
                    self.fail(ctx, Failure::NoProgress)
                } else {
                    GoalStatus::Running
                }
            }
        }
    }

    fn do_talk(
        &mut self,
        ctx: &mut Ctx<'_>,
        quest: &QuestData,
        slot: &Slot<'_>,
        npc: u32,
    ) -> GoalStatus {
        let now = ctx.now_ms;
        match self.reach(ctx, Target::Npc(npc), NPC_RANGE) {
            Reach::Here(obj) => {
                if self.sent_ms == 0 || now.saturating_sub(self.sent_ms) >= TALK_GAP_MS {
                    if self.tries >= MAX_TRIES {
                        return self.fail(ctx, Failure::NoProgress);
                    }
                    ctx.send(cs::quest_talk_made(
                        obj,
                        quest.id,
                        slot.obj.component,
                        slot.obj.act,
                    ));
                    self.tries += 1;
                    self.sent_ms = now;
                }
                GoalStatus::Running
            }
            Reach::Walking => GoalStatus::Running,
            Reach::Missing => self.fail(ctx, Failure::TargetMissing),
            Reach::Blocked => self.fail(ctx, Failure::Unreachable),
        }
    }

    fn do_kill(&mut self, ctx: &mut Ctx<'_>, templates: &[u32]) -> GoalStatus {
        if let Some(obj) = self.pick_mob(ctx, templates) {
            if ctx.motion.is_moving() && self.spot.is_some() {
                ctx.motion.stop();
            }
            self.spot = None;
            self.fighting = Some(obj);
            self.engaged_ms = ctx.now_ms;
            ctx.combat.engage(obj);
            self.claim(ctx, unit_key(obj));
            return GoalStatus::Running;
        }
        let world = ctx.world;
        self.search(ctx, |f: &mut dyn FnMut(Vec3)| {
            for &t in templates {
                for &p in world.npc_spawns(t) {
                    f(p);
                }
            }
        })
    }

    /// Uses one of the doodads `templates` in view; `skill` 0 looks the skill up per doodad.
    fn do_doodad(
        &mut self,
        ctx: &mut Ctx<'_>,
        slot: &Slot<'_>,
        templates: &[u32],
        skill: u32,
    ) -> GoalStatus {
        let now = ctx.now_ms;
        let me = ctx.state.me.pos;
        let bot = ctx.persona.id;
        let mut best: Option<(u32, u32, [f32; 3], f32, bool)> = None;
        for (&obj, d) in &ctx.state.doodads {
            if !templates.contains(&d.template) {
                continue;
            }
            let dist = dist3d(me, d.pos);
            let taken = ctx.hub.contest.others(doodad_key(obj), bot) > 0;
            if best.is_none_or(|b| better_target(taken, dist, b.4, b.3)) {
                best = Some((obj, d.template, d.pos, dist, taken));
            }
        }
        let Some((obj, template, pos, dist, _)) = best else {
            let world = ctx.world;
            return self.search(ctx, |f: &mut dyn FnMut(Vec3)| {
                for &t in templates {
                    for &p in world.doodad_spawns(t) {
                        f(p);
                    }
                }
            });
        };
        self.renew_claim(ctx, doodad_key(obj));
        if dist > DOODAD_RANGE {
            return match self.walk_at(ctx, pos[0], pos[1], Some(pos[2]), DOODAD_STOP) {
                Walk::Failed => self.fail(ctx, Failure::Unreachable),
                _ => GoalStatus::Running,
            };
        }
        if ctx.motion.is_moving() {
            ctx.motion.stop();
        }
        ctx.motion.face(pos[0], pos[1]);
        let skill = if skill != 0 {
            skill
        } else {
            ctx.world.doodad_use_skill(template)
        };
        if skill == 0 {
            return self.fail(ctx, Failure::Unsupported("doodad skill unknown"));
        }
        let gap = cast_gap_ms(ctx.world, skill);
        if self.sent_ms == 0 || now.saturating_sub(self.sent_ms) >= gap {
            if self.tries >= MAX_TRIES {
                return self.fail(ctx, Failure::CastRefused);
            }
            let me_obj = ctx.state.me.obj;
            ctx.send(cs::start_skill_doodad(skill, me_obj, obj));
            self.tries += 1;
            self.sent_ms = now;
            // using the doodad *is* the objective only for an interaction; a harvest pays in items
            if matches!(slot.obj.kind, ObjKind::Interact { .. }) {
                self.cast_skill = skill;
            }
        }
        GoalStatus::Running
    }

    fn do_use_item(
        &mut self,
        ctx: &mut Ctx<'_>,
        quest: &QuestData,
        slot: &Slot<'_>,
        item: u32,
        skill: u32,
    ) -> GoalStatus {
        let now = ctx.now_ms;
        // A stack that is used up is no item, and neither is an instance the bot cast twice with
        // nothing changing: the server's bag no longer has it, whatever the bot's picture says.
        let stale = &self.stale;
        let Some((_, it)) = ctx.state.find_item_where(item, |i| !stale.contains(&i.id)) else {
            // A supplied quest item arrives with the quest; give the server a moment.
            if now.saturating_sub(self.since_ms) > ACK_MS {
                return self.fail(ctx, Failure::MissingItem);
            }
            return GoalStatus::Running;
        };
        let (item_id, template, count) = (it.id, it.template, it.count);
        let seen = (item_id, count, slot.have);
        if self.use_note.3 >= STALE_CASTS
            && (self.use_note.0, self.use_note.1, self.use_note.2) == seen
        {
            ctx.log.say(format!(
                "quest {}: item {item} was cast {STALE_CASTS} times without effect; giving it up",
                self.quest
            ));
            if self.stale.len() >= 2 {
                self.stale.remove(0);
            }
            self.stale.push(item_id);
            self.use_note = (0, 0, 0, 0);
            // the server did not honour the casts (refused them, or the item is gone there)
            return self.fail(ctx, Failure::CastRefused);
        }
        let me_obj = ctx.state.me.obj;
        // Some item skills only work at a certain place (a book read to a cat, a firework next
        // to a stand, a spell inside a quest area).
        let mut target = me_obj;
        for &need in ctx.world.skill_needs(skill) {
            match self.meet(ctx, quest, skill, need) {
                Ok(Some(obj)) => target = obj,
                Ok(None) => {}
                Err(status) => return status,
            }
        }
        // A single target shared by many bots (the tournament dummy of quest 2396): each bot
        // claims it, the lowest bot id goes first and the others wait their turn instead of all
        // casting at it.
        if target != me_obj {
            let bot = ctx.persona.id;
            let key = unit_key(target);
            // Someone already holds it: wait without joining (the holder goes first). Otherwise
            // claim it; when several bots do so in the same tick, the lowest id goes first.
            let queued = if ctx.hub.contest.others(key, bot) > 0 {
                true
            } else {
                self.renew_claim(ctx, key);
                ctx.hub.contest.first_holder(key).is_some_and(|h| h != bot)
            };
            if queued {
                if self.use_wait_ms == 0 {
                    self.use_wait_ms = now;
                }
                if now.saturating_sub(self.use_wait_ms) >= CROWD_WAIT_MS {
                    ctx.hub.contest.release(key, bot);
                    self.use_wait_ms = 0;
                    return self.fail(ctx, Failure::Crowded);
                }
                return GoalStatus::Running;
            }
            self.use_wait_ms = 0;
        } else {
            self.use_wait_ms = 0;
        }
        let gap = cast_gap_ms(ctx.world, skill).max(ITEM_CAST_GAP_MS);
        if self.sent_ms == 0 || now.saturating_sub(self.sent_ms) >= gap {
            if self.tries >= MAX_TRIES {
                return self.fail(ctx, Failure::CastRefused);
            }
            ctx.log.debug(format!(
                "quest {}: using item {item_id} (template {template}, count {count}, objective at {})",
                self.quest, slot.have
            ));
            ctx.send(cs::start_skill_item(
                skill, me_obj, item_id, template, target,
            ));
            self.tries += 1;
            self.sent_ms = now;
            self.cast_skill = skill;
            self.use_note = if (self.use_note.0, self.use_note.1, self.use_note.2) == seen {
                (seen.0, seen.1, seen.2, self.use_note.3.saturating_add(1))
            } else {
                (seen.0, seen.1, seen.2, 1)
            };
        }
        GoalStatus::Running
    }

    /// Gets to where `need` of an item skill is met. `Ok(Some(obj))` names the NPC to cast at,
    /// `Ok(None)` means the bot is in place, `Err` is the status to return while it is not.
    fn meet(
        &mut self,
        ctx: &mut Ctx<'_>,
        quest: &QuestData,
        skill: u32,
        need: SkillNeed,
    ) -> Result<Option<u32>, GoalStatus> {
        let world = ctx.world;
        let me = ctx.state.me.pos;
        // (x, y, z), how close the bot has to be, how close it walks.
        let (spot, reach, stop, obj) = match need {
            SkillNeed::Npc(t) => {
                let bot = ctx.persona.id;
                let taken = |obj: u32| ctx.hub.contest.others(unit_key(obj), bot) > 0;
                let Some((obj, pos)) =
                    nearest_unit_in(ctx.state, std::slice::from_ref(&t), me, &taken)
                else {
                    return Err(self.search(ctx, |f: &mut dyn FnMut(Vec3)| {
                        for &p in world.npc_spawns(t) {
                            f(p);
                        }
                    }));
                };
                let reach = world.skill(skill).map_or(NPC_RANGE, |s| {
                    (f32::from(s.max_range) - 1.0).clamp(NPC_STOP, 20.0)
                });
                (
                    [pos[0], pos[1], pos[2]],
                    reach,
                    (reach - 1.5).max(1.0),
                    Some(obj),
                )
            }
            SkillNeed::NpcGroup(group, negate) => {
                let members = world.quest_monster_group(group);
                let bot = ctx.persona.id;
                let taken = |obj: u32| ctx.hub.contest.others(unit_key(obj), bot) > 0;
                let found = if negate {
                    nearest_unit_excluding(ctx.state, members, me)
                } else {
                    nearest_unit_in(ctx.state, members, me, &taken)
                };
                let Some((obj, pos)) = found else {
                    let center = Vec3::new(me[0], me[1], me[2]);
                    return Err(self.search(ctx, |f: &mut dyn FnMut(Vec3)| {
                        if negate {
                            world.npc_spawns_within(center, SEARCH_RADIUS_M, |t, p| {
                                if !members.contains(&t) {
                                    f(p);
                                }
                            });
                        } else {
                            for &t in members {
                                for &p in world.npc_spawns(t) {
                                    f(p);
                                }
                            }
                        }
                    }));
                };
                let reach = world.skill(skill).map_or(NPC_RANGE, |s| {
                    (f32::from(s.max_range) - 1.0).clamp(NPC_STOP, 20.0)
                });
                (
                    [pos[0], pos[1], pos[2]],
                    reach,
                    (reach - 1.5).max(1.0),
                    Some(obj),
                )
            }
            SkillNeed::Doodad(t, range) => {
                let nearest = world
                    .doodad_spawns(t)
                    .iter()
                    .map(|p| (p, dist3d(me, [p.x, p.y, p.z])))
                    .filter(|(_, d)| *d <= SEARCH_RADIUS_M)
                    .min_by(|a, b| a.1.total_cmp(&b.1));
                let Some((p, _)) = nearest else {
                    return Err(self.fail(ctx, Failure::TargetMissing));
                };
                let reach = (range * 0.7).clamp(2.0, 30.0);
                ([p.x, p.y, p.z], reach, (reach - 2.0).max(1.0), None)
            }
            SkillNeed::Sphere => {
                let sphere = quest
                    .progress
                    .iter()
                    .map(|s| s.component)
                    .chain(std::iter::once(quest.start_component))
                    .find_map(|c| world.quest_sphere(quest.id, c));
                // Without a known area the cast is tried where the bot stands.
                let Some(s) = sphere else { return Ok(None) };
                let reach = (s.radius * 0.6).max(2.0);
                (
                    [s.x, s.y, me[2]],
                    reach,
                    (s.radius * 0.4).clamp(1.0, 4.0),
                    None,
                )
            }
        };
        if dist3d(me, spot) > reach {
            let z = (!matches!(need, SkillNeed::Sphere)).then_some(spot[2]);
            return Err(match self.walk_at(ctx, spot[0], spot[1], z, stop) {
                Walk::Failed => self.fail(ctx, Failure::Unreachable),
                _ => GoalStatus::Running,
            });
        }
        if ctx.motion.is_moving() {
            ctx.motion.stop();
        }
        if obj.is_some() {
            ctx.motion.face(spot[0], spot[1]);
        }
        Ok(obj)
    }

    fn do_sphere(&mut self, ctx: &mut Ctx<'_>, x: f32, y: f32, radius: f32) -> GoalStatus {
        let now = ctx.now_ms;
        let stop = (radius * 0.4).clamp(1.0, 4.0);
        match self.walk(ctx, x, y, stop) {
            Walk::Failed => self.fail(ctx, Failure::Unreachable),
            Walk::Moving => GoalStatus::Running,
            Walk::Arrived => {
                if self.sent_ms == 0 {
                    self.sent_ms = now;
                } else if now.saturating_sub(self.sent_ms) > 10_000 {
                    // The server did not notice: walk out and back in.
                    if self.tries >= 3 {
                        return self.fail(ctx, Failure::NoProgress);
                    }
                    self.tries += 1;
                    self.sent_ms = 0;
                    self.spot = None;
                    ctx.travel(x + radius * 0.2, y, 0.5, Pace::Run);
                }
                GoalStatus::Running
            }
        }
    }

    // ---- turning in ---------------------------------------------------------------------

    fn turn_in(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let (q, now) = (self.quest, ctx.now_ms);
        let world = ctx.world;
        let Some(quest) = world.quest(q) else {
            self.back_to_pick(ctx);
            return GoalStatus::Running;
        };
        let Some(row) = ctx.state.quests.row(q).copied() else {
            self.gone(ctx);
            return GoalStatus::Running;
        };
        if row.status != quest_status::READY {
            // Not ready after all (a second step started): back to the objectives.
            self.begin(ctx, q, Mode::Work, None);
            return GoalStatus::Running;
        }
        let selected = crate::bot::gear::reward_choice(ctx, q, objective::reward_choice(quest));
        match quest.report {
            QuestReport::Npc(t) => match self.reach(ctx, Target::Npc(t), NPC_RANGE) {
                Reach::Here(obj) => self.send_complete(ctx, q, obj, 0, selected),
                Reach::Walking => GoalStatus::Running,
                Reach::Missing => self.fail(ctx, Failure::TargetMissing),
                Reach::Blocked => self.fail(ctx, Failure::Unreachable),
            },
            QuestReport::Doodad(t) => match self.reach(ctx, Target::Doodad(t), NPC_RANGE) {
                Reach::Here(obj) => self.send_complete(ctx, q, 0, obj, selected),
                Reach::Walking => GoalStatus::Running,
                Reach::Missing => self.fail(ctx, Failure::TargetMissing),
                Reach::Blocked => self.fail(ctx, Failure::Unreachable),
            },
            QuestReport::Auto => {
                if now.saturating_sub(self.since_ms) < AUTO_WAIT_MS {
                    return GoalStatus::Running;
                }
                self.send_complete(ctx, q, 0, 0, selected)
            }
            QuestReport::Journal | QuestReport::Unknown => {
                self.send_complete(ctx, q, 0, 0, selected)
            }
        }
    }

    fn send_complete(
        &mut self,
        ctx: &mut Ctx<'_>,
        quest: u32,
        npc: u32,
        doodad: u32,
        selected: i32,
    ) -> GoalStatus {
        let now = ctx.now_ms;
        if self.sent_ms != 0 {
            if let Some(err) = server_error(ctx.state, quest, self.sent_ms) {
                ctx.find(Finding::QuestRefused {
                    quest,
                    code: err,
                    turn_in: true,
                });
                if self.tries >= 2 {
                    return self.fail(ctx, Failure::Server(err));
                }
            } else if now.saturating_sub(self.sent_ms) < ACK_MS {
                return GoalStatus::Running;
            }
            if self.tries >= 3 {
                return self.fail(ctx, Failure::TurnInRefused);
            }
        }
        ctx.send(cs::complete_quest_context(quest, npc, doodad, selected));
        self.tries += 1;
        self.sent_ms = now;
        self.stage = Stage::Wait;
        GoalStatus::Running
    }

    // ---- failures -----------------------------------------------------------------------

    /// Records a failure of the current quest and goes back to choosing.
    fn fail(&mut self, ctx: &mut Ctx<'_>, why: Failure) -> GoalStatus {
        ctx.motion.stop();
        ctx.combat.disengage();
        self.record_failure(ctx, why);
        self.back_to_pick(ctx);
        GoalStatus::Running
    }

    fn record_failure(&mut self, ctx: &mut Ctx<'_>, why: Failure) {
        let (q, now) = (self.quest, ctx.now_ms);
        let (story, name) = ctx
            .world
            .quest(q)
            .map_or((false, ""), |x| (x.story, &*x.name));
        let verdict = mem(ctx.state).fail(q, why, story, now);
        let until = (verdict.until().saturating_sub(now)) / 1000;
        ctx.log.say(match verdict {
            Verdict::Backoff(_) => format!("quest {q} {name}: {why}; backing off {until} s"),
            Verdict::Quarantine(_) if verdict.until() == u64::MAX => {
                format!("quest {q} {name}: {why}; quarantined for the session")
            }
            Verdict::Quarantine(_) => {
                format!("quest {q} {name}: {why}; quarantined for {until} s")
            }
        });
        // A side quest that cannot be done leaves the journal.
        if matches!(verdict, Verdict::Quarantine(_)) && !story && ctx.state.quests.row(q).is_some()
        {
            ctx.send(cs::drop_quest_context(q));
        }
    }

    // ---- walking and finding ------------------------------------------------------------

    /// Walks to (x, y), stopping `radius` metres short. The command is re-issued when something
    /// else (a fight) took the controller over; a trip that failed is reported once.
    fn walk(&mut self, ctx: &mut Ctx<'_>, x: f32, y: f32, radius: f32) -> Walk {
        self.walk_at(ctx, x, y, None, radius)
    }

    /// Like [`Inner::walk`], for a target on a known floor: the distance that must fall within
    /// `radius` is the 3D one, and the navmesh search is asked for that floor (a house storey, a
    /// cave under the terrain). A controller that reports "arrived" while the bot is still 30 m
    /// below the target means the mesh has no way up: the walk fails and the caller reports it.
    fn walk_at(&mut self, ctx: &mut Ctx<'_>, x: f32, y: f32, z: Option<f32>, radius: f32) -> Walk {
        let now = ctx.now_ms;
        let me = ctx.state.me.pos;
        let at = |p: [f32; 3]| z.map_or_else(|| dist2d(me, p), |z| dist3d(me, [p[0], p[1], z]));
        let here = [x, y, z.unwrap_or(me[2])];
        if at(here) <= radius + 0.6 {
            if ctx.motion.is_moving() {
                ctx.motion.stop();
            }
            return Walk::Arrived;
        }
        let same = self
            .spot
            .is_some_and(|s| dist2d([s.0, s.1, 0.0], [x, y, 0.0]) < 2.0);
        match ctx.motion.status() {
            MotionStatus::Moving if same => return Walk::Moving,
            // The controller reached the horizontal goal but not the floor: the route ended
            // under/above the target and there is no way up.
            MotionStatus::Arrived if same && at(here) > radius + 0.6 => {
                self.spot = None;
                return Walk::Failed;
            }
            // The controller cuts a trip off after its time limit; a long way is walked in legs as
            // long as every leg gets closer.
            MotionStatus::Failed(MoveFailure::Timeout)
                if same && self.legs < MAX_LEGS && self.leg_from - at(here) >= LEG_MIN_GAIN_M =>
            {
                self.legs += 1;
                self.leg_from = at(here);
                self.go_ms = 0;
            }
            MotionStatus::Failed(_) if same => {
                self.spot = None;
                return Walk::Failed;
            }
            _ => {}
        }
        if same && now.saturating_sub(self.go_ms) < GO_GAP_MS {
            return Walk::Moving;
        }
        if !same {
            self.legs = 0;
            self.leg_from = at(here);
        }
        match z {
            Some(z) => ctx.travel_to_floor(x, y, z, radius, Pace::Run),
            None => ctx.travel(x, y, radius, Pace::Run),
        }
        self.spot = Some((x, y));
        self.go_ms = now;
        Walk::Moving
    }

    /// Gets next to an NPC or doodad: the nearest one in view, else its spawns one after the
    /// other.
    fn reach(&mut self, ctx: &mut Ctx<'_>, target: Target, near: f32) -> Reach {
        let now = ctx.now_ms;
        let me = ctx.state.me.pos;
        let seen = match target {
            Target::Npc(t) => nearest_unit(ctx.state, t, me),
            Target::Doodad(t) => nearest_doodad(ctx.state, t, me),
        };
        if let Some((obj, pos)) = seen {
            let d = dist3d(me, pos);
            // The server measures a quest request in 3D (`npc_out_of_quest_reach`, 15 m with the
            // slack; a doodad's func reach is 20 m) and refuses it from the floor above/below.
            let server_reach = match target {
                Target::Npc(_) => QUEST_NPC_REACH,
                Target::Doodad(_) => QUEST_DOODAD_REACH,
            };
            if d <= near || d <= server_reach {
                if ctx.motion.is_moving() {
                    ctx.motion.stop();
                }
                self.spot = None;
                return Reach::Here(obj);
            }
            return match self.walk_at(ctx, pos[0], pos[1], Some(pos[2]), (near - 1.5).max(1.0)) {
                Walk::Failed => {
                    if d <= server_reach {
                        // (the pad is out of the mesh but close enough for the server)
                        if ctx.motion.is_moving() {
                            ctx.motion.stop();
                        }
                        self.spot = None;
                        Reach::Here(obj)
                    } else {
                        self.role_unreachable(ctx, target, d);
                        Reach::Blocked
                    }
                }
                _ => Reach::Walking,
            };
        }
        // Not in view: go to a spawn.
        let world = ctx.world;
        let spawns: &[Vec3] = match target {
            Target::Npc(t) => world.npc_spawns(t),
            Target::Doodad(t) => world.doodad_spawns(t),
        };
        let spot = match self.spot {
            Some(s) => s,
            None => {
                if self.spots_tried >= MAX_SPOTS {
                    return Reach::Missing;
                }
                let Some(p) = self.nearest_unvisited(spawns, me, 0.0) else {
                    self.role_missing(ctx, target);
                    return Reach::Missing;
                };
                (p.x, p.y)
            }
        };
        let spot_z = spawns
            .iter()
            .find(|p| p.x == spot.0 && p.y == spot.1)
            .map(|p| p.z);
        match self.walk_at(ctx, spot.0, spot.1, spot_z, NPC_STOP) {
            Walk::Moving => {
                self.arrived_ms = 0;
                Reach::Walking
            }
            Walk::Failed => {
                ctx.log.debug(format!(
                    "quest {}: the way to {target:?} at {:.0}, {:.0} is blocked",
                    self.quest, spot.0, spot.1
                ));
                self.mark_visited(spot.0, spot.1);
                self.spots_tried += 1;
                if self.spots_tried >= MAX_SPOTS {
                    Reach::Blocked
                } else {
                    Reach::Walking
                }
            }
            Walk::Arrived => {
                if self.arrived_ms == 0 {
                    self.arrived_ms = now;
                }
                self.spot = Some(spot);
                if now.saturating_sub(self.arrived_ms) < ARRIVE_GRACE_MS {
                    return Reach::Walking;
                }
                ctx.log.debug(format!(
                    "quest {}: {target:?} is not at {:.0}, {:.0}",
                    self.quest, spot.0, spot.1
                ));
                self.mark_visited(spot.0, spot.1);
                self.spots_tried += 1;
                self.stood = self.stood.saturating_add(1);
                self.spot = None;
                self.arrived_ms = 0;
                if self.spots_tried >= MAX_SPOTS {
                    self.role_missing(ctx, target);
                    Reach::Missing
                } else {
                    Reach::Walking
                }
            }
        }
    }

    /// Looks for something at spawns that are not searched yet. `spawns` feeds every candidate
    /// position to the callback. Gives up after the spots of two rounds were empty.
    ///
    /// Spots other bots work on count as farther away than they are, so the bots spread over the
    /// spawn points of a template. When every spot is searched and other characters are around,
    /// the targets are probably taken as fast as they respawn: the bot waits for them near the
    /// spawn for several rounds ([`CROWD_ROUNDS`]) and then rests the quest ([`Failure::Crowded`])
    /// instead of failing it for a missing target.
    fn search(&mut self, ctx: &mut Ctx<'_>, spawns: impl Fn(&mut dyn FnMut(Vec3))) -> GoalStatus {
        let now = ctx.now_ms;
        let me = ctx.state.me.pos;
        let (bot, hub) = (ctx.persona.id, ctx.hub);
        let spot = match self.spot {
            Some(s) => s,
            None => {
                let mut best: Option<(f32, f32, f32)> = None;
                let mut contested = 0u32;
                spawns(&mut |p| {
                    let others = hub.contest.others(spot_key(p.x, p.y), bot);
                    contested += u32::from(others > 0);
                    let d2 = (p.x - me[0]).powi(2) + (p.y - me[1]).powi(2);
                    if d2 < SPAWN_MIN_M * SPAWN_MIN_M
                        || d2 > SEARCH_RADIUS_M * SEARCH_RADIUS_M
                        || self.visited.contains(&cell(p.x, p.y))
                    {
                        return;
                    }
                    let score = spot_score(d2, others);
                    if best.is_some_and(|b| score >= b.2) {
                        return;
                    }
                    best = Some((p.x, p.y, score));
                });
                match best {
                    Some((x, y, _)) => {
                        self.claim(ctx, spot_key(x, y));
                        (x, y)
                    }
                    None => {
                        // Every spot was searched: wait for respawns, start another round, or
                        // give up.
                        let crowded = contested > 0 || characters_near(ctx.state, CROWD_RADIUS_M);
                        let gap = if crowded {
                            CROWD_ROUND_GAP_MS
                        } else {
                            ROUND_GAP_MS
                        };
                        if now.saturating_sub(self.round_ms) < gap {
                            return GoalStatus::Running;
                        }
                        self.round_ms = now;
                        self.spots_tried += 1;
                        self.visited.clear();
                        let rounds = if crowded { CROWD_ROUNDS } else { 2 };
                        if self.spots_tried >= rounds {
                            let why = if crowded {
                                Failure::Crowded
                            } else {
                                Failure::TargetMissing
                            };
                            return self.fail(ctx, why);
                        }
                        return GoalStatus::Running;
                    }
                }
            }
        };
        match self.walk(ctx, spot.0, spot.1, SPAWN_STOP_M) {
            Walk::Moving => self.renew_claim(ctx, spot_key(spot.0, spot.1)),
            Walk::Failed | Walk::Arrived => {
                // Nothing there (a monster in view would have been picked): the next spot.
                hub.contest.release(spot_key(spot.0, spot.1), bot);
                self.mark_visited(spot.0, spot.1);
                self.spot = None;
            }
        }
        GoalStatus::Running
    }

    fn nearest_unvisited(&self, spawns: &[Vec3], me: [f32; 3], min_m: f32) -> Option<Vec3> {
        let mut best: Option<(Vec3, f32)> = None;
        for p in spawns {
            let d2 = (p.x - me[0]).powi(2) + (p.y - me[1]).powi(2);
            if d2 < min_m * min_m
                || self.visited.contains(&cell(p.x, p.y))
                || best.is_some_and(|b| d2 >= b.1)
            {
                continue;
            }
            best = Some((*p, d2));
        }
        best.map(|b| b.0)
    }

    fn mark_visited(&mut self, x: f32, y: f32) {
        if self.visited.len() >= 4 {
            self.visited.remove(0);
        }
        self.visited.push(cell(x, y));
    }

    /// Monster `obj` is given up: left alone for `base_ms`, or for a long while when it could
    /// not be reached (and its whole kind after the second time).
    fn give_up(&mut self, ctx: &mut Ctx<'_>, obj: u32, base_ms: u64, unreachable: bool) {
        let now = ctx.now_ms;
        ctx.hub.contest.release(unit_key(obj), ctx.persona.id);
        if !unreachable {
            self.ignore_for(obj, now + base_ms);
            return;
        }
        let until = now + UNREACHABLE_MS;
        self.ignore_for(obj, until);
        let template = ctx.state.units.get(&obj).map(|u| u.id);
        let avoid = ctx.state.avoid_mut();
        avoid.ignore_obj(obj, until);
        if let Some(t) = template {
            if avoid.strike(t, now) {
                ctx.log.say(format!(
                    "quest {}: leaving monsters of kind {t} alone, unreachable",
                    self.quest
                ));
            }
        }
    }

    /// Tells the other bots the bot works on `key`.
    fn claim(&mut self, ctx: &Ctx<'_>, key: u64) {
        ctx.hub.contest.claim(key, ctx.persona.id);
        self.claim_key = key;
        self.claim_ms = ctx.now_ms.max(1);
    }

    /// Claims `key` if it is a new one, or renews the claim every [`CLAIM_RENEW_MS`].
    fn renew_claim(&mut self, ctx: &Ctx<'_>, key: u64) {
        if key != self.claim_key || ctx.now_ms.saturating_sub(self.claim_ms) >= CLAIM_RENEW_MS {
            self.claim(ctx, key);
        }
    }

    fn ignore_for(&mut self, obj: u32, until_ms: u64) {
        if self.ignore.len() >= 4 {
            self.ignore.remove(0);
        }
        self.ignore.push((obj, until_ms));
    }

    fn ignored(&self, obj: u32, now_ms: u64) -> bool {
        self.ignore.iter().any(|i| i.0 == obj && now_ms < i.1)
    }

    /// The nearest living monster of one of `templates` in view.
    ///
    /// A monster another bot is already fighting comes second: the bots spread over the monsters
    /// in view before they queue at one. Monsters that were out of reach are skipped.
    fn pick_mob(&self, ctx: &Ctx<'_>, templates: &[u32]) -> Option<u32> {
        let me = ctx.state.me.pos;
        let bot = ctx.persona.id;
        let mut best: Option<(u32, bool, f32)> = None;
        for (&obj, u) in &ctx.state.units {
            if u.kind != UnitKind::Npc
                || u.dead()
                || u.owned()
                || !templates.contains(&u.id)
                || self.ignored(obj, ctx.now_ms)
                || ctx.state.avoids(obj, u.id, ctx.now_ms)
            {
                continue;
            }
            let d = u.dist2d(me);
            if d > VIEW_RADIUS {
                continue;
            }
            // A monster another character tagged, or one that is fighting one, gives no quest
            // credit to the bot; it comes after every free monster in view.
            let taken =
                ctx.hub.contest.others(unit_key(obj), bot) > 0 || ctx.state.taken_by_other(obj);
            if best.is_none_or(|b| better_target(taken, d, b.1, b.2)) {
                best = Some((obj, taken, d));
            }
        }
        best.map(|b| b.0)
    }

    // ---- the detail line ----------------------------------------------------------------

    fn refresh_detail(&mut self, ctx: &Ctx<'_>) {
        let have = ctx
            .state
            .quests
            .row(self.quest)
            .map_or(-1, |r| r.objectives.iter().sum::<i32>());
        let mut key = (self.mode as u64)
            | (self.stage as u64) << 4
            | u64::from(self.obj) << 8
            | u64::from(self.quest) << 16
            | ((have.max(-1) + 1) as u64) << 48;
        let trip = (self.mode == Mode::Travel)
            .then(|| current_trip(ctx.state))
            .flatten();
        if let Some(t) = &trip {
            // (the distance to go changes the line every 100 m)
            let left = dist2d(ctx.state.me.pos, [t.target.x, t.target.y, 0.0]);
            key = (self.mode as u64)
                | ((left / 100.0) as u64) << 8
                | u64::from(t.started_ms as u32) << 24;
        }
        if key == self.detail_key && !self.detail.is_empty() {
            return;
        }
        self.detail_key = key;
        self.detail.clear();
        let world = ctx.world;
        if let Some(t) = trip {
            let left = dist2d(ctx.state.me.pos, [t.target.x, t.target.y, 0.0]);
            let zone = world
                .zone_key_at(t.target.x, t.target.y)
                .and_then(|k| world.zone_name(k))
                .unwrap_or("a far place");
            let _ = match t.target.why {
                Why::Story(q) => write!(
                    self.detail,
                    "heading to {zone} for quest {q} {} ({:.1} km to go)",
                    quest_name(world, q),
                    left / 1000.0
                ),
                Why::Zone(_) => write!(
                    self.detail,
                    "heading to {zone} for quests suited to level {} ({:.1} km to go)",
                    ctx.state.me.level,
                    left / 1000.0
                ),
                Why::Grind(_) => write!(
                    self.detail,
                    "heading to {zone} to hunt monsters ({:.1} km to go)",
                    left / 1000.0
                ),
            };
            return;
        }
        if self.mode == Mode::Pick || self.mode == Mode::Travel || self.quest == 0 {
            self.detail.push_str("looking for a quest");
            return;
        }
        let q = self.quest;
        let _ = write!(self.detail, "Quest {q} {}: ", quest_name(world, q));
        let Some(quest) = world.quest(q) else {
            return;
        };
        match self.mode {
            Mode::Pick | Mode::Travel => {}
            Mode::Accept => match self.giver {
                Some(Giver::Npc(t)) => {
                    let _ = write!(self.detail, "getting it from {}", npc_name(world, t));
                }
                Some(Giver::Doodad(t)) => {
                    let _ = write!(self.detail, "getting it from {}", doodad_name(world, t));
                }
                None => self.detail.push_str("accepting"),
            },
            Mode::TurnIn => match quest.report {
                QuestReport::Npc(t) => {
                    let _ = write!(self.detail, "turning in at {}", npc_name(world, t));
                }
                QuestReport::Doodad(t) => {
                    let _ = write!(self.detail, "turning in at {}", doodad_name(world, t));
                }
                _ => self.detail.push_str("turning in"),
            },
            Mode::Work => {
                let row = ctx.state.quests.row(q);
                let slot = row.and_then(|r| objective::next_unmet(quest, r, self.skip));
                match slot {
                    Some(s) => {
                        objective_text(&mut self.detail, world, &s);
                        let _ = write!(self.detail, " ({}/{})", s.have, s.need);
                    }
                    None => self.detail.push_str("waiting for the server"),
                }
            }
        }
    }
}

/// `supported` for a quest in the journal: only the objectives still to do count.
fn supported_from(
    facts: &dyn objective::Facts,
    quest: &QuestData,
    row: &aa_proto::sc_activity::QuestRow,
    level: u8,
) -> Result<(), Failure> {
    for s in objective::unmet(quest, row) {
        objective::plan(facts, quest, s.obj, level)?;
    }
    objective::report_supported(facts, quest)
}

/// A refusal the server sent for `quest` since `since_ms`.
fn server_error(state: &BotState, quest: u32, since_ms: u64) -> Option<u8> {
    state.errors.iter().rev().find_map(|(at, e)| match e {
        StateError::Quest { quest: q, err } if *q == quest && *at >= since_ms => Some(*err),
        _ => None,
    })
}

/// A unit requirement refusal the server sent for `quest` since `since_ms`: `kind`, `value`.
fn server_unit_req(state: &BotState, quest: u32, since_ms: u64) -> Option<(u8, u32)> {
    state
        .quest_mem
        .as_deref()
        .and_then(|m| m.unit_req)
        .filter(|(q, _, _, at)| *q == quest && *at >= since_ms)
        .map(|(_, kind, value, _)| (kind, value))
}

/// Wait between two casts of `skill`.
fn cast_gap_ms(world: &World, skill: u32) -> u64 {
    let cast = world.skill(skill).map_or(0, |s| u64::from(s.casting_time));
    (cast + 1_500).max(CAST_GAP_MS)
}

/// The nearest NPC of `template` in view that is alive, with its position.
fn nearest_unit(state: &BotState, template: u32, me: [f32; 3]) -> Option<(u32, [f32; 3])> {
    state
        .units
        .iter()
        .filter(|(_, u)| u.kind == UnitKind::Npc && u.id == template && !u.dead())
        .map(|(&obj, u)| (obj, u.pos, u.dist2d(me)))
        .min_by(|a, b| a.2.total_cmp(&b.2))
        .map(|(obj, pos, _)| (obj, pos))
}

/// The nearest living NPC of any of `templates`, a target nobody else holds first.
fn nearest_unit_in(
    state: &BotState,
    templates: &[u32],
    me: [f32; 3],
    taken: &dyn Fn(u32) -> bool,
) -> Option<(u32, [f32; 3])> {
    let mut best: Option<(u32, bool, f32, [f32; 3])> = None;
    for (&obj, u) in &state.units {
        if u.kind != UnitKind::Npc || !templates.contains(&u.id) || u.dead() {
            continue;
        }
        let d = dist3d(me, u.pos);
        let held = taken(obj);
        if best
            .as_ref()
            .is_none_or(|b| better_target(held, d, b.1, b.2))
        {
            best = Some((obj, held, d, u.pos));
        }
    }
    best.map(|b| (b.0, b.3))
}

/// The nearest living NPC whose template is not in `excluded` (a negated target group).
fn nearest_unit_excluding(
    state: &BotState,
    excluded: &[u32],
    me: [f32; 3],
) -> Option<(u32, [f32; 3])> {
    state
        .units
        .iter()
        .filter(|(_, u)| u.kind == UnitKind::Npc && !u.dead() && !excluded.contains(&u.id))
        .map(|(&obj, u)| (obj, u.pos, dist3d(me, u.pos)))
        .min_by(|a, b| a.2.total_cmp(&b.2))
        .map(|(obj, pos, _)| (obj, pos))
}

/// The nearest doodad of `template` in view, with its position.
fn nearest_doodad(state: &BotState, template: u32, me: [f32; 3]) -> Option<(u32, [f32; 3])> {
    state
        .doodads
        .iter()
        .filter(|(_, d)| d.template == template)
        .map(|(&obj, d)| (obj, d.pos, dist2d(me, d.pos)))
        .min_by(|a, b| a.2.total_cmp(&b.2))
        .map(|(obj, pos, _)| (obj, pos))
}

/// How far away a spawn spot counts when `others` other bots work on it already: every one of
/// them makes it 2.5 times as far (squared distances in and out).
fn spot_score(d2: f32, others: usize) -> f32 {
    let k = 1.0 + 1.5 * others as f32;
    d2 * k * k
}

/// Whether target A (`taken` by another bot, `d` metres away) is a better pick than the best so
/// far (`best_taken`, `best_d`): a free target beats a taken one, then the nearer wins.
fn better_target(taken: bool, d: f32, best_taken: bool, best_d: f32) -> bool {
    match (taken, best_taken) {
        (false, true) => true,
        (true, false) => false,
        _ => d < best_d,
    }
}

/// Whether the bot may expect the server to credit a kill it landed: when it saw itself or a
/// party member hit the monster first, yes; when another character tagged it, no. With no hit
/// seen (the monster was already in view before the bot arrived), it is judged as before: an
/// unobserved tagger is assumed only when the bot fights alone.
fn credit_expected(tag: Option<TagOwner>, other_character_near: bool) -> bool {
    match tag {
        Some(TagOwner::Me | TagOwner::Party) => true,
        Some(TagOwner::Other) => false,
        None => !other_character_near,
    }
}

/// Whether another character (a real player or one of our bots) is within `radius` metres.
fn characters_near(state: &BotState, radius: f32) -> bool {
    state
        .units
        .values()
        .any(|u| u.kind == UnitKind::Character && u.dist2d(state.me.pos) <= radius)
}

fn quest_name(world: &World, quest: u32) -> &str {
    world.quest(quest).map_or("?", |q| &q.name)
}

fn npc_name(world: &World, template: u32) -> &str {
    world.npc(template).map_or("an NPC", |n| &n.name)
}

fn doodad_name(world: &World, template: u32) -> &str {
    world.doodad(template).map_or("a doodad", |d| &d.name)
}

/// What the bot does for an objective, as text without the counters.
fn objective_text(out: &mut String, world: &World, slot: &Slot<'_>) {
    match &slot.obj.kind {
        ObjKind::Talk { npc } => {
            let _ = write!(out, "talk to {}", npc_name(world, *npc));
        }
        ObjKind::Hunt { npcs, .. } => {
            let _ = write!(
                out,
                "kill {}",
                npcs.first().map_or("monsters", |t| npc_name(world, *t))
            );
        }
        ObjKind::Gather { item, .. } => {
            let _ = write!(
                out,
                "gather {}",
                world.item_name(*item).unwrap_or("an item")
            );
        }
        ObjKind::UseItem { item, .. } => {
            let _ = write!(out, "use {}", world.item_name(*item).unwrap_or("an item"));
        }
        ObjKind::Interact { doodad, .. } => {
            let _ = write!(out, "use {}", doodad_name(world, *doodad));
        }
        ObjKind::Sphere { .. } => out.push_str("go to the marked area"),
        ObjKind::Craft { .. } => out.push_str("craft"),
        ObjKind::Level { level } => {
            let _ = write!(out, "reach level {level}");
        }
        ObjKind::Other(_) => out.push_str("do something unsupported"),
    }
}

/// One line per active quest for the bot's detail panel: `id name progress`.
pub fn summary(world: &World, state: &BotState) -> String {
    let mut out = String::new();
    for row in &state.quests.active {
        if !out.is_empty() {
            out.push_str("; ");
        }
        let _ = write!(out, "{} {}", row.template, quest_name(world, row.template));
        if row.status == quest_status::READY {
            out.push_str(" (ready)");
        } else if let Some(q) = world.quest(row.template) {
            if let Some(s) = objective::next_unmet(q, row, 0) {
                let _ = write!(out, " ({}/{})", s.have, s.need);
            }
        }
    }
    let done = state.quests.completed.len();
    let mem = state.quest_mem.as_deref();
    let _ = write!(
        out,
        "{}{done} done, {} accepted this session, {} failed",
        if out.is_empty() { "" } else { " | " },
        mem.map_or(0, |m| m.accepted),
        mem.map_or(0, |m| m.failed)
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_goal_is_one_pointer() {
        assert_eq!(std::mem::size_of::<Quest>(), std::mem::size_of::<usize>());
    }

    #[test]
    fn searched_cells_and_ignored_monsters_are_bounded() {
        let mut g = Inner::default();
        for i in 0..10 {
            g.mark_visited(i as f32 * 100.0, 0.0);
            g.ignore_for(i, 1000);
        }
        assert_eq!(g.visited.len(), 4);
        assert_eq!(g.ignore.len(), 4);
        assert!(g.visited.contains(&cell(900.0, 0.0)));
        assert!(!g.visited.contains(&cell(0.0, 0.0)));
    }

    #[test]
    fn ignored_monsters_come_back_after_their_time() {
        let mut g = Inner::default();
        g.ignore_for(7, 5000);
        assert!(g.ignored(7, 4999));
        assert!(!g.ignored(7, 5000));
        assert!(!g.ignored(8, 0));
    }

    #[test]
    fn the_nearest_unvisited_spawn_is_chosen() {
        let mut g = Inner::default();
        let spawns = [
            Vec3::new(100.0, 0.0, 0.0),
            Vec3::new(30.0, 0.0, 0.0),
            Vec3::new(5.0, 0.0, 0.0),
        ];
        let me = [0.0, 0.0, 0.0];
        // the spawn under the bot's feet is skipped when a minimum distance is asked for
        assert_eq!(
            g.nearest_unvisited(&spawns, me, 10.0),
            Some(Vec3::new(30.0, 0.0, 0.0))
        );
        assert_eq!(
            g.nearest_unvisited(&spawns, me, 0.0),
            Some(Vec3::new(5.0, 0.0, 0.0))
        );
        g.mark_visited(5.0, 0.0);
        assert_eq!(
            g.nearest_unvisited(&spawns, me, 0.0),
            Some(Vec3::new(30.0, 0.0, 0.0))
        );
        g.mark_visited(30.0, 0.0);
        g.mark_visited(100.0, 0.0);
        assert_eq!(g.nearest_unvisited(&spawns, me, 0.0), None);
    }

    #[test]
    fn server_refusals_are_matched_by_quest_and_time() {
        let mut s = BotState::new();
        s.now_ms = 5_000;
        s.push_error(StateError::Quest { quest: 7, err: 12 });
        s.now_ms = 6_000;
        s.push_error(StateError::Msg(3));
        assert_eq!(server_error(&s, 7, 4_000), Some(12));
        assert_eq!(server_error(&s, 7, 5_001), None);
        assert_eq!(server_error(&s, 8, 0), None);
    }

    #[test]
    fn unit_requirement_refusals_are_matched_by_quest_and_time() {
        let mut s = BotState::new();
        mem(&mut s).unit_req = Some((1137, 0x25, 1136, 5_000));
        assert_eq!(server_unit_req(&s, 1137, 4_000), Some((0x25, 1136)));
        assert_eq!(server_unit_req(&s, 1137, 5_001), None);
        assert_eq!(server_unit_req(&s, 1138, 0), None);
    }

    #[test]
    fn cells_group_nearby_points() {
        assert_eq!(cell(1.0, 1.0), cell(19.9, 19.9));
        assert_ne!(cell(19.9, 0.0), cell(20.1, 0.0));
    }

    #[test]
    fn a_spot_other_bots_work_on_counts_as_farther_away() {
        // 100 m away and free beats 100 m away and taken
        assert!(spot_score(100.0 * 100.0, 0) < spot_score(100.0 * 100.0, 1));
        // one other bot makes a spot 2.5 times as far: a free spot at 200 m wins over a taken
        // one at 100 m, but not over one at 30 m
        let taken_near = spot_score(100.0 * 100.0, 1);
        assert!(spot_score(200.0 * 200.0, 0) < taken_near);
        assert!(spot_score(300.0 * 300.0, 0) > taken_near);
        // more bots, farther still
        assert!(spot_score(50.0 * 50.0, 3) > spot_score(50.0 * 50.0, 2));
        assert_eq!(spot_score(0.0, 5), 0.0);
    }

    #[test]
    fn a_free_target_beats_a_taken_one_and_then_the_nearer_wins() {
        // (candidate taken, candidate dist, best taken, best dist)
        assert!(better_target(false, 50.0, true, 5.0), "free beats taken");
        assert!(
            !better_target(true, 5.0, false, 50.0),
            "taken loses to free"
        );
        assert!(better_target(false, 5.0, false, 50.0), "nearer wins");
        assert!(!better_target(false, 50.0, false, 5.0));
        assert!(better_target(true, 5.0, true, 50.0));
        assert!(
            !better_target(true, 50.0, true, 50.0),
            "a tie keeps the first"
        );
    }

    #[test]
    fn credit_follows_the_tag_the_bot_saw() {
        // the bot's own or its party's tag: the server credits it
        assert!(credit_expected(Some(TagOwner::Me), true));
        assert!(credit_expected(Some(TagOwner::Party), true));
        // another character tagged it: the killing blow brings no quest credit
        assert!(!credit_expected(Some(TagOwner::Other), false));
        // no hit seen: an unobserved tagger is assumed only when the bot fights alone
        assert!(credit_expected(None, false));
        assert!(!credit_expected(None, true));
    }

    #[test]
    fn the_server_reaches_are_the_3d_ones() {
        // `npc_out_of_quest_reach`: 3 m + 2 m tolerance + 10 m slack; doodad func reach: 5 + 15
        assert_eq!(QUEST_NPC_REACH, 15.0);
        assert_eq!(QUEST_DOODAD_REACH, 20.0);
        // a target 30 m above the spot below it is not "here" even at 2D distance 0
        let me = [0.0, 0.0, 100.0];
        let below = [0.0, 0.0, 130.0];
        assert!(dist2d(me, below) < 1.0);
        assert!(dist3d(me, below) > QUEST_NPC_REACH);
    }

    #[test]
    fn characters_near_ignores_monsters_and_the_far() {
        let mut s = BotState::new();
        s.me.pos = [0.0, 0.0, 0.0];
        let unit = |kind, x: f32| crate::bot::state::Unit {
            kind,
            id: 1,
            pos: [x, 0.0, 0.0],
            hp: 10,
            faction: 0,
            level: 1,
            flags: 0,
            target: 0,
            name: None,
        };
        s.units.insert(1, unit(UnitKind::Npc, 3.0));
        assert!(!characters_near(&s, CROWD_RADIUS_M));
        s.units
            .insert(2, unit(UnitKind::Character, CROWD_RADIUS_M + 1.0));
        assert!(!characters_near(&s, CROWD_RADIUS_M));
        s.units
            .insert(3, unit(UnitKind::Character, CROWD_RADIUS_M - 1.0));
        assert!(characters_near(&s, CROWD_RADIUS_M));
    }

    #[test]
    #[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
    fn event_quests_wait_for_their_event() {
        use aa_world::WorldPaths;
        use std::path::PathBuf;
        let (Some(compact), Some(server_data)) = (
            std::env::var_os("AA_COMPACT"),
            std::env::var_os("AA_SERVER_DATA"),
        ) else {
            eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
            return;
        };
        let world = World::load(&WorldPaths {
            compact: PathBuf::from(compact),
            server_data: PathBuf::from(server_data),
            baked: std::env::var_os("AA_BAKED").map(PathBuf::from),
            world: "main_world".into(),
        })
        .unwrap();
        // 6209 "Armies of Castigant" and 6211 "Clockwork Rebellion" hunt Rift wave NPCs;
        // 8000004 is a seasonal event quest. None may be taken while its event is down.
        let event_quest = world.quest(6209).unwrap();
        let targets = event_targets(&world, event_quest).expect("6209 is event-gated");
        assert!(targets.contains(&14580) && targets.contains(&14581));
        assert!(event_targets(&world, world.quest(8000004).unwrap()).is_some());
        assert!(event_targets(&world, world.quest(2261).unwrap()).is_none());

        let events = EventBoard::manual();
        let no = |_: u32| false;
        let all = |_: u32| true;
        let elig = Eligibility {
            world: &world,
            facts: Env {
                world: &world,
                from: None,
                navmesh: false,
            },
            level: 80,
            faction: 0,
            who: aa_world::Who {
                level: 80,
                race: 1,
                gender: 1,
                mother_faction: 0,
                done: &all,
                in_journal: &no,
                owns: &all,
            },
            judge: Judge {
                level: 80,
                done: &no,
                active: &no,
                usable: &all,
            },
            events: &events,
        };
        assert_eq!(
            elig.explain(event_quest, Giver::Npc(0)),
            Err("event not running")
        );
    }

    #[test]
    fn crowded_spawns_get_more_patience_than_empty_ones() {
        // the waits are what the module docs promise
        const { assert!(CROWD_ROUNDS > 2) };
        const { assert!(CROWD_ROUND_GAP_MS > ROUND_GAP_MS) };
        const { assert!(memory::CROWD_BACKOFF_MS >= memory::BACKOFF_STEPS_MS[0]) };
    }
}
