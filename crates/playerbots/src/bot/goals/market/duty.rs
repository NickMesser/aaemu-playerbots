//! The market duty: what a bot does for the trade-chat market whatever goal it is running.
//!
//! Every brain tick (cheaply: a look at the surroundings every few seconds, at once when lines
//! wait) the duty tells the board where the bot is, lets [`AdState`] read what was heard and post
//! an advertisement, says the lines that are due (within the process-wide limits) and reports
//! chat refusals. When a sale is agreed it asks the brain to start the market goal, which does the
//! meeting and the trade.

use aa_proto::cs::{self, chat_type};
use aa_world::{ItemFacts, World};
use smallvec::SmallVec;

use super::meet::{self, Side};
use super::report;
use crate::bot::cmd::GoalKind;
use crate::bot::ctx::Ctx;
use crate::bot::goals::{trade, work};
use crate::bot::guild::supply;
use crate::bot::market::ads::board::{self, Peer, PEER_FRESH_MS};
use crate::bot::market::ads::lines::{self, Meet, Words};
use crate::bot::market::ads::names;
use crate::bot::market::ads::price::{PriceSource, VendorPrices};
use crate::bot::market::ads::talk::{Medium, PlaceQ};
use crate::bot::market::ads::{Env, Funding, Need, Rules, Surplus};
use crate::bot::market::ledger::{bag_count, Ledger};
use crate::bot::market::needs::{guild_recipe, QuestNeeds};
use crate::bot::state::{Item, StateError, UnitKind};
use crate::hub::unix_ms;

/// The status line of the duty comes this often.
const STATUS_MS: u64 = 3 * 60_000;
/// A look at the surroundings at least this often.
const CHECK_MS: u64 = 4_000;
/// The bot tells the board where it is this often.
const REPORT_MS: u64 = 10_000;
/// All bots together say at most this many market lines per minute.
const LINES_PER_MIN: u32 = 14;
/// A refused line is tried again after this long.
const RETRY_MS: u64 = 6_000;
/// Items the bot offers at once (the rest waits for the next advertisement).
const MAX_SURPLUS: usize = 6;
/// Kinds of goods a bot wants at once (all of the shared trade goods).
const WANTS: usize = trade::GOODS.len();
/// Units a bot wants of a good it needs.
const NEED_QTY: u32 = 10;
/// A bot with fewer than this many coppers does not shop.
const SHOP_MONEY: i64 = 500;
/// A bot holding this many units of a good does not need it.
const ENOUGH: i64 = 5;
/// A refusal of a whisper is blamed on the line when it comes this soon after it.
const WHISPER_ERR_MS: u64 = 6_000;
/// Hurt below this, the bot does not go to a meeting.
const FIT_PCT: u8 = 50;

/// What a bot offers: its clean surplus (tradable items only, less what its active quests need,
/// `needs`), one stack's worth of each item at most (a trade puts up one stack), and not an item
/// the server is known to refuse.
pub fn surplus_of(
    ledger: &Ledger,
    world: &World,
    bag: &rustc_hash::FxHashMap<u16, Item>,
    needs: &QuestNeeds,
) -> SmallVec<[Surplus; MAX_SURPLUS]> {
    let mut out: SmallVec<[Surplus; MAX_SURPLUS]> = SmallVec::new();
    for (template, count) in ledger.sellable_surplus(world, bag, needs) {
        if out.len() >= MAX_SURPLUS || trade::is_bound(template) {
            continue;
        }
        let stack = bag
            .values()
            .filter(|i| i.template == template && i.count > 0)
            .map(|i| i.count.unsigned_abs())
            .max()
            .unwrap_or(0);
        let count = count.min(stack);
        if count > 0 {
            out.push(Surplus { template, count });
        }
    }
    out
}

/// What a bot would buy: the shared trade goods it holds hardly any of (the order, and so the one
/// it advertises first, changes every hour). Only goods that are `tradable` (see
/// [`aa_world::ItemFacts::tradable`]) are wanted: nobody sells the others.
pub fn wanted_goods(
    bag: &rustc_hash::FxHashMap<u16, Item>,
    money: i64,
    bot: u32,
    unix_ms: u64,
    tradable: impl Fn(u32) -> bool,
) -> SmallVec<[Need; WANTS]> {
    let mut out: SmallVec<[Need; WANTS]> = SmallVec::new();
    if money < SHOP_MONEY {
        return out;
    }
    let goods = trade::GOODS;
    let start = (u64::from(bot) + unix_ms / 3_600_000) as usize;
    for k in 0..WANTS {
        let template = goods[(start + k) % goods.len()];
        if !tradable(template) {
            continue;
        }
        let have: i64 = bag
            .values()
            .filter(|i| i.template == template)
            .map(|i| i64::from(i.count))
            .sum();
        if have < ENOUGH {
            out.push(Need {
                template,
                qty: NEED_QTY,
                order_price: None,
                funding: None,
            });
        }
    }
    out
}

/// A character name the server takes as a whisper target.
pub fn valid_target(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= 24
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
}

/// Whether the duty may start the market goal over `running`.
fn may_interrupt(running: Option<GoalKind>) -> bool {
    !matches!(
        running,
        Some(
            GoalKind::Market
                | GoalKind::Trade
                | GoalKind::Duel
                | GoalKind::Dungeon
                | GoalKind::Sail
                | GoalKind::Rest
        )
    )
}

/// The mother faction of a faction id (0 = unknown).
fn mother(world: &World, faction: u32) -> u32 {
    if faction == 0 {
        0
    } else {
        world.mother_faction(faction)
    }
}

/// The nearest town meeting point (the auctioneer's portal doodad) to `pos`.
pub fn town_spot(world: &World, pos: [f32; 3]) -> Option<[f32; 2]> {
    let m = world.mirage()?;
    let p = m.nearest_portal(aa_world::Vec3::new(pos[0], pos[1], pos[2]))?;
    Some([p.x, p.y])
}

/// One tick of the duty. Returns whether the brain should start the market goal now.
pub fn duty(ctx: &mut Ctx<'_>, running: Option<GoalKind>) -> bool {
    let cfg = ctx.cfg;
    if !(cfg.market.enabled && cfg.market.chat_ads) {
        return false;
    }
    if !ctx.state.me.spawned || ctx.state.me.dead {
        return false;
    }
    let now = ctx.now_ms;
    let unix = unix_ms();
    let world = ctx.world;
    let hub = ctx.hub;
    let persona = ctx.persona;
    let name: &str = &persona.name;
    let board = board::global();
    let real_near = ctx.real_player_near;
    let bot_id = persona.id;
    let st = &mut *ctx.state;
    let pos = st.me.pos;
    let group = if st.place.in_instance() {
        0
    } else {
        world.zone_group_at(pos[0], pos[1]).unwrap_or(0)
    };
    let my_faction = mother(world, st.me.faction);
    let money = st.me.money;

    let inner = st.market.inner_mut();
    if inner.ads.report_due(now, REPORT_MS) {
        board.set_peer(
            name,
            Peer {
                faction: my_faction,
                pos: [pos[0], pos[1]],
                group,
                at_ms: unix,
            },
        );
    }
    let pending = inner.ads.deal().is_some();
    if !inner.ads.check_due(now, CHECK_MS) {
        return pending && ready_for_meeting(ctx, running);
    }

    // what the bot has and wants
    crate::bot::guild::treasury::note_pack_budget(ctx);
    let work_order = work::selected_order(ctx);
    let training = work::training::Allowance::for_order(ctx, work_order);
    let guild_recipe = guild_recipe(ctx);
    let customer_provides_inputs =
        work::customer::active(ctx).is_some_and(|order| order.awaits_materials());
    let recovery_orders = crate::bot::guild::recovery::shopping(ctx);
    let recovery_stock = crate::bot::guild::recovery::stock(ctx);
    let land_order = crate::bot::guild::land::shopping(ctx);
    let supply_contract = supply::shopping(ctx);
    let item_needs = QuestNeeds::for_bot(ctx);
    let st = &mut *ctx.state;
    let inner = st.market.inner_mut();
    let surplus = surplus_of(&inner.ledger, world, &st.bag, &item_needs);
    let tradable = |t| world.item_facts(t).is_some_and(ItemFacts::tradable);
    let prices = VendorPrices(world);
    let mut needs = wanted_goods(&st.bag, money, bot_id, unix, tradable);
    if customer_provides_inputs {
        if let Some(craft) = guild_recipe {
            needs.retain(|need| {
                !craft
                    .materials
                    .iter()
                    .any(|&(item, _)| item == need.template)
            });
        }
    } else if let Some(craft) = guild_recipe.filter(|c| st.me.level >= c.req_level) {
        for &(template, quantity) in craft.materials.iter().rev().filter(|&&(t, _)| tradable(t)) {
            let count = bag_count(&st.bag, template);
            let missing = u32::try_from(i64::from(quantity) - count).unwrap_or(0);
            if let Some((allowance, price)) = training
                .as_ref()
                .and_then(|allowance| allowance.price(template).map(|price| (allowance, price)))
            {
                needs.retain(|need| need.template != template);
                let qty = u64::from(missing).min(allowance.copper / u64::from(price)) as u32;
                if qty != 0 {
                    needs.insert(
                        0,
                        Need {
                            template,
                            qty,
                            order_price: Some(price),
                            funding: Some(Funding::Training(allowance.guild)),
                        },
                    );
                }
            } else if money >= SHOP_MONEY && missing != 0 {
                needs.retain(|need| need.template != template);
                needs.insert(
                    0,
                    Need {
                        template,
                        qty: missing,
                        order_price: prices
                            .quote(template)
                            .filter(|_| craft.is_pack)
                            .map(|q| q.ask),
                        funding: None,
                    },
                );
            }
        }
    }
    let requested = match work_order {
        Some((work::Order::Gather { item, doodad }, _))
            if world.gather_doodads(item).contains(&doodad) =>
        {
            Some((item, NEED_QTY))
        }
        Some((work::Order::Hunt { item, count }, _)) if !world.work_droppers(item).is_empty() => {
            Some((item, u32::from(count)))
        }
        _ => None,
    };
    if let Some((item, target)) = requested.filter(|(item, _)| tradable(*item)) {
        let count = bag_count(&st.bag, item);
        if let Some(need) = material_request(
            item,
            target,
            count,
            &prices,
            hub.market_prices.average(item, unix),
        ) {
            needs.retain(|n| n.template != item);
            needs.insert(0, need);
        }
    }
    needs.retain(|need| {
        !recovery_stock
            .iter()
            .any(|&(item, _)| item == need.template)
    });
    for order in recovery_orders.into_iter().rev() {
        needs.insert(0, order);
    }
    if let Some(order) = land_order {
        needs.retain(|need| need.template != order.template);
        needs.insert(0, order);
    }
    if let Some(order) = supply_contract {
        needs.retain(|need| need.template != order.template);
        needs.insert(0, order);
    }
    let audience = board.audience(name, group, unix) + usize::from(real_near);
    let index = names::global(world);
    let units = &st.units;
    let item_name = |t: u32| world.item_name(t).unwrap_or("goods").to_owned();
    let is_bot = |n: &str| hub.chat.names.contains(n);
    let permits_supplier = |funding, name: &str| {
        supply::permits_supplier(&cfg.guild, &hub.guilds, funding, name, None)
    };
    let trusted_companion = |name: &str| {
        let Some(character) = units.values().find(|unit| {
            unit.kind == UnitKind::Character
                && unit
                    .name
                    .as_deref()
                    .is_some_and(|seen| seen.eq_ignore_ascii_case(name))
        }) else {
            return false;
        };
        hub.journal.read(bot_id, |j| {
            j.relationships
                .get(character.id)
                .is_some_and(|r| r.trust >= 20)
        })
    };
    let interest = |template: u32, qty: u32, ask: u32| {
        let need = needs.iter().find(|n| n.template == template)?;
        let q = need.quote(&prices)?;
        let afford = u32::try_from(money / i64::from(ask.max(1))).unwrap_or(u32::MAX);
        let want = need.qty.min(qty).min(afford);
        (want > 0).then_some((want, q.ceiling))
    };
    let me_side = Side {
        faction: my_faction,
        pos: [pos[0], pos[1]],
    };
    let town = town_spot(world, pos);
    let mirage_ok = world.mirage().is_some();
    let meet_at = cfg.market.meet_at;
    let peer_side = |peer: &str| -> Option<Side> {
        if let Some(p) = board.peer(peer, unix, PEER_FRESH_MS) {
            return Some(Side {
                faction: p.faction,
                pos: p.pos,
            });
        }
        units
            .values()
            .find(|u| {
                u.kind == UnitKind::Character
                    && u.name
                        .as_deref()
                        .is_some_and(|n| n.eq_ignore_ascii_case(peer))
            })
            .map(|u| Side {
                faction: mother(world, u.faction),
                pos: [u.pos[0], u.pos[1]],
            })
    };
    let mut place = |q: PlaceQ<'_>| -> Option<Meet> {
        match q {
            PlaceQ::Choose { peer } => {
                meet::choose(meet_at, me_side, peer_side(peer), town, mirage_ok)
            }
            PlaceQ::Told { peer, mirage } => {
                if mirage {
                    mirage_ok.then_some(Meet::Mirage)
                } else {
                    board
                        .meet_for(peer, name, unix)
                        .or_else(|| meet::local_town(me_side, peer_side(peer), town))
                }
            }
        }
    };
    let mut env = Env {
        now_ms: now,
        unix_ms: unix,
        rules: Rules {
            chat_ads: true,
            sell_to_players: cfg.may_sell_to_players(),
            max_ads_per_hour: cfg.market.max_ads_per_hour,
            ad_interval_ms: cfg.market.ad_interval_secs.saturating_mul(1000),
        },
        me: name,
        prices: &prices,
        names: index,
        item_name: &item_name,
        surplus: &surplus,
        needs: &needs,
        money,
        audience,
        is_bot: &is_bot,
        permits_supplier: &permits_supplier,
        trusted_companion: &trusted_companion,
        interest: &interest,
        place: &mut place,
        board,
        rng: &mut *ctx.rng,
    };
    inner.ads.step(&mut env);
    let pending = inner.ads.deal().is_some();
    if inner.ads.status_due(now, STATUS_MS) {
        let line = format!(
            "market: {} goods to sell, {} wanted, {} bots read the trade chat (group {group}); heard {}, {} ads posted, {} talks, {} deals agreed",
            surplus.len(),
            needs.len(),
            audience,
            inner.ads.heard,
            inner.ads.posted,
            inner.ads.talks_opened,
            inner.ads.agreed,
        );
        ctx.log.say(line);
    }

    say_due(ctx, now, unix);
    watch_chat_errors(ctx, now);
    pending && ready_for_meeting(ctx, running)
}

fn material_request(
    item: u32,
    target: u32,
    count: i64,
    prices: &dyn PriceSource,
    recent: Option<f32>,
) -> Option<Need> {
    let recent = recent?;
    let normal = prices.quote(item)?;
    if recent < normal.ask as f32 * 1.25 {
        return None;
    }
    let qty = u32::try_from(i64::from(target).saturating_sub(count)).ok()?;
    let price = (recent * 0.9).floor() as u32;
    (qty > 0 && price > 0).then_some(Need {
        template: item,
        qty,
        order_price: Some(price),
        funding: None,
    })
}

/// Whether the bot may leave what it does for a meeting now.
fn ready_for_meeting(ctx: &Ctx<'_>, running: Option<GoalKind>) -> bool {
    may_interrupt(running) && !ctx.combat.busy() && ctx.state.me.hp_pct() >= FIT_PCT
}

/// Says the lines that are due, within the process-wide limit; a line the limit refuses waits.
pub fn say_due(ctx: &mut Ctx<'_>, now: u64, unix: u64) {
    let board = board::global();
    for _ in 0..3 {
        let Some(inner) = ctx.state.market.get_mut() else {
            return;
        };
        let Some(due) = inner.ads.next_due(now) else {
            return;
        };
        if !board.take_line(unix, LINES_PER_MIN) {
            inner.ads.postpone(due, now + RETRY_MS);
            return;
        }
        let world = ctx.world;
        let item = due
            .speech
            .template()
            .and_then(|t| world.item_name(t))
            .unwrap_or("goods");
        let place = match due.speech.meet() {
            Some(Meet::Town { x, y }) => world
                .zone_key_at(x, y)
                .and_then(|k| world.zone_name(k))
                .unwrap_or("town"),
            _ => "Mirage Isle",
        };
        let text = lines::render(&due.speech, &Words { item, place }, &mut *ctx.rng);
        let to = (!due.to.is_empty()).then_some(&*due.to);
        let (kind, target, text) = match due.medium {
            Medium::Channel => (chat_type::TRADE, "", lines::addressed(to, &text)),
            Medium::Whisper => (chat_type::WHISPER, &*due.to, lines::addressed(None, &text)),
        };
        if kind == chat_type::WHISPER && !valid_target(target) {
            continue;
        }
        let Some(packet) = cs::try_chat(kind, target, &text) else {
            ctx.log.debug("market: refused to send an unsafe line");
            continue;
        };
        ctx.out.push(packet);
        ctx.log.say(if target.is_empty() {
            format!("market: trade chat: {text}")
        } else {
            format!("market: whisper to {target}: {text}")
        });
        if kind == chat_type::WHISPER {
            if let Some(inner) = ctx.state.market.get_mut() {
                inner.ads.note_whisper(now);
            }
        }
    }
}

/// A refusal of a whisper right after the bot sent one is reported as local information: the
/// bot answered in the medium it was spoken to, so it should not happen.
fn watch_chat_errors(ctx: &mut Ctx<'_>, now: u64) {
    let Some(inner) = ctx.state.market.get_mut() else {
        return;
    };
    let sent = inner.ads.last_whisper_ms();
    if sent == 0 {
        return;
    }
    let seen = inner.ads.chat_error_seen_ms();
    let found = ctx.state.errors.iter().find_map(|&(t, e)| match e {
        StateError::Msg(c @ (report::CODE_WHISPER_HOSTILE | report::CODE_WHISPER_OFFLINE))
            if t > seen && t >= sent && t.saturating_sub(sent) <= WHISPER_ERR_MS =>
        {
            Some((t, c))
        }
        _ => None,
    });
    if let Some((t, code)) = found {
        if let Some(inner) = ctx.state.market.get_mut() {
            inner.ads.note_chat_error(t);
        }
        ctx.log.say(format!(
            "market: the server refused a whisper (error {code})"
        ));
        ctx.report_issue(report::chat_error(code, true));
    }
    let _ = now;
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustc_hash::FxHashMap;

    fn bag(items: &[(u16, u32, i32)]) -> FxHashMap<u16, Item> {
        items
            .iter()
            .map(|&(slot, template, count)| {
                (
                    slot,
                    Item {
                        id: u64::from(slot) + 1,
                        template,
                        count,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn whisper_targets_are_plain_names() {
        assert!(valid_target("Marta"));
        assert!(valid_target("a_b-c9"));
        assert!(!valid_target(""));
        assert!(!valid_target("two words"));
        assert!(!valid_target("/gm"));
        assert!(!valid_target("averyveryverylongcharactername"));
    }

    #[test]
    fn a_poor_bot_or_one_with_enough_wants_nothing() {
        assert!(wanted_goods(&bag(&[]), 100, 1, 0, |_| true).is_empty());
        let rich = wanted_goods(&bag(&[]), 10_000, 1, 0, |_| true);
        assert_eq!(rich.len(), WANTS);
        for n in &rich {
            assert!(trade::GOODS.contains(&n.template));
            assert_eq!(n.qty, NEED_QTY);
        }
        let held: Vec<(u16, u32, i32)> = rich
            .iter()
            .enumerate()
            .map(|(i, n)| (i as u16, n.template, 50))
            .collect();
        assert!(wanted_goods(&bag(&held), 10_000, 1, 0, |_| true).is_empty());
        // (one of them held: the other two are still wanted)
        assert_eq!(
            wanted_goods(&bag(&held[..1]), 10_000, 1, 0, |_| true).len(),
            WANTS - 1
        );
    }

    #[test]
    fn a_good_that_is_not_tradable_is_never_wanted() {
        // (a quest item, a bound or an unsellable good: nobody may sell it to the bot)
        let blocked = trade::GOODS[2];
        for hour in 0..7 {
            let got = wanted_goods(&bag(&[]), 10_000, 3, hour * 3_600_000, |t| t != blocked);
            assert_eq!(got.len(), WANTS - 1);
            assert!(got.iter().all(|n| n.template != blocked));
        }
        assert!(wanted_goods(&bag(&[]), 10_000, 3, 0, |_| false).is_empty());
    }

    #[test]
    fn the_wish_changes_by_the_hour_and_by_the_bot() {
        let b = bag(&[]);
        let w = |bot, hour: u64| {
            wanted_goods(&b, 10_000, bot, hour * 3_600_000, |_| true)
                .iter()
                .map(|n| n.template)
                .collect::<Vec<_>>()
        };
        assert_ne!(w(1, 0), w(1, 1));
        assert_ne!(w(1, 0), w(2, 0));
        assert_eq!(w(1, 0), w(1, 0));
    }

    #[test]
    fn only_idle_goals_make_way_for_a_meeting() {
        assert!(may_interrupt(None));
        assert!(may_interrupt(Some(GoalKind::Wander)));
        assert!(may_interrupt(Some(GoalKind::Grind)));
        for busy in [
            GoalKind::Market,
            GoalKind::Trade,
            GoalKind::Duel,
            GoalKind::Dungeon,
            GoalKind::Sail,
            GoalKind::Rest,
        ] {
            assert!(!may_interrupt(Some(busy)), "{busy:?}");
        }
    }
}
