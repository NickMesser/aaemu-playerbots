//! Bid on an authored siege camp lot. Auction mail handles settlement.

use aa_proto::auction::{self as wire, Lot, Search};
use aa_world::Vec3;

use crate::bot::ctx::Ctx;
use crate::bot::guild::siege::{self, CampBid, CAMP_START_COPPER};
use crate::bot::pvp::walk::{Walk, Walker};
use crate::bot::state::dist3d;

use super::super::{auction, FailReason, GoalStatus};

const AUCTION_CATEGORY: u8 = 11;
const AUCTION_REACH_M: f32 = 4.0;
const REPLY_MS: u64 = 8_000;

#[derive(Debug)]
enum Phase {
    Walking,
    Search { object: u32, until: u64 },
    Bid { lot: u64, copper: i32, until: u64 },
}

#[derive(Debug)]
pub(super) struct Auction {
    order: CampBid,
    spot: auction::Spot,
    walker: Walker,
    phase: Phase,
}

impl Auction {
    pub(super) fn new(ctx: &Ctx<'_>) -> Option<Self> {
        let order = siege::camp_bid(ctx)?;
        let [x, y, z] = ctx.pos();
        let npc = ctx
            .world
            .nearest_auctioneer(Vec3::new(x, y, z), auction::MAX_TRIP_M, |_| false)?;
        let spot = auction::Spot {
            template: npc.template,
            pos: [npc.pos.x, npc.pos.y, npc.pos.z],
        };
        Some(Self {
            order,
            spot,
            walker: Walker::new(),
            phase: Phase::Walking,
        })
    }

    pub(super) fn group(&self) -> u16 {
        self.order.declaration.group
    }

    pub(super) fn tick(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        if let Phase::Bid { lot, copper, until } = self.phase {
            let bid = ctx.state.market.inner_mut().auction.inbox.bid.take();
            if bid.is_some_and(|a| {
                a.what.lot_id == lot
                    && a.what.money == copper
                    && !a.what.buyout
                    && a.what.template == self.order.declaration.camp
            }) {
                ctx.log.say(format!(
                    "siege: bid {copper} copper for the camp for territory {}",
                    self.group()
                ));
                return GoalStatus::Done;
            }
            return if ctx.now_ms >= until {
                GoalStatus::Failed(FailReason::Timeout)
            } else {
                GoalStatus::Running
            };
        }
        let Some(current) = siege::camp_bid(ctx).filter(|order| {
            order.cycle == self.order.cycle && order.declaration.group == self.group()
        }) else {
            return GoalStatus::Done;
        };
        match self.phase {
            Phase::Walking => {
                let npc = auction::find_npc(ctx.state, self.spot, ctx.pos());
                if let Some((object, _)) =
                    npc.filter(|(_, pos)| dist3d(ctx.pos(), *pos) <= AUCTION_REACH_M)
                {
                    ctx.motion.stop();
                    ctx.state.market.inner_mut().auction.inbox.page = None;
                    ctx.send(wire::search(
                        object,
                        object,
                        &Search {
                            categories: [AUCTION_CATEGORY, 0, 0],
                            ..Default::default()
                        },
                    ));
                    self.phase = Phase::Search {
                        object,
                        until: ctx.now_ms + REPLY_MS,
                    };
                } else {
                    let [x, y, _] = npc.map_or(self.spot.pos, |(_, pos)| pos);
                    if self.walker.step(ctx, [x, y], 2.0) != Walk::Going {
                        return GoalStatus::Failed(FailReason::NoTarget);
                    }
                }
            }
            Phase::Search { object, until } => {
                let page = ctx.state.market.inner_mut().auction.inbox.page.take();
                if let Some(page) = page {
                    let camp = self.order.declaration.camp;
                    let Some(lot) = page.what.lots.iter().find(|lot| is_camp(lot, camp)) else {
                        return GoalStatus::Failed(FailReason::NoTarget);
                    };
                    let character = ctx.state.me.char_id;
                    if lot.bidder_id == character {
                        self.note_attempt(ctx);
                        return GoalStatus::Done;
                    }
                    let Some(copper) = bid_price(lot, current.budget) else {
                        return GoalStatus::Failed(FailReason::Other(
                            "siege camp bid exceeds budget",
                        ));
                    };
                    let name = &ctx.persona.name;
                    let packet = wire::bid(object, object, lot, character, name, copper);
                    self.note_attempt(ctx);
                    ctx.state.market.inner_mut().auction.inbox.bid = None;
                    ctx.send(packet);
                    self.phase = Phase::Bid {
                        lot: lot.id,
                        copper,
                        until: ctx.now_ms + REPLY_MS,
                    };
                } else if ctx.now_ms >= until {
                    return GoalStatus::Failed(FailReason::Timeout);
                }
            }
            Phase::Bid { .. } => unreachable!(),
        }
        GoalStatus::Running
    }

    fn note_attempt(&self, ctx: &Ctx<'_>) {
        ctx.hub.journal.update(ctx.persona.id, |journal| {
            journal.siege_camp_bid_cycle = journal.siege_camp_bid_cycle.max(self.order.cycle);
        });
    }
}

fn is_camp(lot: &Lot, template: u32) -> bool {
    lot.id != 0
        && lot.template == template
        && lot.client_id == 0
        && lot.buyout == 0
        && lot.count == 1
        && lot.start >= CAMP_START_COPPER
}

fn bid_price(lot: &Lot, limit: i32) -> Option<i32> {
    let copper = lot.start.max(lot.bid_money.checked_add(1)?);
    (copper <= limit).then_some(copper)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bid_only_covers_an_exact_system_camp_within_budget() {
        let mut lot = Lot {
            id: 99,
            template: 21_135,
            count: 1,
            start: CAMP_START_COPPER,
            ..Default::default()
        };
        assert!(is_camp(&lot, 21_135));
        assert!(!is_camp(&lot, 21_136));
        assert_eq!(bid_price(&lot, CAMP_START_COPPER), Some(CAMP_START_COPPER));
        lot.bid_money = CAMP_START_COPPER;
        assert_eq!(bid_price(&lot, CAMP_START_COPPER), None);
        assert_eq!(
            bid_price(&lot, CAMP_START_COPPER + 1),
            Some(CAMP_START_COPPER + 1)
        );
        lot.client_id = 6;
        assert!(!is_camp(&lot, 21_135));
    }
}
