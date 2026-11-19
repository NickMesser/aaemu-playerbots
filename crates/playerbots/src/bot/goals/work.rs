//! Guild production orders, completed only when the server delivers the item.

pub mod assignment;
pub mod customer;
pub(crate) mod treasure;

use aa_proto::cs;
use aa_world::geom::Vec3;
use serde::{Deserialize, Serialize};

use super::{FailReason, GoalStatus};
use crate::bot::combat::CombatStatus;
use crate::bot::ctx::Ctx;
use crate::bot::goals::grind::Grind;
use crate::bot::guild::profession::{membership, Profession};
use crate::bot::pvp::walk::{Walk, Walker};
use crate::bot::state::{dist3d, StateError};

const GATHER_SEARCH_M: f32 = 240.0;
const GATHER_MISSING_M: f32 = 40.0;
const GATHER_STREAM_WAIT_MS: u64 = 3_000;
const GATHER_BACKOFF_MS: u64 = 5 * 60_000;
const MAX_GATHER_REFUSALS: usize = 8;
const INVALID_TARGET: u8 = 0x22;

#[derive(Debug, Clone, Copy)]
struct RefusedSite {
    template: u32,
    position: [f32; 2],
    until_ms: u64,
}

impl RefusedSite {
    fn matches(&self, template: u32, position: [f32; 2]) -> bool {
        self.template == template
            && (self.position[0] - position[0]).hypot(self.position[1] - position[1]) <= 1.5
    }
}

#[derive(Debug, Default)]
pub(crate) struct GatherBackoff(Vec<RefusedSite>);

impl GatherBackoff {
    fn blocks(&self, template: u32, position: [f32; 2], now_ms: u64) -> bool {
        self.0
            .iter()
            .any(|site| site.until_ms > now_ms && site.matches(template, position))
    }

    fn refused(&mut self, template: u32, position: [f32; 2], now_ms: u64) {
        self.0
            .retain(|site| site.until_ms > now_ms && !site.matches(template, position));
        if self.0.len() >= MAX_GATHER_REFUSALS {
            self.0.remove(0);
        }
        self.0.push(RefusedSite {
            template,
            position,
            until_ms: now_ms.saturating_add(GATHER_BACKOFF_MS),
        });
    }
}

fn gather_blocked(ctx: &Ctx<'_>, template: u32, position: [f32; 2]) -> bool {
    ctx.state
        .guild
        .get()
        .is_some_and(|guild| guild.gather_backoff.blocks(template, position, ctx.now_ms))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Order {
    Craft {
        recipe: u32,
        training_recipe: Option<u32>,
    },
    Gather {
        doodad: u32,
        item: u32,
    },
    Hunt {
        item: u32,
        count: u16,
    },
}

impl std::fmt::Display for Order {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Craft {
                recipe,
                training_recipe,
            } => {
                write!(f, "craft recipe {recipe}")?;
                if let Some(training) = training_recipe {
                    write!(f, " (train with recipe {training})")?;
                }
                Ok(())
            }
            Self::Gather { doodad, item } => write!(f, "gather item {item} from doodad {doodad}"),
            Self::Hunt { item, count } => write!(f, "hunt monsters for {count} of item {item}"),
        }
    }
}

impl Order {
    pub fn allowed_in(self, profession: Profession) -> bool {
        match (self, profession) {
            (
                Self::Craft {
                    recipe,
                    training_recipe,
                },
                Profession::Crafter,
            ) => {
                recipe != 0
                    && training_recipe.is_none_or(|training| training != 0 && training != recipe)
            }
            (
                Self::Gather {
                    doodad: 1..,
                    item: 1..,
                },
                Profession::Gatherer,
            ) => true,
            (
                Self::Hunt {
                    item: 1..,
                    count: 1..=1000,
                },
                Profession::Adventurer,
            ) => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Job {
    order: Order,
    training: bool,
    item: u32,
    increase: i64,
    object: u32,
    template: u32,
    destination: [f32; 3],
}

pub fn selected_order(ctx: &Ctx<'_>) -> Option<(Order, bool)> {
    let (guild, profession, false) = membership(ctx)? else {
        return None;
    };
    let order = assignment::order(ctx, guild)?;
    if !order.allowed_in(profession) {
        return None;
    }
    if let Order::Craft {
        recipe,
        training_recipe,
    } = order
    {
        let craft = ctx.world.craft(recipe)?;
        let points = ctx
            .state
            .econ
            .proficiency(u32::from(craft.actability_group));
        if points < craft.actability_limit {
            let training = ctx.world.craft(training_recipe?)?;
            return (training.actability_group == craft.actability_group
                && training.labor > 0
                && training.actability_limit <= points)
                .then_some((
                    Order::Craft {
                        recipe: training.id,
                        training_recipe: None,
                    },
                    true,
                ));
        }
    }
    Some((order, false))
}

fn available_order(ctx: &Ctx<'_>) -> Option<(Order, bool)> {
    if ctx.state.place.in_instance() || ctx.state.place.loading() || ctx.state.me.hp_pct() < 60 {
        return None;
    }
    selected_order(ctx)
}

fn plan(ctx: &Ctx<'_>, order: Order, training: bool) -> Option<Job> {
    let gather_doodad;
    let (doodads, item, increase) = match order {
        Order::Craft { recipe, .. } => {
            let craft = ctx.world.craft(recipe)?;
            if craft.is_pack
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
        Order::Hunt { .. } => return None,
    };
    let pos = ctx.pos();
    let visible = ctx
        .state
        .doodads
        .iter()
        .filter(|(_, d)| {
            doodads.contains(&d.template)
                && dist3d(pos, d.pos) <= 120.0
                && !ctx.in_war_zone(d.pos[0], d.pos[1])
                && (!matches!(order, Order::Gather { .. })
                    || !gather_blocked(ctx, d.template, [d.pos[0], d.pos[1]]))
        })
        .min_by(|(id_a, a), (id_b, b)| {
            dist3d(pos, a.pos)
                .total_cmp(&dist3d(pos, b.pos))
                .then(id_a.cmp(id_b))
        });
    let (object, template, destination) = if let Some((&object, target)) = visible {
        (object, target.template, target.pos)
    } else {
        let Order::Gather { doodad, .. } = order else {
            return None;
        };
        let from = Vec3::new(pos[0], pos[1], pos[2]);
        let spawn = ctx
            .world
            .doodad_spawns(doodad)
            .iter()
            .filter_map(|spawn| {
                let distance = from.dist2d(*spawn);
                ((GATHER_MISSING_M..=GATHER_SEARCH_M).contains(&distance)
                    && !ctx.world.zone_closed_at(spawn.x, spawn.y)
                    && !ctx.in_war_zone(spawn.x, spawn.y)
                    && !gather_blocked(ctx, doodad, [spawn.x, spawn.y]))
                .then_some((spawn, distance))
            })
            .min_by(|(_, left), (_, right)| left.total_cmp(right))?
            .0;
        (0, doodad, [spawn.x, spawn.y, spawn.z])
    };
    Some(Job {
        order,
        training,
        item,
        increase,
        object,
        template,
        destination,
    })
}

pub fn score(ctx: &Ctx<'_>) -> f32 {
    if let Some(commission) = customer::active(ctx).filter(customer::Commission::needs_trade) {
        return if customer::can_trade(ctx, &commission) {
            1.0
        } else {
            0.0
        };
    }
    match available_order(ctx) {
        Some((Order::Hunt { item, count }, _)) if Hunt::start(ctx, item, count).is_some() => 1.0,
        Some((order, training)) if plan(ctx, order, training).is_some() => 1.0,
        _ if treasure::find(ctx).is_some() => 1.0,
        _ => 0.0,
    }
}

pub(super) fn forage_ready(ctx: &Ctx<'_>) -> bool {
    available_order(ctx).is_some_and(|(order, training)| {
        matches!(order, Order::Gather { .. }) && plan(ctx, order, training).is_some()
    })
}

#[derive(Debug)]
struct Hunt {
    item: u32,
    count: u16,
    grind: Grind,
}

impl Hunt {
    fn start(ctx: &Ctx<'_>, item: u32, count: u16) -> Option<Self> {
        if ctx.state.item_count(item) >= i64::from(count)
            || !ctx
                .world
                .item_facts(item)
                .is_some_and(|facts| facts.tradable())
        {
            return None;
        }
        Some(Self {
            item,
            count,
            grind: Grind::for_material(ctx, item)?,
        })
    }
}

#[derive(Debug, Default)]
pub struct Work {
    job: Option<Job>,
    hunt: Option<Box<Hunt>>,
    treasure: Option<Box<treasure::Hunt>>,
    exchange: Option<Box<customer::Exchange>>,
    walker: Walker,
    deadline: u64,
    expected: Option<i64>,
    arrived_ms: Option<u64>,
    gather_cast: Option<GatherCast>,
}

#[derive(Debug, Clone, Copy)]
struct GatherCast {
    skill: u32,
    sent_ms: u64,
    position: [f32; 2],
}

impl GatherCast {
    fn refusal(&self, errors: &[(u64, StateError)]) -> Option<u8> {
        errors.iter().rev().find_map(|&(at, error)| match error {
            StateError::Skill { skill, result } if at >= self.sent_ms && skill == self.skill => {
                Some(result)
            }
            _ => None,
        })
    }
}

impl Work {
    pub fn start(&mut self, ctx: &mut Ctx<'_>) {
        if let Some(commission) = customer::active(ctx).filter(customer::Commission::needs_trade) {
            self.exchange = customer::Exchange::start(ctx, commission).map(Box::new);
            return;
        }
        match available_order(ctx) {
            Some((Order::Hunt { item, count }, _)) => {
                self.hunt = Hunt::start(ctx, item, count).map(Box::new);
            }
            Some((order, training)) => self.job = plan(ctx, order, training),
            None => self.treasure = treasure::find(ctx).map(Box::new),
        }
        if let Some(job) = self.job.filter(|job| job.object == 0) {
            let [x, y, _] = job.destination;
            ctx.log.say(format!(
                "guild production: looking for a source of item {} at {x:.0}, {y:.0}",
                job.item
            ));
        }
        self.deadline = ctx.now_ms + 90_000;
    }

    pub fn owns_combat(&self) -> bool {
        self.hunt.is_some()
    }

    pub fn handles_trade(&self) -> bool {
        self.exchange.is_some()
    }

    pub fn tick(&mut self, ctx: &mut Ctx<'_>, combat: CombatStatus) -> GoalStatus {
        if let Some(exchange) = &mut self.exchange {
            return exchange.tick(ctx);
        }
        if let Some(treasure) = &mut self.treasure {
            return treasure.tick(ctx, &mut self.walker);
        }
        if let Some(hunt) = &mut self.hunt {
            if ctx.state.item_count(hunt.item) >= i64::from(hunt.count) {
                note_receipt(ctx, hunt.item, false);
                return GoalStatus::Done;
            }
            return match hunt.grind.tick(ctx, combat) {
                GoalStatus::Done => GoalStatus::Failed(FailReason::NoTarget),
                status => status,
            };
        }
        let Some(job) = self.job else {
            return GoalStatus::Failed(FailReason::NoTarget);
        };
        if self
            .expected
            .is_some_and(|expected| ctx.state.item_count(job.item) >= expected)
        {
            note_receipt(ctx, job.item, job.training);
            if let (Order::Craft { recipe, .. }, false) = (job.order, job.training) {
                customer::crafted(ctx, recipe);
            }
            return GoalStatus::Done;
        }
        if ctx.now_ms >= self.deadline {
            return GoalStatus::Failed(FailReason::Timeout);
        }
        if self.expected.is_some() {
            if let Some(cast) = self.gather_cast {
                let Some(result) = cast.refusal(&ctx.state.errors) else {
                    return GoalStatus::Running;
                };
                if result == INVALID_TARGET {
                    ctx.state.guild.mem().gather_backoff.refused(
                        job.template,
                        cast.position,
                        ctx.now_ms,
                    );
                }
                ctx.log.say(format!(
                    "guild production: gathering skill {} was refused with result {result}",
                    cast.skill
                ));
                return GoalStatus::Failed(FailReason::Other("gathering skill refused"));
            }
            return GoalStatus::Running;
        }
        let [x, y, _] = job.destination;
        match self.walker.step(ctx, [x, y], 1.8) {
            Walk::Going => return GoalStatus::Running,
            Walk::Stuck => return GoalStatus::Failed(FailReason::Stuck),
            Walk::There => {}
        }
        let home = ctx.pos();
        let matches = |target: &crate::bot::state::Doodad| {
            target.template == job.template
                && dist3d(home, target.pos) <= 3.5
                && !ctx.in_war_zone(target.pos[0], target.pos[1])
                && (!matches!(job.order, Order::Gather { .. })
                    || !gather_blocked(ctx, job.template, [target.pos[0], target.pos[1]]))
        };
        let object = if job.object != 0 {
            ctx.state
                .doodads
                .get(&job.object)
                .filter(|target| matches(target))
                .map(|target| (job.object, target.pos))
        } else {
            ctx.state
                .doodads
                .iter()
                .filter(|(_, target)| matches(target))
                .min_by_key(|(&id, _)| id)
                .map(|(&id, target)| (id, target.pos))
        };
        let Some((object, position)) = object else {
            if job.object == 0 {
                let arrived = *self.arrived_ms.get_or_insert(ctx.now_ms);
                if ctx.now_ms - arrived < GATHER_STREAM_WAIT_MS {
                    return GoalStatus::Running;
                }
            }
            return GoalStatus::Failed(FailReason::NoTarget);
        };
        let packet = match job.order {
            Order::Craft { recipe, .. } => cs::execute_craft(recipe, object, 1),
            Order::Gather { doodad, .. } => {
                let skill = ctx.world.doodad_use_skill(doodad);
                self.gather_cast = Some(GatherCast {
                    skill,
                    sent_ms: ctx.now_ms,
                    position: [position[0], position[1]],
                });
                cs::start_skill_doodad(skill, ctx.state.me.obj, object)
            }
            Order::Hunt { .. } => return GoalStatus::Failed(FailReason::NoTarget),
        };
        self.expected = Some(ctx.state.item_count(job.item).saturating_add(job.increase));
        self.deadline = ctx.now_ms + 30_000;
        ctx.send(packet);
        GoalStatus::Running
    }

    pub fn stop(&mut self, ctx: &mut Ctx<'_>) {
        if let Some(exchange) = &mut self.exchange {
            exchange.stop(ctx);
        }
        if let Some(hunt) = &mut self.hunt {
            hunt.grind.stop(ctx);
        }
        if let Some(treasure) = &self.treasure {
            treasure.stop(ctx);
        }
        ctx.motion.stop();
    }

    pub fn describe(&self, out: &mut String) {
        if let Some(exchange) = &self.exchange {
            out.push_str(if exchange.purchasing() {
                "work: buying ingredients from the customer"
            } else {
                "work: delivering a customer's craft order"
            });
            return;
        }
        if let Some(hunt) = &self.treasure {
            out.push_str("work: ");
            out.push_str(hunt.name());
            return;
        }
        if let Some(hunt) = &self.hunt {
            use std::fmt::Write;
            let _ = write!(out, "Hunting for {} of item {}. ", hunt.count, hunt.item);
            hunt.grind.describe(out);
            return;
        }
        if let Some(job) = self.job {
            use std::fmt::Write;
            let action = if job.training {
                "training"
            } else {
                "production order"
            };
            let _ = write!(out, "Guild {action} for item {}", job.item);
        }
    }
}

fn note_receipt(ctx: &Ctx<'_>, item: u32, training: bool) {
    ctx.hub.journal.update(ctx.persona.id, |j| {
        let count = if training {
            &mut j.production_training_completed
        } else {
            &mut j.production_completed
        };
        *count = count.saturating_add(1);
    });
    let action = if training { "training" } else { "production" };
    ctx.log.say(format!("guild {action}: received item {item}"));
}

#[cfg(test)]
mod tests {
    use crate::config::Config;

    use super::{GatherBackoff, GatherCast, StateError, GATHER_BACKOFF_MS, INVALID_TARGET};

    #[test]
    fn a_gathering_refusal_applies_to_the_active_cast_and_site() {
        let cast = GatherCast {
            skill: 13986,
            sent_ms: 1_000,
            position: [10.0, 20.0],
        };
        let failure = StateError::Skill {
            skill: cast.skill,
            result: INVALID_TARGET,
        };
        let other = StateError::Skill {
            skill: 13625,
            result: INVALID_TARGET,
        };
        assert_eq!(cast.refusal(&[(999, failure), (1_001, other)]), None);
        assert_eq!(
            cast.refusal(&[(1_001, failure), (1_002, other)]),
            Some(INVALID_TARGET)
        );

        let mut sites = GatherBackoff::default();
        sites.refused(1671, cast.position, 1_001);
        assert!(sites.blocks(1671, [10.0, 21.0], 1_002));
        assert!(!sites.blocks(1671, [10.0, 24.0], 1_002));
        assert!(!sites.blocks(2246, cast.position, 1_002));
        assert!(!sites.blocks(1671, cast.position, 1_001 + GATHER_BACKOFF_MS));
    }

    #[test]
    fn production_orders_require_a_matching_profession() {
        let config = r#"
[guild]
enabled = true
professions = { "44" = "crafter", "45" = "gatherer", "46" = "adventurer" }
production_orders = { "44" = { kind = "craft", recipe = 123 }, "45" = { kind = "gather", doodad = 456, item = 789 }, "46" = { kind = "hunt", item = 890, count = 3 } }
"#;
        assert_eq!(
            Config::parse(config).unwrap().guild.production_orders.len(),
            3
        );
        assert!(Config::parse(&config.replace("crafter", "trader")).is_err());
        assert!(Config::parse(&config.replace("item = 789", "item = 0")).is_err());
        assert!(Config::parse(&config.replace("count = 3", "count = 0")).is_err());
    }

    #[test]
    fn a_member_specializes_without_changing_the_guild_default() {
        let config = r#"
[guild]
enabled = true
professions = { "44" = "crafter" }
production_orders = { "44" = { kind = "craft", recipe = 123 } }
member_production_orders = { "44" = { "12" = { kind = "craft", recipe = 456, training_recipe = 789 } } }
"#;
        let guild = Config::parse(config).unwrap().guild;
        assert!(matches!(
            guild.production_for(44, 12),
            Some(super::Order::Craft {
                recipe: 456,
                training_recipe: Some(789)
            })
        ));
        assert!(matches!(
            guild.production_for(44, 13),
            Some(super::Order::Craft {
                recipe: 123,
                training_recipe: None
            })
        ));
        assert_eq!(guild.orders().count(), 2);
        assert!(guild.production_for(45, 12).is_none());
        assert!(Config::parse(&config.replace("\"12\"", "\"0\"")).is_err());
    }
}
