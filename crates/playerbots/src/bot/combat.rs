//! The combat controller: engage a target, get into range, face it, cast the archetype's
//! rotation (or, without learned skills, its starter skill and the auto attack), and loot what
//! dies.
//!
//! The skill choice is the rotation engine's ([`super::rotation`]): once the bot knows skills,
//! [`Combat::tick`] asks the archetype's compiled rotation what to cast, falling back to the
//! starter skill and the basic attack when the rotation has nothing castable. The controller
//! also learns skills and unlocks abilities as the level grows ([`super::rotation::learn`]) and
//! keeps the bot engaged with its opponent during a duel ([`DuelWatch`]).
//!
//! The API is **stable on purpose** so the goals never had to change when the rotation arrived:
//!
//! * [`Combat::engage`] / [`Combat::disengage`] set or clear the target,
//! * [`Combat::tick`] runs one step and returns a [`CombatStatus`],
//! * [`Combat::on_event`] feeds it the server's answers (skill failures, loot windows),
//! * [`Combat::busy`] says whether a fight or a loot job is under way.
//!
//! Everything it decides is in small pure pieces ([`pick_skill`], [`Backoffs`], [`LootJob`],
//! [`arrival_radius`]) with their own tests; [`Combat::tick`] only gathers the facts from the
//! [`Ctx`] and acts on the verdict.

use aa_proto::cs;
use aa_proto::sc::ServerEvent;
use aa_proto::sc_world::WorldEvent;
use aa_world::{NpcTemplate, Relation, World};
use smallvec::SmallVec;

use super::ctx::Ctx;
use super::goals::duel::{Answer, DuelPhase, DuelWatch};
use super::motion::{MotionStatus, Pace};
use super::pvp;
use super::rotation::{
    self, CastKind, Decision, Inputs, LearnCtx, Learner, PartyView, RotState, Rotation, SkillBook,
    TargetView,
};
use super::state::{dist2d, Unit, UnitKind};
use super::watch::classify::{self, Refusal};
use super::watch::Finding;

/// The server takes loot from within 5 m of its picture of the bot (which can lag, see
/// [`super::motion::Motion::pin`]); the bot opens a loot window only this close.
pub const LOOT_OPEN_RANGE: f32 = 3.5;
/// How close a bot walks to a corpse before it opens the loot.
const LOOT_WALK_RADIUS: f32 = 2.5;
/// How long a kill waits for the server's "has loot" notice.
const LOOT_AWAIT_MS: u64 = 3000;
/// How long an opened loot window may take.
const LOOT_OPEN_TIMEOUT_MS: u64 = 4000;
/// Margin added to a skill's cooldown, which the server starts when the skill fires.
const COOLDOWN_SLACK_MS: u64 = 60;
/// `SkillResult::CooldownTime`: a cooldown or the global cooldown refused the cast.
const COOLDOWN_REFUSED: u8 = 8;
/// `SkillResult::LackMana`.
const MANA_REFUSED: u8 = 0xB;
/// `SkillResult::TooFarRange`, and the step and limit of the distance penalty it causes.
const TOO_FAR_REFUSED: u8 = 0xF;
const RANGE_PEN_STEP_M: f32 = 1.0;
const RANGE_PEN_MAX_M: f32 = 4.0;
/// Bounds and step of the adaptive global cooldown estimate, and how many accepted casts in a
/// row narrow it again.
const GCD_MIN_MS: u16 = rotation::GCD_MS as u16;
const GCD_MAX_MS: u16 = 2400;
const GCD_STEP_MS: u16 = 300;
const GCD_RELAX_AFTER: u8 = 8;
/// The global cooldown the server starts with an auto attack (`default_gcd`).
const AUTO_GCD_MS: u64 = 1000;
/// With a rotation, an auto attack goes out only this long after the rotation's last cast and
/// its global cooldown.
const AUTO_QUIET_MS: u64 = 300;
/// Global cooldown between two skills.
pub const GCD_MS: u64 = 1000;
/// Time between two auto attacks.
pub const AUTO_ATTACK_GAP_MS: u64 = 1200;
/// A fight against one target ends after this long.
pub const FIGHT_TIMEOUT_MS: u64 = 120_000;
/// A fight in which neither side lost a hit point for this long is given up: the target cannot
/// be reached, evades, or is out of sight of every skill.
pub const NO_DAMAGE_MS: u64 = 30_000;
/// `SkillResult::Obstacle`: terrain or a model blocks the line of sight to the target.
const OBSTACLE_REFUSED: u8 = 12;
/// The server's `NoTarget`: the unit the cast was aimed at does not exist for the server (it
/// despawned, or the bot's picture of it is stale).
const NO_TARGET_REFUSED: u8 = 9;
/// Refusals with [`NO_TARGET_REFUSED`] against the current target after which it is given up.
const NO_TARGET_GIVE_UP: u8 = 3;
/// A unit the server does not know is left alone this long (longer than a fight can last).
const GONE_IGNORE_MS: u64 = FIGHT_TIMEOUT_MS + 30_000;
/// Blocked casts in one fight after which a monster counts as unreachable.
const OBSTACLE_GIVE_UP: u8 = 4;
/// After a blocked cast the bot casts nothing for this long while it takes another position.
const OBSTACLE_HOLD_MS: u64 = 1_500;
/// Angle (degrees around the target) of the first position tried after a blocked cast, and what
/// each further blocked cast adds.
const SIDESTEP_DEG: (f32, f32) = (50.0, 25.0);
/// A target further away than this is lost.
const LOST_DISTANCE: f32 = 80.0;
/// Pauses after a refused skill: first, second and later failures.
const BACKOFF_MS: [u64; 3] = [2_000, 8_000, 30_000];
/// Pause after an error message that probably belongs to the last cast.
const SHORT_BACKOFF_MS: u64 = 1_500;
/// An error message this soon after a cast is blamed on it.
const ERROR_BLAME_MS: u64 = 600;
/// A unit that targeted the bot this long ago still counts as a threat.
const THREAT_WINDOW_MS: u64 = 10_000;
/// Hit point loss this recent makes the bot look for an attacker.
const DAMAGE_WINDOW_MS: u64 = 3_000;
/// How far the bot looks for an attacker it cannot name.
const REACT_RANGE: f32 = 12.0;
/// A goal that walks somewhere (a quest trip, a trade run) refreshes its note this often; the
/// controller treats the bot as travelling for [`TRAVEL_NOTE_MS`] after the last one.
const TRAVEL_NOTE_MS: u64 = 2_500;
/// While travelling, a unit that targeted the bot longer ago than this is no threat.
const TRAVEL_THREAT_WINDOW_MS: u64 = 4_000;
/// While travelling, the bot only stops for attackers closer than this.
const TRAVEL_FIGHT_M: f32 = 15.0;
/// While travelling, the bot gives up a defence against an attacker that falls behind this far.
const TRAVEL_LOSE_M: f32 = 25.0;
/// While travelling, the bot looks for an unnamed attacker only this close.
const TRAVEL_REACT_M: f32 = 6.0;
/// Slack on a skill's range.
const RANGE_SLACK: f32 = 0.25;
/// A duel opponent is only lost beyond this distance (the server ends a duel at 75 m).
const DUEL_LOST_DISTANCE: f32 = 150.0;
/// A ranged bot backs away from a player closer than this share of its fighting range.
const KITE_CLOSE: f32 = 0.35;
/// A kiting step lasts at most this long.
const KITE_MS: u64 = 2_500;
/// Pause between two kiting steps.
const KITE_GAP_MS: u64 = 7_000;
/// Hit points (percent) below which an unexpected duel challenge is declined.
const DUEL_MIN_HP_PCT: u8 = 50;

/// What one combat step came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CombatStatus {
    /// No target, nothing to do.
    Idle,
    /// Walking into range of the target.
    Approaching,
    /// In range and attacking.
    Fighting,
    /// The target just died (reported once).
    TargetDead {
        /// Object id of the dead unit.
        obj: u32,
    },
    /// The target left the view, ran too far or the fight took too long.
    Lost,
}

/// Why the controller gave up a target ([`CombatStatus::Lost`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LostReason {
    /// The target left the view (died elsewhere, evaded, despawned).
    Gone,
    /// The target ran farther than the controller follows.
    Far,
    /// The fight took longer than [`FIGHT_TIMEOUT_MS`].
    Timeout,
    /// Nobody lost a hit point for [`NO_DAMAGE_MS`]: the bot could not hurt the target.
    Stalled,
    /// The way to the target is blocked, or the server kept refusing casts for an obstacle.
    Blocked,
    /// A defence against an attacker was dropped because the bot is on its way somewhere.
    Travel,
    /// The bot is too hurt to fight a character on (`[pvp] retreat_hp_pct`); it runs.
    Retreat,
    /// The server accepts cast after cast at a character and nothing happens to it: the zone
    /// protects it. The bot gives up and leaves it alone for a while.
    Protected,
}

/// How the last [`CombatStatus::Lost`] came about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Loss {
    /// Why the target was given up.
    pub reason: LostReason,
    /// The target lost hit points during the fight (the bot can hurt it at all).
    pub dealt: bool,
}

impl Loss {
    /// Whether the target looks out of the bot's reach: the fight stalled, ran into a wall, or
    /// ran out of time without the target ever losing a hit point. Goals leave such a target
    /// alone for a long time. (A target that left the view or ran away is just gone.)
    pub fn unreachable(self) -> bool {
        match self.reason {
            LostReason::Stalled | LostReason::Blocked => true,
            LostReason::Timeout => !self.dealt,
            // (a character the bot cannot hurt is shunned for a shorter time, see `pvp_aftermath`)
            LostReason::Gone
            | LostReason::Far
            | LostReason::Travel
            | LostReason::Retreat
            | LostReason::Protected => false,
        }
    }
}

/// What the controller saw of the fight against the current target.
#[derive(Debug, Clone, Copy)]
struct Watch {
    /// Bot clock of the last hit point loss on either side (the engagement before that).
    progress_ms: u64,
    /// The target's hit points at the last step; `i32::MIN` before the first.
    target_hp: i32,
    /// The bot's `last_damage_ms` at the last step.
    damage_seen: u64,
    /// The target lost hit points during this fight.
    dealt: bool,
    /// Casts the server refused for an obstacle during this fight.
    obstacles: u8,
    /// Casts the server refused because it does not know the target.
    no_target: u8,
    /// A blocked cast waits for the bot to take another position.
    sidestep: bool,
    /// Until then the bot casts nothing (it takes another position).
    hold_until_ms: u64,
    /// Casts at this target the server accepted (`SkillStarted` with result 0).
    accepted: u8,
    /// Bot clock of the first of them.
    first_accept_ms: u64,
    /// The "takes no damage" finding was left for this target.
    immune_told: bool,
}

impl Default for Watch {
    fn default() -> Self {
        Watch {
            progress_ms: 0,
            target_hp: i32::MIN,
            damage_seen: 0,
            dealt: false,
            obstacles: 0,
            no_target: 0,
            sidestep: false,
            hold_until_ms: 0,
            accepted: 0,
            first_accept_ms: 0,
            immune_told: false,
        }
    }
}

impl Watch {
    /// Notes the target's hit points and the bot's last hit point loss at `now_ms`.
    fn observe(&mut self, now_ms: u64, target_hp: i32, last_damage_ms: u64) {
        if self.target_hp == i32::MIN {
            self.progress_ms = now_ms;
            self.damage_seen = last_damage_ms;
        } else if target_hp < self.target_hp {
            self.dealt = true;
            self.progress_ms = now_ms;
        }
        if last_damage_ms > self.damage_seen {
            self.damage_seen = last_damage_ms;
            self.progress_ms = now_ms;
        }
        self.target_hp = target_hp;
    }

    /// Whether neither side lost a hit point for [`NO_DAMAGE_MS`].
    fn stalled(&self, now_ms: u64) -> bool {
        self.target_hp != i32::MIN && now_ms.saturating_sub(self.progress_ms) > NO_DAMAGE_MS
    }
}

/// A skill the bot could cast right now, as far as the facts go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkillCand {
    /// Skill id.
    pub id: u32,
    /// The basic attack (spaced by [`AUTO_ATTACK_GAP_MS`] instead of the global cooldown).
    pub is_auto: bool,
    /// Learned, off cooldown, affordable and not backed off.
    pub ready: bool,
    /// The target is within the skill's range.
    pub in_range: bool,
}

/// Picks the skill to cast: the first ready candidate in range whose timer allows it.
/// `cands` is in order of preference (starter skill, then the auto attack).
pub fn pick_skill(
    cands: &[SkillCand],
    now_ms: u64,
    casting_until_ms: u64,
    gcd_until_ms: u64,
    next_auto_ms: u64,
) -> Option<u32> {
    if now_ms < casting_until_ms {
        return None;
    }
    cands
        .iter()
        .filter(|c| c.ready && c.in_range)
        .find(|c| {
            if c.is_auto {
                now_ms >= next_auto_ms
            } else {
                now_ms >= gcd_until_ms
            }
        })
        .map(|c| c.id)
}

/// How close the bot walks to a target it wants to fight from `preferred` metres.
pub fn arrival_radius(preferred: f32) -> f32 {
    (preferred - 1.0).max(1.0)
}

/// Whether the bot has to move to fight a target `dist` metres away.
pub fn must_approach(dist: f32, preferred: f32) -> bool {
    dist > preferred + 1.0
}

/// A chase is aimed again when the target stands this far (metres) from the point the bot runs
/// to. Farther, a bot that chases a moving target stops where the target was, short of it, and
/// starts again at once: a stutter for whoever watches.
pub const CHASE_RETARGET_M: f32 = 1.0;

/// Whether the chase of a target at `target` has to be started, or aimed again: the bot stands
/// (`moving` false), or the target is more than [`CHASE_RETARGET_M`] from `last_goto`, the
/// point the chase runs to.
pub fn chase_again(last_goto: Option<(f32, f32)>, target: [f32; 3], moving: bool) -> bool {
    !moving
        || last_goto
            .is_none_or(|g| dist2d([g.0, g.1, 0.0], [target[0], target[1], 0.0]) > CHASE_RETARGET_M)
}

/// Skills the server refused, with growing pauses.
#[derive(Debug, Default, Clone)]
pub struct Backoffs {
    /// `(skill, blocked until ms, failures in a row)`.
    list: SmallVec<[(u32, u64, u8); 4]>,
}

impl Backoffs {
    /// The server refused `skill`: block it for longer each time in a row.
    pub fn fail(&mut self, skill: u32, now_ms: u64) {
        self.block(skill, now_ms, true);
    }

    /// An error that may belong to `skill`: a short pause, no escalation.
    pub fn fail_short(&mut self, skill: u32, now_ms: u64) {
        self.block(skill, now_ms, false);
    }

    fn block(&mut self, skill: u32, now_ms: u64, escalate: bool) {
        if let Some(e) = self.list.iter_mut().find(|e| e.0 == skill) {
            if escalate {
                e.2 = (e.2 + 1).min(BACKOFF_MS.len() as u8);
            }
            let ms = if escalate {
                BACKOFF_MS[usize::from(e.2.saturating_sub(1)).min(BACKOFF_MS.len() - 1)]
            } else {
                SHORT_BACKOFF_MS
            };
            e.1 = e.1.max(now_ms + ms);
            return;
        }
        if self.list.len() >= 4 {
            self.list.remove(0);
        }
        let ms = if escalate {
            BACKOFF_MS[0]
        } else {
            SHORT_BACKOFF_MS
        };
        self.list.push((skill, now_ms + ms, u8::from(escalate)));
    }

    /// The skill worked: forget its failures.
    pub fn succeed(&mut self, skill: u32) {
        self.list.retain(|e| e.0 != skill);
    }

    /// Whether `skill` is paused at `now_ms`.
    pub fn blocked(&self, skill: u32, now_ms: u64) -> bool {
        self.list.iter().any(|e| e.0 == skill && now_ms < e.1)
    }
}

/// Where a loot job is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LootStage {
    /// Waiting for the server to say the corpse has loot.
    Await,
    /// Walking to the corpse.
    Walk,
    /// The loot window was asked for.
    Opened,
}

/// What the loot job sees of its corpse.
#[derive(Debug, Clone, Copy)]
pub struct LootView {
    /// The corpse is still in view.
    pub exists: bool,
    /// The server says it has loot.
    pub has_loot: bool,
    /// Distance to the corpse.
    pub dist: f32,
    /// The bot clock.
    pub now_ms: u64,
}

/// What the loot job wants done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LootAction {
    /// Nothing yet.
    Wait,
    /// Walk to the corpse.
    WalkTo,
    /// Send the open request.
    Open,
    /// The job is over (looted, gone or given up).
    Done,
}

/// Looting one corpse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LootJob {
    /// Object id of the corpse.
    pub owner: u32,
    /// Where the job is.
    pub stage: LootStage,
    /// Bot clock at the start of the stage.
    pub since_ms: u64,
}

impl LootJob {
    /// A job for a fresh kill.
    pub fn new(owner: u32, now_ms: u64) -> LootJob {
        LootJob {
            owner,
            stage: LootStage::Await,
            since_ms: now_ms,
        }
    }

    /// One step of the job.
    pub fn step(&mut self, v: &LootView) -> LootAction {
        if !v.exists {
            return LootAction::Done;
        }
        if self.stage == LootStage::Await {
            if v.has_loot {
                self.stage = LootStage::Walk;
                self.since_ms = v.now_ms;
            } else if v.now_ms.saturating_sub(self.since_ms) > LOOT_AWAIT_MS {
                return LootAction::Done;
            } else {
                return LootAction::Wait;
            }
        }
        match self.stage {
            LootStage::Walk => {
                if v.dist > LOOT_OPEN_RANGE {
                    LootAction::WalkTo
                } else {
                    self.stage = LootStage::Opened;
                    self.since_ms = v.now_ms;
                    LootAction::Open
                }
            }
            LootStage::Opened => {
                if !v.has_loot || v.now_ms.saturating_sub(self.since_ms) > LOOT_OPEN_TIMEOUT_MS {
                    LootAction::Done
                } else {
                    LootAction::Wait
                }
            }
            LootStage::Await => LootAction::Wait,
        }
    }
}

/// The combat controller of one bot. See the module docs.
#[derive(Debug)]
pub struct Combat {
    target: Option<u32>,
    announced: bool,
    last_goto: Option<(f32, f32)>,
    engaged_at_ms: Option<u64>,
    status: CombatStatus,
    casting_until_ms: u64,
    gcd_until_ms: u64,
    next_auto_ms: u64,
    last_cast: Option<(u32, u64)>,
    backoffs: Backoffs,
    loot: Option<LootJob>,
    kills: u32,
    casts: u32,
    cast_failed: u32,
    /// Estimate of the global cooldown after a rotation cast; widened when the server refuses a
    /// cast for the cooldown (plot skills start further cooldowns of their own).
    gcd_ms: u16,
    /// Accepted casts in a row since the last cooldown refusal.
    gcd_ok: u8,
    /// Metres the bot pretends to be farther from its target: grows when the server says a cast
    /// was too far (its distance differs from the bot's), shrinks with accepted casts.
    range_pen: f32,
    /// A cast the server refused for lack of mana, until [`Combat::tick`] notes the bot's mana.
    mana_refused: u32,
    rot: RotState,
    learner: Learner,
    /// The duel watch: the duel goal reads it, the controller answers challenges through it.
    pub duel: DuelWatch,
    /// Object id of the opponent of the duel the controller engaged for (0 = none).
    duel_opponent: u32,
    me_obj: u32,
    /// The last unit seen casting at the target `(caster, skill, bot clock)`.
    target_cast: Option<(u32, u32, u64)>,
    /// The last refusal of a cast of the bot `(skill, result)`, to log.
    last_refusal: Option<(u32, u8)>,
    kite_until_ms: u64,
    next_kite_ms: u64,
    /// Until this bot clock a goal says the bot is travelling, see [`Combat::note_travelling`].
    travelling_until_ms: u64,
    /// The target the controller picked to defend against (0 = it is no defence).
    defending: u32,
    /// The target the controller took up to help the party (0 = it is no assist).
    assisting: u32,
    /// What the controller saw of the fight against the current target.
    watch: Watch,
    /// How the last target was lost.
    loss: Option<Loss>,
    /// Casts the server refused for an obstacle so far (this process).
    obstructed: u32,
    /// Object id of a target the server called immune to a skill, until [`Combat::tick`] reports it.
    immune_refused: u32,
    /// The bot is running from a fight with a character (see [`pvp::fight::start_flee`]).
    fleeing: bool,
}

impl Default for Combat {
    fn default() -> Self {
        Combat::new()
    }
}

impl Combat {
    /// An idle controller.
    pub fn new() -> Combat {
        Combat {
            target: None,
            announced: false,
            last_goto: None,
            engaged_at_ms: None,
            status: CombatStatus::Idle,
            casting_until_ms: 0,
            gcd_until_ms: 0,
            next_auto_ms: 0,
            last_cast: None,
            backoffs: Backoffs::default(),
            loot: None,
            kills: 0,
            casts: 0,
            cast_failed: 0,
            gcd_ms: rotation::GCD_MS as u16,
            gcd_ok: 0,
            range_pen: 0.0,
            mana_refused: 0,
            rot: RotState::default(),
            learner: Learner::default(),
            duel: DuelWatch::default(),
            duel_opponent: 0,
            me_obj: 0,
            target_cast: None,
            last_refusal: None,
            kite_until_ms: 0,
            next_kite_ms: 0,
            travelling_until_ms: 0,
            defending: 0,
            assisting: 0,
            watch: Watch::default(),
            loss: None,
            obstructed: 0,
            immune_refused: 0,
            fleeing: false,
        }
    }

    /// A goal that is walking somewhere (a quest trip, a trade run's way to the bench or buyer)
    /// says so every tick it walks. For a moment after the last note the controller defends the
    /// bot only against units that really attack it and are close, and drops a defence against an
    /// attacker that falls far behind; everything else lets the bot keep going.
    pub fn note_travelling(&mut self, now_ms: u64) {
        self.travelling_until_ms = now_ms + TRAVEL_NOTE_MS;
    }

    /// Whether a goal said at the last tick that the bot is travelling.
    pub fn travelling(&self, now_ms: u64) -> bool {
        now_ms < self.travelling_until_ms
    }

    /// Fights the unit with object id `target_obj` (a no-op if it is the target already).
    pub fn engage(&mut self, target_obj: u32) {
        if self.target == Some(target_obj) {
            return;
        }
        self.target = Some(target_obj);
        self.defending = 0;
        self.assisting = 0;
        self.announced = false;
        self.last_goto = None;
        self.engaged_at_ms = None;
        self.rot.reset_fight(self.me_obj);
        self.target_cast = None;
        self.watch = Watch::default();
    }

    /// Like [`Combat::engage`] for a target taken up to help the party (the kill is the bot's to
    /// loot, as with the controller's own assists).
    pub fn engage_assist(&mut self, target_obj: u32) {
        self.engage(target_obj);
        self.assisting = target_obj;
    }

    /// A fresh start against the current target: forgets what the fight so far taught the
    /// controller (skill back-offs, range corrections, the stall watch) and keeps the target.
    pub fn restart_fight(&mut self) {
        let Some(target) = self.target else { return };
        self.disengage();
        self.engage(target);
        self.backoffs = Backoffs::default();
        self.range_pen = 0.0;
        self.kite_until_ms = 0;
        self.next_kite_ms = 0;
    }

    /// The server-side target was cleared (or replaced) by someone else: announce the target
    /// afresh before the next cast.
    pub fn target_cleared(&mut self) {
        self.announced = false;
    }

    /// Stops fighting (a running loot job continues).
    pub fn disengage(&mut self) {
        self.target = None;
        self.defending = 0;
        self.assisting = 0;
        self.announced = false;
        self.last_goto = None;
        self.engaged_at_ms = None;
    }

    /// Forgets everything (death, reconnect).
    pub fn reset(&mut self) {
        let (kills, casts, failed, obstructed) =
            (self.kills, self.casts, self.cast_failed, self.obstructed);
        *self = Combat::new();
        self.kills = kills;
        self.casts = casts;
        self.cast_failed = failed;
        self.obstructed = obstructed;
    }

    /// The current target.
    pub fn target(&self) -> Option<u32> {
        self.target
    }

    /// Whether a fight or a loot job is under way. The end of a duel counts until the duel goal
    /// has taken it, so that no other goal (resting after the fight) pushes the goal aside first.
    pub fn busy(&self) -> bool {
        self.target.is_some()
            || self.fleeing
            || self.loot.is_some()
            || (self.duel.attended() && self.duel.peek_end().is_some())
    }

    /// Marks the bot as running from a fight (the run itself is [`pvp::fight::flee_step`]); goals
    /// that do not drive the controller wait while it lasts.
    pub fn set_fleeing(&mut self, on: bool) {
        self.fleeing = on;
    }

    /// Whether the bot is running from a fight with a character.
    pub fn fleeing(&self) -> bool {
        self.fleeing
    }

    /// The result of the last [`Combat::tick`].
    pub fn status(&self) -> CombatStatus {
        self.status
    }

    /// Kills so far (this process).
    pub fn kills(&self) -> u32 {
        self.kills
    }

    /// Skills cast so far (this process).
    pub fn casts(&self) -> u32 {
        self.casts
    }

    /// Casts the server refused so far (this process).
    pub fn casts_refused(&self) -> u32 {
        self.cast_failed
    }

    /// Casts the server refused because something blocked the line of sight (this process).
    pub fn casts_obstructed(&self) -> u32 {
        self.obstructed
    }

    /// Whether the current target lost hit points since the controller engaged it.
    pub fn dealt_damage(&self) -> bool {
        self.watch.dealt
    }

    /// How the last target was lost; read it when [`Combat::tick`] returned
    /// [`CombatStatus::Lost`].
    pub fn last_loss(&self) -> Option<Loss> {
        self.loss
    }

    /// Skills learned and learn requests the server ignored.
    pub fn learned(&self) -> (u16, u16) {
        self.learner.counts()
    }

    /// The loot job, if any.
    pub fn loot_job(&self) -> Option<&LootJob> {
        self.loot.as_ref()
    }

    /// Feeds the controller the server's answers: skill results, buffs, duel packets and the
    /// loot window.
    pub fn on_event(&mut self, ev: &ServerEvent, me_obj: u32, now_ms: u64, out: &mut Vec<cs::Out>) {
        self.me_obj = me_obj;
        self.duel.on_event(ev, me_obj, now_ms);
        let ServerEvent::World(w) = ev else { return };
        match w {
            WorldEvent::SkillStarted {
                caster_obj,
                skill,
                result,
                target_obj,
                ..
            } if *caster_obj == me_obj && me_obj != 0 => {
                if *result == COOLDOWN_REFUSED {
                    // The global cooldown, or one the bot did not know of: a short wait, and a
                    // wider estimate so the next casts are spaced better.
                    self.backoffs.fail_short(*skill, now_ms);
                    self.cast_failed += 1;
                    self.last_refusal = Some((*skill, *result));
                    self.gcd_ms = (self.gcd_ms + GCD_STEP_MS).min(GCD_MAX_MS);
                    self.gcd_ok = 0;
                    self.gcd_until_ms = self.gcd_until_ms.max(now_ms + u64::from(GCD_STEP_MS));
                } else if *result == OBSTACLE_REFUSED {
                    // Something stands between the bot and the target. That is the position's
                    // fault, not the skill's: a short pause (no escalating back-off, no failure
                    // count) while the controller takes another position.
                    self.backoffs.fail_short(*skill, now_ms);
                    self.obstructed += 1;
                    self.watch.obstacles = self.watch.obstacles.saturating_add(1);
                    self.watch.sidestep = true;
                    self.watch.hold_until_ms = now_ms + OBSTACLE_HOLD_MS;
                    self.last_refusal = Some((*skill, *result));
                } else if *result != 0 {
                    if classify::refusal(*result) == Refusal::Immune {
                        self.immune_refused = *target_obj;
                    }
                    if *result == MANA_REFUSED {
                        self.mana_refused = *skill;
                    } else if *result == TOO_FAR_REFUSED {
                        self.range_pen = (self.range_pen + RANGE_PEN_STEP_M).min(RANGE_PEN_MAX_M);
                    } else if *result == NO_TARGET_REFUSED && Some(*target_obj) == self.target {
                        self.watch.no_target = self.watch.no_target.saturating_add(1);
                    }
                    self.backoffs.fail(*skill, now_ms);
                    self.cast_failed += 1;
                    self.last_refusal = Some((*skill, *result));
                } else {
                    if self.target.is_some() && Some(*target_obj) == self.target {
                        if self.watch.accepted == 0 {
                            self.watch.first_accept_ms = now_ms;
                        }
                        self.watch.accepted = self.watch.accepted.saturating_add(1);
                    }
                    self.backoffs.succeed(*skill);
                    self.watch.obstacles = 0;
                    self.watch.no_target = 0;
                    self.range_pen = (self.range_pen - RANGE_PEN_STEP_M / 4.0).max(0.0);
                    self.gcd_ok += 1;
                    if self.gcd_ok >= GCD_RELAX_AFTER {
                        self.gcd_ok = 0;
                        self.gcd_ms = self.gcd_ms.saturating_sub(GCD_STEP_MS).max(GCD_MIN_MS);
                    }
                }
                self.rot.on_cast_result(*skill, *result, now_ms);
            }
            WorldEvent::SkillStarted {
                caster_obj,
                skill,
                result: 0,
                ..
            } if Some(*caster_obj) == self.target => {
                self.target_cast = Some((*caster_obj, *skill, now_ms));
            }
            WorldEvent::BuffCreated {
                obj,
                index,
                caster_obj,
                skill_id,
                duration_ms,
                ..
            } => self.rot.on_buff_created(
                now_ms,
                me_obj,
                *obj,
                *index,
                *caster_obj,
                *skill_id,
                *duration_ms,
            ),
            WorldEvent::BuffRemoved { obj, index } => self.rot.on_buff_removed(*obj, *index),
            WorldEvent::ErrorMsg { .. } => {
                if let Some((skill, at)) = self.last_cast {
                    if now_ms.saturating_sub(at) <= ERROR_BLAME_MS {
                        self.backoffs.fail_short(skill, now_ms);
                    }
                }
            }
            WorldEvent::CastingStopped { .. } => self.casting_until_ms = 0,
            WorldEvent::LootBag {
                owner_obj,
                owner_type,
                items,
                ..
            } if self.loot.is_some_and(|j| j.owner == *owner_obj) => {
                for it in items {
                    out.push(cs::loot_item(it.index, *owner_type, *owner_obj));
                }
                out.push(cs::loot_close_bag(0, *owner_type, *owner_obj));
            }
            _ => {}
        }
    }

    /// One combat step. See the module docs.
    pub fn tick(ctx: &mut Ctx<'_>) -> CombatStatus {
        ctx.combat.me_obj = ctx.state.me.obj;
        if ctx.combat.mana_refused != 0 {
            let skill = std::mem::take(&mut ctx.combat.mana_refused);
            ctx.combat.rot.learn_mana_floor(skill, ctx.state.me.mp);
        }
        let status = if ctx.state.me.dead || ctx.state.me.obj == 0 {
            ctx.combat.reset();
            CombatStatus::Idle
        } else {
            Self::duel_step(ctx);
            Self::learn_step(ctx);
            Self::step(ctx)
        };
        if let Some((skill, result)) = ctx.combat.last_refusal.take() {
            ctx.log
                .debug(format!("skill {skill} was refused (result {result})"));
            // A refusal the bot's own checks rule out is the server's to explain; the cooldown,
            // mana, range, sight and movement refusals are the bot's rough pre-checks.
            if classify::refusal(result) == Refusal::Unexpected {
                ctx.find(Finding::SkillRefused {
                    skill,
                    code: result,
                });
            }
        }
        if ctx.combat.immune_refused != 0 {
            let obj = std::mem::take(&mut ctx.combat.immune_refused);
            let npc = ctx
                .state
                .units
                .get(&obj)
                .filter(|u| u.kind == UnitKind::Npc)
                .map(|u| u.id);
            if let Some(npc) = npc {
                ctx.find(Finding::TargetImmune {
                    npc,
                    casts: 0,
                    refused: true,
                });
            }
        }
        ctx.combat.status = status;
        status
    }

    /// Answers duel challenges, keeps the bot engaged with its duel opponent and reports a duel
    /// that ended while no duel goal was watching.
    fn duel_step(ctx: &mut Ctx<'_>) {
        let now = ctx.now_ms;
        let idle = !ctx.combat.busy()
            && ctx.state.me.hp_pct() >= DUEL_MIN_HP_PCT
            && ctx.cfg.behavior.duel > 0.0;
        match ctx.combat.duel.answer(now, idle, ctx.rng) {
            Answer::Accept(challenger) => {
                ctx.log.say(format!(
                    "accepting a duel from {}",
                    char_name(ctx, challenger)
                ));
                ctx.out.push(cs::start_duel(challenger, cs::DUEL_ACCEPT));
            }
            Answer::Decline(challenger) => {
                ctx.log.say(format!(
                    "declining a duel from {}",
                    char_name(ctx, challenger)
                ));
                ctx.out.push(cs::start_duel(challenger, cs::DUEL_DECLINE));
            }
            Answer::Wait | Answer::Drop => {}
        }
        match ctx.combat.duel.phase() {
            DuelPhase::Active { opponent, .. } => {
                ctx.combat.duel_opponent = opponent;
                if ctx.combat.duel.withdrawing() {
                    // The goal is walking away from a duel that went nowhere.
                    if ctx.combat.target == Some(opponent) {
                        ctx.combat.disengage();
                    }
                } else if ctx.combat.target != Some(opponent) {
                    ctx.combat.engage(opponent);
                }
            }
            _ => {
                let opp = std::mem::take(&mut ctx.combat.duel_opponent);
                if opp != 0 && ctx.combat.target == Some(opp) {
                    ctx.combat.disengage();
                }
            }
        }
        if !ctx.combat.duel.attended() {
            if let Some(end) = ctx.combat.duel.take_end() {
                ctx.log.say(format!("duel ended: {}", end.outcome.word()));
            }
        }
    }

    /// Learns skills and adds abilities as the level grows (out of combat only).
    fn learn_step(ctx: &mut Ctx<'_>) {
        if ctx.combat.busy() || ctx.combat.duel.in_duel() {
            return;
        }
        let c = LearnCtx {
            now_ms: ctx.now_ms,
            level: ctx.state.me.level,
            me_obj: ctx.state.me.obj,
            abilities: ctx.state.me.abilities,
            known: &ctx.state.skills,
            book: ctx.world,
            arch: &ctx.persona.archetype,
        };
        if let Some(out) = ctx.combat.learner.tick(&c) {
            ctx.log
                .debug(format!("skills: sent opcode {:#x}", out.opcode));
            ctx.out.push(out);
        }
    }

    fn step(ctx: &mut Ctx<'_>) -> CombatStatus {
        let now = ctx.now_ms;
        let travelling = ctx.combat.travelling(now);
        if ctx.combat.fleeing {
            // running from a character: no new fight until the run is over
            if pvp::fight::flee_step(ctx) {
                return CombatStatus::Idle;
            }
        }
        if ctx.combat.target.is_none() {
            let limits = if travelling {
                TRAVEL_LIMITS
            } else {
                FIGHT_LIMITS
            };
            if let Some(obj) = pick_threat(ctx, &limits) {
                ctx.log.debug(format!("defending against unit {obj}"));
                ctx.combat.engage(obj);
                ctx.combat.defending = obj;
            } else if let Some(obj) = pvp::defence::pick(ctx, travelling) {
                // (a hostile character that attacks the bot: `[pvp] defend`)
                ctx.log
                    .say(format!("pvp: defending against {}", char_name_of(ctx, obj)));
                ctx.combat.engage(obj);
                ctx.combat.defending = obj;
            } else if let Some(obj) = pick_assist(ctx, travelling) {
                // (a team member's fight: help, and share the loot)
                ctx.log
                    .debug(format!("assisting the party against unit {obj}"));
                ctx.combat.engage(obj);
                ctx.combat.assisting = obj;
            }
        }
        let Some(obj) = ctx.combat.target else {
            Self::loot_step(ctx);
            return CombatStatus::Idle;
        };
        let engaged_at = *ctx.combat.engaged_at_ms.get_or_insert(now);

        let Some((upos, dead, is_char, target_hp)) = ctx
            .state
            .units
            .get(&obj)
            .map(|u| (u.pos, u.dead(), u.kind == UnitKind::Character, u.hp))
        else {
            return Self::lose(ctx, LostReason::Gone);
        };
        if dead {
            let ours = !is_char
                && (ctx.state.kills.iter().any(|k| k.0 == obj) || ctx.combat.assisting == obj);
            ctx.combat.disengage();
            if !is_char {
                ctx.combat.kills += 1;
            }
            if ours {
                ctx.combat.loot = Some(LootJob::new(obj, now));
            }
            return CombatStatus::TargetDead { obj };
        }

        let me = ctx.state.me.pos;
        let dist = dist2d(me, upos);
        // a fight with a character outside a duel is a pvp fight (`bot/pvp`): it has limits
        let pvp_fight = is_char && !ctx.combat.duel.in_duel();
        let (lost_at, timeout) = if pvp_fight {
            (pvp::fight::LOST_DISTANCE, pvp::fight::FIGHT_TIMEOUT_MS)
        } else if is_char {
            (DUEL_LOST_DISTANCE, u64::MAX)
        } else {
            (LOST_DISTANCE, FIGHT_TIMEOUT_MS)
        };
        ctx.combat
            .watch
            .observe(now, target_hp, ctx.state.last_damage_ms);
        let w = &mut ctx.combat.watch;
        if !is_char
            && !w.immune_told
            && classify::target_immune(w.accepted, w.dealt, now.saturating_sub(w.first_accept_ms))
        {
            // The server accepted cast after cast at this monster and it never lost a hit point.
            w.immune_told = true;
            let (npc, casts) = (ctx.state.units.get(&obj).map_or(0, |u| u.id), w.accepted);
            ctx.find(Finding::TargetImmune {
                npc,
                casts,
                refused: false,
            });
        }
        let pvp_break = if pvp_fight {
            let w = &ctx.combat.watch;
            pvp::fight::judge(&pvp::fight::Facts {
                hp_pct: ctx.state.me.hp_pct(),
                retreat_pct: ctx.cfg.pvp.retreat_hp_pct,
                accepted: w.accepted,
                dealt: w.dealt,
                since_first_ms: now.saturating_sub(w.first_accept_ms),
            })
        } else {
            None
        };
        let reason = if dist > lost_at {
            Some(LostReason::Far)
        } else if pvp_break == Some(pvp::fight::Break::Protected) {
            Some(LostReason::Protected)
        } else if pvp_break == Some(pvp::fight::Break::Retreat) {
            Some(LostReason::Retreat)
        } else if now.saturating_sub(engaged_at) > timeout {
            Some(LostReason::Timeout)
        } else if travelling && ctx.combat.defending == obj && dist > TRAVEL_LOSE_M {
            Some(LostReason::Travel)
        } else if !is_char && ctx.combat.watch.no_target >= NO_TARGET_GIVE_UP {
            // The server keeps saying the unit does not exist: the bot's picture of it is stale.
            Some(LostReason::Gone)
        } else if !is_char && ctx.combat.watch.stalled(now) {
            // No hit point lost on either side for a long while: the target cannot be hurt from
            // here (an unreachable ledge, a wall, evading), whatever the casts say.
            Some(LostReason::Stalled)
        } else if !is_char && ctx.combat.watch.obstacles >= OBSTACLE_GIVE_UP {
            Some(LostReason::Blocked)
        } else {
            None
        };
        if let Some(reason) = reason {
            ctx.log.debug(format!("giving up unit {obj}: {reason:?}"));
            return Self::lose(ctx, reason);
        }
        // A cast the server refused for an obstacle: take another position around the target
        // and cast nothing until the bot is there.
        if std::mem::take(&mut ctx.combat.watch.sidestep) {
            Self::sidestep(ctx, upos, dist);
        }
        if now < ctx.combat.watch.hold_until_ms {
            if ctx.motion.is_moving() {
                ctx.motion.face(upos[0], upos[1]);
            }
            return CombatStatus::Fighting;
        }

        // What the rotation wants this tick.
        let persona = ctx.persona;
        let rotation = rotation_of(ctx);
        let mut preferred = hold_range(ctx.world, persona, rotation);
        let decision = match rotation {
            Some(rot) => Self::rotate(ctx, rot, obj, upos, dist + ctx.combat.range_pen, is_char),
            None => Decision::default(),
        };
        if let Some(r) = decision.reach {
            preferred = preferred.min(r).max(1.5);
        }
        // The server judged a cast too far: stand closer than the data says.
        let range_pen = ctx.combat.range_pen;
        if range_pen > 0.0 {
            preferred = (preferred - range_pen).max(1.5);
        }

        // Cast first: a gap closer or a buff can go while the bot is still walking.
        let mut cast_now = false;
        if let Some(c) = decision.cast {
            let instant = ctx.world.facts(c.skill).is_none_or(|f| f.cast_ms == 0);
            if ctx.motion.is_moving() && !instant {
                // A spell with a cast time needs the bot standing: stop and cast next tick.
                ctx.motion.stop();
                ctx.combat.last_goto = None;
                return CombatStatus::Fighting;
            }
            if !ctx.combat.announced {
                ctx.out.push(cs::change_target(obj));
                ctx.combat.announced = true;
            }
            Self::send_rotation_cast(ctx, c.skill, c.kind);
            cast_now = true;
        }

        let casting = now < ctx.combat.casting_until_ms;
        // A retreat in progress is not interrupted by the approach logic.
        if now < ctx.combat.kite_until_ms && ctx.motion.is_moving() {
            ctx.motion.face(upos[0], upos[1]);
            return CombatStatus::Fighting;
        }
        let want_retreat = decision.retreat
            || (is_char
                && persona.archetype.range == crate::archetype::RangeKind::Ranged
                && dist < preferred * KITE_CLOSE);
        if want_retreat
            && !casting
            && now >= ctx.combat.next_kite_ms
            && Self::retreat(ctx, upos, preferred)
        {
            return CombatStatus::Fighting;
        }

        if !casting && must_approach(dist, preferred) {
            if ctx.combat.last_goto.is_some()
                && matches!(ctx.motion.status(), MotionStatus::Failed(_))
            {
                // The way to the target is blocked: give up on it.
                ctx.motion.stop();
                return Self::lose(ctx, LostReason::Blocked);
            }
            if chase_again(ctx.combat.last_goto, upos, ctx.motion.is_moving()) {
                ctx.motion
                    .chase_to(upos[0], upos[1], arrival_radius(preferred), Pace::Run, now);
                ctx.combat.last_goto = Some((upos[0], upos[1]));
            }
            return CombatStatus::Approaching;
        }

        if ctx.motion.is_moving() {
            ctx.motion.stop();
            ctx.combat.last_goto = None;
        }
        ctx.motion.face(upos[0], upos[1]);
        if !ctx.combat.announced {
            ctx.out.push(cs::change_target(obj));
            ctx.combat.announced = true;
        }
        if !cast_now {
            // With a rotation only the auto attack is left, and only when the rotation has been
            // quiet: the server starts a global cooldown with it, which would refuse the next cast.
            let quiet = ctx
                .combat
                .last_cast
                .is_none_or(|(_, at)| now.saturating_sub(at) >= rotation::GCD_MS + AUTO_QUIET_MS);
            if rotation.is_none() || quiet {
                Self::cast_basic(ctx, obj, dist + range_pen, preferred, rotation.is_some());
            }
        }
        CombatStatus::Fighting
    }

    /// Gives the target up: notes how, and disengages.
    fn lose(ctx: &mut Ctx<'_>, reason: LostReason) -> CombatStatus {
        let dealt = ctx.combat.watch.dealt;
        let loss = Loss { reason, dealt };
        ctx.combat.loss = Some(loss);
        if let Some(obj) = ctx.combat.target {
            Self::shun(ctx, obj, loss);
            Self::pvp_aftermath(ctx, obj, reason);
        }
        ctx.combat.disengage();
        CombatStatus::Lost
    }

    /// What a pvp fight that ended without a winner leaves behind: the character is shunned for a
    /// while and a bot that is too hurt runs.
    fn pvp_aftermath(ctx: &mut Ctx<'_>, obj: u32, reason: LostReason) {
        let Some((pos, is_char)) = ctx
            .state
            .units
            .get(&obj)
            .map(|u| (u.pos, u.kind == UnitKind::Character))
        else {
            return;
        };
        if !is_char || ctx.combat.duel.in_duel() {
            return;
        }
        let now = ctx.now_ms;
        let name = char_name_of(ctx, obj);
        match reason {
            LostReason::Protected => {
                ctx.log.say(format!(
                    "pvp: {name} takes no damage here (protected); leaving it alone"
                ));
                ctx.state.pvp_mut().shun(obj, now + pvp::fight::SHUN_MS);
                ctx.state.pvp_mut().gave_up += 1;
            }
            LostReason::Retreat => {
                ctx.log.say(format!(
                    "pvp: {} percent of hit points left, retreating from {name}",
                    ctx.state.me.hp_pct()
                ));
                ctx.state.pvp_mut().shun(obj, now + pvp::fight::SHUN_MS / 3);
                ctx.state.pvp_mut().gave_up += 1;
                pvp::fight::start_flee(ctx, obj, pos);
            }
            LostReason::Timeout | LostReason::Far => {
                ctx.state.pvp_mut().shun(obj, now + pvp::fight::SHUN_MS / 3);
                ctx.state.pvp_mut().gave_up += 1;
            }
            _ => {}
        }
    }

    /// Leaves `obj` alone after the controller gave it up. A goal that picked the fight does the
    /// same on its own terms (and for the whole kind); this covers what nobody picked: a defence
    /// would take up the same unit again as soon as it targets the bot, and a unit the server
    /// does not know would be tried again by whatever goal still has it in view.
    fn shun(ctx: &mut Ctx<'_>, obj: u32, loss: Loss) {
        let now = ctx.now_ms;
        let unknown = ctx.combat.watch.no_target >= NO_TARGET_GIVE_UP;
        if loss.unreachable() {
            ctx.state
                .avoid_mut()
                .ignore_obj(obj, now + super::avoid::UNREACHABLE_MS);
        } else if unknown {
            ctx.state.avoid_mut().ignore_obj(obj, now + GONE_IGNORE_MS);
        }
        if unknown {
            ctx.state.units.remove(&obj);
        }
    }

    /// Walks to another place around the target after the server refused a cast because
    /// something blocks the line of sight: on a circle around the target, a little closer, at a
    /// wider angle with every further refusal, alternating the side. Returns whether the bot
    /// started walking.
    fn sidestep(ctx: &mut Ctx<'_>, upos: [f32; 3], dist: f32) -> bool {
        let now = ctx.now_ms;
        let me = ctx.state.me.pos;
        let n = ctx.combat.watch.obstacles.max(1);
        let bearing = (me[1] - upos[1]).atan2(me[0] - upos[0]);
        let deg = SIDESTEP_DEG.0 + SIDESTEP_DEG.1 * f32::from(n - 1);
        let first = if (u32::from(n) + ctx.persona.id) % 2 == 1 {
            1.0
        } else {
            -1.0
        };
        let radius = (dist * 0.75).clamp(2.0, 30.0);
        let tries = [
            (first, radius),
            (-first, radius),
            (0.0, (dist * 0.5).clamp(1.5, 30.0)),
        ];
        for (sign, r) in tries {
            let a = bearing + sign * deg.to_radians();
            let (x, y) = (upos[0] + a.cos() * r, upos[1] + a.sin() * r);
            if !super::goals::walkable(ctx.world, x, y) {
                continue;
            }
            ctx.motion.chase_to(x, y, 1.0, Pace::Run, now);
            let c = &mut *ctx.combat;
            c.last_goto = None;
            c.kite_until_ms = now + OBSTACLE_HOLD_MS + 1_000;
            ctx.log.debug(format!(
                "line of sight blocked (try {n}): stepping to {x:.0}, {y:.0}"
            ));
            return true;
        }
        false
    }

    /// Asks the rotation what to do against `target` at `dist` metres.
    fn rotate(
        ctx: &mut Ctx<'_>,
        rot: &Rotation,
        target: u32,
        upos: [f32; 3],
        dist: f32,
        is_char: bool,
    ) -> Decision {
        let now = ctx.now_ms;
        let world = ctx.world;
        let faction = ctx.state.me.faction;
        let me = ctx.state.me.pos;
        // Distances to the hostile units around, only when the rotation counts enemies.
        let mut enemies: SmallVec<[f32; 16]> = SmallVec::new();
        let radius = rot.max_enemy_radius();
        if radius > 0.0 {
            for u in ctx.state.units.values() {
                let d = u.dist2d(me);
                if d <= radius && enemies.len() < 16 && attackable(world, faction, u).is_some() {
                    enemies.push(d);
                }
            }
            if is_char && dist <= radius {
                enemies.push(dist);
            }
        }
        let me_state = &ctx.state.me;
        // The party members the rotation may heal: the bot itself and the members the team
        // packets placed in the same world (their hit points come from those packets).
        let mut party: SmallVec<[PartyView; 6]> = SmallVec::new();
        party.push(PartyView {
            obj: me_state.obj,
            pos: me,
            dist: 0.0,
            hp_pct: me_state.hp_pct(),
            dead: me_state.dead,
        });
        if let Some(team) = ctx.state.team().filter(|t| t.in_team()) {
            let instance = ctx.state.place.instance_id();
            for r in &team.remote {
                if r.char_id == me_state.char_id || !r.online || r.obj_id == 0 {
                    continue;
                }
                if r.instance_id != instance {
                    continue;
                }
                let (Some(remote_pct), true) = (r.hp_pct(), r.located()) else {
                    continue;
                };
                let unit = ctx.state.units.get(&r.obj_id);
                let pos = unit.map_or(r.pos, |u| u.pos);
                if !pos[0].is_finite() || !pos[1].is_finite() {
                    continue;
                }
                // (the unit packets of a member in view are fresher than the party snapshot)
                let (hp_pct, dead) = match unit {
                    Some(u) if u.dead() => (0, true),
                    Some(u) if u.hp > 0 && r.hp_max > 0 => (
                        (i64::from(u.hp) * 100 / i64::from(r.hp_max)).clamp(0, 100) as u8,
                        false,
                    ),
                    _ => (remote_pct, r.hp <= 0),
                };
                party.push(PartyView {
                    obj: r.obj_id,
                    pos,
                    dist: dist2d(me, pos),
                    hp_pct,
                    dead,
                });
            }
        }
        let c = &mut *ctx.combat;
        let casting = c
            .target_cast
            .filter(|(obj, _, _)| *obj == target)
            .is_some_and(|(_, skill, at)| {
                let cast_ms = world
                    .facts(skill)
                    .map_or(1500, |f| u64::from(f.cast_ms).max(500));
                now.saturating_sub(at) < cast_ms
            });
        let inp = Inputs {
            now_ms: now,
            book: world,
            known: &ctx.state.skills,
            abilities: c.learner.abilities().unwrap_or(me_state.abilities),
            level: me_state.level,
            cooldowns: &ctx.state.cooldowns,
            backoffs: &c.backoffs,
            me_obj: me_state.obj,
            hp: me_state.hp,
            hp_pct: me_state.hp_pct(),
            mp: me_state.mp,
            labor: me_state.labor,
            target: Some(TargetView {
                obj: target,
                pos: upos,
                dist,
                character: is_char,
                casting,
            }),
            party: &party,
            enemies: &enemies,
            gcd_until_ms: c.gcd_until_ms,
            casting_until_ms: c.casting_until_ms,
        };
        rotation::decide(rot, &mut c.rot, &inp, ctx.rng)
    }

    /// Sends a rotation cast and books its timers.
    fn send_rotation_cast(ctx: &mut Ctx<'_>, skill: u32, kind: CastKind) {
        let now = ctx.now_ms;
        let me_obj = ctx.state.me.obj;
        let enemy = ctx.combat.target;
        let (cooldown, cast_time) = ctx.world.skill(skill).map_or((0, 0), |s| {
            (u64::from(s.cooldown_time), u64::from(s.casting_time))
        });
        let c = &mut *ctx.combat;
        c.casting_until_ms = now + cast_time + 100;
        c.gcd_until_ms = now + u64::from(c.gcd_ms);
        c.last_cast = Some((skill, now));
        c.casts += 1;
        if cooldown > 0 {
            // the server starts the cooldown when the skill fires, after the cast time
            ctx.state
                .set_cooldown(skill, now + cast_time + cooldown + COOLDOWN_SLACK_MS);
        }
        match kind {
            CastKind::SelfCast => ctx.out.push(cs::start_skill_unit(skill, me_obj, me_obj)),
            CastKind::Unit(t) => ctx.out.push(cs::start_skill_unit(skill, me_obj, t)),
            CastKind::Ally(t) => {
                // The server takes the cast on the ally we target for it; the bot's own target
                // goes back to the enemy at once so the fight (and the next damage skill) keeps
                // it.
                ctx.out.push(cs::change_target(t));
                ctx.out.push(cs::start_skill_unit(skill, me_obj, t));
                if let Some(enemy) = enemy {
                    ctx.out.push(cs::change_target(enemy));
                    ctx.combat.announced = true;
                }
            }
            CastKind::Position(p) => {
                ctx.out.push(cs::start_skill_position(
                    skill,
                    me_obj,
                    (p[0], p[1], p[2]),
                    ctx.state.me.yaw,
                ));
            }
        }
        ctx.log.debug(format!(
            "cast {skill} ({}) {kind:?}",
            ctx.world.skill(skill).map_or("?", |s| &*s.name)
        ));
    }

    /// Backs away from the target to its fighting range; false when there is no place to go.
    fn retreat(ctx: &mut Ctx<'_>, upos: [f32; 3], preferred: f32) -> bool {
        let me = ctx.state.me.pos;
        let (dx, dy) = (me[0] - upos[0], me[1] - upos[1]);
        let len = (dx * dx + dy * dy).sqrt();
        let (ux, uy) = if len < 0.1 {
            let a = ctx.rng.f32() * std::f32::consts::TAU;
            (a.cos(), a.sin())
        } else {
            (dx / len, dy / len)
        };
        let (x, y) = (upos[0] + ux * preferred, upos[1] + uy * preferred);
        if !super::goals::walkable(ctx.world, x, y) {
            return false;
        }
        let now = ctx.now_ms;
        ctx.motion.chase_to(x, y, 1.0, Pace::Run, now);
        let c = &mut *ctx.combat;
        c.last_goto = None;
        c.kite_until_ms = now + KITE_MS;
        c.next_kite_ms = now + KITE_GAP_MS;
        true
    }

    /// Casts the starter skill or the auto attack if the timers and the facts allow it.
    fn cast_basic(ctx: &mut Ctx<'_>, target: u32, dist: f32, preferred: f32, auto_only: bool) {
        let now = ctx.now_ms;
        let arch = &ctx.persona.archetype;
        let auto = arch.auto_attack_skill();
        let mut cands = [
            Self::candidate(ctx, arch.starter_skill, false, dist, preferred),
            Self::candidate(ctx, auto, true, dist, preferred),
        ];
        if auto_only {
            cands[0].ready = false;
        }
        let c = &mut *ctx.combat;
        let Some(skill) = pick_skill(
            &cands,
            now,
            c.casting_until_ms,
            c.gcd_until_ms,
            c.next_auto_ms,
        ) else {
            return;
        };
        let is_auto = skill == auto;
        let (cooldown, cast_time) = ctx.world.skill(skill).map_or((0, 0), |s| {
            (u64::from(s.cooldown_time), u64::from(s.casting_time))
        });
        c.casting_until_ms = now + cast_time + 100;
        if is_auto {
            c.next_auto_ms = now + AUTO_ATTACK_GAP_MS.max(cooldown);
            c.gcd_until_ms = c.gcd_until_ms.max(now + AUTO_GCD_MS);
        } else {
            c.gcd_until_ms = now + GCD_MS;
        }
        c.last_cast = Some((skill, now));
        c.casts += 1;
        if cooldown > 0 {
            ctx.state
                .set_cooldown(skill, now + cast_time + cooldown + COOLDOWN_SLACK_MS);
        }
        let me_obj = ctx.state.me.obj;
        ctx.out.push(cs::start_skill_unit(skill, me_obj, target));
    }

    fn candidate(ctx: &Ctx<'_>, skill: u32, is_auto: bool, dist: f32, preferred: f32) -> SkillCand {
        let now = ctx.now_ms;
        let info = ctx.world.skill(skill);
        let known =
            is_auto || ctx.state.skills.is_empty() || ctx.state.skill_level(skill).is_some();
        let level = ctx.state.me.level;
        let mana_ok =
            info.is_none_or(|i| i64::from(i.mana_cost(level)) <= i64::from(ctx.state.me.mp));
        let max_range = info
            .map(|i| f32::from(i.max_range))
            .filter(|r| *r > 0.0)
            .unwrap_or(preferred + 1.0)
            .min(preferred + 1.5);
        let min_range = info.map_or(0.0, |i| f32::from(i.min_range));
        SkillCand {
            id: skill,
            is_auto,
            ready: known
                && mana_ok
                && !ctx.combat.backoffs.blocked(skill, now)
                && ctx.state.cooldown_ready(skill, now),
            in_range: dist <= max_range + RANGE_SLACK && dist >= min_range,
        }
    }

    /// Runs the loot job; true while it is still going.
    fn loot_step(ctx: &mut Ctx<'_>) -> bool {
        let Some(mut job) = ctx.combat.loot else {
            return false;
        };
        let now = ctx.now_ms;
        let view = match ctx.state.units.get(&job.owner) {
            Some(u) => (
                LootView {
                    exists: true,
                    has_loot: u.has_loot(),
                    dist: dist2d(ctx.state.me.pos, u.pos),
                    now_ms: now,
                },
                u.pos,
            ),
            None => (
                LootView {
                    exists: false,
                    has_loot: false,
                    dist: 0.0,
                    now_ms: now,
                },
                [0.0; 3],
            ),
        };
        let action = job.step(&view.0);
        match action {
            LootAction::Wait => {}
            LootAction::WalkTo => {
                if !ctx.motion.is_moving() {
                    ctx.motion
                        .chase_to(view.1[0], view.1[1], LOOT_WALK_RADIUS, Pace::Run, now);
                }
            }
            LootAction::Open => {
                ctx.motion.stop();
                ctx.out.push(cs::loot_open_bag(job.owner, true));
            }
            LootAction::Done => {
                ctx.combat.loot = None;
                return false;
            }
        }
        ctx.combat.loot = Some(job);
        true
    }
}

/// Whether a unit is a monster the bot may attack: an NPC the bot's faction is hostile to that no
/// player owns. Returns its template.
pub fn attackable<'w>(world: &'w World, own_faction: u32, unit: &Unit) -> Option<&'w NpcTemplate> {
    if unit.kind != UnitKind::Npc || unit.dead() || unit.owned() {
        return None;
    }
    let template = world.npc(unit.id)?;
    let faction = if unit.faction != 0 {
        unit.faction
    } else {
        template.faction_id
    };
    (world.relation(own_faction, faction) == Relation::Hostile).then_some(template)
}

/// The compiled rotation of the bot's build, once the bot knows skills.
fn rotation_of<'a>(ctx: &Ctx<'a>) -> Option<&'a Rotation> {
    if ctx.state.skills.is_empty() {
        return None;
    }
    let persona: &'a super::ctx::Persona = ctx.persona;
    persona.archetype.rotation().map(|r| &**r)
}

/// The distance a bot fights from: the archetype's, but never beyond the reach of the
/// rotation's anchor skill.
fn hold_range(world: &World, persona: &super::ctx::Persona, rot: Option<&Rotation>) -> f32 {
    let base = persona.archetype.preferred_range;
    let anchor = rot
        .and_then(Rotation::anchor)
        .and_then(|s| world.facts(s))
        .map(|f| rotation::reach_of(&f));
    match anchor {
        Some(r) if persona.archetype.range == crate::archetype::RangeKind::Ranged && r > 4.0 => {
            base.min(r - 1.0)
        }
        _ => base,
    }
}

/// The name of the character with id `char_id` if it is in view.
fn char_name(ctx: &Ctx<'_>, char_id: u32) -> String {
    ctx.state
        .units
        .values()
        .find(|u| u.kind == UnitKind::Character && u.id == char_id)
        .and_then(|u| u.name.as_deref())
        .map_or_else(|| format!("character {char_id}"), str::to_owned)
}

/// The name of the unit with object id `obj` if it is a character in view.
fn char_name_of(ctx: &Ctx<'_>, obj: u32) -> String {
    ctx.state
        .units
        .get(&obj)
        .and_then(|u| u.name.as_deref())
        .map_or_else(|| format!("unit {obj}"), str::to_owned)
}

/// How far the controller goes to defend the bot.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ThreatLimits {
    /// A unit that targeted the bot longer ago than this is no threat.
    window_ms: u64,
    /// Threats farther away than this are left alone.
    max_dist: f32,
    /// How far the bot looks for an attacker it cannot name (when it loses hit points).
    react_range: f32,
}

/// A bot that stands, wanders or hunts defends itself against whoever targeted it lately.
const FIGHT_LIMITS: ThreatLimits = ThreatLimits {
    window_ms: THREAT_WINDOW_MS,
    max_dist: f32::INFINITY,
    react_range: REACT_RANGE,
};

/// How far a bot in a team goes to help a team member (standing, and travelling).
const ASSIST_M: f32 = 45.0;
const TRAVEL_ASSIST_M: f32 = TRAVEL_FIGHT_M;
/// A bot a player told to stay only helps a team member this close.
const HOLD_ASSIST_M: f32 = 12.0;

/// A bot on its way somewhere only turns on units that attack it right now and are close.
const TRAVEL_LIMITS: ThreatLimits = ThreatLimits {
    window_ms: TRAVEL_THREAT_WINDOW_MS,
    max_dist: TRAVEL_FIGHT_M,
    react_range: TRAVEL_REACT_M,
};

/// The nearest monster that targeted the bot within the limits.
fn named_attacker(
    threats: &[(u32, u64)],
    units: &rustc_hash::FxHashMap<u32, Unit>,
    me: [f32; 3],
    now_ms: u64,
    limits: &ThreatLimits,
) -> Option<u32> {
    let mut best: Option<(u32, f32)> = None;
    for &(obj, at) in threats {
        if now_ms.saturating_sub(at) > limits.window_ms {
            continue;
        }
        let Some(u) = units.get(&obj) else {
            continue;
        };
        if u.dead() || u.kind != UnitKind::Npc {
            continue;
        }
        let d = u.dist2d(me);
        if d > limits.max_dist {
            continue;
        }
        if best.is_none_or(|b| d < b.1) {
            best = Some((obj, d));
        }
    }
    best.map(|b| b.0)
}

/// The monster a team member needs help with: one that attacks a member (the nearest first), else
/// one a member targets. `ok` says whether a unit may be fought (hostile, alive, not given up).
fn assist_target(
    team: &super::party::TeamLog,
    units: &rustc_hash::FxHashMap<u32, Unit>,
    me: [f32; 3],
    now_ms: u64,
    max_dist: f32,
    ok: impl Fn(u32, &Unit) -> bool,
) -> Option<u32> {
    let nearest = |objs: &mut dyn Iterator<Item = u32>| {
        objs.filter_map(|o| units.get(&o).map(|u| (o, u)))
            .filter(|(o, u)| u.dist2d(me) <= max_dist && ok(*o, u))
            .min_by(|a, b| a.1.dist2d(me).total_cmp(&b.1.dist2d(me)))
            .map(|(o, _)| o)
    };
    nearest(&mut team.attackers_at(now_ms).map(|a| a.0))
        .or_else(|| nearest(&mut team.member_targets_at(now_ms).map(|t| t.1)))
}

/// The unit to help a team member against; `None` for a bot that is in no team (solo bots
/// behave as they always did).
fn pick_assist(ctx: &Ctx<'_>, travelling: bool) -> Option<u32> {
    let team = ctx.state.team().filter(|t| t.in_team())?;
    let faction = ctx.state.me.faction;
    let (world, state, now) = (ctx.world, &*ctx.state, ctx.now_ms);
    assist_target(
        team,
        &state.units,
        state.me.pos,
        now,
        if super::companion::holding(state) {
            HOLD_ASSIST_M
        } else if travelling {
            TRAVEL_ASSIST_M
        } else {
            ASSIST_M
        },
        |obj, u| attackable(world, faction, u).is_some() && !state.avoids(obj, u.id, now),
    )
}

/// The unit to defend against: one that targets the bot, else (when the bot is losing hit
/// points) the nearest monster close by.
fn pick_threat(ctx: &Ctx<'_>, limits: &ThreatLimits) -> Option<u32> {
    let me = ctx.state.me.pos;
    let hurt = ctx.state.damaged_within(DAMAGE_WINDOW_MS);
    // Units the controller gave up on (nobody could hurt anybody) are not defended against again,
    // unless the bot is losing hit points after all.
    let named = if ctx.state.avoid.is_some() && !hurt {
        let threats: SmallVec<[(u32, u64); 8]> = ctx
            .state
            .threats
            .iter()
            .copied()
            .filter(|&(obj, _)| {
                !ctx.state
                    .units
                    .get(&obj)
                    .is_some_and(|u| ctx.state.avoids(obj, u.id, ctx.now_ms))
            })
            .collect();
        named_attacker(&threats, &ctx.state.units, me, ctx.now_ms, limits)
    } else {
        named_attacker(&ctx.state.threats, &ctx.state.units, me, ctx.now_ms, limits)
    };
    if named.is_some() {
        return named;
    }
    if !hurt {
        return None;
    }
    let mut near: Option<(u32, f32)> = None;
    for (&obj, u) in &ctx.state.units {
        let d = u.dist2d(me);
        if d > limits.react_range || attackable(ctx.world, ctx.state.me.faction, u).is_none() {
            continue;
        }
        if near.is_none_or(|b| d < b.1) {
            near = Some((obj, d));
        }
    }
    near.map(|n| n.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aa_proto::opcodes::cs as op;
    use aa_proto::sc_activity::{duel_det, ActivityEvent};
    use aa_proto::sc_world::LootItem;

    fn cand(id: u32, is_auto: bool, ready: bool, in_range: bool) -> SkillCand {
        SkillCand {
            id,
            is_auto,
            ready,
            in_range,
        }
    }

    #[test]
    fn the_starter_skill_goes_first_then_the_auto_attack() {
        let both = [cand(18132, false, true, true), cand(2, true, true, true)];
        assert_eq!(pick_skill(&both, 5000, 0, 0, 0), Some(18132));
        // global cooldown running: the auto attack fills in
        assert_eq!(pick_skill(&both, 5000, 0, 5500, 0), Some(2));
        // both timers running
        assert_eq!(pick_skill(&both, 5000, 0, 5500, 6000), None);
        // still casting
        assert_eq!(pick_skill(&both, 5000, 5100, 0, 0), None);
        // casting just ended
        assert_eq!(pick_skill(&both, 5100, 5100, 0, 0), Some(18132));
    }

    #[test]
    fn unready_or_out_of_range_skills_are_skipped() {
        let c = [cand(1, false, false, true), cand(2, true, true, false)];
        assert_eq!(pick_skill(&c, 0, 0, 0, 0), None);
        let c = [cand(1, false, false, true), cand(2, true, true, true)];
        assert_eq!(pick_skill(&c, 0, 0, 0, 0), Some(2));
        assert_eq!(pick_skill(&[], 0, 0, 0, 0), None);
    }

    #[test]
    fn refused_skills_back_off_longer_each_time() {
        let mut b = Backoffs::default();
        assert!(!b.blocked(7, 0));
        b.fail(7, 1000);
        assert!(b.blocked(7, 2999) && !b.blocked(7, 3000));
        b.fail(7, 3000);
        assert!(b.blocked(7, 10_999) && !b.blocked(7, 11_000));
        b.fail(7, 11_000);
        b.fail(7, 41_000);
        assert!(
            b.blocked(7, 70_000) && !b.blocked(7, 71_000),
            "capped at the last step"
        );
        b.succeed(7);
        assert!(!b.blocked(7, 0));
        // a short pause does not escalate
        b.fail_short(8, 0);
        assert!(b.blocked(8, 1499) && !b.blocked(8, 1500));
        b.fail_short(8, 2000);
        assert!(!b.blocked(8, 3500));
        // bounded
        for s in 0..20 {
            b.fail(100 + s, 0);
        }
        assert!(b.list.len() <= 4);
    }

    fn npc(pos: [f32; 3]) -> Unit {
        Unit {
            kind: UnitKind::Npc,
            id: 100,
            pos,
            hp: 100,
            faction: 0,
            level: 5,
            flags: 0,
            name: None,
        }
    }

    #[test]
    fn a_travelling_bot_only_turns_on_close_and_current_attackers() {
        let mut units = rustc_hash::FxHashMap::default();
        units.insert(1, npc([8.0, 0.0, 0.0])); // close
        units.insert(2, npc([40.0, 0.0, 0.0])); // far
        let now = 20_000;
        fn pick(
            units: &rustc_hash::FxHashMap<u32, Unit>,
            threats: &[(u32, u64)],
            l: &ThreatLimits,
        ) -> Option<u32> {
            named_attacker(threats, units, [0.0; 3], 20_000, l)
        }
        // standing: everything that targeted the bot within ten seconds, however far
        assert_eq!(pick(&units, &[(2, now - 9_000)], &FIGHT_LIMITS), Some(2));
        assert_eq!(pick(&units, &[(1, now - 9_000)], &FIGHT_LIMITS), Some(1));
        assert_eq!(pick(&units, &[(1, now - 11_000)], &FIGHT_LIMITS), None);
        // travelling: close ones that attack right now
        assert_eq!(pick(&units, &[(1, now - 1_000)], &TRAVEL_LIMITS), Some(1));
        assert_eq!(
            pick(&units, &[(1, now - 5_000)], &TRAVEL_LIMITS),
            None,
            "stale"
        );
        assert_eq!(
            pick(&units, &[(2, now - 1_000)], &TRAVEL_LIMITS),
            None,
            "too far"
        );
        // the closer of two attackers
        assert_eq!(
            pick(&units, &[(2, now - 500), (1, now - 500)], &FIGHT_LIMITS),
            Some(1)
        );
        // a dead unit and a player are no monster threat
        units.get_mut(&1).unwrap().flags = crate::bot::state::U_DEAD;
        assert_eq!(pick(&units, &[(1, now - 100)], &TRAVEL_LIMITS), None);
        units.get_mut(&1).unwrap().flags = 0;
        units.get_mut(&1).unwrap().kind = UnitKind::Character;
        assert_eq!(pick(&units, &[(1, now - 100)], &TRAVEL_LIMITS), None);
    }

    fn team_with(members: &[(u32, u32)]) -> crate::bot::party::TeamLog {
        // members are (char id, object id)
        let mut t = crate::bot::party::TeamLog {
            team_id: 9,
            owner: members[0].0,
            ..Default::default()
        };
        for &(c, o) in members {
            t.members.push(crate::bot::party::Member {
                char_id: c,
                level: 20,
                obj_id: o,
            });
        }
        t
    }

    fn character(char_id: u32, pos: [f32; 3]) -> Unit {
        Unit {
            kind: UnitKind::Character,
            id: char_id,
            ..npc(pos)
        }
    }

    #[test]
    fn a_team_member_in_a_fight_gets_help_and_a_bot_without_a_team_none() {
        // me = object 1 (char 10); a friend = object 2 (char 20); three monsters
        let mut units = rustc_hash::FxHashMap::default();
        units.insert(2, character(20, [3.0, 0.0, 0.0]));
        units.insert(50, npc([12.0, 0.0, 0.0])); // fights the friend
        units.insert(51, npc([30.0, 0.0, 0.0])); // the friend targets it
        units.insert(52, npc([100.0, 0.0, 0.0])); // far away
        let mut team = team_with(&[(10, 1), (20, 2)]);
        let now = 50_000;
        let ok = |_: u32, _: &Unit| true;
        let pick = |team: &crate::bot::party::TeamLog, max: f32| {
            assist_target(team, &units, [0.0; 3], now, max, ok)
        };
        // nothing is going on
        assert_eq!(pick(&team, ASSIST_M), None);
        // the friend targets monster 51
        team.on_target(2, 51, 1, &units, now - 1_000);
        assert_eq!(pick(&team, ASSIST_M), Some(51));
        // a monster that attacks the friend comes first, even though the friend targets another
        team.on_target(50, 2, 1, &units, now - 500);
        assert_eq!(pick(&team, ASSIST_M), Some(50));
        // travelling: only what is close
        assert_eq!(pick(&team, TRAVEL_ASSIST_M), Some(50));
        team.on_target(50, 0, 1, &units, now - 400); // it stopped attacking
        assert_eq!(pick(&team, TRAVEL_ASSIST_M), None, "51 is 30 m away");
        // old news is forgotten
        assert_eq!(
            assist_target(&team, &units, [0.0; 3], now + 2 * 12_000, ASSIST_M, ok),
            None
        );
        // a monster far away is no concern, and a unit the bot may not fight is skipped
        team.on_target(2, 52, 1, &units, now - 100);
        assert_eq!(pick(&team, ASSIST_M), None);
        let not_51 = |o: u32, _: &Unit| o != 51;
        team.on_target(2, 51, 1, &units, now - 100);
        assert_eq!(
            assist_target(&team, &units, [0.0; 3], now, ASSIST_M, not_51),
            None
        );
        // the bot itself is never the one it helps
        team.on_target(1, 51, 1, &units, now);
        assert_eq!(
            team.member_targets_at(now).filter(|t| t.0 == 1).count(),
            0,
            "the controller ignores the bot's own target"
        );
    }

    #[test]
    fn a_bot_that_left_its_team_helps_nobody() {
        let mut units = rustc_hash::FxHashMap::default();
        units.insert(2, character(20, [3.0, 0.0, 0.0]));
        units.insert(51, npc([30.0, 0.0, 0.0]));
        let mut team = team_with(&[(10, 1), (20, 2)]);
        team.on_target(2, 51, 1, &units, 1_000);
        assert_eq!(
            assist_target(&team, &units, [0.0; 3], 2_000, ASSIST_M, |_, _| true),
            Some(51)
        );
        team.apply(
            &aa_proto::sc_activity::ActivityEvent::LeftTeam {
                team_id: 9,
                kicked: false,
                dismissed: false,
            },
            3_000,
        );
        assert!(!team.in_team());
        assert_eq!(
            assist_target(&team, &units, [0.0; 3], 3_100, ASSIST_M, |_, _| true),
            None
        );
    }

    // the travelling limits are the narrower ones, and a defence is not dropped the moment it
    // starts
    const _: () = {
        assert!(TRAVEL_THREAT_WINDOW_MS < THREAT_WINDOW_MS);
        assert!(TRAVEL_REACT_M < REACT_RANGE);
        assert!(TRAVEL_FIGHT_M < TRAVEL_LOSE_M);
    };

    #[test]
    fn a_goal_note_makes_the_bot_travel_for_a_moment() {
        let mut c = Combat::new();
        assert!(!c.travelling(1_000));
        c.note_travelling(1_000);
        assert!(c.travelling(1_000 + TRAVEL_NOTE_MS - 1));
        assert!(
            !c.travelling(1_000 + TRAVEL_NOTE_MS),
            "it lapses on its own"
        );
        // a defence is tied to the target the controller picked itself
        c.defending = 7;
        c.engage(8);
        assert_eq!(c.defending, 0, "a goal's own target is no defence");
        c.defending = 8;
        c.disengage();
        assert_eq!(c.defending, 0);
    }

    #[test]
    fn range_helpers() {
        assert!(!must_approach(4.0, 3.0));
        assert!(must_approach(4.1, 3.0));
        assert!(!must_approach(19.0, 18.0));
        assert!(must_approach(19.5, 18.0));
        assert_eq!(arrival_radius(3.0), 2.0);
        assert_eq!(arrival_radius(18.0), 17.0);
        assert_eq!(arrival_radius(1.5), 1.0);
    }

    fn view(exists: bool, has_loot: bool, dist: f32, now_ms: u64) -> LootView {
        LootView {
            exists,
            has_loot,
            dist,
            now_ms,
        }
    }

    #[test]
    fn a_loot_job_waits_walks_opens_and_finishes() {
        let mut j = LootJob::new(9, 1000);
        assert_eq!(j.step(&view(true, false, 2.0, 1500)), LootAction::Wait);
        // the notice arrives while the corpse is 12 m away
        assert_eq!(j.step(&view(true, true, 12.0, 2000)), LootAction::WalkTo);
        assert_eq!(j.stage, LootStage::Walk);
        assert_eq!(j.step(&view(true, true, 6.0, 2500)), LootAction::WalkTo);
        // 4 m from the corpse is too far to open: the server may see the bot farther back
        assert_eq!(j.step(&view(true, true, 4.0, 2800)), LootAction::WalkTo);
        assert_eq!(j.step(&view(true, true, 3.5, 3000)), LootAction::Open);
        assert_eq!(j.stage, LootStage::Opened);
        assert_eq!(j.step(&view(true, true, 2.5, 3500)), LootAction::Wait);
        // everything taken: the server clears the flag
        assert_eq!(j.step(&view(true, false, 2.5, 3600)), LootAction::Done);
    }

    #[test]
    fn a_loot_job_gives_up_when_nothing_happens() {
        let mut j = LootJob::new(9, 0);
        assert_eq!(
            j.step(&view(true, false, 1.0, 3001)),
            LootAction::Done,
            "no notice"
        );
        let mut j = LootJob::new(9, 0);
        assert_eq!(
            j.step(&view(false, true, 1.0, 10)),
            LootAction::Done,
            "corpse gone"
        );
        let mut j = LootJob::new(9, 0);
        j.step(&view(true, true, 1.0, 100));
        assert_eq!(j.stage, LootStage::Opened);
        assert_eq!(
            j.step(&view(true, true, 1.0, 4101)),
            LootAction::Done,
            "window timeout"
        );
        // looting right next to the corpse opens at once
        let mut j = LootJob::new(9, 0);
        assert_eq!(j.step(&view(true, true, 1.0, 0)), LootAction::Open);
    }

    #[test]
    fn failed_skill_starts_pause_that_skill_only_for_the_bot() {
        let mut c = Combat::new();
        let mut out = Vec::new();
        let started = |caster: u32, result: u8| {
            ServerEvent::World(WorldEvent::SkillStarted {
                caster_obj: caster,
                skill: 18132,
                tl: 1,
                target_obj: 5,
                result,
            })
        };
        c.on_event(&started(0x500, 3), 0x400, 1000, &mut out);
        assert!(!c.backoffs.blocked(18132, 1000), "someone else's skill");
        c.on_event(&started(0x400, 3), 0x400, 1000, &mut out);
        assert!(c.backoffs.blocked(18132, 1500));
        c.on_event(&started(0x400, 0), 0x400, 1600, &mut out);
        assert!(!c.backoffs.blocked(18132, 1600));
        assert!(out.is_empty());
    }

    #[test]
    fn cooldown_refusals_widen_the_global_cooldown_estimate_and_accepted_casts_narrow_it() {
        let mut c = Combat::new();
        let mut out = Vec::new();
        let started = |result: u8| {
            ServerEvent::World(WorldEvent::SkillStarted {
                caster_obj: 0x400,
                skill: 18132,
                tl: 1,
                target_obj: 5,
                result,
            })
        };
        let base = c.gcd_ms;
        c.on_event(&started(COOLDOWN_REFUSED), 0x400, 1000, &mut out);
        assert_eq!(c.gcd_ms, base + GCD_STEP_MS);
        assert!(c.gcd_until_ms >= 1000 + u64::from(GCD_STEP_MS));
        assert!(
            !c.backoffs.blocked(18132, 1000 + SHORT_BACKOFF_MS + 1),
            "only a short pause, no escalation"
        );
        for _ in 0..20 {
            c.on_event(&started(COOLDOWN_REFUSED), 0x400, 1000, &mut out);
        }
        assert_eq!(c.gcd_ms, GCD_MAX_MS, "bounded");
        for _ in 0..(8 * GCD_RELAX_AFTER) {
            c.on_event(&started(0), 0x400, 2000, &mut out);
        }
        assert_eq!(c.gcd_ms, GCD_MIN_MS, "relaxes back to the minimum");
        // another refusal (not a cooldown) leaves the estimate alone
        c.on_event(&started(0x13), 0x400, 3000, &mut out);
        assert_eq!(c.gcd_ms, GCD_MIN_MS);
    }

    #[test]
    fn too_far_refusals_make_the_bot_stand_closer_and_accepted_casts_undo_it() {
        let mut c = Combat::new();
        let mut out = Vec::new();
        let started = |result: u8| {
            ServerEvent::World(WorldEvent::SkillStarted {
                caster_obj: 0x400,
                skill: 10752,
                tl: 1,
                target_obj: 5,
                result,
            })
        };
        assert_eq!(c.range_pen, 0.0);
        for _ in 0..10 {
            c.on_event(&started(TOO_FAR_REFUSED), 0x400, 1000, &mut out);
        }
        assert_eq!(c.range_pen, RANGE_PEN_MAX_M, "bounded");
        for _ in 0..40 {
            c.on_event(&started(0), 0x400, 2000, &mut out);
        }
        assert_eq!(c.range_pen, 0.0);
    }

    #[test]
    fn the_end_of_a_duel_keeps_the_controller_busy_until_the_goal_takes_it() {
        let mut c = Combat::new();
        let mut out = Vec::new();
        assert!(!c.busy());
        c.duel.set_attended(true);
        let ended = ServerEvent::Activity(ActivityEvent::DuelEnded {
            winner_id: 1,
            loser_id: 2,
            winner_obj: 0x400,
            loser_obj: 0x500,
            det: duel_det::WIN,
        });
        c.on_event(&ended, 0x400, 1000, &mut out);
        assert!(c.busy(), "the goal has not taken the end yet");
        assert!(c.duel.take_end().is_some());
        assert!(!c.busy());
        // without a duel goal nobody is waiting for the end
        c.duel.set_attended(false);
        c.on_event(&ended, 0x400, 2000, &mut out);
        assert!(!c.busy());
    }

    #[test]
    fn an_error_right_after_a_cast_is_blamed_on_the_cast() {
        let mut c = Combat::new();
        let mut out = Vec::new();
        let err = ServerEvent::World(WorldEvent::ErrorMsg {
            err: 1,
            err2: 1,
            kind: 0,
            notify: false,
        });
        c.on_event(&err, 0x400, 1000, &mut out);
        c.last_cast = Some((18132, 5000));
        c.on_event(&err, 0x400, 5700, &mut out); // too late to be the cast's
        assert!(!c.backoffs.blocked(18132, 5700));
        c.on_event(&err, 0x400, 5500, &mut out);
        assert!(c.backoffs.blocked(18132, 5600));
    }

    #[test]
    fn the_loot_window_is_emptied_and_closed() {
        let mut c = Combat::new();
        let mut out = Vec::new();
        let bag = |owner: u32| {
            ServerEvent::World(WorldEvent::LootBag {
                owner_obj: owner,
                owner_type: 1,
                items: [
                    LootItem {
                        index: 3,
                        template: 500,
                        count: 1,
                    },
                    LootItem {
                        index: 4,
                        template: 501,
                        count: 2,
                    },
                ]
                .into_iter()
                .collect(),
                loot_all: true,
            })
        };
        // no loot job: ignore
        c.on_event(&bag(9), 0x400, 0, &mut out);
        assert!(out.is_empty());
        c.loot = Some(LootJob::new(9, 0));
        c.on_event(&bag(8), 0x400, 0, &mut out);
        assert!(out.is_empty(), "another corpse");
        c.on_event(&bag(9), 0x400, 0, &mut out);
        let ops: Vec<u16> = out.iter().map(|o| o.opcode).collect();
        assert_eq!(
            ops,
            [
                op::CSLootItemPacket,
                op::CSLootItemPacket,
                op::CSLootCloseBagPacket
            ]
        );
        // first item: index 3, owner type 1, owner 9
        assert_eq!(out[0].body[..4], [3, 0, 1, 0]);
    }

    #[test]
    fn the_watch_sees_progress_on_either_side_and_stalls_without_it() {
        let mut w = Watch::default();
        assert!(!w.stalled(1_000_000), "nothing observed yet");
        w.observe(1_000, 100, 0);
        assert!(!w.dealt);
        assert!(!w.stalled(1_000 + NO_DAMAGE_MS));
        assert!(w.stalled(1_000 + NO_DAMAGE_MS + 1));
        // the target loses hit points: progress, and the bot can hurt it at all
        w.observe(20_000, 90, 0);
        assert!(w.dealt);
        assert!(!w.stalled(20_000 + NO_DAMAGE_MS));
        // healing is not progress
        w.observe(30_000, 100, 0);
        assert!(w.stalled(20_000 + NO_DAMAGE_MS + 1));
        // the bot is hurt: progress too (the monster does fight back)
        w.observe(45_000, 100, 44_000);
        assert!(!w.stalled(45_000 + NO_DAMAGE_MS));
        assert!(w.stalled(45_000 + NO_DAMAGE_MS + 1));
        // damage the bot took before the fight is no progress of the fight
        let mut w = Watch::default();
        w.observe(5_000, 100, 4_000);
        w.observe(6_000, 100, 4_000);
        assert!(w.stalled(5_000 + NO_DAMAGE_MS + 1));
        assert!(!w.dealt);
    }

    #[test]
    fn a_target_that_was_never_hurt_is_called_unreachable_after_a_timeout() {
        let loss = |reason, dealt| Loss { reason, dealt };
        for dealt in [false, true] {
            assert!(loss(LostReason::Stalled, dealt).unreachable());
            assert!(loss(LostReason::Blocked, dealt).unreachable());
            assert!(!loss(LostReason::Gone, dealt).unreachable());
            assert!(!loss(LostReason::Far, dealt).unreachable());
            assert!(!loss(LostReason::Travel, dealt).unreachable());
        }
        assert!(loss(LostReason::Timeout, false).unreachable());
        assert!(
            !loss(LostReason::Timeout, true).unreachable(),
            "a long fight that did hurt the target is just a hard monster"
        );
        // the give-up comes well before the fight timeout
        const _: () = assert!(NO_DAMAGE_MS * 2 < FIGHT_TIMEOUT_MS);
    }

    #[test]
    fn a_line_of_sight_refusal_repositions_instead_of_counting_as_a_skill_failure() {
        let mut c = Combat::new();
        let mut out = Vec::new();
        let started = |caster: u32, result: u8| {
            ServerEvent::World(WorldEvent::SkillStarted {
                caster_obj: caster,
                skill: 18132,
                tl: 1,
                target_obj: 5,
                result,
            })
        };
        let gcd = c.gcd_ms;
        // someone else's refusal is none of the bot's business
        c.on_event(&started(0x500, OBSTACLE_REFUSED), 0x400, 900, &mut out);
        assert_eq!(c.casts_obstructed(), 0);
        assert!(!c.watch.sidestep);

        c.on_event(&started(0x400, OBSTACLE_REFUSED), 0x400, 1000, &mut out);
        assert_eq!(c.casts_obstructed(), 1);
        assert_eq!(c.casts_refused(), 0, "not a failure of the skill");
        assert!(c.watch.sidestep, "the controller takes another position");
        assert_eq!(c.watch.hold_until_ms, 1000 + OBSTACLE_HOLD_MS);
        assert_eq!(c.gcd_ms, gcd, "no cooldown lesson either");
        assert_eq!(c.range_pen, 0.0);
        assert!(c.backoffs.blocked(18132, 1001));
        assert!(!c.backoffs.blocked(18132, 1000 + SHORT_BACKOFF_MS));
        // repeated refusals never escalate the back-off
        for i in 1..OBSTACLE_GIVE_UP {
            c.on_event(
                &started(0x400, OBSTACLE_REFUSED),
                0x400,
                1000 + u64::from(i) * 5_000,
                &mut out,
            );
        }
        assert_eq!(c.watch.obstacles, OBSTACLE_GIVE_UP);
        let last = 1000 + u64::from(OBSTACLE_GIVE_UP - 1) * 5_000;
        assert!(!c.backoffs.blocked(18132, last + SHORT_BACKOFF_MS));
        assert_eq!(c.casts_refused(), 0);
        // a cast that goes through ends the streak
        c.on_event(&started(0x400, 0), 0x400, last + 2_000, &mut out);
        assert_eq!(c.watch.obstacles, 0);
        // a real refusal still counts
        c.on_event(&started(0x400, 3), 0x400, last + 3_000, &mut out);
        assert_eq!(c.casts_refused(), 1);
        // the totals survive a reset, the per-fight streak does not
        c.on_event(
            &started(0x400, OBSTACLE_REFUSED),
            0x400,
            last + 4_000,
            &mut out,
        );
        c.reset();
        assert_eq!(c.casts_obstructed(), u32::from(OBSTACLE_GIVE_UP) + 1);
        assert_eq!(c.watch.obstacles, 0);
        assert!(!c.watch.sidestep);
    }

    #[test]
    fn refusals_for_a_missing_target_are_counted_against_the_current_target() {
        let mut c = Combat::new();
        let mut out = Vec::new();
        let started = |target: u32, result: u8| {
            ServerEvent::World(WorldEvent::SkillStarted {
                caster_obj: 0x400,
                skill: 18132,
                tl: 1,
                target_obj: target,
                result,
            })
        };
        // no target engaged: nothing to count
        c.on_event(&started(5, NO_TARGET_REFUSED), 0x400, 1000, &mut out);
        assert_eq!(c.watch.no_target, 0);
        c.engage(5);
        c.on_event(&started(6, NO_TARGET_REFUSED), 0x400, 1100, &mut out);
        assert_eq!(c.watch.no_target, 0, "another unit");
        for i in 1..=NO_TARGET_GIVE_UP {
            c.on_event(&started(5, NO_TARGET_REFUSED), 0x400, 2000, &mut out);
            assert_eq!(c.watch.no_target, i);
        }
        // these are failures of the skill like any other
        assert_eq!(c.casts_refused(), u32::from(NO_TARGET_GIVE_UP) + 2);
        // a cast that goes through shows that the target exists
        c.on_event(&started(5, 0), 0x400, 3000, &mut out);
        assert_eq!(c.watch.no_target, 0);
        // a new fight starts clean
        c.on_event(&started(5, NO_TARGET_REFUSED), 0x400, 3100, &mut out);
        c.engage(7);
        assert_eq!(c.watch.no_target, 0);
    }

    #[test]
    fn restarting_a_fight_keeps_the_target_and_forgets_the_lessons() {
        let mut c = Combat::new();
        c.restart_fight();
        assert!(c.target().is_none(), "nothing to restart");
        c.engage(5);
        c.watch.dealt = true;
        c.watch.obstacles = 3;
        c.watch.sidestep = true;
        c.range_pen = 3.0;
        c.backoffs.fail(7, 0);
        c.kite_until_ms = 99;
        c.next_kite_ms = 99;
        c.announced = true;
        c.restart_fight();
        assert_eq!(c.target(), Some(5));
        assert!(!c.dealt_damage());
        assert_eq!((c.watch.obstacles, c.watch.sidestep), (0, false));
        assert_eq!(c.range_pen, 0.0);
        assert!(!c.backoffs.blocked(7, 1));
        assert_eq!((c.kite_until_ms, c.next_kite_ms), (0, 0));
        assert!(!c.announced, "the target is announced afresh");
    }

    #[test]
    fn engage_and_disengage_and_reset() {
        let mut c = Combat::new();
        assert!(!c.busy());
        c.engage(5);
        assert!(c.busy() && c.target() == Some(5));
        c.announced = true;
        c.engage(5);
        assert!(c.announced, "re-engaging the same target keeps its state");
        c.engage(6);
        assert!(!c.announced);
        c.disengage();
        assert!(!c.busy());
        c.loot = Some(LootJob::new(1, 0));
        assert!(c.busy(), "a loot job keeps the controller busy");
        c.kills = 3;
        c.reset();
        assert!(!c.busy());
        assert_eq!(c.kills(), 3);
    }
}
