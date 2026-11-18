//! The market goal's glue: reads the tick into [`Facts`], steps the [`Machine`], carries out its
//! [`Act`]s and runs the trade goal inside the meeting.
//!
//! The goal has two modes. With a deal agreed in chat (the duty starts it then) it does the
//! meeting: walk to the town spot or through the island's portal, wait for the other side, trade
//! under the agreed [`Deal`], leave. Started by the operator without a deal it posts an
//! advertisement at once and listens for a few minutes, so a buyer's answer turns into a meeting.

use aa_proto::cs;

use super::machine::{Act, Facts, Machine, Phase, Problem, Status, Where};
use super::report::{self, Transfer};
use crate::bot::cmd::{PairKind, PairOrder, PairRole};
use crate::bot::ctx::Ctx;
use crate::bot::goals::trade::{Deal, Plan, Trade};
use crate::bot::goals::{FailReason, GoalStatus};
use crate::bot::market::ads::lines::{Meet, Speech};
use crate::bot::market::ads::talk::{Closed, Role};
use crate::bot::market::ads::{AdState, Agreed};
use crate::bot::market::needs::QuestNeeds;
use crate::bot::motion::{MotionStatus, Pace};
use crate::bot::state::{StateError, UnitKind};

/// Listening for buyers after an operator's order, without a deal.
const LISTEN_MS: u64 = 4 * 60_000;
/// How long after a trade the bag and purse are looked at.
const AUDIT_MS: u64 = 3_000;
/// Within this many metres of a door the skill reaches it.
const DOOR_M: f32 = 4.5;
/// Within this many metres of a meeting spot the bot is there.
const SPOT_M: f32 = 5.0;
/// Walks end this far from their target.
const STOP_M: f32 = 2.0;
/// Failed walks in a row before the bot gives up on the place.
const MAX_FAILS: u8 = 3;
/// The pause before the bot says it is there.
const HERE_DELAY_MS: u64 = 800;

/// What to compare after a trade.
#[derive(Debug, Clone, Copy)]
struct Audit {
    template: u32,
    qty: i64,
    total: i64,
    seller: bool,
    items_before: i64,
    money_before: i64,
}

/// The state of a running market goal.
#[derive(Debug, Default)]
pub struct Run {
    machine: Option<Machine>,
    deal: Option<Agreed>,
    trade: Option<Box<Trade>>,
    trade_result: Option<bool>,
    listen_until_ms: u64,
    cast_skill: u32,
    cast_doodad: u32,
    cast_ms: u64,
    fails: u8,
    audit: Option<Audit>,
    closed: Option<Closed>,
    closed_ms: u64,
}

fn flat(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt()
}

fn ads<'c>(ctx: &'c mut Ctx<'_>) -> &'c mut AdState {
    &mut ctx.state.market.inner_mut().ads
}

/// Units of `template` in the bag.
fn count(ctx: &Ctx<'_>, template: u32) -> i64 {
    ctx.state
        .bag
        .values()
        .filter(|i| i.template == template)
        .map(|i| i64::from(i.count))
        .sum()
}

/// The doodad of `template` in view nearest to the bot: object id and position.
fn doodad(ctx: &Ctx<'_>, template: u32) -> Option<(u32, [f32; 3])> {
    let me = ctx.state.me.pos;
    ctx.state
        .doodads
        .iter()
        .filter(|(_, d)| d.template == template)
        .map(|(&obj, d)| (obj, d.pos))
        .min_by(|a, b| flat(a.1, me).total_cmp(&flat(b.1, me)))
}

/// The character called `name` in view: object id, position, character id.
fn unit_named(ctx: &Ctx<'_>, name: &str) -> Option<(u32, [f32; 3], u32)> {
    ctx.state
        .units
        .iter()
        .find(|(_, u)| {
            u.kind == UnitKind::Character
                && u.name
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(name))
        })
        .map(|(&obj, u)| (obj, u.pos, u.id))
}

/// A short phrase for the log.
pub fn meet_word(meet: Meet) -> &'static str {
    match meet {
        Meet::Mirage => "Mirage Isle",
        Meet::Town { .. } => "town",
    }
}

impl Run {
    /// Called once when the brain picks the goal.
    pub fn start(&mut self, ctx: &mut Ctx<'_>) {
        if self.begin(ctx) {
            return;
        }
        // no deal: post an advertisement now and listen for the answer
        self.listen_until_ms = ctx.now_ms + LISTEN_MS;
        ads(ctx).post_now();
        ctx.log
            .say("market: no deal yet; advertising and listening for buyers");
    }

    fn begin(&mut self, ctx: &mut Ctx<'_>) -> bool {
        let Some(d) = ctx.state.market.get().and_then(|i| i.ads.deal().cloned()) else {
            return false;
        };
        ctx.log.say(format!(
            "market: meeting {} ({}) for {} x {}",
            d.peer,
            meet_word(d.meet),
            d.qty,
            ctx.world.item_name(d.template).unwrap_or("goods")
        ));
        self.machine = Some(Machine::new(d.role, d.meet, ctx.now_ms));
        self.deal = Some(d);
        self.fails = 0;
        true
    }

    /// One tick.
    pub fn tick(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let now = ctx.now_ms;
        if self.machine.is_none() && !self.begin(ctx) {
            return if now >= self.listen_until_ms {
                GoalStatus::Done
            } else {
                GoalStatus::Running
            };
        }
        if let Some(closed) = self.closed {
            return self.after(ctx, closed);
        }
        self.decline_strangers(ctx);
        let trade_end = self.tick_trade(ctx);
        let Some(mut m) = self.machine.take() else {
            return GoalStatus::Done;
        };
        let facts = self.facts(ctx, &m, trade_end);
        let step = m.step(&facts);
        for act in &step.acts {
            self.act(ctx, &m, *act);
        }
        if m.phase() != Phase::Trade && self.trade.is_some() {
            self.drop_trade(ctx);
        }
        let status = step.status;
        self.machine = Some(m);
        match status {
            Status::Running => GoalStatus::Running,
            Status::Over(closed) => self.finish(ctx, closed),
        }
    }

    /// Called when the goal ends or is interrupted.
    pub fn stop(&mut self, ctx: &mut Ctx<'_>) {
        self.drop_trade(ctx);
        if ctx.motion.is_moving() {
            ctx.motion.stop();
        }
        if self.closed.is_none() && self.deal.is_some() {
            // (interrupted in the middle of a meeting: the other side is told)
            let now = ctx.now_ms;
            let a = ads(ctx);
            a.tell_peer(Speech::Cancel, now, HERE_DELAY_MS);
            a.finish_deal(Closed::Cancelled);
            ctx.log.say("market: the meeting was interrupted");
        }
    }

    /// A line for the web UI.
    pub fn describe(&self, out: &mut String) {
        let Some(m) = self.machine.as_ref() else {
            out.push_str("market: advertising, listening for buyers");
            return;
        };
        let who = self.deal.as_ref().map_or("", |d| &*d.peer);
        let place = meet_word(m.meet());
        let verb = if m.role() == Role::Seller {
            "selling to"
        } else {
            "buying from"
        };
        let phase = match m.phase() {
            Phase::Go => "on the way to",
            Phase::Enter => "entering the portal to",
            Phase::Inside => "walking to the mailbox on",
            Phase::Wait => "waiting on",
            Phase::Trade => "trading on",
            Phase::Leave => "leaving",
            Phase::Over => "done with",
        };
        out.push_str(&format!("market: {verb} {who}, {phase} {place}"));
    }

    // ---- the trade ---------------------------------------------------------------------------

    /// Declines a trade request from anybody but the agreed character.
    fn decline_strangers(&mut self, ctx: &mut Ctx<'_>) {
        if self.trade.is_some() {
            return;
        }
        let Some((obj, _)) = ctx.state.trade.incoming else {
            return;
        };
        let peer_obj = self
            .deal
            .as_ref()
            .and_then(|d| unit_named(ctx, &d.peer))
            .map(|u| u.0);
        if peer_obj != Some(obj) {
            ctx.send(cs::cannot_start_trade(obj, 0));
            ctx.state.trade.incoming = None;
            ctx.log
                .say("market: declined a trade request from a stranger");
        }
    }

    fn tick_trade(&mut self, ctx: &mut Ctx<'_>) -> Option<bool> {
        if let Some(r) = self.trade_result {
            return Some(r);
        }
        let mut t = self.trade.take()?;
        let result = match t.tick(ctx) {
            GoalStatus::Running => None,
            GoalStatus::Done => Some(true),
            GoalStatus::Failed(_) => Some(false),
        };
        match result {
            None => self.trade = Some(t),
            Some(r) => {
                t.stop(ctx);
                self.trade_result = Some(r);
            }
        }
        result
    }

    fn drop_trade(&mut self, ctx: &mut Ctx<'_>) {
        if let Some(mut t) = self.trade.take() {
            t.stop(ctx);
        }
    }

    fn start_trade(&mut self, ctx: &mut Ctx<'_>) {
        let Some(d) = self.deal.clone() else { return };
        let total = d.total();
        let template = d.template;
        let qty = i64::from(d.qty);
        let seller = d.role == Role::Seller;
        let unit = unit_named(ctx, &d.peer);
        let money = i32::try_from(total).ok();
        // (a quest taken since the deal may need the goods: those are not for sale any more)
        let free = count(ctx, template)
            - i64::from(QuestNeeds::of(ctx.world, &ctx.state.quests).count(template));
        let plan = money.and_then(|money| {
            if seller {
                if free < qty {
                    return None;
                }
                let slot = ctx
                    .state
                    .bag
                    .iter()
                    .filter(|(s, i)| {
                        **s <= u16::from(u8::MAX)
                            && i.template == template
                            && i64::from(i.count) >= qty
                    })
                    .map(|(s, _)| *s)
                    .min()?;
                Some((
                    Plan {
                        slot: slot as u8,
                        template,
                        amount: i32::try_from(qty).ok()?,
                        money: 0,
                    },
                    money,
                ))
            } else {
                (ctx.state.me.money >= total).then_some((
                    Plan {
                        slot: 0,
                        template: 0,
                        amount: 0,
                        money,
                    },
                    0,
                ))
            }
        });
        let (Some((give, ask_money)), Some((_, pos, char_id))) = (plan, unit) else {
            ctx.log
                .say("market: the goods or the money are not there; the trade is off");
            self.trade_result = Some(false);
            let now = ctx.now_ms;
            ads(ctx).tell_peer(Speech::Cancel, now, HERE_DELAY_MS);
            return;
        };
        let items: &[(u32, i32)] = if seller {
            &[]
        } else {
            &[(template, i32::try_from(qty).unwrap_or(i32::MAX))]
        };
        let deal = Deal::agreed(give, items, ask_money);
        let order = PairOrder {
            kind: PairKind::Trade,
            role: if seller {
                PairRole::Responder
            } else {
                PairRole::Initiator
            },
            partner_bot: 0,
            partner_char: char_id,
            partner_name: d.peer.clone(),
            meet_at: (pos[0], pos[1]),
        };
        let mut t = Trade::for_deal(order, deal);
        t.start(ctx);
        self.audit = Some(Audit {
            template,
            qty,
            total,
            seller,
            items_before: count(ctx, template),
            money_before: ctx.state.me.money,
        });
        self.trade = Some(Box::new(t));
    }

    // ---- facts and acts ------------------------------------------------------------------------

    fn facts(&mut self, ctx: &Ctx<'_>, m: &Machine, trade_end: Option<bool>) -> Facts {
        let now = ctx.now_ms;
        let pos = ctx.state.me.pos;
        let mirage = ctx.world.mirage();
        let place = &ctx.state.place;
        let at = if !place.in_instance() {
            Where::Main
        } else if mirage.is_some_and(|x| u32::from(x.zone_key) == place.zone_key()) {
            Where::Mirage
        } else {
            Where::Other
        };
        let portal = mirage.and_then(|x| doodad(ctx, x.portal.doodad));
        let exit = mirage
            .and_then(|x| x.exit)
            .and_then(|(template, _, _)| doodad(ctx, template));
        let portal_ready = portal.is_some_and(|(_, p)| flat(pos, p) <= DOOR_M);
        let exit_ready = exit.is_some_and(|(_, p)| flat(pos, p) <= DOOR_M);
        let there = match (m.phase(), m.meet()) {
            (Phase::Go | Phase::Enter, Meet::Mirage) => {
                portal_ready
                    || mirage
                        .and_then(|x| x.nearest_portal(aa_world::Vec3::new(pos[0], pos[1], pos[2])))
                        .is_some_and(|p| flat(pos, [p.x, p.y, p.z]) <= SPOT_M)
            }
            (Phase::Go | Phase::Wait, Meet::Town { x, y }) => flat(pos, [x, y, 0.0]) <= SPOT_M,
            (Phase::Inside, _) => mirage
                .map(|x| x.meeting_point)
                .is_some_and(|p| flat(pos, [p.x, p.y, p.z]) <= SPOT_M),
            _ => false,
        };
        let peer = self.deal.as_ref().and_then(|d| unit_named(ctx, &d.peer));
        let peer_asked = match (ctx.state.trade.incoming, peer) {
            (Some((obj, at)), Some((pobj, _, _))) => obj == pobj && now.saturating_sub(at) < 60_000,
            _ => false,
        };
        // (a failed walk is tried again; the third one in a row ends the meeting)
        let failed = matches!(ctx.motion.status(), MotionStatus::Failed(_));
        if failed {
            self.fails = self.fails.saturating_add(1);
        } else if ctx.motion.is_moving() {
            self.fails = 0;
        }
        let stuck = failed && self.fails >= MAX_FAILS;
        let refused = self.cast_skill != 0
            && ctx.state.errors.iter().any(|&(t, e)| {
                t >= self.cast_ms
                    && matches!(e, StateError::Skill { skill, result }
                        if skill == self.cast_skill && result != 0)
            });
        Facts {
            now_ms: now,
            at,
            loading: place.loading(),
            there,
            stuck,
            portal_seen: portal.is_some(),
            portal_ready,
            exit_ready,
            peer_m: peer.map(|(_, p, _)| flat(pos, p)),
            peer_asked,
            trade_end,
            deal_alive: ads_alive(ctx),
            refused,
        }
    }

    fn act(&mut self, ctx: &mut Ctx<'_>, m: &Machine, act: Act) {
        let now = ctx.now_ms;
        let pos = ctx.state.me.pos;
        let mirage = ctx.world.mirage();
        match act {
            Act::ToSpot => {
                let target = match m.meet() {
                    Meet::Town { x, y } => Some((x, y)),
                    Meet::Mirage => mirage.map(|x| (x.meeting_point.x, x.meeting_point.y)),
                };
                if let Some((x, y)) = target {
                    let far = !ctx.state.place.in_instance();
                    walk(ctx, x, y, STOP_M, far);
                }
            }
            Act::ToPortal => {
                let spot = doodad(ctx, mirage.map_or(0, |x| x.portal.doodad))
                    .map(|(_, p)| (p[0], p[1]))
                    .or_else(|| {
                        mirage
                            .and_then(|x| {
                                x.nearest_portal(aa_world::Vec3::new(pos[0], pos[1], pos[2]))
                            })
                            .map(|p| (p.x, p.y))
                    });
                if let Some((x, y)) = spot {
                    walk(ctx, x, y, STOP_M, true);
                }
            }
            Act::ToExit => {
                let spot = mirage.and_then(|x| x.exit).map(|(_, p, _)| (p.x, p.y));
                if let Some((x, y)) = spot {
                    walk(ctx, x, y, STOP_M, false);
                }
            }
            Act::ToPeer => {
                let peer = self.deal.as_ref().and_then(|d| unit_named(ctx, &d.peer));
                if let Some((_, p, _)) = peer {
                    walk(ctx, p[0], p[1], 2.5, false);
                }
            }
            Act::Stop => {
                if ctx.motion.is_moving() {
                    ctx.motion.stop();
                }
            }
            Act::CastEnter => {
                if let Some(x) = mirage {
                    let skill = x.portal.skill;
                    self.cast(ctx, x.portal.doodad, skill);
                }
            }
            Act::CastExit => {
                if let Some((template, _, skill)) = mirage.and_then(|x| x.exit) {
                    self.cast(ctx, template, skill);
                }
            }
            Act::Announce => {
                ads(ctx).tell_peer(Speech::Here, now, HERE_DELAY_MS);
            }
            Act::StartTrade => self.start_trade(ctx),
            Act::Report(problem) => {
                let (skill, template) = match problem {
                    Problem::NeverLoaded { .. } | Problem::PortalMissing => (
                        mirage.map_or(0, |x| x.portal.skill),
                        mirage.map_or(0, |x| x.portal.doodad),
                    ),
                    Problem::NeverLeft { .. } => (
                        mirage.and_then(|x| x.exit).map_or(0, |e| e.2),
                        mirage.and_then(|x| x.exit).map_or(0, |e| e.0),
                    ),
                };
                let seen = if problem == Problem::PortalMissing {
                    near_doodads(ctx)
                } else {
                    String::new()
                };
                ctx.log
                    .say(format!("market: Mirage {problem:?} (skill {skill}) {seen}"));
                ctx.report_issue(report::mirage(problem, skill, template, &seen));
            }
        }
    }

    fn cast(&mut self, ctx: &mut Ctx<'_>, template: u32, skill: u32) {
        let Some((obj, pos)) = doodad(ctx, template) else {
            return;
        };
        if ctx.motion.is_moving() {
            ctx.motion.stop();
        }
        ctx.motion.face(pos[0], pos[1]);
        let skill = if skill != 0 {
            skill
        } else {
            ctx.world.doodad_use_skill(template)
        };
        let me = ctx.state.me.obj;
        ctx.send(cs::start_skill_doodad(skill, me, obj));
        self.cast_skill = skill;
        self.cast_doodad = template;
        self.cast_ms = ctx.now_ms;
        ctx.log
            .say(format!("market: casting skill {skill} on doodad {obj}"));
    }

    // ---- the end -------------------------------------------------------------------------------

    fn finish(&mut self, ctx: &mut Ctx<'_>, closed: Closed) -> GoalStatus {
        let now = ctx.now_ms;
        self.closed = Some(closed);
        self.closed_ms = now;
        {
            let a = ads(ctx);
            match closed {
                Closed::Done => a.tell_peer(Speech::Thanks, now, HERE_DELAY_MS),
                Closed::TimedOut => a.tell_peer(Speech::Cancel, now, HERE_DELAY_MS),
                _ => {}
            }
            a.finish_deal(closed);
        }
        ctx.log.say(format!("market: meeting over ({closed:?})"));
        if closed == Closed::Done && self.audit.is_some() {
            return GoalStatus::Running;
        }
        Self::status_of(closed)
    }

    fn after(&mut self, ctx: &mut Ctx<'_>, closed: Closed) -> GoalStatus {
        if ctx.now_ms.saturating_sub(self.closed_ms) < AUDIT_MS {
            return GoalStatus::Running;
        }
        if let Some(a) = self.audit.take() {
            self.check_transfer(ctx, a);
        }
        Self::status_of(closed)
    }

    fn status_of(closed: Closed) -> GoalStatus {
        match closed {
            Closed::Done => GoalStatus::Done,
            Closed::TimedOut => GoalStatus::Failed(FailReason::Timeout),
            _ => GoalStatus::Failed(FailReason::Other("the sale fell through")),
        }
    }

    /// Both sides confirmed: the bag and the purse must show it.
    fn check_transfer(&mut self, ctx: &mut Ctx<'_>, a: Audit) {
        let items = count(ctx, a.template) - a.items_before;
        let money = ctx.state.me.money - a.money_before;
        let (want_items, want_money) = if a.seller {
            (-a.qty, a.total)
        } else {
            (a.qty, -a.total)
        };
        let t = Transfer {
            template: a.template,
            want_items,
            got_items: items,
            want_money,
            got_money: money,
        };
        if t.is_complete() {
            if a.seller {
                use crate::bot::{cmd::GoalKind, guild::treasury};
                treasury::note_receipt(ctx, GoalKind::Market, a.total.max(0) as u64);
                if ctx.cfg.guild.enabled && !ctx.cfg.guild.professions.is_empty() {
                    let sale = crate::bot::guild::prices::Receipt {
                        item: a.template,
                        quantity: a.qty as u32,
                        copper: a.total as u32,
                        at_ms: crate::hub::unix_ms(),
                    };
                    ctx.hub.market_prices.remember(sale);
                    ctx.hub.journal.update(ctx.persona.id, |j| {
                        j.market_sales.insert(0, sale);
                        j.market_sales.truncate(24);
                    });
                }
            }
            ctx.log.say(format!(
                "market: the trade checks out ({items:+} items, {money:+} copper)"
            ));
        } else {
            ctx.log.say(format!(
                "market: the trade did not move what was agreed (items {items:+} of {want_items:+}, copper {money:+} of {want_money:+})"
            ));
            ctx.report_issue(report::transfer(t, a.seller));
        }
    }
}

/// The doodads within 30 m of the bot, as "count x template" for a report.
fn near_doodads(ctx: &Ctx<'_>) -> String {
    let me = ctx.state.me.pos;
    let mut by_template: Vec<(u32, u32)> = Vec::new();
    for d in ctx.state.doodads.values() {
        if flat(me, d.pos) > 30.0 {
            continue;
        }
        match by_template.iter_mut().find(|e| e.0 == d.template) {
            Some(e) => e.1 += 1,
            None => by_template.push((d.template, 1)),
        }
    }
    by_template.sort_unstable();
    if by_template.is_empty() {
        return "none within 30 m".into();
    }
    by_template
        .iter()
        .take(12)
        .map(|(t, n)| format!("{n} x {t}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether the ad state still holds the deal.
fn ads_alive(ctx: &Ctx<'_>) -> bool {
    ctx.state
        .market
        .get()
        .is_some_and(|i| i.ads.deal().is_some())
}

/// Starts a walk to (x, y) unless one to that place is under way. `far` allows the long-trip
/// logic of the main world; on the island the motion controller walks directly.
fn walk(ctx: &mut Ctx<'_>, x: f32, y: f32, radius: f32, far: bool) {
    let heading = ctx.motion.is_moving()
        && ctx
            .motion
            .destination()
            .is_some_and(|(dx, dy)| ((dx - x).powi(2) + (dy - y).powi(2)).sqrt() <= 3.0);
    if heading {
        return;
    }
    if far {
        ctx.travel(x, y, radius, Pace::Run);
    } else {
        let now = ctx.now_ms;
        ctx.motion.go_to(x, y, radius, Pace::Run, now);
    }
}
