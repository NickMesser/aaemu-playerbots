//! Declare war with the guild's camp, register for a siege, and fight for the tower.

mod camp_auction;

use aa_proto::{cs, sc_siege};

use crate::bot::combat::{Combat, CombatStatus};
use crate::bot::ctx::Ctx;
use crate::bot::guild::siege::{self, Declaration, Order, Side};
use crate::bot::motion::Pace;
use crate::bot::state::{dist2d, UnitKind};

use super::{FailReason, GoalStatus};

const ENGRAVE: u32 = 20_866;
const DECLARE: u32 = 16_918;
const DECLARE_WAIT_MS: u64 = 18_000;
const CAMP_APPROACH_M: f32 = 2.0;
const JOIN_RETRY_MS: u64 = 10_000;
const ENGRAVE_RETRY_MS: u64 = 15_000;
const STAND_M: f32 = 23.0;
const AREA_M: f32 = 80.0;

#[derive(Debug, Default)]
pub struct Goal {
    order: Option<Order>,
    declaration: Option<Declaration>,
    camp_auction: Option<Box<camp_auction::Auction>>,
    joined: bool,
    fought: bool,
    attempts: u8,
    approach_attempts: u8,
    next_try: u64,
    attack_next: u64,
}

pub fn score(ctx: &Ctx<'_>, _base: f32) -> f32 {
    if ctx.state.me.hp_pct() < 60 {
        return 0.0;
    }
    match siege::available(ctx, None) {
        Some(order) if order.period == sc_siege::BATTLE => 4.0,
        Some(order) if order.period == sc_siege::WARMUP => 2.0,
        _ if siege::declaration(ctx).is_some() => 2.0,
        _ if camp_auction::Auction::new(ctx).is_some() => 1.5,
        _ => 0.0,
    }
}

impl Goal {
    pub fn start(&mut self, ctx: &mut Ctx<'_>) {
        self.order =
            siege::available(ctx, None).filter(|order| order.period != sc_siege::DECLARATION);
        if self.order.is_none() {
            self.declaration = siege::declaration(ctx);
            if self.declaration.is_none() {
                self.camp_auction = camp_auction::Auction::new(ctx).map(Box::new);
            }
        }
    }

    pub fn tick(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        if let Some(auction) = &mut self.camp_auction {
            return auction.tick(ctx);
        }
        if let Some(order) = self.declaration {
            return self.declare(ctx, order);
        }
        let Some(previous) = self.order else {
            return GoalStatus::Failed(FailReason::NoTarget);
        };
        let guild_id = ctx.state.guild.log().map_or(0, |log| log.id);
        let owned = siege::owner(ctx, previous.group) == Some(guild_id) && guild_id != 0;
        if self.fought && previous.side == Side::Attack && owned {
            self.note_outcome(ctx, previous.group, true);
            return GoalStatus::Done;
        }
        let Some(order) = siege::available(ctx, Some(previous.group)) else {
            if self.fought
                && previous.side == Side::Defend
                && owned
                && siege::battle_finished(ctx, previous.group)
            {
                self.note_outcome(ctx, previous.group, false);
            }
            return GoalStatus::Done;
        };
        self.order = Some(order);
        if !order.enrolled {
            if self.joined || self.attempts >= 3 && ctx.now_ms >= self.next_try {
                return GoalStatus::Failed(FailReason::Other("siege enrollment refused"));
            }
            if ctx.now_ms >= self.next_try {
                ctx.send(cs::join_siege(order.group));
                self.attempts += 1;
                self.next_try = ctx.now_ms + JOIN_RETRY_MS;
            }
            return GoalStatus::Running;
        }
        if !self.joined {
            self.joined = true;
            self.next_try = ctx.now_ms;
            ctx.log
                .say(format!("siege: enrolled for zone group {}", order.group));
        }
        if order.period != sc_siege::BATTLE {
            return GoalStatus::Running;
        }
        let distance = dist2d(ctx.pos(), order.position);
        if distance > STAND_M && ctx.combat.target().is_none() {
            if !ctx.motion.is_moving() {
                if self.approach_attempts >= 3 {
                    return GoalStatus::Failed(FailReason::Stuck);
                }
                self.approach_attempts += 1;
                ctx.travel(
                    order.position[0],
                    order.position[1],
                    STAND_M - 1.0,
                    Pace::Run,
                );
            }
            return GoalStatus::Running;
        }
        self.approach_attempts = 0;
        self.fought = true;
        let Some(mem) = ctx.state.guild.get() else {
            return GoalStatus::Done;
        };
        if order.side == Side::Attack && mem.sieges.engravable(order.group) {
            ctx.combat.disengage();
            ctx.motion.stop();
            if distance <= STAND_M
                && ctx.now_ms >= self.next_try
                && !mem.sieges.engraving(order.group)
            {
                ctx.motion.face(order.position[0], order.position[1]);
                ctx.send(cs::change_target(order.tower));
                ctx.send(cs::start_skill_unit(ENGRAVE, ctx.state.me.obj, order.tower));
                self.next_try = ctx.now_ms + ENGRAVE_RETRY_MS;
            }
            return GoalStatus::Running;
        }
        if ctx.now_ms < self.attack_next {
            return GoalStatus::Running;
        }
        let target = target(ctx, order);
        if let Some(object) = target {
            if ctx.combat.target() != Some(object) {
                ctx.combat.disengage();
                ctx.combat.engage(object);
            }
            if matches!(Combat::tick(ctx), CombatStatus::Lost) {
                self.attack_next = ctx.now_ms + 8_000;
            }
        } else if ctx.combat.target().is_some() {
            ctx.combat.disengage();
        }
        GoalStatus::Running
    }

    fn declare(&mut self, ctx: &mut Ctx<'_>, order: Declaration) -> GoalStatus {
        if siege::declared_by(ctx, order) {
            if self.next_try != 0 {
                ctx.hub.journal.update(ctx.persona.id, |journal| {
                    journal.sieges_declared = journal.sieges_declared.saturating_add(1);
                });
                ctx.log
                    .say(format!("siege: declared war in territory {}", order.group));
            }
            return GoalStatus::Done;
        }
        if !siege::declaration_open(ctx, order) {
            return GoalStatus::Failed(FailReason::NoTarget);
        }
        if self.next_try != 0 {
            return if ctx.now_ms >= self.next_try {
                GoalStatus::Failed(FailReason::Other("siege camp was not accepted"))
            } else {
                GoalStatus::Running
            };
        }
        let Some((_, item)) = ctx.state.find_item(order.camp) else {
            return GoalStatus::Failed(FailReason::NoTarget);
        };
        let [x, y] = order.position;
        if dist2d(ctx.pos(), [x, y, 0.0]) > CAMP_APPROACH_M {
            if !ctx.motion.is_moving() {
                if self.approach_attempts >= 3 {
                    return GoalStatus::Failed(FailReason::Stuck);
                }
                self.approach_attempts += 1;
                ctx.motion
                    .go_to(x, y, CAMP_APPROACH_M - 0.5, Pace::Run, ctx.now_ms);
            }
            return GoalStatus::Running;
        }
        if !ctx
            .world
            .zone_info_at(ctx.pos()[0], ctx.pos()[1])
            .is_some_and(|zone| zone.group == order.group)
        {
            return GoalStatus::Failed(FailReason::NoTarget);
        }
        let me = ctx.state.me.obj;
        let cast = cs::start_skill_item(DECLARE, me, item.id, order.camp, me);
        ctx.motion.stop();
        ctx.send(cast);
        self.next_try = ctx.now_ms + DECLARE_WAIT_MS;
        GoalStatus::Running
    }

    pub fn stop(&mut self, ctx: &mut Ctx<'_>) {
        ctx.combat.disengage();
        ctx.motion.stop();
    }

    pub fn describe(&self, out: &mut String) {
        if let Some(order) = self.order {
            let action = if order.side == Side::Attack {
                "attacking"
            } else {
                "defending"
            };
            out.push_str(&format!("siege {action} zone group {}", order.group));
        } else if let Some(order) = self.declaration {
            out.push_str(&format!("siege declaring war in territory {}", order.group));
        } else if let Some(auction) = &self.camp_auction {
            out.push_str(&format!(
                "siege seeking the camp for territory {}",
                auction.group()
            ));
        } else {
            out.push_str("siege");
        }
    }

    fn note_outcome(&self, ctx: &mut Ctx<'_>, group: u16, captured: bool) {
        ctx.hub.journal.update(ctx.persona.id, |j| {
            let count = if captured {
                &mut j.sieges_won
            } else {
                &mut j.sieges_held
            };
            *count = count.saturating_add(1);
        });
        let result = if captured { "captured" } else { "held" };
        ctx.log.say(format!("siege: {result} territory {group}"));
    }
}

fn target(ctx: &Ctx<'_>, order: Order) -> Option<u32> {
    if order.side == Side::Attack {
        return ctx
            .state
            .units
            .get(&order.tower)
            .filter(|unit| !unit.dead())
            .map(|_| order.tower);
    }
    let guild = ctx.state.guild.get()?;
    ctx.state
        .units
        .iter()
        .filter(|(_, unit)| unit.kind == UnitKind::Character && !unit.dead())
        .filter(|(_, unit)| {
            let allowed = if ctx.hub.is_bot_char(unit.id) {
                ctx.cfg.pvp.attack_bots
            } else {
                ctx.cfg.may_attack_players()
                    && unit.name.as_deref().is_some_and(|name| {
                        ctx.cfg.pvp.only_names.is_empty()
                            || ctx
                                .cfg
                                .pvp
                                .only_names
                                .iter()
                                .any(|candidate| candidate.eq_ignore_ascii_case(name))
                    })
            };
            allowed
                && unit.name.as_deref().is_some_and(|name| {
                    !ctx.cfg
                        .pvp
                        .never_names
                        .iter()
                        .any(|denied| denied.eq_ignore_ascii_case(name))
                })
                && guild.sieges.enemy(order, unit.id)
                && dist2d(order.position, unit.pos) <= AREA_M
        })
        .min_by(|(_, a), (_, b)| a.dist2d(ctx.pos()).total_cmp(&b.dist2d(ctx.pos())))
        .map(|(&object, _)| object)
}
