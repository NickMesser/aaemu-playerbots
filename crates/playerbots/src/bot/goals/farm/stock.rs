//! Feed, treat or collect from a guild member's own livestock.

use aa_proto::cs;
use aa_world::StockAction;

use crate::bot::ctx::Ctx;
use crate::bot::farm::{Plot, OWN_LAND, SAME_SPOT_M};
use crate::bot::goals::{FailReason, GoalStatus};
use crate::bot::guild::profession::{self, Profession};
use crate::bot::motion::Pace;
use crate::bot::state::{dist2d, dist3d};

const REACH: f32 = 2.5;
const WAIT_FOR_VIEW_MS: u64 = 5_000;
const MAX_TRIP_MS: u64 = 8 * 60_000;
const RETRY_MS: u64 = 5 * 60_000;

pub fn enabled(ctx: &Ctx<'_>) -> bool {
    ctx.cfg.farm.husbandry
        && matches!(
            profession::membership(ctx),
            Some((_, Profession::Potato, false))
        )
}

pub fn choose(ctx: &Ctx<'_>) -> Option<Job> {
    if !enabled(ctx) {
        return None;
    }
    let book = ctx.world.farm();
    let mem = ctx.state.market.farm()?;
    let now = ctx.now_ms;
    let (plot, action, _) = mem
        .plots
        .iter()
        .filter(|p| p.land == OWN_LAND || p.kind() == Some(aa_world::FarmKind::Ranch))
        .filter(|p| {
            !mem.stock_retry.iter().any(|(pos, phase, until)| {
                *phase == p.phase && now < *until && dist2d(*pos, p.pos) < SAME_SPOT_M
            })
        })
        .filter_map(|p| {
            let action = book.stock_action(p.doodad, p.phase)?;
            let range = dist2d(ctx.pos(), p.pos);
            let skill = ctx.world.skill(action.skill())?;
            let labor = u32::try_from(ctx.state.me.labor).unwrap_or(0);
            (range <= ctx.cfg.farm.max_trip_m && labor >= u32::from(skill.labor))
                .then_some((*p, action, range))
        })
        .min_by(|a, b| a.2.total_cmp(&b.2))?;
    Some(Job {
        plot,
        action,
        started: now,
        arrived: None,
        cast: None,
        counts_before: Vec::new(),
        success: false,
    })
}

#[derive(Debug)]
pub struct Job {
    plot: Plot,
    action: StockAction,
    started: u64,
    arrived: Option<u64>,
    cast: Option<u64>,
    counts_before: Vec<(u32, i64)>,
    success: bool,
}

impl Job {
    pub fn description(&self) -> &'static str {
        match self.action {
            StockAction::Feed(_) => " feeding livestock",
            StockAction::Treat(_) => " treating livestock",
            StockAction::Collect(_) => " collecting livestock produce",
        }
    }

    pub fn tick(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let now = ctx.now_ms;
        let current = ctx
            .state
            .market
            .farm()
            .and_then(|mem| mem.at(self.plot.pos[0], self.plot.pos[1]))
            .copied()
            .filter(|p| p.doodad == self.plot.doodad);
        if let Some(deadline) = self.cast {
            let changed = current.is_some_and(|p| p.phase != self.plot.phase);
            let collected = self
                .counts_before
                .iter()
                .any(|&(item, before)| ctx.state.item_count(item) > before);
            if changed && (collected || !matches!(self.action, StockAction::Collect(_))) {
                self.success = true;
                ctx.hub.journal.update(ctx.persona.id, |j| {
                    j.livestock_actions = j.livestock_actions.saturating_add(1);
                });
                ctx.log.say(format!(
                    "farm: livestock {} completed skill {}",
                    self.plot.doodad,
                    self.action.skill()
                ));
                return GoalStatus::Done;
            }
            if now >= deadline {
                return GoalStatus::Failed(FailReason::Timeout);
            }
            return GoalStatus::Running;
        }
        if now.saturating_sub(self.started) >= MAX_TRIP_MS {
            return GoalStatus::Failed(FailReason::Timeout);
        }
        if dist3d(ctx.pos(), self.plot.pos) > REACH {
            if !ctx.motion.is_moving() {
                ctx.travel(self.plot.pos[0], self.plot.pos[1], REACH - 0.5, Pace::Run);
            }
            return GoalStatus::Running;
        }
        let arrived = *self.arrived.get_or_insert(now);
        let Some(plot) = current.filter(|p| p.obj != 0) else {
            return if now.saturating_sub(arrived) >= WAIT_FOR_VIEW_MS {
                GoalStatus::Failed(FailReason::NoTarget)
            } else {
                GoalStatus::Running
            };
        };
        if plot.phase != self.plot.phase {
            return GoalStatus::Done;
        }
        let skill = self.action.skill();
        let casting_time = ctx
            .world
            .skill(skill)
            .map_or(0, |s| u64::from(s.casting_time));
        if let StockAction::Collect(_) = self.action {
            let yields = ctx
                .world
                .farm()
                .livestock_yields(plot.doodad)
                .unwrap_or_default();
            self.counts_before = yields
                .iter()
                .map(|y| (y.item, ctx.state.item_count(y.item)))
                .collect();
            ctx.state.market.open_harvest(plot.honest);
        }
        ctx.motion.stop();
        ctx.motion.face(plot.pos[0], plot.pos[1]);
        ctx.send(cs::start_skill_doodad(skill, ctx.state.me.obj, plot.obj));
        self.cast = Some(now + casting_time + 7_000);
        GoalStatus::Running
    }

    pub fn stop(&self, ctx: &mut Ctx<'_>) {
        ctx.motion.stop();
        ctx.state.market.close_harvest();
        if !self.success {
            let attempts = &mut ctx.state.market.farm_mut().stock_retry;
            attempts.retain(|(_, _, until)| *until > ctx.now_ms);
            if attempts.len() >= attempts.inline_size() {
                attempts.remove(0);
            }
            attempts.push((self.plot.pos, self.plot.phase, ctx.now_ms + RETRY_MS));
        }
    }
}
