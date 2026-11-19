//! The gear goal's state machine: carries out one [`Job`] of the planner. A pure function of its
//! state, an [`Input`] (what the bot knows this tick) and the bot's [`GearMem`], returning [`Act`]s,
//! so a whole run is tested against a scripted server without a network or a
//! [`Ctx`](crate::bot::ctx::Ctx).
//!
//! The phases:
//!
//! * `Shop` (a purchase): walk to the merchant's spawn, find it in view, face it, `CSBuyItems`
//!   one piece at the merchant's grade, wait for it in the bag (at most [`MAX_BUY_TRIES`]);
//! * `Mint` (a minted piece, gem or scroll): one GM line, wait for the item in the bag;
//! * `Equip`: put the new piece on (`CSSwapItems`), wait until it is worn; a refusal is tried once
//!   more, then the piece is left for [`REFUSE_MS`](super::REFUSE_MS). A piece that is not worth
//!   it after all (the operator did not ask for it) stays in the bag;
//! * `Cast` (a gem or a regrade): cast the gem's or scroll's use skill with the item as the caster
//!   on the worn piece (`CSStartSkill` with an item caster and an item target) and wait for the
//!   result (`SCItemSocketingLunagemResult`, `SCGradeEnchantResult`).

use crate::bot::motion::{MotionStatus, MoveFailure};
use crate::bot::state::{dist2d, StateError};

use super::super::goals::{FailReason, GoalStatus};
use super::eval::Move;
use super::plan::Job;
use super::{refused_since, GearMem, EQUIP_WAIT_MS};

/// A run ends after this long.
pub const RUN_LIMIT_MS: u64 = 10 * 60_000;
/// Wait for a purchase to show up in the bag.
pub const BUY_WAIT_MS: u64 = 4_000;
/// Purchases tried before the merchant is given up.
pub const MAX_BUY_TRIES: u8 = 3;
/// Wait for a minted item to show up in the bag.
pub const MINT_WAIT_MS: u64 = 10_000;
/// Equips tried before the piece is given up.
pub const MAX_EQUIP_TRIES: u8 = 2;
/// Wait for a socketing or regrade result, on top of the cast time.
pub const CAST_EXTRA_MS: u64 = 6_000;
/// The merchant must be within 3 m (the server's `SHOP_RANGE`); the bot stops this close.
const SHOP_STOP_M: f32 = 2.0;
/// The bot buys when it is at most this far from the merchant.
const SHOP_REACH_M: f32 = 2.6;
/// How long a merchant may be out of view once the bot stands at its spawn.
const VENDOR_VIEW_MS: u64 = 10_000;
/// Gap between two walk commands towards the same spot.
const GO_GAP_MS: u64 = 1_000;
/// A trip the motion controller cuts off at its time limit is continued, at most this many legs.
const MAX_LEGS: u8 = 6;
/// A leg must have brought the bot at least this much closer to count as progress.
const LEG_MIN_GAIN_M: f32 = 30.0;

/// What the machine asks the glue to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Act {
    /// A line for the bot's log.
    Say(String),
    /// A GM command line.
    Gm(String),
    /// Walk to (x, y), stopping `radius` metres short.
    Travel {
        /// X.
        x: f32,
        /// Y.
        y: f32,
        /// Stop this far short.
        radius: f32,
    },
    /// Turn towards (x, y).
    Face {
        /// X.
        x: f32,
        /// Y.
        y: f32,
    },
    /// Stop walking.
    StopMotion,
    /// `CSBuyItems` of one `item` at `grade` from the merchant `npc` (object id).
    Buy {
        /// The merchant's object id.
        npc: u32,
        /// The item template.
        item: u32,
        /// The grade the merchant sells it at.
        grade: u8,
    },
    /// `CSSwapItems`: put a bag piece on.
    Equip(Move),
    /// `CSStartSkill` of `skill` with the bag item `item_id` (of `item_template`) as the caster
    /// on the worn piece `target`.
    Cast {
        /// The skill.
        skill: u32,
        /// The gem or scroll.
        item_id: u64,
        /// Its template.
        item_template: u32,
        /// The worn piece.
        target: u64,
    },
    /// Something for the bot's journal.
    Journal(Entry),
}

/// What the journal is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    /// A piece was bought for `price` copper.
    Bought {
        /// The template.
        template: u32,
        /// The price.
        price: u32,
        /// The purchase also reduced the purse by the quoted amount.
        confirmed_debit: bool,
    },
    /// A piece was minted.
    Minted {
        /// The template.
        template: u32,
    },
    /// A piece was put on.
    Equipped {
        /// The template.
        template: u32,
        /// The equipment slot.
        slot: u8,
    },
    /// A gem went in (`ok`) or the socketing failed and wiped the piece's `held` gems.
    Socket {
        /// The piece's template.
        piece: u32,
        /// The gem.
        gem: u32,
        /// Gems the piece held before.
        held: u8,
        /// It worked.
        ok: bool,
    },
    /// A regrade answered `result` (`grade_enchant_result`), grade `before` to `after`.
    Regrade {
        /// The piece's template.
        piece: u32,
        /// The result byte.
        result: u8,
        /// The grade before.
        before: u8,
        /// The grade after.
        after: u8,
    },
}

/// What the bot knows this tick.
pub struct Input<'a> {
    /// The bot clock in milliseconds.
    pub now_ms: u64,
    /// The bot's position.
    pub pos: [f32; 3],
    /// What the movement controller is doing.
    pub motion: MotionStatus,
    /// The bot's money in copper.
    pub money: i64,
    /// Recent refusals, newest last.
    pub errors: &'a [(u64, StateError)],
    /// The instance id of a bag item of a template.
    pub bag_item: &'a dyn Fn(u32) -> Option<u64>,
    /// Where worn piece `id` is: `(equipment slot, template)`.
    pub worn: &'a dyn Fn(u64) -> Option<(u8, u32)>,
    /// Where the bag piece `id` goes (`forced`: also when it is worse).
    pub place: &'a dyn Fn(u64, bool) -> Option<Move>,
    /// The merchant NPC of a template that is in view: `(object id, position)`.
    pub merchant: &'a dyn Fn(u32) -> Option<(u32, [f32; 3])>,
    /// The skill an item casts when used (0: none).
    pub use_skill: &'a dyn Fn(u32) -> u32,
    /// The cast time of a skill in milliseconds.
    pub cast_ms: &'a dyn Fn(u32) -> u64,
}

/// The phase of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Walk to the merchant and buy.
    Shop,
    /// Mint with a GM command.
    Mint,
    /// Put the new piece on.
    Equip,
    /// Cast the gem or scroll on the worn piece.
    Cast,
}

/// Walking to a point in legs.
#[derive(Debug, Clone, Copy, Default)]
struct Walker {
    spot: Option<(f32, f32)>,
    legs: u8,
    leg_from: f32,
    go_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Walk {
    Arrived,
    Moving,
    Failed,
}

impl Walker {
    /// Walks to (x, y), stopping `radius` metres short; a long way is walked in legs as long as
    /// every leg gets closer.
    fn walk(&mut self, io: &Input<'_>, acts: &mut Vec<Act>, x: f32, y: f32, radius: f32) -> Walk {
        let here = dist2d(io.pos, [x, y, 0.0]);
        if here <= radius + 0.6 {
            acts.push(Act::StopMotion);
            self.spot = None;
            return Walk::Arrived;
        }
        let same = self
            .spot
            .is_some_and(|s| dist2d([s.0, s.1, 0.0], [x, y, 0.0]) < 2.0);
        match io.motion {
            MotionStatus::Moving if same => return Walk::Moving,
            MotionStatus::Failed(MoveFailure::Timeout)
                if same && self.legs < MAX_LEGS && self.leg_from - here >= LEG_MIN_GAIN_M =>
            {
                self.legs += 1;
                self.leg_from = here;
                self.go_ms = 0;
            }
            MotionStatus::Failed(_) if same => {
                self.spot = None;
                return Walk::Failed;
            }
            _ => {}
        }
        if same && io.now_ms.saturating_sub(self.go_ms) < GO_GAP_MS {
            return Walk::Moving;
        }
        if !same {
            self.legs = 0;
            self.leg_from = here;
        }
        acts.push(Act::Travel { x, y, radius });
        self.spot = Some((x, y));
        self.go_ms = io.now_ms;
        Walk::Moving
    }
}

/// One run of the gear goal. See the module docs.
#[derive(Debug, Clone)]
pub struct Machine {
    job: Job,
    phase: Phase,
    started_ms: u64,
    walker: Walker,
    tries: u8,
    sent_ms: u64,
    before: Option<u64>,
    purchase_money: Option<i64>,
    got: u64,
    vendor_seen_ms: u64,
    skill: u32,
    note: &'static str,
}

impl Machine {
    /// A run of `job` starting at `now_ms`.
    pub fn new(job: Job, now_ms: u64) -> Machine {
        let phase = match job {
            Job::Equip { .. } => Phase::Equip,
            Job::Buy { .. } => Phase::Shop,
            Job::Mint { .. } => Phase::Mint,
            Job::Socket { mint, .. } | Job::Regrade { mint, .. } => {
                if mint {
                    Phase::Mint
                } else {
                    Phase::Cast
                }
            }
        };
        Machine {
            job,
            phase,
            started_ms: now_ms,
            walker: Walker::default(),
            tries: 0,
            sent_ms: 0,
            before: None,
            purchase_money: None,
            got: 0,
            vendor_seen_ms: 0,
            skill: 0,
            note: "starting",
        }
    }

    /// The job.
    pub fn job(&self) -> &Job {
        &self.job
    }

    /// The phase.
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// What the run is doing, for the bot's description.
    pub fn note(&self) -> &'static str {
        self.note
    }

    /// The run was interrupted.
    pub fn stop(&mut self, acts: &mut Vec<Act>) {
        acts.push(Act::StopMotion);
    }

    fn enter(&mut self, phase: Phase) {
        self.phase = phase;
        self.tries = 0;
        self.sent_ms = 0;
    }

    /// Whether this run puts on what it gets even when it is worse.
    fn forced(&self) -> bool {
        matches!(
            self.job,
            Job::Equip { forced: true, .. }
                | Job::Buy { forced: true, .. }
                | Job::Mint { forced: true, .. }
        )
    }

    /// One tick.
    pub fn step(&mut self, io: &Input<'_>, mem: &mut GearMem, acts: &mut Vec<Act>) -> GoalStatus {
        if io.now_ms.saturating_sub(self.started_ms) > RUN_LIMIT_MS {
            acts.push(Act::StopMotion);
            return GoalStatus::Failed(FailReason::Timeout);
        }
        match self.phase {
            Phase::Shop => self.shop(io, mem, acts),
            Phase::Mint => self.mint(io, mem, acts),
            Phase::Equip => self.equip(io, mem, acts),
            Phase::Cast => self.cast(io, mem, acts),
        }
    }

    /// A new item of `template` in the bag (not the one that was there before).
    fn arrived(&self, io: &Input<'_>, template: u32) -> Option<u64> {
        (io.bag_item)(template).filter(|&id| Some(id) != self.before)
    }

    fn shop(&mut self, io: &Input<'_>, mem: &mut GearMem, acts: &mut Vec<Act>) -> GoalStatus {
        let Job::Buy {
            template,
            grade,
            price,
            vendor,
            ..
        } = self.job
        else {
            return GoalStatus::Done;
        };
        self.note = "buying a piece";
        if self.tries > 0 {
            if let Some(id) = self.arrived(io, template) {
                self.got = id;
                mem.note_new(id, grade);
                mem.fetched_now(template);
                mem.stats.bought += 1;
                mem.stats.spent += i64::from(price);
                let confirmed_debit = self
                    .purchase_money
                    .is_some_and(|before| before.saturating_sub(io.money) == i64::from(price));
                acts.push(Act::Journal(Entry::Bought {
                    template,
                    price,
                    confirmed_debit,
                }));
                acts.push(Act::Say(format!(
                    "gear: bought {template} for {price} copper"
                )));
                self.enter(Phase::Equip);
                return GoalStatus::Running;
            }
        } else {
            self.before = (io.bag_item)(template);
        }
        if io.money < i64::from(price) {
            return GoalStatus::Failed(FailReason::Other("not enough money"));
        }
        match (io.merchant)(vendor.npc) {
            Some((npc_obj, pos)) => {
                self.vendor_seen_ms = io.now_ms;
                if dist2d(io.pos, pos) > SHOP_REACH_M {
                    return match self.walker.walk(io, acts, pos[0], pos[1], SHOP_STOP_M) {
                        Walk::Failed => {
                            GoalStatus::Failed(FailReason::Other("the merchant is out of reach"))
                        }
                        _ => GoalStatus::Running,
                    };
                }
                acts.push(Act::StopMotion);
                acts.push(Act::Face {
                    x: pos[0],
                    y: pos[1],
                });
                if self.sent_ms == 0 || io.now_ms.saturating_sub(self.sent_ms) >= BUY_WAIT_MS {
                    if self.tries >= MAX_BUY_TRIES {
                        return GoalStatus::Failed(FailReason::Other("the merchant sold nothing"));
                    }
                    acts.push(Act::Buy {
                        npc: npc_obj,
                        item: template,
                        grade,
                    });
                    self.purchase_money.get_or_insert(io.money);
                    self.tries += 1;
                    self.sent_ms = io.now_ms;
                }
                GoalStatus::Running
            }
            None => {
                // not in view: go to the merchant's spawn and look
                if dist2d(io.pos, vendor.pos) <= SHOP_REACH_M + 4.0 {
                    if self.vendor_seen_ms == 0 {
                        self.vendor_seen_ms = io.now_ms;
                    }
                    if io.now_ms.saturating_sub(self.vendor_seen_ms) > VENDOR_VIEW_MS {
                        return GoalStatus::Failed(FailReason::Other("the merchant is not there"));
                    }
                    acts.push(Act::StopMotion);
                    return GoalStatus::Running;
                }
                self.vendor_seen_ms = 0;
                match self
                    .walker
                    .walk(io, acts, vendor.pos[0], vendor.pos[1], SHOP_STOP_M + 2.0)
                {
                    Walk::Failed => {
                        GoalStatus::Failed(FailReason::Other("the merchant is out of reach"))
                    }
                    _ => GoalStatus::Running,
                }
            }
        }
    }

    /// The template and grade the run mints.
    fn mint_target(&self) -> (u32, u8) {
        match self.job {
            Job::Mint {
                template, grade, ..
            } => (template, grade),
            Job::Socket { gem, .. } => (gem, 0),
            Job::Regrade { scroll, .. } => (scroll, 0),
            Job::Equip { template, .. } | Job::Buy { template, .. } => (template, 0),
        }
    }

    fn mint(&mut self, io: &Input<'_>, mem: &mut GearMem, acts: &mut Vec<Act>) -> GoalStatus {
        let (template, grade) = self.mint_target();
        self.note = "minting";
        if self.tries == 0 {
            self.before = (io.bag_item)(template);
            let line = if grade == 0 {
                format!("/item add self {template} 1")
            } else {
                format!("/item add self {template} 1 {grade}")
            };
            acts.push(Act::Gm(line));
            self.tries = 1;
            self.sent_ms = io.now_ms;
            return GoalStatus::Running;
        }
        if let Some(id) = self.arrived(io, template) {
            self.got = id;
            if matches!(self.job, Job::Mint { .. }) {
                mem.note_new(id, grade);
                mem.fetched_now(template);
                mem.stats.minted += 1;
                acts.push(Act::Journal(Entry::Minted { template }));
                acts.push(Act::Say(format!("gear: minted {template} (grade {grade})")));
                self.enter(Phase::Equip);
            } else {
                self.enter(Phase::Cast);
            }
            return GoalStatus::Running;
        }
        if io.now_ms.saturating_sub(self.sent_ms) > MINT_WAIT_MS {
            return GoalStatus::Failed(FailReason::Other("the minted item did not arrive"));
        }
        GoalStatus::Running
    }

    fn equip(&mut self, io: &Input<'_>, mem: &mut GearMem, acts: &mut Vec<Act>) -> GoalStatus {
        self.note = "putting a piece on";
        if self.got == 0 {
            let Job::Equip { template, .. } = self.job else {
                return GoalStatus::Failed(FailReason::Other("nothing to put on"));
            };
            match (io.bag_item)(template) {
                Some(id) => self.got = id,
                None => {
                    return GoalStatus::Failed(FailReason::Other("the piece is not in the bag"))
                }
            }
        }
        if let Some((slot, template)) = (io.worn)(self.got) {
            mem.stats.equipped += 1;
            let text = format!("put on {template} in {}", aa_world::gear::slot_name(slot));
            mem.changed(&text);
            acts.push(Act::Journal(Entry::Equipped { template, slot }));
            acts.push(Act::Say(format!("gear: {text}")));
            return GoalStatus::Done;
        }
        if self.sent_ms != 0 {
            let refused = refused_since(io.errors, self.sent_ms);
            if !refused && io.now_ms < self.sent_ms + EQUIP_WAIT_MS {
                return GoalStatus::Running;
            }
            self.sent_ms = 0;
            if self.tries >= MAX_EQUIP_TRIES {
                mem.refuse(self.got, io.now_ms);
                return GoalStatus::Failed(FailReason::Other("the server refused the piece"));
            }
        }
        match (io.place)(self.got, self.forced()) {
            Some(m) => {
                acts.push(Act::Equip(m));
                self.tries += 1;
                self.sent_ms = io.now_ms;
                GoalStatus::Running
            }
            None => {
                acts.push(Act::Say(
                    "gear: the new piece is no better than what the bot wears; it stays in the bag"
                        .into(),
                ));
                GoalStatus::Done
            }
        }
    }

    fn cast(&mut self, io: &Input<'_>, mem: &mut GearMem, acts: &mut Vec<Act>) -> GoalStatus {
        let (target, item_template, socket) = match self.job {
            Job::Socket { target, gem, .. } => (target, gem, true),
            Job::Regrade { target, scroll, .. } => (target, scroll, false),
            _ => return GoalStatus::Done,
        };
        self.note = if socket {
            "socketing a gem"
        } else {
            "regrading"
        };
        let Some((_, piece)) = (io.worn)(target) else {
            if self.sent_ms != 0 && !socket {
                // a regrade that broke the piece: the result says so
                if let Some(st) = self.regrade_result(mem, acts, 0) {
                    return st;
                }
            }
            return GoalStatus::Failed(FailReason::Other("the piece is no longer worn"));
        };
        if self.sent_ms == 0 {
            let Some(item_id) = (io.bag_item)(item_template) else {
                return GoalStatus::Failed(FailReason::Other(
                    "the gem or scroll is not in the bag",
                ));
            };
            self.skill = (io.use_skill)(item_template);
            if self.skill == 0 {
                return GoalStatus::Failed(FailReason::Other("the item has no use skill"));
            }
            acts.push(Act::StopMotion);
            acts.push(Act::Cast {
                skill: self.skill,
                item_id,
                item_template,
                target,
            });
            self.sent_ms = io.now_ms;
            return GoalStatus::Running;
        }
        if socket {
            if let Some(r) = mem
                .socket_result
                .filter(|r| r.at_ms >= self.sent_ms && r.item_id == target)
            {
                let Job::Socket { gem, held, .. } = self.job else {
                    return GoalStatus::Done;
                };
                let ok = r.result == 1;
                if ok {
                    mem.stats.socketed += 1;
                    mem.note_gems(target, held + 1);
                    mem.changed(&format!("socketed gem {gem} into {piece}"));
                } else {
                    mem.stats.socket_fails += 1;
                    mem.note_gems(target, 0);
                    mem.changed(&format!(
                        "socketing gem {gem} into {piece} failed and took {held} gems out"
                    ));
                }
                acts.push(Act::Journal(Entry::Socket {
                    piece,
                    gem,
                    held,
                    ok,
                }));
                acts.push(Act::Say(format!("gear: {}", mem.last_change)));
                return GoalStatus::Done;
            }
        } else if let Some(st) = self.regrade_result(mem, acts, piece) {
            return st;
        }
        let refused = io.errors.iter().any(|(at, e)| {
            *at >= self.sent_ms
                && matches!(e, StateError::Skill { skill, .. } if *skill == self.skill)
        });
        if refused {
            return GoalStatus::Failed(FailReason::Other("the server refused the cast"));
        }
        if io.now_ms > self.sent_ms + (io.cast_ms)(self.skill) + CAST_EXTRA_MS {
            return GoalStatus::Failed(FailReason::Timeout);
        }
        GoalStatus::Running
    }

    /// The regrade's answer, if it is there.
    fn regrade_result(
        &mut self,
        mem: &mut GearMem,
        acts: &mut Vec<Act>,
        piece: u32,
    ) -> Option<GoalStatus> {
        let Job::Regrade { target, .. } = self.job else {
            return None;
        };
        let r = mem
            .grade_result
            .filter(|r| r.at_ms >= self.sent_ms && r.item_id == target)?;
        let up = r.result >= aa_proto::sc_world::grade_enchant_result::SUCCESS;
        if up {
            mem.stats.regraded += 1;
        } else {
            mem.stats.regrade_fails += 1;
        }
        mem.changed(&format!(
            "regrade of {piece}: result {}, grade {} to {}",
            r.result,
            aa_world::gear::grade_name(r.before),
            aa_world::gear::grade_name(r.after)
        ));
        acts.push(Act::Journal(Entry::Regrade {
            piece,
            result: r.result,
            before: r.before,
            after: r.after,
        }));
        acts.push(Act::Say(format!("gear: {}", mem.last_change)));
        Some(GoalStatus::Done)
    }
}
