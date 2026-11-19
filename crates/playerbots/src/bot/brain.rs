//! The brain: chooses the next goal by utility scoring and runs it.
//!
//! Each goal gets a score: its configured weight, scaled by the bot's temperament (a quester likes
//! quests, a homebody likes housing and resting) and by the situation (rest when hurt, no grinding
//! when hurt or nothing to hit). The next goal is drawn at random in proportion to the scores, so
//! the bots do not all do the same thing at the same time. Goals that report
//! [`FailReason::NotImplemented`] are switched off for good, goals that fail otherwise rest for a
//! while ([`FAIL_COOLDOWN_MS`]).
//!
//! The dungeon, sail, auction, market, guild, gank, warzone, event, companion and farm goals are the
//! exception: the features that own them score them from the whole tick context (`own_scores`),
//! with none of the factors above (the guild, gank, warzone, companion and farm goals are stubs and
//! score 0 until their features exist). The event goal also has a duty hook: a running event it may
//! join pulls a bot out of the small goals at once ([`super::goals::event::duty`]), so a rift does
//! not wait for the end of a grind.
//!
//! Goals that cannot succeed for the bot right now (a trade run below the level of the lowest
//! pack recipe, housing without GM help or a design item) are left out of the draw up front
//! ([`gate`]); the answer is reused for [`GATE_TTL_MS`] and asked again after a level-up.
//!
//! The brain also calls the combat controller on every tick, so a bot defends itself whatever it is
//! doing, runs the chat duty ([`Social::tick`]) and, when `llm.director` is on, asks the LLM
//! director for a short plan now and then and after a level-up, a death, a failed goal or a new
//! zone ([`director`]): the model picks among the goals (and concrete variants of them) the
//! scoring itself would allow, the brain starts the plan's next step whenever a goal ends, and a
//! step that is no longer eligible is dropped silently. Every [`crate::agent::AGENT_EVERY_MS`] it
//! also tells the agent API which goals it may start ([`crate::agent::AgentSlot`]).

use std::cell::Cell;

use smallvec::SmallVec;

use super::cmd::{BotCmd, GoalArgs, GoalChoice, GoalKind, GoalMask, PairKind, PairOrder};
use super::combat::Combat;
use super::ctx::Ctx;
use super::director::{self, Ask, AskRequest, Decided, Director, Situation, Trigger, What};
use super::goals::social::{self, ChatEvent, Social};
use super::goals::{FailReason, Goal, GoalStatus};
use super::guild::profession::{self, ServiceTime};
use super::party::{PartyOrder, PartyPurpose};
use super::state::UnitKind;
use crate::config::BehaviorCfg;
use crate::roster::Temperament;

/// A failed goal is not picked again for this long.
pub const FAIL_COOLDOWN_MS: u64 = 60_000;
/// Hit points (percent) below which a hurt bot rests instead of carrying on.
const HURT_PCT: u8 = 30;
/// Pause between two goals.
const GAP_MS: (u64, u64) = (800, 4_000);
/// A forced goal that keeps failing is dropped after this many failures in a row.
const MAX_FORCED_FAILS: u8 = 3;
/// How long the answer of the eligibility predicates ([`gate`]) is reused. A level-up asks again
/// at once.
pub const GATE_TTL_MS: u64 = 30_000;

/// The goals the bot cannot make anything of right now, as a bit per [`GoalKind::index`]. Every
/// entry is a cheap predicate the goal's module exports; a goal without one is never gated.
pub fn gate(ctx: &Ctx<'_>) -> GoalMask {
    let mut mask = 0;
    if super::goals::trade_run::unavailable(ctx).is_some() {
        mask |= 1 << GoalKind::TradeRun.index();
    }
    if super::goals::housing::unavailable(ctx).is_some() {
        mask |= 1 << GoalKind::Housing.index();
    }
    mask
}

/// What the scoring looks at.
#[derive(Debug, Clone, Copy)]
pub struct ScoreInput {
    /// Hit points in percent.
    pub hp_pct: u8,
    /// Mana in percent.
    pub mp_pct: u8,
    /// A fair monster is in view or has a known spawn close by.
    pub monsters_near: bool,
    /// The goal that ended last.
    pub last: Option<GoalKind>,
    /// Goals that must not be picked (stubs, cooling down), as a bit per [`GoalKind::index`].
    pub blocked: GoalMask,
}

/// The configured weight of every goal scaled by the temperament, in [`GoalKind::index`] order.
pub fn base_weights(b: &BehaviorCfg, t: &Temperament) -> [f32; GoalKind::COUNT] {
    let mut w = [0.0; GoalKind::COUNT];
    w[GoalKind::Quest.index()] = b.quest * (0.4 + 1.2 * t.quester);
    w[GoalKind::Grind.index()] = b.grind * (0.7 + 0.6 * (1.0 - t.quester));
    w[GoalKind::Duel.index()] = b.duel * (0.2 + 1.6 * t.duelist);
    w[GoalKind::Trade.index()] = b.trade * (0.2 + 1.6 * t.trader);
    w[GoalKind::TradeRun.index()] = b.trade_run * (0.2 + 1.6 * t.merchant);
    w[GoalKind::Housing.index()] = b.housing * (0.4 + 1.2 * t.homebody);
    w[GoalKind::Rest.index()] = b.rest * (0.5 + t.homebody);
    w[GoalKind::Wander.index()] = b.wander * (0.6 + 0.8 * t.homebody);
    // (the travel, market, guild, pvp, event, party and farming goals own their weights, see
    // `own_scores`)
    w[GoalKind::Dungeon.index()] = super::goals::dungeon::base_weight(b, t);
    w[GoalKind::Sail.index()] = super::goals::sail::base_weight(b, t);
    w[GoalKind::Auction.index()] = super::goals::auction::base_weight(b, t);
    w[GoalKind::Market.index()] = super::goals::market::base_weight(b, t);
    w[GoalKind::Guild.index()] = super::goals::guild::base_weight(b, t);
    w[GoalKind::Gank.index()] = super::goals::gank::base_weight(b, t);
    w[GoalKind::Warzone.index()] = super::goals::warzone::base_weight(b, t);
    w[GoalKind::Event.index()] = super::goals::event::base_weight(b, t);
    w[GoalKind::Companion.index()] = super::goals::companion::base_weight(b, t);
    w[GoalKind::Farm.index()] = super::goals::farm::base_weight(b, t);
    w[GoalKind::Explore.index()] = super::goals::explore::base_weight(b, t);
    w[GoalKind::Gear.index()] = super::goals::gear::base_weight(b, t);
    w
}

/// A goal's own `score` function (`goals::<name>::score`).
type OwnScore = fn(&Ctx<'_>, f32) -> f32;

/// Goals that score themselves from the whole tick context: the dungeons, sailing, auction,
/// market, guild, pvp, event, party and farming features own their eligibility (a party, a boat
/// scroll, goods to sell, the `behavior.dungeons`, `behavior.sailing`, `[market]`, `[guild]`,
/// `[pvp]`, `[events]`, `[party]` and `[farm]` switches), so the brain applies none of its factors
/// to them. A goal that is blocked (switched off, cooling down, gated) is not asked; a score that
/// is not a positive number is 0.
fn own_scores(
    ctx: &Ctx<'_>,
    base: &[f32; GoalKind::COUNT],
    blocked: GoalMask,
    s: &mut [f32; GoalKind::COUNT],
) {
    let dungeon = GoalKind::Dungeon.index();
    let sail = GoalKind::Sail.index();
    let auction = GoalKind::Auction.index();
    let market = GoalKind::Market.index();
    s[dungeon] = own_score(blocked, dungeon, || {
        super::goals::dungeon::score(ctx, base[dungeon])
    });
    s[sail] = own_score(blocked, sail, || super::goals::sail::score(ctx, base[sail]));
    s[auction] = own_score(blocked, auction, || {
        super::goals::auction::score(ctx, base[auction])
    });
    s[market] = own_score(blocked, market, || {
        super::goals::market::score(ctx, base[market])
    });
    // (the wave-one goals that are not built yet are stubs: 0 until their features exist)
    let wave_one: [(GoalKind, OwnScore); 9] = [
        (GoalKind::Guild, super::goals::guild::score),
        (GoalKind::Gank, super::goals::gank::score),
        (GoalKind::Warzone, super::goals::warzone::score),
        (GoalKind::Event, super::goals::event::score),
        (GoalKind::Companion, super::goals::companion::score),
        (GoalKind::Farm, super::goals::farm::score),
        (GoalKind::Explore, super::goals::explore::score),
        (GoalKind::Gear, super::goals::gear::score),
        (GoalKind::Siege, super::goals::siege::score),
    ];
    for (kind, score) in wave_one {
        let i = kind.index();
        s[i] = own_score(blocked, i, || score(ctx, base[i]));
    }
}

/// `raw()` unless goal `index` is in `blocked` or the value is not a positive number.
fn own_score(blocked: GoalMask, index: usize, raw: impl FnOnce() -> f32) -> f32 {
    if blocked & (1 << index) != 0 {
        return 0.0;
    }
    let v = raw();
    if v.is_finite() && v > 0.0 {
        v
    } else {
        0.0
    }
}

/// The score of every goal. A blocked goal or a weight of 0 scores 0.
pub fn scores(base: &[f32; GoalKind::COUNT], i: &ScoreInput) -> [f32; GoalKind::COUNT] {
    let hp = f32::from(i.hp_pct) / 100.0;
    let mp = f32::from(i.mp_pct) / 100.0;
    let mut s = *base;
    // The more hurt (or drained), the more the bot wants to rest; a healthy bot hardly rests.
    let need = (1.0 - hp).max((0.7 - mp).max(0.0));
    s[GoalKind::Rest.index()] *= if need < 0.05 { 0.1 } else { 1.0 + 6.0 * need };
    if hp < 0.5 {
        for g in [GoalKind::Grind, GoalKind::Quest, GoalKind::TradeRun] {
            s[g.index()] *= 0.1;
        }
    }
    if !i.monsters_near {
        s[GoalKind::Grind.index()] *= 0.5;
    }
    if let Some(last) = i.last {
        s[last.index()] *= 0.4;
    }
    for g in GoalKind::ALL {
        if i.blocked & (1 << g.index()) != 0 || !s[g.index()].is_finite() || s[g.index()] < 0.0 {
            s[g.index()] = 0.0;
        }
    }
    s
}

/// Draws a goal at random in proportion to `scores`; `None` when all are 0.
pub fn choose(scores: &[f32; GoalKind::COUNT], rng: &mut fastrand::Rng) -> Option<GoalKind> {
    let total: f32 = scores.iter().sum();
    if total <= 0.0 {
        return None;
    }
    let mut roll = rng.f32() * total;
    let mut last_nonzero = None;
    for g in GoalKind::ALL {
        let s = scores[g.index()];
        if s <= 0.0 {
            continue;
        }
        last_nonzero = Some(g);
        if roll < s {
            return Some(g);
        }
        roll -= s;
    }
    last_nonzero
}

/// Until when each goal rests after a failure, on the bot clock. Whole seconds in a `u32` (136
/// years): half the size of milliseconds in a `u64`. The table is allocated by the first failure
/// (one pointer until then), which the per-bot budget needs now that there are eighteen goals
/// (`size_of::<Bot>() < 3200` is asserted).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Cooldowns(Option<Box<[u32; GoalKind::COUNT]>>);

impl Cooldowns {
    /// `kind` rests until `until_ms` (rounded up to the next second).
    fn set(&mut self, kind: GoalKind, until_ms: u64) {
        let table = self.0.get_or_insert_with(|| Box::new([0; GoalKind::COUNT]));
        table[kind.index()] = u32::try_from(until_ms.div_ceil(1000)).unwrap_or(u32::MAX);
    }

    /// Whether `kind` is still resting at the bot clock `now_ms`.
    fn active(&self, kind: GoalKind, now_ms: u64) -> bool {
        self.0
            .as_ref()
            .is_some_and(|t| now_ms < u64::from(t[kind.index()]) * 1000)
    }
}

/// The chat duty and the director of one bot. Created at the first tick, so a bot that has not
/// started yet costs a pointer.
#[derive(Debug)]
struct Chat {
    social: Social,
    director: Director,
    /// When the agent API's view of the bot is refreshed next (bot clock).
    agent_next_ms: u64,
    guild_service: ServiceTime,
}

// The goals after Gear score themselves entirely from the live situation.
const STORED_BASE_WEIGHTS: usize = GoalKind::Gear as usize + 1;

/// Chooses and runs goals for one bot. See the module docs.
#[derive(Debug)]
pub struct Brain {
    base: [f32; STORED_BASE_WEIGHTS],
    goal: Option<Goal>,
    choice: GoalChoice,
    last: Option<GoalKind>,
    /// Goals that reported `NotImplemented`.
    disabled: GoalMask,
    /// Until when a failed goal is not picked.
    cooldown_until: Cooldowns,
    next_think_ms: u64,
    /// A goal to start at the next tick (operator command, pairing, recovery after death).
    queued: Option<(GoalKind, Option<PairOrder>)>,
    /// The arguments that go with the queued goal; set only by [`Brain::queue_with`] and dropped by
    /// every other way of queueing.
    args: Option<Box<GoalArgs>>,
    /// The party order that goes with a queued [`GoalKind::Dungeon`]; set only by
    /// [`BotCmd::Party`] and dropped by every other way of queueing.
    party: Option<Box<PartyOrder>>,
    forced_fails: u8,
    /// The last answer of [`gate`]: the bot clock it is good until, the level it was asked at and
    /// the goals it blocks.
    gated: Cell<(u64, u8, GoalMask)>,
    /// Chat and director state, see [`Chat`].
    ext: Option<Box<Chat>>,
}

impl Brain {
    /// A brain with the goal weights of `behavior` scaled by the bot's `temper`.
    pub fn new(behavior: &BehaviorCfg, temper: &Temperament) -> Brain {
        let weights = base_weights(behavior, temper);
        let mut base = [0.0; STORED_BASE_WEIGHTS];
        base.copy_from_slice(&weights[..STORED_BASE_WEIGHTS]);
        Brain {
            base,
            goal: None,
            choice: GoalChoice::Auto,
            last: None,
            disabled: 0,
            cooldown_until: Cooldowns::default(),
            next_think_ms: 0,
            queued: None,
            args: None,
            party: None,
            forced_fails: 0,
            gated: Cell::new((0, 0, 0)),
            ext: None,
        }
    }

    /// The chat and director state, created on first use.
    fn chat(&mut self, rng: &mut fastrand::Rng, director_interval_secs: u64) -> &mut Chat {
        self.ext.get_or_insert_with(|| {
            Box::new(Chat {
                social: Social::new(rng),
                director: Director::new(rng, director_interval_secs),
                agent_next_ms: 0,
                guild_service: ServiceTime::default(),
            })
        })
    }

    /// Whether a dungeon run is under way (it survives the bot's death inside the dungeon, so
    /// the resurrection must not queue a rest).
    pub fn in_dungeon_run(&self) -> bool {
        matches!(self.goal, Some(Goal::Dungeon(_)))
    }

    /// The running goal.
    pub fn goal(&self) -> Option<&Goal> {
        self.goal.as_ref()
    }

    /// The name of the running goal (`"-"` when none).
    pub fn goal_name(&self) -> &'static str {
        self.goal.as_ref().map_or("-", |g| g.kind().name())
    }

    /// What the operator chose.
    pub fn choice(&self) -> GoalChoice {
        self.choice
    }

    /// Whether goal `kind` is a stub that was found out.
    pub fn is_disabled(&self, kind: GoalKind) -> bool {
        self.disabled & (1 << kind.index()) != 0
    }

    /// Applies an operator or coordinator command. `Pause`, `Resume` and `Say` belong to the bot
    /// task and are ignored here.
    pub fn command(&mut self, cmd: &BotCmd) {
        match cmd {
            BotCmd::Goal(choice) => {
                self.choice = *choice;
                self.forced_fails = 0;
                // (somebody else decides now: the director's plan is dropped)
                self.drop_plan();
                match choice {
                    GoalChoice::Forced(kind) => self.queue(*kind, None),
                    GoalChoice::Auto => {}
                }
            }
            BotCmd::Pair(order) => {
                let kind = match order.kind {
                    PairKind::Duel => GoalKind::Duel,
                    PairKind::Trade => GoalKind::Trade,
                };
                self.queue(kind, Some(order.clone()));
            }
            BotCmd::Party(order) => {
                let kind = match order.purpose {
                    PartyPurpose::Dungeon { .. } => GoalKind::Dungeon,
                    PartyPurpose::Guild => GoalKind::Guild,
                    PartyPurpose::Field(_) => GoalKind::FieldParty,
                    PartyPurpose::Ambush { .. } => GoalKind::Gank,
                };
                self.queue(kind, None);
                self.party = Some(order.clone());
            }
            BotCmd::GoalWith { kind, args } => {
                if matches!(kind, GoalKind::FieldParty | GoalKind::Fish)
                    && self.goal.as_ref().is_some_and(|g| g.kind() == *kind)
                {
                    return;
                }
                self.forced_fails = 0;
                self.drop_plan();
                self.queue_with(*kind, args.clone());
            }
            BotCmd::Pause | BotCmd::Resume | BotCmd::Say(_) | BotCmd::Whisper { .. } => {}
            BotCmd::Profile(_) => {}
            BotCmd::DuelWith(_) | BotCmd::TradeWith(_) | BotCmd::ServerLogLine { .. } => {}
            // (the nav result belongs to the motion controller)
            BotCmd::PathReady { .. } => {}
        }
    }

    /// Queues `kind` to start at the next tick, replacing the running goal. Drops a party order
    /// that was waiting.
    pub fn queue(&mut self, kind: GoalKind, order: Option<PairOrder>) {
        self.queued = Some((kind, order));
        self.party = None;
        self.args = None;
    }

    /// Queues `kind` with `args` to start at the next tick, replacing the running goal (one time:
    /// afterwards the bot chooses again, or goes back to an operator's forced goal).
    pub fn queue_with(&mut self, kind: GoalKind, args: Box<GoalArgs>) {
        self.queue(kind, None);
        self.args = Some(args);
    }

    /// One brain tick.
    pub fn tick(&mut self, ctx: &mut Ctx<'_>) {
        let interval = ctx.cfg.llm.director_interval_secs;
        self.chat(&mut *ctx.rng, interval).social.tick(ctx);
        if ctx.state.me.dead {
            let base = (ctx.cfg.bots.move_speed, ctx.cfg.bots.walk_speed);
            super::mount::on_gone(ctx.state, ctx.motion, ctx.out, base);
            self.director_wake(ctx, Trigger::Died);
            // Dead bots drop their goal (a dungeon run goes on inside the dungeon); the bot task
            // asks for the resurrection.
            let keep = match self.goal.as_mut() {
                Some(Goal::Dungeon(g)) => g.on_dead(ctx),
                _ => false,
            };
            if !keep {
                self.end_goal(ctx, None);
            }
            return;
        }
        if !ctx.state.me.spawned {
            return;
        }
        // Payment mails of sold trade packs are collected whatever the bot is doing.
        super::goals::trade_run::collect_payments(ctx);
        // The same for the mail the auction house sends.
        super::goals::auction::duty(ctx);
        super::guild::treasury::duty(ctx);
        let combat = Combat::tick(ctx);
        // Mounts and gliders: get off for a fight, summon for a long trip, glide off a ledge.
        super::mount::duty(ctx);
        let now = ctx.now_ms;
        // A pack carrier (or a bot going back for a dropped one) does not take up anything new
        // but its trade run: no market deal, trade, party, event or duel.
        let pack_busy = super::goals::trade_run::pack_busy(ctx)
            || super::goals::fish::carries_catch(ctx)
            || self
                .goal
                .as_ref()
                .is_some_and(|g| g.kind() == GoalKind::Fish);

        self.director_step(ctx);
        self.publish_agent(ctx);
        if let Some((kind, order)) = self.queued.take() {
            let args = self.args.take();
            self.start_goal_with(ctx, kind, order, args);
        }
        // Market duty: negotiates in the trade chat whatever the bot does; a sale agreed there
        // starts the market goal (the meeting and the trade).
        if !pack_busy
            && super::goals::market::duty(ctx, self.goal.as_ref().map(Goal::kind))
            && !self.is_disabled(GoalKind::Market)
        {
            self.start_goal(ctx, GoalKind::Market, None);
        }
        // Trade requests no goal asked for are answered here; the house marker follows `houses`.
        // (a pack carrier declines: the run is not interrupted for a trade)
        let running = self.goal.as_ref().map(Goal::kind);
        let craft_trade = super::goals::work::customer::requested_trade(ctx);
        if craft_trade
            && !pack_busy
            && !ctx.combat.busy()
            && self.choice == GoalChoice::Auto
            && matches!(running, None | Some(GoalKind::Rest | GoalKind::Wander))
            && !self.is_disabled(GoalKind::Work)
            && !self.cooldown_until.active(GoalKind::Work, now)
            && self.guild_work_allowed(ctx, GoalKind::Work)
        {
            self.start_goal(ctx, GoalKind::Work, None);
        } else if !matches!(&self.goal, Some(Goal::Work(work)) if work.handles_trade())
            && super::goals::trade::unsolicited(
                ctx,
                running,
                self.base[GoalKind::Trade.index()],
                self.is_disabled(GoalKind::Trade) || pack_busy || craft_trade,
            )
        {
            self.start_goal(ctx, GoalKind::Trade, None);
        }
        super::goals::dungeon::unsolicited(ctx, self.goal.as_ref().map(Goal::kind));
        // Party duty: answers the invitations of real players, keeps a companion's membership in
        // order and reads the chat commands whatever the bot does; a bot that is in a real player's
        // party and runs nothing it must not drop turns to the companion goal.
        if !pack_busy
            && super::companion::duty(ctx, self.goal.as_ref().map(Goal::kind))
            && !matches!(self.choice, GoalChoice::Forced(_))
            && !self.is_disabled(GoalKind::Companion)
            && !self.cooldown_until.active(GoalKind::Companion, now)
        {
            self.start_goal(ctx, GoalKind::Companion, None);
        }
        if ctx.cfg.party.trade_escorts {
            let accompanying = self
                .goal
                .as_ref()
                .is_some_and(|g| g.kind() == GoalKind::Companion);
            let was_escorting = accompanying
                && self
                    .ext
                    .as_ref()
                    .is_some_and(|ext| ext.guild_service.active());
            let available = self.guild_service_available(now);
            let escorting = super::companion::escort::tick(ctx, accompanying, available);
            if escorting != was_escorting {
                let service = &mut self.chat(ctx.rng, interval).guild_service;
                if escorting {
                    service.start(now);
                } else {
                    service.finish(now, ctx.cfg.guild.work_percent);
                }
            }
        }
        // Guild duty: answers invitations, recruits, promotes and talks whatever the bot does.
        super::guild::duty(ctx, self.goal.as_ref().map(Goal::kind));
        // Equip duty: puts on the better piece of the bag whatever the bot does (not in a fight).
        // A pack carrier is left alone: a back-slot change could replace or drop the pack.
        if !pack_busy {
            super::gear::duty(ctx, self.goal.as_ref().map(Goal::kind));
        }
        super::goals::housing::sync_marker(ctx);
        // A rift is running and the bot is free enough to help: the event goal replaces the small
        // things (never an operator's choice, and not while the goal cools down after a failure).
        if !pack_busy
            && !matches!(self.choice, GoalChoice::Forced(_))
            && !self.is_disabled(GoalKind::Event)
            && !self.cooldown_until.active(GoalKind::Event, now)
            && super::goals::event::duty(ctx, self.goal.as_ref().map(Goal::kind))
        {
            self.start_goal(ctx, GoalKind::Event, None);
        }
        if !pack_busy
            && self.choice == GoalChoice::Auto
            && self.goal.as_ref().is_some_and(|g| {
                matches!(
                    g.kind(),
                    GoalKind::Companion | GoalKind::Wander | GoalKind::Rest
                )
            })
            && !self.cooldown_until.active(GoalKind::Delivery, now)
            && self.guild_work_allowed(ctx, GoalKind::Delivery)
            && super::goals::delivery::ready(ctx)
        {
            self.start_goal(ctx, GoalKind::Delivery, None);
        }
        if self.goal.is_none() && now >= self.next_think_ms {
            self.pick(ctx);
        } else if !pack_busy && self.hurt_and_idle(ctx) {
            self.start_goal(ctx, GoalKind::Rest, None);
        }
        // Portal duty: records book entries, steps a far trip by portal, hops through a gate. While
        // it casts or waits at a gate the goal waits too.
        let travelling = super::travel::duty(ctx, self.goal.as_ref().map(Goal::kind));
        let Some(goal) = self.goal.as_mut() else {
            return;
        };
        if travelling || (ctx.combat.busy() && !goal.owns_combat()) {
            // The controller is defending the bot, or a portal cast is under way; the goal waits.
            return;
        }
        match goal.tick(ctx, combat) {
            GoalStatus::Running => {}
            GoalStatus::Done => {
                self.forced_fails = 0;
                self.end_goal(ctx, None);
            }
            GoalStatus::Failed(why) => self.end_goal(ctx, Some(why)),
        }
    }

    /// A badly hurt bot that is not fighting and not resting yet.
    fn hurt_and_idle(&self, ctx: &Ctx<'_>) -> bool {
        ctx.state.me.hp_pct() < HURT_PCT
            && !ctx.combat.busy()
            && !matches!(self.choice, GoalChoice::Forced(_))
            && self.goal.as_ref().is_some_and(|g| {
                !matches!(
                    g.kind(),
                    GoalKind::Rest | GoalKind::Dungeon | GoalKind::Companion
                )
            })
    }

    fn start_goal(&mut self, ctx: &mut Ctx<'_>, kind: GoalKind, order: Option<PairOrder>) {
        self.start_goal_with(ctx, kind, order, None);
    }

    /// [`Brain::start_goal`] with the arguments the goal was queued with.
    fn start_goal_with(
        &mut self,
        ctx: &mut Ctx<'_>,
        kind: GoalKind,
        order: Option<PairOrder>,
        args: Option<Box<GoalArgs>>,
    ) {
        if args.is_some() && !self.guild_work_allowed(ctx, kind) {
            ctx.log.debug(format!(
                "guild work: {} unavailable during personal time",
                kind.name()
            ));
            return;
        }
        if self.goal.is_some() {
            self.end_goal(ctx, None);
        }
        let party = self.party.take();
        let forage_leader = kind == GoalKind::FieldParty
            && party.as_ref().is_some_and(|order| {
                order.is_leader(ctx.persona.id)
                    && order.purpose
                        == super::party::PartyPurpose::Field(super::party::FieldActivity::Forage)
            });
        let work_available = self.guild_service_available(ctx.now_ms);
        let territory_work =
            kind == GoalKind::Warzone && super::goals::warzone::territory_patrol_ready(ctx);
        let naval_work = kind == GoalKind::Sail && super::goals::sail::naval_ready(ctx);
        if (forage_leader || territory_work || naval_work) && !work_available {
            return;
        }
        let why = args.as_ref().and_then(|a| a.why.clone());
        let mut goal = Goal::with_args(kind, order, party, args);
        if let Goal::FieldParty(field) = &mut goal {
            field.permit_forage(work_available);
        }
        goal.start(ctx);
        match why {
            Some(why) => ctx.log.say(format!("goal: {} ({why})", kind.name())),
            None => ctx.log.say(format!("goal: {}", kind.name())),
        }
        let professional = profession::membership(ctx).is_some_and(|(_, profession, master)| {
            !master
                && (profession.goals().contains(&kind)
                    || kind == GoalKind::Delivery && super::goals::delivery::scouting(ctx))
        });
        if professional || forage_leader || territory_work || naval_work || kind == GoalKind::Siege
        {
            let interval = ctx.cfg.llm.director_interval_secs;
            self.chat(ctx.rng, interval).guild_service.start(ctx.now_ms);
        }
        self.goal = Some(goal);
    }

    fn end_goal(&mut self, ctx: &mut Ctx<'_>, why: Option<FailReason>) {
        let Some(mut goal) = self.goal.take() else {
            return;
        };
        let kind = goal.kind();
        let partner = match &goal {
            Goal::Duel(d) => d.order().map(|o| o.partner_name.clone()),
            Goal::Trade(t) => t.order().map(|o| o.partner_name.clone()),
            _ => None,
        };
        goal.stop(ctx);
        super::travel::on_goal_end(ctx);
        self.last = Some(kind);
        if let Some(ext) = self.ext.as_mut() {
            ext.guild_service
                .finish(ctx.now_ms, ctx.cfg.guild.work_percent);
            ext.director.note_goal(kind, why.is_none());
            if why.is_none() {
                match kind {
                    GoalKind::Duel => ext.social.note(ChatEvent::DuelDone(partner), ctx.now_ms),
                    GoalKind::Trade => ext.social.note(ChatEvent::TradeDone(partner), ctx.now_ms),
                    _ => {}
                }
            }
        }
        self.next_think_ms = ctx.now_ms + super::goals::jitter_ms(ctx.rng, GAP_MS.0, GAP_MS.1);
        let Some(why) = why else { return };
        match why {
            FailReason::NotImplemented => {
                self.disabled |= 1 << kind.index();
                ctx.log.debug(format!(
                    "goal {} is not implemented yet; switched off",
                    kind.name()
                ));
                if self.choice == GoalChoice::Forced(kind) {
                    ctx.log
                        .say(format!("goal {} is not implemented yet", kind.name()));
                    self.choice = GoalChoice::Auto;
                }
            }
            other => {
                ctx.log
                    .debug(format!("goal {} failed: {other:?}", kind.name()));
                self.cooldown_until.set(kind, ctx.now_ms + FAIL_COOLDOWN_MS);
                self.director_wake(ctx, Trigger::Failed(kind));
                if self.choice == GoalChoice::Forced(kind) {
                    self.forced_fails += 1;
                    if self.forced_fails >= MAX_FORCED_FAILS {
                        ctx.log
                            .say(format!("goal {} keeps failing; back to auto", kind.name()));
                        self.choice = GoalChoice::Auto;
                    }
                }
            }
        }
    }

    fn pick(&mut self, ctx: &mut Ctx<'_>) {
        let now = ctx.now_ms;
        if let GoalChoice::Forced(kind) = self.choice {
            if !self.is_disabled(kind) && !self.cooldown_until.active(kind, now) {
                self.start_goal(ctx, kind, None);
                return;
            }
            // The forced goal is cooling down: fill the time with something harmless.
            let filler = if ctx.state.me.hp_pct() < 90 {
                GoalKind::Rest
            } else {
                GoalKind::Wander
            };
            self.start_goal(ctx, filler, None);
            return;
        }
        if let Some((kind, args)) = self.next_step(ctx) {
            self.start_goal_with(ctx, kind, None, Some(args));
            return;
        }
        let s = self.scores_now(ctx);
        let kind = match choose(&s, ctx.rng) {
            Some(kind) => kind,
            // (everything but the run is blocked: wait out its cooldown without wandering off)
            None if super::goals::trade_run::pack_busy(ctx)
                || super::goals::fish::carries_catch(ctx) =>
            {
                GoalKind::Rest
            }
            None => GoalKind::Wander,
        };
        self.start_goal(ctx, kind, None);
    }

    /// The score of every goal for the situation of this tick: the one table both the utility
    /// choice and the director's eligibility read.
    fn scores_now(&self, ctx: &Ctx<'_>) -> [f32; GoalKind::COUNT] {
        let mut blocked = self.disabled | self.gated_goals(ctx);
        // (asked again here: the gate answer is cached, a pack can start or end in between)
        let pack_busy = super::goals::trade_run::pack_busy(ctx);
        let fish_busy = super::goals::fish::carries_catch(ctx);
        if pack_busy {
            blocked |= super::goals::trade_run::pack_block_mask();
        }
        if fish_busy {
            blocked |= !(1 << GoalKind::Fish.index());
        }
        if !self.guild_service_available(ctx.now_ms) {
            blocked |= 1 << GoalKind::Siege.index();
            if super::goals::warzone::territory_patrol_ready(ctx) {
                blocked |= 1 << GoalKind::Warzone.index();
            }
            if super::goals::sail::naval_ready(ctx) {
                blocked |= 1 << GoalKind::Sail.index();
            }
        }
        for g in GoalKind::ALL {
            if self.cooldown_until.active(g, ctx.now_ms) {
                blocked |= 1 << g.index();
            }
        }
        let input = ScoreInput {
            hp_pct: ctx.state.me.hp_pct(),
            mp_pct: ctx.state.me.mp_pct(),
            monsters_near: monsters_near(ctx),
            last: self.last,
            blocked,
        };
        let mut base = [0.0; GoalKind::COUNT];
        base[..STORED_BASE_WEIGHTS].copy_from_slice(&self.base);
        let mut s = scores(&base, &input);
        own_scores(ctx, &base, blocked, &mut s);
        let work = GoalKind::Work.index();
        s[work] = own_score(blocked, work, || super::goals::work::score(ctx));
        let fish = GoalKind::Fish.index();
        s[fish] = own_score(blocked, fish, || super::goals::fish::score(ctx));
        let delivery = GoalKind::Delivery.index();
        s[delivery] = own_score(blocked, delivery, || {
            if self.guild_work_allowed(ctx, GoalKind::Delivery)
                && super::goals::delivery::ready(ctx)
            {
                10.0
            } else {
                0.0
            }
        });
        let field = GoalKind::FieldParty.index();
        s[field] = own_score(blocked, field, || {
            if super::goals::field::ready(ctx) {
                0.3 + 0.8 * super::goals::field::cooperation(ctx)
            } else {
                0.0
            }
        });
        // the population director's goal multipliers (all 1.0 unless its policy sets some)
        ctx.hub.pop.bias.apply(&mut s);
        if let Some((_, role, master)) = profession::membership(ctx) {
            let available = !master && self.guild_service_available(ctx.now_ms);
            for kind in role.goals() {
                if !(pack_busy && *kind == GoalKind::TradeRun
                    || fish_busy && *kind == GoalKind::Fish)
                {
                    s[kind.index()] *= if available { 2.0 } else { 0.0 };
                }
            }
        }
        s
    }

    fn guild_service_available(&self, now: u64) -> bool {
        self.ext
            .as_ref()
            .is_none_or(|ext| ext.guild_service.available(now))
    }

    fn guild_work_allowed(&self, ctx: &Ctx<'_>, kind: GoalKind) -> bool {
        if kind == GoalKind::TradeRun && super::goals::trade_run::pack_busy(ctx) {
            return true;
        }
        if kind == GoalKind::Fish && super::goals::fish::carries_catch(ctx) {
            return true;
        }
        if kind == GoalKind::Delivery && super::goals::delivery::scouting(ctx) {
            return self.guild_service_available(ctx.now_ms);
        }
        if kind == GoalKind::Siege {
            return self.guild_service_available(ctx.now_ms);
        }
        if kind == GoalKind::Sail && super::goals::sail::naval_ready(ctx) {
            return self.guild_service_available(ctx.now_ms);
        }
        if kind == GoalKind::Warzone && super::goals::warzone::territory_patrol_ready(ctx) {
            return self.guild_service_available(ctx.now_ms);
        }
        profession::membership(ctx).is_none_or(|(_, profession, master)| {
            !profession.goals().contains(&kind)
                || (!master && self.guild_service_available(ctx.now_ms))
        })
    }

    fn guild_work_context(&self, ctx: &Ctx<'_>) -> Option<String> {
        let siege = super::guild::siege::context(ctx);
        let Some((id, role, master)) = profession::membership(ctx) else {
            return siege;
        };
        let state = if master {
            "guildmaster"
        } else if self
            .ext
            .as_ref()
            .is_some_and(|ext| ext.guild_service.active())
        {
            "on guild duty"
        } else if self.guild_service_available(ctx.now_ms) {
            "available for guild work"
        } else {
            "personal time"
        };
        let mut context = format!("Guild {id}, {} profession, {state}.", role.title());
        if let Some(siege) = siege {
            context.push(' ');
            context.push_str(&siege);
        }
        if let Some(item) = profession::farm_order(ctx) {
            context.push_str(&format!(
                " The farm goal tends ripe crops and plants item {item}."
            ));
        }
        if role == profession::Profession::Potato && !master {
            let gardens = super::guild::leases::available(ctx.state, &ctx.cfg.guild).count();
            if gardens != 0 {
                context.push_str(&format!(
                    " The guild has {gardens} leased gardens available nearby."
                ));
            }
        }
        if role == profession::Profession::Potato && !master && ctx.cfg.farm.husbandry {
            let actions = ctx
                .hub
                .journal
                .read(ctx.persona.id, |j| j.livestock_actions);
            context.push_str(&format!(
                " Confirmed livestock care and collection actions: {actions}."
            ));
        }
        if let Some(order) = ctx.cfg.guild.production_for(id, ctx.state.me.char_id) {
            let (done, training) = ctx.hub.journal.read(ctx.persona.id, |j| {
                (j.production_completed, j.production_training_completed)
            });
            context.push_str(&format!(
                " Production order: {order}. Lifetime orders completed: {done}. Training crafts: {training}."
            ));
        }
        if let Some(order) = super::goals::work::customer::summary(ctx) {
            context.push_str(&order);
        }
        for (item, stock) in super::guild::recovery::stock(ctx) {
            let count = ctx.state.item_count(item);
            let name = ctx.world.item_name(item).unwrap_or("recovery items");
            context.push_str(&format!(" Recovery supplies: {count} of {stock} {name}."));
        }
        if let Some(recipe) = profession::trade_pack_order(ctx).and_then(|id| ctx.world.craft(id)) {
            let pack = ctx
                .world
                .item_name(recipe.product.0)
                .unwrap_or("trade pack");
            let missing = recipe
                .materials
                .iter()
                .filter(|&&(item, count)| ctx.state.item_count(item) < i64::from(count))
                .count();
            context.push_str(&format!(
                " The guild assigned recipe {} ({pack}). Missing ingredients: {missing} kinds.",
                recipe.id
            ));
        }
        if ctx.cfg.guild.treasuries.contains_key(&id.to_string()) {
            ctx.hub.journal.read(ctx.persona.id, |j| {
                if master {
                    let week = crate::hub::unix_ms() / super::guild::treasury::WEEK_MS;
                    let current = j.guild_treasury.get(&id).map_or(0, |a| a.current_contributions(week));
                    context.push_str(&format!(
                        " Guild contributors paid {} copper into the guildmaster's purse, including {current} copper this week.",
                        j.guild_copper_received
                    ));
                } else if let Some(a) = j.guild_treasury.get(&id) {
                    context.push_str(&format!(
                        " Unpaid guild commission and rent total {} copper. Guild rank allowances received: {} copper.",
                        a.unpaid_commission, a.support_received
                    ));
                    if a.support_supplies_spent > 0 {
                        context.push_str(&format!(" I spent {} of my rank allowance on recovery supplies.", a.support_supplies_spent));
                    }
                    if a.support_gear_spent > 0 {
                        context.push_str(&format!(" I spent {} of my rank allowance on gear.", a.support_gear_spent));
                    }
                    let pending = a.bounty_earned.saturating_sub(a.bounty_paid);
                    if pending > 0 {
                        context.push_str(&format!(" I earned {pending} copper in bounty rewards awaiting collection."));
                    }
                    if a.loan.principal > 0 {
                        context.push_str(&format!(
                            " I owe {} copper on my trade pack advance, including {} reserved for repayment.",
                            a.loan.principal, a.loan.repayment
                        ));
                    }
                }
            });
        }
        if role == profession::Profession::BountyHunter && !master {
            let completed = ctx
                .hub
                .journal
                .read(ctx.persona.id, |j| j.bounties_completed);
            context.push_str(&format!(" Confirmed bounty patrol kills: {completed}."));
            if let Some(case) = super::goals::warzone::bounty(ctx, |c| ctx.state.at_war(c.group)) {
                context.push_str(&format!(
                    " A trade runner reported {} in war zone group {}.",
                    case.name, case.group
                ));
            }
        }
        if role == profession::Profession::Trader {
            if ctx.cfg.sail.trade_passages {
                if let Some(buyer) = ctx.cfg.guild.voyage_buyers.get(&id.to_string()) {
                    context.push_str(&format!(" Priority sea shipment to pack buyer {buyer} when an owned boat and a safe route are available."));
                }
            }
            ctx.hub.journal.read(ctx.persona.id, |j| {
                if ctx.cfg.guild.auction_flips {
                    context.push_str(&format!(
                        " Collected auction resale profit: {} copper.",
                        j.auction_resale_profit
                    ));
                }
                let mut shown = smallvec::SmallVec::<[u32; 4]>::new();
                let now = crate::hub::unix_ms();
                for sale in &j.market_sales {
                    if shown.len() == 4 {
                        break;
                    }
                    if !shown.contains(&sale.item) {
                        if let Some(price) = ctx.hub.market_prices.average(sale.item, now) {
                            context.push_str(&format!(
                                " Recent sales of item {} averaged {:.1} copper per unit.",
                                sale.item, price
                            ));
                            shown.push(sale.item);
                        }
                    }
                }
            });
        }
        if super::goals::work::treasure::allowed(ctx) {
            let chests = ctx.hub.journal.read(ctx.persona.id, |j| j.treasure_chests);
            let maps = ctx
                .state
                .treasure
                .as_deref()
                .map_or(0, |t| t.coordinates.len());
            context.push_str(&format!(
                " Treasure chests looted: {chests}. Coordinate maps in my bag: {maps}."
            ));
        }
        if role == profession::Profession::Thief {
            ctx.hub.journal.read(ctx.persona.id, |j| {
                if ctx.cfg.sail.naval_raids {
                    context.push_str(&format!(
                        " Confirmed cannon hits on enemy ships: {}.",
                        j.naval_hits
                    ));
                }
                for raid in j.raid_sites.iter().take(3) {
                    let [x, y] = raid.position;
                    context.push_str(&format!(
                        " Previous ambushes near {x:.0}, {y:.0} recovered {} packs.",
                        raid.recovered
                    ));
                }
            });
            if super::goals::sail::naval_ready(ctx) {
                context.push_str(" I have an armed ship and Steel Ammo for a naval raid.");
            }
        }
        Some(context)
    }

    /// The goals [`gate`] blocks, asked again when the last answer is old or the level changed.
    fn gated_goals(&self, ctx: &Ctx<'_>) -> GoalMask {
        let (until, level, mask) = self.gated.get();
        let now = ctx.now_ms;
        if now < until && level == ctx.state.me.level {
            return mask;
        }
        let fresh = gate(ctx);
        if fresh != mask {
            for g in GoalKind::ALL {
                let bit = 1 << g.index();
                if (fresh ^ mask) & bit != 0 {
                    ctx.log.debug(format!(
                        "goal {}: {}",
                        g.name(),
                        if fresh & bit != 0 {
                            "cannot succeed now; left out of the choice"
                        } else {
                            "can be tried again"
                        }
                    ));
                }
            }
        }
        self.gated
            .set((now + GATE_TTL_MS, ctx.state.me.level, fresh));
        fresh
    }

    /// The goals the director may offer or apply right now.
    fn eligible(&self, ctx: &Ctx<'_>) -> director::Goals {
        director::eligible_goals(&self.scores_now(ctx), ctx.gm_allowed())
    }

    /// [`Brain::eligible`] with the scores, best first.
    fn eligible_scored(&self, ctx: &Ctx<'_>) -> director::Scored {
        director::eligible_scored(&self.scores_now(ctx), ctx.gm_allowed())
    }

    // ---- the director ------------------------------------------------------------------

    /// The director state, created with the chat state on first use.
    fn director(&mut self, rng: &mut fastrand::Rng, interval_secs: u64) -> &mut Director {
        &mut self.chat(rng, interval_secs).director
    }

    /// Forgets the director's plan (somebody else chose the bot's goal).
    fn drop_plan(&mut self) {
        if let Some(ext) = self.ext.as_mut() {
            ext.director.clear_plan();
        }
    }

    /// Tells the director that something happened, when it and its triggers are on.
    fn director_wake(&mut self, ctx: &Ctx<'_>, t: Trigger) {
        let llm = &ctx.cfg.llm;
        if !llm.director || !llm.director_triggers {
            return;
        }
        if let Some(ext) = self.ext.as_mut() {
            ext.director.wake(t);
        }
    }

    /// Looks at what happened, collects the model's answer and, when it is time, asks again.
    /// Never blocks: the request runs on its own task.
    fn director_step(&mut self, ctx: &mut Ctx<'_>) {
        if !ctx.cfg.llm.director || matches!(self.choice, GoalChoice::Forced(_)) {
            return;
        }
        let Some(llm) = ctx.hub.llm().cloned() else {
            return;
        };
        let cfg = &ctx.cfg.llm;
        let (now, interval) = (ctx.now_ms, cfg.director_interval_secs);
        let (level, pos) = (ctx.state.me.level, ctx.state.me.pos);
        let zone = ctx.world.zone_key_at(pos[0], pos[1]);
        let exploring = self.goal.as_ref().map(Goal::kind) == Some(GoalKind::Explore);
        let (hub, id) = (ctx.hub, ctx.persona.id);
        self.director(&mut *ctx.rng, interval)
            .observe(level, zone, exploring, |z| {
                hub.journal
                    .read(id, |j| (j.seen.has_zone(z), j.seen.zones.last().copied()))
            });
        if let Some(decided) = self.director(&mut *ctx.rng, interval).poll(now) {
            self.on_decision(ctx, decided);
        }
        let d = self.director(&mut *ctx.rng, interval);
        let Some(ask) = d.due(now, cfg.min_gap_ms(), cfg.director_triggers) else {
            return;
        };
        // Whatever comes of it, the next question is a gap (and an interval) away.
        d.asked(now, &mut *ctx.rng, interval);
        let recent: SmallVec<[(GoalKind, bool); 3]> = d.recent().into();
        let scored = self.eligible_scored(ctx);
        let menu = director::menu(ctx, &scored);
        if menu.len() < 2 {
            return;
        }
        let zone_name = |k: u16| ctx.world.zone_name(k).map(str::to_owned);
        let lines: Vec<String> = menu
            .iter()
            .map(|o| o.line(&zone_name, [pos[0], pos[1]]))
            .collect();
        let (notes, zones_seen) = hub.journal.read(id, |j| {
            (director::notes_shown(&j.notes), j.seen.zones.len())
        });
        let why_now = match ask {
            Ask::Trigger(t) => Some(t.text(&zone_name)),
            Ask::Timer => None,
        };
        let personality = if ctx.cfg.personality.enabled {
            ctx.persona.profile.describe()
        } else {
            String::new()
        };
        let current = self.goal.as_ref().map(Goal::kind);
        let me = &ctx.state.me;
        let characters_near = ctx
            .state
            .units
            .values()
            .filter(|u| u.kind == UnitKind::Character && u.dist2d(me.pos) <= 60.0)
            .count();
        let facts = social::facts(ctx);
        let mut situation = director::describe(&Situation {
            facts,
            level: me.level,
            hp_pct: me.hp_pct(),
            mp_pct: me.mp_pct(),
            money: me.money,
            current,
            recent: &recent,
            monsters_near: monsters_near(ctx),
            characters_near,
            real_player_near: ctx.real_player_near,
            personality: &personality,
            zones_seen,
            notes: &notes,
            why_now: why_now.as_deref(),
        });
        if let Some(guild) = self.guild_work_context(ctx) {
            situation.push_str(&guild);
            situation.push('\n');
        }
        if scored.iter().any(|&(goal, _)| goal == GoalKind::Fish) {
            use std::fmt::Write;
            hub.journal.read(id, |j| {
                let _ = writeln!(
                    situation,
                    "Sport fishing history: {} catches sold for {} copper in total.",
                    j.fish_turn_ins, j.fish_sale_copper
                );
            });
        }
        if scored.iter().any(|&(goal, _)| goal == GoalKind::TradeRun) {
            use std::fmt::Write;
            hub.journal.read(id, |j| {
                for route in j.trade_routes.iter().take(3) {
                    let costs = route.estimated_material_copper.map_or_else(String::new, |copper| format!(" Estimated replacement materials cost {copper} copper."));
                    let _ = writeln!(situation,
                        "Trade route by {}, item {} to buyer {}. {} sold, {} failed, last gross quote {} copper.{costs}",
                        if route.sea { "sea" } else { "land" }, route.item, route.buyer, route.sales, route.failed_runs, route.quoted_copper);
                }
            });
        }
        let system = director::system_prompt(&facts);
        let steps = ctx.cfg.llm.plan_steps();
        self.director(&mut *ctx.rng, interval).ask(AskRequest {
            now_ms: now,
            llm,
            system,
            situation,
            offered: menu,
            lines,
            steps,
        });
    }

    /// The model answered: keep its note, keep its steps as the plan, and start the first step
    /// now if the bot is between things (else it waits for the next goal change).
    fn on_decision(&mut self, ctx: &mut Ctx<'_>, d: Decided) {
        if let Some(note) = d.note.as_deref().filter(|_| ctx.cfg.llm.director_notes) {
            let at = crate::hub::unix_ms() / 1000;
            if let Some(kept) = ctx
                .hub
                .journal
                .update(ctx.persona.id, |j| j.notes.push_clean(at, note))
            {
                ctx.log.debug(format!("director note: {kept}"));
            }
        }
        let zone_name = |k: u16| ctx.world.zone_name(k).map(str::to_owned);
        let labels: Vec<String> = d.steps.iter().map(|s| s.label(&zone_name)).collect();
        ctx.log.say(format!(
            "director: {} ({})",
            labels.join(", then "),
            d.reason
        ));
        let (now, interval) = (ctx.now_ms, ctx.cfg.llm.director_interval_secs);
        self.director(&mut *ctx.rng, interval)
            .set_plan(&d.steps, now);
        let running = self.goal.as_ref().map(Goal::kind);
        let between = running.is_none_or(|k| matches!(k, GoalKind::Wander | GoalKind::Rest));
        if between && self.queued.is_none() {
            if let Some((kind, args)) = self.next_step(ctx) {
                // (a plain stroll or rest the bot is already on goes on as it is)
                let plain = args.dest.is_none() && args.zone.is_none() && !args.wild;
                if running != Some(kind) || !plain {
                    self.queue_with(kind, args);
                }
            }
        }
    }

    /// The next step of the director's plan that may still start (the ones that may not are
    /// dropped), with its arguments; counted as a director pick. Wakes the director when the
    /// plan runs out.
    fn next_step(&mut self, ctx: &mut Ctx<'_>) -> Option<(GoalKind, Box<GoalArgs>)> {
        let now = ctx.now_ms;
        if !self.ext.as_ref().is_some_and(|e| e.director.has_plan(now)) {
            return None;
        }
        let eligible = self.eligible(ctx);
        let wild_ok = director::wild_ok(ctx);
        let (hub, id) = (ctx.hub, ctx.persona.id);
        let zone_name = |k: u16| ctx.world.zone_name(k).map(str::to_owned);
        let ext = self.ext.as_mut()?;
        let mut found = None;
        while let Some((step, n, total)) = ext.director.pop_step(now) {
            let seen = |z| hub.journal.read(id, |j| j.seen.has_zone(z));
            let at_war =
                matches!(step.what, What::Point { at, .. } if ctx.in_war_zone(at[0], at[1]));
            if !at_war && director::step_allowed(&step, &eligible, seen, wild_ok) {
                found = Some((step, n, total));
                break;
            }
            ctx.log.debug(format!(
                "director: {} is no longer allowed; dropped",
                step.label(&zone_name)
            ));
        }
        if !ext.director.has_plan(now) && ctx.cfg.llm.director_triggers {
            ext.director.wake(Trigger::PlanDone);
        }
        let (step, n, total) = found?;
        ctx.hub
            .chat
            .gate
            .stats
            .director_picks
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Some((step.kind, step.args(&format!("director {n}/{total}"))))
    }

    /// Refreshes what the agent API shows about the bot: the goals it may start now, the
    /// director's plan, what is around it (every [`crate::agent::AGENT_EVERY_MS`]).
    fn publish_agent(&mut self, ctx: &mut Ctx<'_>) {
        let (now, interval) = (ctx.now_ms, ctx.cfg.llm.director_interval_secs);
        let chat = self.chat(&mut *ctx.rng, interval);
        if now < chat.agent_next_ms {
            return;
        }
        chat.agent_next_ms = now + crate::agent::AGENT_EVERY_MS;
        let plan: SmallVec<[director::Opt; director::MAX_PLAN]> =
            chat.director.plan().iter().copied().collect();
        let eligible = self
            .eligible(ctx)
            .iter()
            .fold(0 as GoalMask, |m, g| m | (1 << g.index()));
        let me = &ctx.state.me;
        let (mut characters, mut players) = (0u16, 0u16);
        for u in ctx.state.units.values() {
            if u.kind == UnitKind::Character && u.dist2d(me.pos) <= 60.0 {
                characters = characters.saturating_add(1);
                if !ctx.hub.is_bot_char(u.id) {
                    players = players.saturating_add(1);
                }
            }
        }
        let zone_name = |k: u16| ctx.world.zone_name(k).map(str::to_owned);
        let view = crate::agent::AgentSlot {
            at_unix_ms: crate::hub::unix_ms(),
            eligible,
            mp_pct: me.mp_pct(),
            monsters_near: monsters_near(ctx),
            characters_near: characters,
            players_near: players,
            plan: plan.iter().map(|s| s.label(&zone_name)).collect(),
            guild_work: self.guild_work_context(ctx),
        };
        ctx.log.with_slot(|g| g.agent = view);
    }
}

/// Whether there is something to hit: a fair monster in view, else a known spawn within 100 m.
fn monsters_near(ctx: &Ctx<'_>) -> bool {
    let me = ctx.state.me.pos;
    let (faction, level) = (ctx.state.me.faction, ctx.state.me.level);
    let in_view = ctx.state.units.values().any(|u| {
        u.dist2d(me) <= 70.0
            && super::combat::attackable(ctx.world, faction, u)
                .is_some_and(|t| t.grade <= 1 && super::goals::level_ok(level, t.level))
    });
    if in_view {
        return true;
    }
    let mut found = false;
    ctx.world.npc_spawns_within(
        aa_world::Vec3::new(me[0], me[1], me[2]),
        100.0,
        |template, _| {
            if found {
                return;
            }
            if let Some(t) = ctx.world.npc(template) {
                found = t.grade <= 1
                    && super::goals::level_ok(level, t.level)
                    && ctx.world.relation(faction, t.faction_id) == aa_world::Relation::Hostile;
            }
        },
    );
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(hp: u8) -> ScoreInput {
        ScoreInput {
            hp_pct: hp,
            mp_pct: 100,
            monsters_near: true,
            last: None,
            blocked: 0,
        }
    }

    fn base() -> [f32; GoalKind::COUNT] {
        base_weights(&BehaviorCfg::default(), &Temperament::NEUTRAL)
    }

    #[test]
    fn config_weights_reach_the_scores() {
        let b = BehaviorCfg {
            quest: 0.0,
            grind: 5.0,
            ..BehaviorCfg::default()
        };
        let w = base_weights(&b, &Temperament::NEUTRAL);
        assert_eq!(w[GoalKind::Quest.index()], 0.0);
        assert!(w[GoalKind::Grind.index()] > w[GoalKind::Wander.index()] * 4.0);
    }

    #[test]
    fn temperament_shifts_the_weights() {
        let b = BehaviorCfg::default();
        let duelist = Temperament {
            duelist: 1.0,
            ..Temperament::NEUTRAL
        };
        let peaceful = Temperament {
            duelist: 0.05,
            ..Temperament::NEUTRAL
        };
        let a = base_weights(&b, &duelist)[GoalKind::Duel.index()];
        let p = base_weights(&b, &peaceful)[GoalKind::Duel.index()];
        assert!(a > p * 4.0, "{a} vs {p}");
    }

    #[test]
    fn a_hurt_bot_rests_and_a_healthy_one_does_not() {
        let b = base();
        let hurt = scores(&b, &input(20));
        let well = scores(&b, &input(100));
        let top = |s: &[f32; GoalKind::COUNT]| {
            GoalKind::ALL
                .into_iter()
                .max_by(|a, b| s[a.index()].total_cmp(&s[b.index()]))
                .unwrap()
        };
        assert_eq!(top(&hurt), GoalKind::Rest);
        assert_ne!(top(&well), GoalKind::Rest);
        assert!(hurt[GoalKind::Grind.index()] < well[GoalKind::Grind.index()] * 0.2);
    }

    #[test]
    fn no_monsters_makes_grinding_less_likely() {
        let b = base();
        let mut i = input(100);
        let with = scores(&b, &i)[GoalKind::Grind.index()];
        i.monsters_near = false;
        let without = scores(&b, &i)[GoalKind::Grind.index()];
        assert!(without < with);
        assert!(without > 0.0);
    }

    #[test]
    fn blocked_goals_and_zero_weights_never_win() {
        let b = BehaviorCfg {
            wander: 0.0,
            ..BehaviorCfg::default()
        };
        let base = base_weights(&b, &Temperament::NEUTRAL);
        let mut i = input(100);
        i.blocked = (1 << GoalKind::Grind.index()) | (1 << GoalKind::Quest.index());
        let s = scores(&base, &i);
        assert_eq!(s[GoalKind::Grind.index()], 0.0);
        assert_eq!(s[GoalKind::Quest.index()], 0.0);
        assert_eq!(s[GoalKind::Wander.index()], 0.0);
        let mut rng = fastrand::Rng::with_seed(7);
        for _ in 0..2000 {
            let g = choose(&s, &mut rng).unwrap();
            assert!(![GoalKind::Grind, GoalKind::Quest, GoalKind::Wander].contains(&g));
        }
        assert_eq!(choose(&[0.0; GoalKind::COUNT], &mut rng), None);
    }

    #[test]
    fn gated_goals_are_never_drawn_and_their_bits_fit_the_mask() {
        const _: () = assert!(GoalKind::COUNT <= GoalMask::BITS as usize);
        let b = base();
        let mut i = input(100);
        i.blocked = (1 << GoalKind::TradeRun.index()) | (1 << GoalKind::Housing.index());
        let s = scores(&b, &i);
        assert_eq!(s[GoalKind::TradeRun.index()], 0.0);
        assert_eq!(s[GoalKind::Housing.index()], 0.0);
        let mut rng = fastrand::Rng::with_seed(11);
        for _ in 0..5000 {
            let g = choose(&s, &mut rng).unwrap();
            assert!(g != GoalKind::TradeRun && g != GoalKind::Housing, "{g:?}");
        }
        // without the gate both are drawn now and then
        i.blocked = 0;
        let s = scores(&b, &i);
        let mut drawn = |kind| (0..5000).any(|_| choose(&s, &mut rng) == Some(kind));
        assert!(drawn(GoalKind::TradeRun) && drawn(GoalKind::Housing));
    }

    #[test]
    fn a_pack_carrier_is_only_offered_the_trade_run() {
        let mask = super::super::goals::trade_run::pack_block_mask();
        for g in GoalKind::ALL {
            let blocked = mask & (1 << g.index()) != 0;
            assert_eq!(blocked, g != GoalKind::TradeRun, "{g:?}");
        }
        // with that mask the draw can only land on the trade run
        let b = [1.0; GoalKind::COUNT];
        let mut i = input(100);
        i.blocked = mask;
        let s = scores(&b, &i);
        let mut rng = fastrand::Rng::with_seed(17);
        for _ in 0..2_000 {
            assert_eq!(choose(&s, &mut rng), Some(GoalKind::TradeRun));
        }
    }

    #[test]
    fn the_same_goal_twice_in_a_row_is_less_likely() {
        let b = base();
        let mut i = input(100);
        let fresh = scores(&b, &i);
        i.last = Some(GoalKind::Wander);
        let after = scores(&b, &i);
        assert!(
            (after[GoalKind::Wander.index()] - fresh[GoalKind::Wander.index()] * 0.4).abs() < 1e-5
        );
        assert_eq!(
            after[GoalKind::Grind.index()],
            fresh[GoalKind::Grind.index()]
        );
    }

    #[test]
    fn the_draw_follows_the_scores() {
        let mut s = [0.0; GoalKind::COUNT];
        s[GoalKind::Grind.index()] = 3.0;
        s[GoalKind::Wander.index()] = 1.0;
        let mut rng = fastrand::Rng::with_seed(42);
        let (mut grind, mut wander) = (0u32, 0u32);
        for _ in 0..10_000 {
            match choose(&s, &mut rng).unwrap() {
                GoalKind::Grind => grind += 1,
                GoalKind::Wander => wander += 1,
                other => panic!("{other:?} has no score"),
            }
        }
        let ratio = f64::from(grind) / f64::from(wander);
        assert!((2.6..3.4).contains(&ratio), "ratio {ratio}");
    }

    #[test]
    fn commands_queue_goals_and_pairings() {
        let mut brain = Brain::new(&BehaviorCfg::default(), &Temperament::NEUTRAL);
        assert_eq!(brain.choice(), GoalChoice::Auto);
        brain.command(&BotCmd::Goal(GoalChoice::Forced(GoalKind::Rest)));
        assert_eq!(brain.choice(), GoalChoice::Forced(GoalKind::Rest));
        assert!(matches!(brain.queued, Some((GoalKind::Rest, None))));
        brain.command(&BotCmd::Goal(GoalChoice::Auto));
        assert_eq!(brain.choice(), GoalChoice::Auto);
        let order = PairOrder {
            kind: PairKind::Trade,
            role: crate::bot::cmd::PairRole::Initiator,
            partner_bot: 2,
            partner_char: 20,
            partner_name: "Other".into(),
            meet_at: (1.0, 2.0),
        };
        brain.command(&BotCmd::Pair(order.clone()));
        match &brain.queued {
            Some((GoalKind::Trade, Some(o))) => assert_eq!(o, &order),
            other => panic!("{other:?}"),
        }
        assert_eq!(brain.goal_name(), "-");
        assert!(!brain.is_disabled(GoalKind::Quest));
    }

    #[test]
    fn the_travel_goals_are_never_drawn_by_weight() {
        // the sailing stub's weight is 0, so the table alone never picks it; the dungeon goal's
        // weight is its temperament formula (the goal scores itself on top, see `own_scores`) ...
        let w = base();
        assert!(w[GoalKind::Dungeon.index()] > 0.0);
        assert_eq!(w[GoalKind::Sail.index()], 0.0);
        // ... the other goals' numbers are what they were
        assert_eq!(w[GoalKind::Quest.index()], 1.0 * (0.4 + 1.2 * 0.5));
        assert_eq!(w[GoalKind::Wander.index()], 0.5 * (0.6 + 0.8 * 0.5));
        let s = scores(&w, &input(100));
        assert_eq!(s[GoalKind::Dungeon.index()], w[GoalKind::Dungeon.index()]);
        assert_eq!(s[GoalKind::Sail.index()], 0.0);
        let mut rng = fastrand::Rng::with_seed(5);
        for _ in 0..5000 {
            let g = choose(&s, &mut rng).unwrap();
            assert!(g != GoalKind::Sail, "{g:?}");
        }
    }

    #[test]
    fn the_market_goals_are_never_drawn_by_weight_until_their_features_exist() {
        let w = base();
        let s = scores(&w, &input(100));
        for g in [GoalKind::Auction, GoalKind::Market] {
            assert_eq!(w[g.index()], 0.0, "{g:?}");
            assert_eq!(s[g.index()], 0.0, "{g:?}");
        }
        let mut rng = fastrand::Rng::with_seed(9);
        for _ in 0..5000 {
            let g = choose(&s, &mut rng).unwrap();
            assert!(g != GoalKind::Auction && g != GoalKind::Market, "{g:?}");
        }
        // forcing one from the web UI queues it, and its stub switches it off for the bot
        let mut brain = Brain::new(&BehaviorCfg::default(), &Temperament::NEUTRAL);
        brain.command(&BotCmd::Goal(GoalChoice::Forced(GoalKind::Market)));
        assert!(matches!(brain.queued, Some((GoalKind::Market, None))));
    }

    #[test]
    fn the_wave_one_goals_are_never_drawn_by_weight_until_their_features_exist() {
        let wave_one = [
            GoalKind::Guild,
            GoalKind::Gank,
            GoalKind::Warzone,
            GoalKind::Event,
            GoalKind::Companion,
            GoalKind::Farm,
            GoalKind::Explore,
            GoalKind::Gear,
        ];
        let w = base();
        let s = scores(&w, &input(100));
        for g in wave_one {
            assert_eq!(w[g.index()], 0.0, "{g:?}");
            assert_eq!(s[g.index()], 0.0, "{g:?}");
        }
        let mut rng = fastrand::Rng::with_seed(13);
        for _ in 0..5000 {
            let g = choose(&s, &mut rng).unwrap();
            assert!(!wave_one.contains(&g), "{g:?}");
        }
        // forcing one from the web UI queues it, and its stub switches it off for the bot
        for g in wave_one {
            let mut brain = Brain::new(&BehaviorCfg::default(), &Temperament::NEUTRAL);
            brain.command(&BotCmd::Goal(GoalChoice::Forced(g)));
            assert!(matches!(brain.queued, Some((k, None)) if k == g), "{g:?}");
            assert!(!brain.is_disabled(g));
        }
    }

    #[test]
    fn the_goal_masks_hold_every_goal() {
        // every goal has its own bit, including the last one
        let mut all: GoalMask = 0;
        for g in GoalKind::ALL {
            all |= 1 << g.index();
        }
        assert_eq!(all.count_ones() as usize, GoalKind::COUNT);
        let last = GoalKind::ALL[GoalKind::COUNT - 1];
        assert_eq!(own_score(1 << last.index(), last.index(), || 1.0), 0.0);
        assert_eq!(
            own_score(all & !(1 << last.index()), last.index(), || 1.0),
            1.0
        );
        let mut i = input(100);
        i.blocked = 1 << last.index();
        let b = [1.0; GoalKind::COUNT];
        assert_eq!(scores(&b, &i)[last.index()], 0.0);
        assert!(scores(&b, &i)[GoalKind::Quest.index()] > 0.0);
    }

    #[test]
    fn a_failed_goal_rests_for_a_minute_to_the_second() {
        let mut c = Cooldowns::default();
        for g in GoalKind::ALL {
            assert!(!c.active(g, 0), "{g:?}");
        }
        c.set(GoalKind::Quest, 1_000 + FAIL_COOLDOWN_MS);
        assert!(c.active(GoalKind::Quest, 0));
        assert!(c.active(GoalKind::Quest, 60_999));
        assert!(!c.active(GoalKind::Quest, 61_000));
        // rounded up, never short
        c.set(GoalKind::Duel, 1_500);
        assert!(c.active(GoalKind::Duel, 1_999));
        assert!(!c.active(GoalKind::Duel, 2_000));
        // the goals do not share a slot
        assert!(!c.active(GoalKind::Rest, 1_000));
        // far in the future saturates instead of wrapping
        c.set(GoalKind::Grind, u64::MAX);
        assert!(c.active(GoalKind::Grind, 1 << 40));
    }

    #[test]
    fn a_goal_that_scores_itself_is_blocked_or_cleaned_up_but_not_scaled() {
        let dungeon = GoalKind::Dungeon.index();
        // the feature's number comes through as it is
        assert_eq!(own_score(0, dungeon, || 2.5), 2.5);
        // not asked at all while switched off, cooling down or gated
        assert_eq!(
            own_score(1 << dungeon, dungeon, || panic!("asked while blocked")),
            0.0
        );
        // another goal's bit does not block it
        assert_eq!(own_score(1 << GoalKind::Sail.index(), dungeon, || 1.0), 1.0);
        // nonsense is 0
        for bad in [0.0, -1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(own_score(0, dungeon, || bad), 0.0, "{bad}");
        }
    }

    #[test]
    fn a_goal_with_arguments_is_queued_once_with_them() {
        let mut brain = Brain::new(&BehaviorCfg::default(), &Temperament::NEUTRAL);
        let args = GoalArgs {
            zone: Some(3),
            why: Some("director: new sights".into()),
            ..GoalArgs::default()
        };
        brain.command(&BotCmd::GoalWith {
            kind: GoalKind::Explore,
            args: Box::new(args.clone()),
        });
        assert!(matches!(brain.queued, Some((GoalKind::Explore, None))));
        assert_eq!(brain.args.as_deref(), Some(&args));
        // the operator's choice is not touched: it is a one-time order
        assert_eq!(brain.choice(), GoalChoice::Auto);
        // any other way of queueing drops the arguments
        brain.queue(GoalKind::Rest, None);
        assert!(brain.args.is_none());
    }

    #[test]
    fn a_guild_order_queues_the_guild_goal_with_its_order() {
        use crate::bot::party::PartyPurpose;
        let order = PartyOrder {
            party_id: 8,
            leader: 1,
            members: [1, 2, 3, 4, 5].into_iter().collect(),
            purpose: PartyPurpose::Guild,
        };
        let mut brain = Brain::new(&BehaviorCfg::default(), &Temperament::NEUTRAL);
        brain.command(&BotCmd::Party(Box::new(order.clone())));
        assert!(matches!(brain.queued, Some((GoalKind::Guild, None))));
        assert_eq!(brain.party.as_deref(), Some(&order));
        // the goal is made with the order
        let goal = Goal::with_party(GoalKind::Guild, None, brain.party.take());
        assert_eq!(goal.kind(), GoalKind::Guild);
    }

    #[test]
    fn a_party_order_queues_the_dungeon_goal_with_its_order() {
        use crate::bot::party::PartyPurpose;
        let order = PartyOrder {
            party_id: 4,
            leader: 1,
            members: [1, 2, 3].into_iter().collect(),
            purpose: PartyPurpose::Dungeon { zone_group: 9 },
        };
        let mut brain = Brain::new(&BehaviorCfg::default(), &Temperament::NEUTRAL);
        brain.command(&BotCmd::Party(Box::new(order.clone())));
        assert!(matches!(brain.queued, Some((GoalKind::Dungeon, None))));
        assert_eq!(brain.party.as_deref(), Some(&order));
        // any other way of queueing a goal drops the waiting order
        brain.command(&BotCmd::Goal(GoalChoice::Forced(GoalKind::Dungeon)));
        assert!(matches!(brain.queued, Some((GoalKind::Dungeon, None))));
        assert!(brain.party.is_none());
        brain.command(&BotCmd::Party(Box::new(order)));
        brain.queue(GoalKind::Rest, None);
        assert!(brain.party.is_none());
        // the nav answer is the motion controller's, not the brain's
        brain.command(&BotCmd::PathReady {
            ticket: 1,
            result: Box::new(aa_world::nav::PathResult::Unreachable),
        });
        assert!(matches!(brain.queued, Some((GoalKind::Rest, None))));
    }
}
