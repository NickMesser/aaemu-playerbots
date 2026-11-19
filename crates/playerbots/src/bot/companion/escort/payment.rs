//! A completed escort's fee settles in an ordinary, exact native trade.

use serde::{Deserialize, Serialize};

use crate::bot::cmd::{GoalKind, PairKind, PairOrder, PairRole};
use crate::bot::ctx::Ctx;
use crate::bot::goals::trade::{Deal, Plan, Trade};
use crate::bot::goals::GoalStatus;
use crate::bot::guild::{
    profession::{membership, Profession},
    treasury,
};
use crate::bot::state::{dist3d, Unit, UnitKind};
use crate::hub::unix_ms;

use super::{mem_of, tell};

const COLLECTION_MS: u64 = 30 * 60_000;
const REQUEST_MS: u64 = 15_000;
const CONFIRM_MS: u64 = 4_000;
const TRADE_REACH_M: f32 = 5.0;

#[derive(Debug, PartialEq, Eq, Clone, Copy, Deserialize, Serialize)]
pub struct Invoice {
    guild: u32,
    pub(super) customer: u32,
    pub(super) copper: u32,
    expires_ms: u64,
}

impl Invoice {
    pub(super) fn new(guild: u32, customer: u32, copper: u32) -> Option<Self> {
        (copper != 0).then_some(Self {
            guild,
            customer,
            copper,
            expires_ms: unix_ms() + COLLECTION_MS,
        })
    }
}

pub(super) fn outstanding(ctx: &Ctx<'_>) -> Option<Invoice> {
    let invoice = ctx.hub.journal.read(ctx.persona.id, |j| j.escort_invoice)?;
    (unix_ms() < invoice.expires_ms
        && membership(ctx) == Some((invoice.guild, Profession::Adventurer, false)))
    .then_some(invoice)
}

fn nearby_customer<'a>(ctx: &'a Ctx<'_>, customer: u32) -> Option<&'a Unit> {
    ctx.state.units.values().find(|unit| {
        unit.kind == UnitKind::Character
            && unit.id == customer
            && !unit.dead()
            && dist3d(ctx.pos(), unit.pos) <= TRADE_REACH_M
    })
}

pub(super) fn request(ctx: &mut Ctx<'_>, customer: u32, name: &str) {
    let Some(invoice) = outstanding(ctx).filter(|invoice| invoice.customer == customer) else {
        tell(ctx, name, "You have no unpaid escort with me.");
        return;
    };
    if nearby_customer(ctx, customer).is_none() {
        tell(ctx, name, "Come a little closer to pay for the escort.");
        return;
    }
    if ctx.state.trade.window.is_some() {
        return;
    }
    mem_of(ctx.state).escort_payment_until = Some(ctx.now_ms + REQUEST_MS);
    tell(
        ctx,
        name,
        &format!(
            "I'll open a trade for the agreed {} copper.",
            invoice.copper
        ),
    );
}

#[derive(Debug)]
pub struct Payment {
    invoice: Invoice,
    name: Box<str>,
    trade: Trade,
    money_before: i64,
    confirm_until: Option<u64>,
}

impl Payment {
    pub fn start(ctx: &mut Ctx<'_>, leader: u32) -> Option<Self> {
        if !ctx.cfg.party.trade_escorts
            || ctx.state.place.in_instance()
            || ctx.state.trade.window.is_some()
        {
            return None;
        }
        mem_of(ctx.state)
            .escort_payment_until
            .take()
            .filter(|&until| ctx.now_ms < until)?;
        let invoice = outstanding(ctx).filter(|i| i.customer == leader)?;
        let customer = nearby_customer(ctx, leader)?;
        let [x, y, _] = customer.pos;
        let name = customer.name.clone()?;
        let due = i32::try_from(invoice.copper).ok()?;
        let deal = Deal::fixed(Plan::default(), &[], due);
        let mut trade = Trade::for_deal(
            PairOrder {
                kind: PairKind::Trade,
                role: PairRole::Initiator,
                partner_bot: 0,
                partner_char: leader,
                partner_name: name.clone(),
                meet_at: (x, y),
            },
            deal,
        );
        trade.start(ctx);
        Some(Self {
            invoice,
            name,
            trade,
            money_before: ctx.state.me.money,
            confirm_until: None,
        })
    }

    /// False when the window and any confirmation are finished.
    pub fn tick(&mut self, ctx: &mut Ctx<'_>, leader: u32) -> bool {
        if self.confirm_until.is_none() {
            if self.invoice.customer != leader || outstanding(ctx) != Some(self.invoice) {
                return false;
            }
            match self.trade.tick(ctx) {
                GoalStatus::Running => return true,
                GoalStatus::Done => {
                    self.confirm_until = Some(ctx.now_ms + CONFIRM_MS);
                    ctx.hub
                        .journal
                        .update(ctx.persona.id, |j| j.escort_invoice = None);
                }
                GoalStatus::Failed(_) => return false,
            }
        }
        let due = i64::from(self.invoice.copper);
        if ctx.state.me.money - self.money_before == due {
            ctx.hub.journal.update(ctx.persona.id, |j| {
                j.escort_fees_copper = j.escort_fees_copper.saturating_add(due as u64);
            });
            treasury::note_receipt(ctx, GoalKind::Companion, due as u64);
            ctx.log
                .say(format!("escort: collected {due} copper from {}", self.name));
            tell(ctx, &self.name, "Payment received. Thanks for hiring me.");
            return false;
        }
        self.confirm_until.is_some_and(|until| ctx.now_ms < until)
    }

    pub fn stop(&mut self, ctx: &mut Ctx<'_>) {
        self.trade.stop(ctx);
    }
}

pub fn summary(ctx: &Ctx<'_>) -> Option<String> {
    if !ctx.cfg.party.trade_escorts {
        return None;
    }
    let paid = ctx
        .hub
        .journal
        .read(ctx.persona.id, |j| j.escort_fees_copper);
    let mut context = if paid > 0 {
        format!(" I collected {paid} copper in confirmed escort fees.")
    } else {
        String::new()
    };
    if let Some(due) = outstanding(ctx) {
        context.push_str(&format!(" An Adventurer escort is complete with {} copper due from its leader. The leader can whisper 'escort pay' while nearby.", due.copper));
    }
    (!context.is_empty()).then_some(context)
}
