//! Gear: tier up, socket and regrade the equipment as the bot levels (the gear feature).
//!
//! One run does one [`Job`] of the planner ([`crate::bot::gear::plan`]): buy a better piece from a
//! merchant, mint one with a GM command (`bots.gm_bootstrap` and `[gear] mint`), socket a lunagem
//! into a free socket, or regrade a worn piece (`[gear] regrade`, off by default); the new piece is
//! put on by the run itself. Started with an item ([`GoalArgs::item`]) the run gets that piece (or
//! gem) and puts it on even when it is worse. The state machine is
//! [`crate::bot::gear::machine`]; this file builds its input from [`Ctx`], applies its acts and
//! holds the scoring.
//!
//! Putting on better pieces from the bag is not the goal's job: the equip duty
//! ([`crate::bot::gear::duty`]) does that whatever the bot does.
//!
//! # Scoring
//!
//! [`score`] is 0 unless `[gear] enabled`, the bot is settled and healthy in the main world, it is
//! not waiting after a failed run, and the duty's last look ([`Want`]) found something to do. The
//! weight is [`weight`] (from the bot's `gearhead`); a purchase or a minted piece scores
//! `weight * (0.3 + 2 * deficit)` (the deficit is how far the core slots are below the tier), a
//! gem 0.7 and a regrade 0.4 times the weight.

use aa_proto::cs::{self, BuyItem, SHOP_MONEY};
use pb_web::Activity;

use crate::bot::cmd::GoalArgs;
use crate::bot::ctx::Ctx;
use crate::bot::gear::eval::{self, Fit};
use crate::bot::gear::machine::{Act, Entry, Input, Machine};
use crate::bot::gear::plan::{self, Job, Me, Want};
use crate::bot::gear::{self as gear, WorldCatalog};
use crate::bot::motion::Pace;
use crate::bot::state::{BotState, UnitKind};
use crate::config::BehaviorCfg;
use crate::roster::Temperament;

use super::{FailReason, GoalStatus};

/// A bot that has been online for less than this does not run the goal yet.
const SETTLE_MS: u64 = 30_000;
/// Hit points (percent) below which the bot does not set out.
const MIN_HP_PCT: u8 = 50;
/// After a failed run the goal waits this long.
const FAIL_QUIET_MS: u64 = 5 * 60_000;
/// After a run that found nothing to do the goal waits this long.
const IDLE_QUIET_MS: u64 = 20 * 60_000;

/// Tiering up, socketing and regrading. See the module docs.
#[derive(Debug, Default)]
pub struct Gear {
    /// What the goal was started with (`None`: the bot's own choice).
    args: Option<Box<GoalArgs>>,
    run: Option<Box<Machine>>,
}

impl Gear {
    /// A fresh goal (the brain starts it with [`Goal::with_args`](super::Goal::with_args)).
    pub fn new(args: Option<Box<GoalArgs>>) -> Gear {
        Gear { args, run: None }
    }

    /// The arguments it was started with.
    pub fn args(&self) -> Option<&GoalArgs> {
        self.args.as_deref()
    }

    /// The run, once started.
    pub fn machine(&self) -> Option<&Machine> {
        self.run.as_deref()
    }

    /// Called once when the brain picks the goal: chooses the job.
    pub fn start(&mut self, ctx: &mut Ctx<'_>) {
        self.run = None;
        if !ctx.cfg.gear.enabled {
            return;
        }
        let item = self.args.as_ref().and_then(|a| a.item);
        match choose(ctx, item) {
            Ok(job) => {
                ctx.log
                    .say(format!("goal: gear ({})", describe_job(ctx, &job)));
                self.run = Some(Box::new(Machine::new(job, ctx.now_ms)));
            }
            Err(why) => {
                ctx.log.say(format!("goal: gear (nothing to do: {why})"));
                if let Some(mem) = ctx.state.gear.as_deref_mut() {
                    mem.quiet_until_ms = ctx.now_ms + IDLE_QUIET_MS;
                    mem.want = Want::default();
                }
            }
        }
    }

    /// One tick.
    pub fn tick(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let Some(machine) = self.run.as_mut() else {
            return GoalStatus::Failed(FailReason::NoTarget);
        };
        if !ctx.cfg.gear.enabled {
            return GoalStatus::Failed(FailReason::Other("gear is switched off"));
        }
        if ctx.state.trade.active() || ctx.state.place.in_instance() {
            return GoalStatus::Failed(FailReason::Other("busy with something else"));
        }
        let mut mem = ctx.state.gear.take().unwrap_or_default();
        let mut acts = Vec::new();
        let status = {
            let facts = ctx.world.gear();
            let world = ctx.world;
            let fit = Fit::of(&ctx.persona.archetype);
            let st: &BotState = ctx.state;
            let worn = gear::worn_of(st, Some(&mem), facts, &fit);
            let mut bag = Vec::new();
            for (&slot, it) in &st.bag {
                if facts.item(it.template).is_some() {
                    bag.push(eval::BagPiece {
                        slot,
                        id: it.id,
                        template: it.template,
                        grade: mem.grade_of(it.id),
                        gems: mem.gems_of(it.id),
                    });
                }
            }
            let (level, gender) = (st.me.level, st.me.gender);
            let free = (mem.bag_slots > 0)
                .then(|| usize::from(mem.bag_slots).saturating_sub(st.bag.len()));
            let place = |id: u64, forced: bool| {
                let p = bag.iter().find(|b| b.id == id)?;
                eval::place(&worn, p, facts, &fit, level, gender, free, !forced)
            };
            let bag_item = |t: u32| st.find_item(t).map(|(_, i)| i.id);
            let worn_at = |id: u64| {
                st.equipment
                    .iter()
                    .find(|e| e.id == id && id != 0)
                    .map(|e| (e.slot, e.template))
            };
            let pos = st.me.pos;
            let merchant = |npc: u32| nearest_merchant(st, npc, pos);
            let use_skill = |t: u32| world.item_use_skill(t);
            let cast_ms = |s: u32| world.skill(s).map_or(0, |s| u64::from(s.casting_time));
            let io = Input {
                now_ms: ctx.now_ms,
                pos,
                motion: ctx.motion.status(),
                money: st.me.money,
                errors: st.errors.as_slice(),
                bag_item: &bag_item,
                worn: &worn_at,
                place: &place,
                merchant: &merchant,
                use_skill: &use_skill,
                cast_ms: &cast_ms,
            };
            machine.step(&io, &mut mem, &mut acts)
        };
        if matches!(status, GoalStatus::Failed(_)) {
            mem.quiet_until_ms = ctx.now_ms + FAIL_QUIET_MS;
        }
        ctx.state.gear = Some(mem);
        apply(ctx, acts);
        status
    }

    /// Called when the goal ends, fails or is interrupted: the walk stops and the equip duty looks
    /// again at once.
    pub fn stop(&mut self, ctx: &mut Ctx<'_>) {
        if let Some(mut machine) = self.run.take() {
            let mut acts = Vec::new();
            machine.stop(&mut acts);
            apply(ctx, acts);
        }
        if let Some(mem) = ctx.state.gear.as_deref_mut() {
            mem.want = Want::default();
            mem.next_check_ms = 0;
        }
        ctx.motion.stop();
    }

    /// Whether the goal drives the combat controller itself (see [`super::Goal::owns_combat`]).
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
        out.push_str("gear");
        if let Some(m) = &self.run {
            out.push_str(": ");
            out.push_str(m.note());
            out.push_str(" (");
            out.push_str(m.job().name());
            out.push(')');
        }
    }
}

/// The goal's weight before the situation is looked at: 0 (the goal scores itself, see [`score`]).
pub fn base_weight(_behavior: &BehaviorCfg, _temper: &Temperament) -> f32 {
    0.0
}

/// The weight of the goal for a bot with this much `gearhead`: from 0.12 (does not care) to 0.48
/// (lives for its gear), 0.3 for the neutral profile.
pub fn weight(gearhead: f32) -> f32 {
    0.30 * (0.4 + 1.2 * gearhead.clamp(0.0, 1.0))
}

/// The score of what the goal could do (see the module docs).
pub fn score_of(want: &Want, gearhead: f32) -> f32 {
    let w = weight(gearhead);
    let mut s: f32 = 0.0;
    if want.buy || want.mint {
        s = w * (0.3 + 2.0 * want.deficit.clamp(0.0, 1.0));
    }
    if want.socket {
        s = s.max(w * 0.7);
    }
    if want.regrade {
        s = s.max(w * 0.4);
    }
    s
}

/// The score of the goal for this tick. 0 keeps the bot from picking it. Reads only what the equip
/// duty worked out, so it is cheap.
pub fn score(ctx: &Ctx<'_>, _base: f32) -> f32 {
    if !ctx.cfg.gear.enabled {
        return 0.0;
    }
    let Some(mem) = ctx.state.gear.as_deref() else {
        return 0.0;
    };
    let now = ctx.now_ms;
    if now < mem.quiet_until_ms
        || now < SETTLE_MS
        || mem.pending.is_some()
        || ctx.state.place.in_instance()
        || ctx.state.me.hp_pct() < MIN_HP_PCT
    {
        return 0.0;
    }
    score_of(&mem.want, ctx.persona.profile.gearhead)
}

/// The job of a run (see [`plan::choose`]).
fn choose(ctx: &Ctx<'_>, item: Option<u32>) -> Result<Job, &'static str> {
    let facts = ctx.world.gear();
    let fit = Fit::of(&ctx.persona.archetype);
    let st: &BotState = ctx.state;
    let mem = st.gear.as_deref();
    let worn = gear::worn_of(st, mem, facts, &fit);
    let reserved = super::work::customer::gear_reservation(ctx);
    let mut bag = Vec::new();
    for (&slot, it) in &st.bag {
        if reserved.is_some_and(|(template, stock)| {
            it.template == template && stock.is_none_or(|id| id == it.id)
        }) {
            continue;
        }
        if facts.item(it.template).is_some() || facts.gem(it.template).is_some() {
            bag.push(eval::BagPiece {
                slot,
                id: it.id,
                template: it.template,
                grade: mem.map_or(0, |m| m.grade_of(it.id)),
                gems: mem.and_then(|m| m.gems_of(it.id)),
            });
        }
    }
    let fetched: &[u32] = mem.map_or(&[], |m| &m.fetched);
    let stats = mem.map(|m| m.stats).unwrap_or_default();
    let me = Me {
        id: ctx.persona.id,
        level: st.me.level,
        gender: st.me.gender,
        money: st.me.money,
        gearhead: ctx.persona.profile.gearhead,
        fit: &fit,
        worn: &worn,
        bag: &bag,
        fetched,
        minted: stats.minted,
        bought: stats.bought,
    };
    let cat = WorldCatalog {
        world: ctx.world,
        pos: st.me.pos,
        max_trip_m: ctx.cfg.gear.max_trip_m,
    };
    plan::choose(&me, &gear::rules(ctx), &cat, item)
}

/// A line for the log about a job.
fn describe_job(ctx: &Ctx<'_>, job: &Job) -> String {
    let name = |t: u32| ctx.world.item_name(t).unwrap_or("?").to_owned();
    match *job {
        Job::Equip { template, .. } => format!("put on {} ({template})", name(template)),
        Job::Buy {
            template,
            price,
            vendor,
            ..
        } => format!(
            "buy {} ({template}) for {price} copper from merchant {} {:.0} m away",
            name(template),
            vendor.npc,
            vendor.dist
        ),
        Job::Mint {
            template, grade, ..
        } => format!(
            "mint {} ({template}) at grade {}",
            name(template),
            aa_world::gear::grade_name(grade)
        ),
        Job::Socket {
            gem,
            slot,
            mint,
            chance,
            ..
        } => format!(
            "socket {} ({gem}{}) into the {} piece at {} %",
            name(gem),
            if mint { ", minted" } else { "" },
            aa_world::gear::slot_name(slot),
            chance / 100
        ),
        Job::Regrade {
            scroll, slot, from, ..
        } => format!(
            "regrade the {} piece ({}) with {} ({scroll})",
            aa_world::gear::slot_name(slot),
            aa_world::gear::grade_name(from),
            name(scroll)
        ),
    }
}

/// The nearest merchant NPC of `template` that is in view and alive: `(object id, position)`.
fn nearest_merchant(state: &BotState, template: u32, me: [f32; 3]) -> Option<(u32, [f32; 3])> {
    state
        .units
        .iter()
        .filter(|(_, u)| u.kind == UnitKind::Npc && u.id == template && !u.dead())
        .map(|(&obj, u)| (obj, u.pos, u.dist2d(me)))
        .min_by(|a, b| a.2.total_cmp(&b.2))
        .map(|(obj, pos, _)| (obj, pos))
}

/// Carries out what the machine asked for.
fn apply(ctx: &mut Ctx<'_>, acts: Vec<Act>) {
    for act in acts {
        match act {
            Act::Say(text) => ctx.log.say(text),
            Act::Gm(line) => {
                ctx.gm(&line);
            }
            Act::Travel { x, y, radius } => ctx.travel(x, y, radius, Pace::Run),
            Act::Face { x, y } => ctx.motion.face(x, y),
            Act::StopMotion => {
                if ctx.motion.is_moving() {
                    ctx.motion.stop();
                }
            }
            Act::Buy { npc, item, grade } => ctx.send(cs::buy_items(
                npc,
                &[BuyItem {
                    item,
                    grade,
                    count: 1,
                    currency: SHOP_MONEY,
                }],
            )),
            Act::Equip(m) => {
                if let Some(out) = gear::equip_packet(&m) {
                    ctx.send(out);
                }
            }
            Act::Cast {
                skill,
                item_id,
                item_template,
                target,
            } => {
                let me = ctx.state.me.obj;
                ctx.send(cs::start_skill_item_on_item(
                    skill,
                    me,
                    item_id,
                    item_template,
                    target,
                ));
            }
            Act::Journal(e) => {
                let unix = gear::unix_now();
                let last = ctx
                    .state
                    .gear
                    .as_deref()
                    .map(|m| m.last_change.to_string())
                    .unwrap_or_default();
                gear::note_journal(ctx, |j| match e {
                    Entry::Bought { price, .. } => j.bought(price, unix),
                    Entry::Minted { .. } => j.minted(unix),
                    Entry::Equipped { .. } => j.changed(&last, unix),
                    Entry::Socket {
                        piece,
                        gem,
                        held,
                        ok,
                    } => j.socket(piece, gem, held, ok, unix),
                    Entry::Regrade { result, .. } => j.regrade(
                        result >= aa_proto::sc_world::grade_enchant_result::SUCCESS,
                        unix,
                    ),
                });
            }
        }
    }
    if let Some(mem) = ctx.state.gear.as_deref_mut() {
        mem.dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_goal_shows_as_idle_or_traveling_and_keeps_its_arguments() {
        let g = Gear::new(None);
        assert_eq!(g.activity(false, false), Activity::Idle);
        assert_eq!(g.activity(true, false), Activity::Traveling);
        assert_eq!(g.activity(true, true), Activity::Fighting);
        assert!(!g.owns_combat());
        assert!(g.args().is_none());
        let mut text = String::new();
        g.describe(&mut text);
        assert!(text.starts_with("gear"), "{text}");
    }

    #[test]
    fn the_score_follows_the_deficit_and_the_gearhead() {
        let b = BehaviorCfg::default();
        assert_eq!(base_weight(&b, &Temperament::NEUTRAL), 0.0);
        assert_eq!(score_of(&Want::default(), 0.5), 0.0);
        let naked = Want {
            deficit: 1.0,
            mint: true,
            ..Want::default()
        };
        let fine = Want {
            deficit: 0.0,
            buy: true,
            ..Want::default()
        };
        assert!(score_of(&naked, 0.5) > score_of(&fine, 0.5));
        assert!(score_of(&naked, 1.0) > score_of(&naked, 0.0));
        assert!((weight(0.5) - 0.3).abs() < 1e-6);
        let gem = Want {
            socket: true,
            ..Want::default()
        };
        assert!(score_of(&gem, 0.5) > 0.0 && score_of(&gem, 0.5) < score_of(&naked, 0.5));
    }
}
