//! Guild production orders, completed only when the server delivers the item.

use aa_proto::cs;
use serde::Deserialize;

use super::{FailReason, GoalStatus};
use crate::bot::ctx::Ctx;
use crate::bot::guild::profession::{membership, Profession};
use crate::bot::pvp::walk::{Walk, Walker};
use crate::bot::state::dist3d;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Order {
    Craft { recipe: u32 },
    Gather { doodad: u32, item: u32 },
}

impl std::fmt::Display for Order {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Craft { recipe } => write!(f, "craft recipe {recipe}"),
            Self::Gather { doodad, item } => write!(f, "gather item {item} from doodad {doodad}"),
        }
    }
}

impl Order {
    pub fn allowed_in(self, profession: Profession) -> bool {
        matches!(
            (self, profession),
            (Self::Craft { recipe: 1.. }, Profession::Crafter)
                | (
                    Self::Gather {
                        doodad: 1..,
                        item: 1..
                    },
                    Profession::Gatherer
                )
        )
    }
}

#[derive(Debug, Clone, Copy)]
struct Job {
    order: Order,
    item: u32,
    increase: i64,
    object: u32,
    template: u32,
    destination: [f32; 3],
}

fn plan(ctx: &Ctx<'_>) -> Option<Job> {
    if ctx.state.place.in_instance() || ctx.state.place.loading() || ctx.state.me.hp_pct() < 60 {
        return None;
    }
    let (guild, profession, false) = membership(ctx)? else {
        return None;
    };
    let order = *ctx.cfg.guild.production_orders.get(&guild.to_string())?;
    if !order.allowed_in(profession) {
        return None;
    }
    let gather_doodad;
    let (doodads, item, increase) = match order {
        Order::Craft { recipe } => {
            let craft = ctx.world.craft(recipe)?;
            if craft.is_pack
                || craft.actability_limit != 0
                || craft.product.0 == 0
                || craft.product.1 == 0
                || ctx.state.me.level < craft.req_level
                || i32::from(ctx.state.me.labor) < i32::from(craft.labor)
                || craft
                    .materials
                    .iter()
                    .any(|&(item, count)| ctx.state.item_count(item) < i64::from(count))
            {
                return None;
            }
            (&*craft.benches, craft.product.0, i64::from(craft.product.1))
        }
        Order::Gather { doodad, item } => {
            let skill = ctx.world.skill(ctx.world.doodad_use_skill(doodad))?;
            if i32::from(ctx.state.me.labor) < i32::from(skill.labor) {
                return None;
            }
            // Only select catalogue-confirmed sources of the item.
            if !ctx.world.gather_doodads(item).contains(&doodad) {
                return None;
            }
            gather_doodad = [doodad];
            (&gather_doodad[..], item, 1)
        }
    };
    let pos = ctx.pos();
    let (&object, target) = ctx
        .state
        .doodads
        .iter()
        .filter(|(_, d)| {
            doodads.contains(&d.template)
                && dist3d(pos, d.pos) <= 120.0
                && !ctx.in_war_zone(d.pos[0], d.pos[1])
        })
        .min_by(|(id_a, a), (id_b, b)| {
            dist3d(pos, a.pos)
                .total_cmp(&dist3d(pos, b.pos))
                .then(id_a.cmp(id_b))
        })?;
    Some(Job {
        order,
        item,
        increase,
        object,
        template: target.template,
        destination: target.pos,
    })
}

pub fn score(ctx: &Ctx<'_>) -> f32 {
    if !ctx.cfg.guild.production_orders.is_empty() && plan(ctx).is_some() {
        1.0
    } else {
        0.0
    }
}

#[derive(Debug, Default)]
pub struct Work {
    job: Option<Job>,
    walker: Walker,
    deadline: u64,
    expected: Option<i64>,
}

impl Work {
    pub fn start(&mut self, ctx: &mut Ctx<'_>) {
        self.job = plan(ctx);
        self.deadline = ctx.now_ms + 90_000;
    }

    pub fn tick(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let Some(job) = self.job else {
            return GoalStatus::Failed(FailReason::NoTarget);
        };
        if self
            .expected
            .is_some_and(|expected| ctx.state.item_count(job.item) >= expected)
        {
            ctx.hub.journal.update(ctx.persona.id, |j| {
                j.production_completed = j.production_completed.saturating_add(1);
            });
            ctx.log
                .say(format!("guild work: received item {}", job.item));
            return GoalStatus::Done;
        }
        if ctx.now_ms >= self.deadline {
            return GoalStatus::Failed(FailReason::Timeout);
        }
        if self.expected.is_some() {
            return GoalStatus::Running;
        }
        let [x, y, _] = job.destination;
        match self.walker.step(ctx, [x, y], 1.8) {
            Walk::Going => return GoalStatus::Running,
            Walk::Stuck => return GoalStatus::Failed(FailReason::Stuck),
            Walk::There => {}
        }
        let Some(target) = ctx.state.doodads.get(&job.object) else {
            return GoalStatus::Failed(FailReason::NoTarget);
        };
        if target.template != job.template || dist3d(ctx.pos(), target.pos) > 3.5 {
            return GoalStatus::Failed(FailReason::NoTarget);
        }
        let packet = match job.order {
            Order::Craft { recipe } => cs::execute_craft(recipe, job.object, 1),
            Order::Gather { doodad, .. } => {
                let skill = ctx.world.doodad_use_skill(doodad);
                cs::start_skill_doodad(skill, ctx.state.me.obj, job.object)
            }
        };
        self.expected = Some(ctx.state.item_count(job.item).saturating_add(job.increase));
        self.deadline = ctx.now_ms + 30_000;
        ctx.send(packet);
        GoalStatus::Running
    }

    pub fn stop(&mut self, ctx: &mut Ctx<'_>) {
        ctx.motion.stop();
    }

    pub fn describe(&self, out: &mut String) {
        if let Some(job) = self.job {
            use std::fmt::Write;
            let _ = write!(out, "Guild production order for item {}", job.item);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::config::Config;

    #[test]
    fn production_orders_require_a_matching_profession() {
        let config = r#"
[guild]
enabled = true
professions = { "44" = "crafter", "45" = "gatherer" }
production_orders = { "44" = { kind = "craft", recipe = 123 }, "45" = { kind = "gather", doodad = 456, item = 789 } }
"#;
        assert_eq!(
            Config::parse(config).unwrap().guild.production_orders.len(),
            2
        );
        assert!(Config::parse(&config.replace("crafter", "trader")).is_err());
        assert!(Config::parse(&config.replace("item = 789", "item = 0")).is_err());
    }
}
