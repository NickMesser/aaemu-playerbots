//! What the gear goal does in a run: the [`Job`] (pure: it reads the bot's worn set and bag through
//! [`Me`] and the world through a [`Catalog`], so it is tested with made-up items).
//!
//! The order of the sources (the bag comes first, but the equip duty handles it on its own):
//!
//! 1. **Buy** a piece from a merchant: the offer must be worth putting on ([`eval::worth`]), cost
//!    at most `max_spend_pct` percent of the bot's money, and the merchant must stand within the
//!    trip limit. While minting is allowed only an offer that meets the bot's tier counts (a
//!    level-30 bot does not buy the level-14 starter set to throw it away at once).
//! 2. **Mint** a piece with a GM command (`/item add self <template> 1 <grade>`): the best piece of
//!    a wanted slot ([`eval::wanted_slots`]) that suits the build well, is not above the bot's
//!    level and is no quest or test item, at the grade of the tier.
//! 3. **Socket** a lunagem into a free socket of a worn piece whose gems the bot knows, when the
//!    chance is good enough for the bot ([`may_socket`]); the gem comes from the bag, or is minted.
//! 4. **Regrade** a worn piece below the tier's grade with a scroll, only when a failure cannot
//!    destroy it, and a failure that lowers the grade only for a bot that likes the risk.
//!
//! An operator's item ([`choose`] with `item`) is put on even when it is worse: taken from the
//! bag, bought or minted.

use aa_world::gear::{GearClass, GearItem, GradeFacts, Lunagem, SoldGear, NO_GRADE};

use super::eval::{self, BagPiece, Facts, Fit, Tier, Worn, SLOTS};

/// A piece minted for a bot must suit the build at least this well ([`Fit::affinity`]).
pub const MINT_FIT: f32 = 0.9;
/// A bot with less gearhead than this never risks the gems a piece holds.
pub const CAUTIOUS: f32 = 0.75;
/// A bot with at least this much gearhead risks a regrade that can lower the grade.
pub const RISKY: f32 = 0.8;
/// The most pieces a bot mints in a session (a guard against a loop).
pub const MAX_MINTS: u32 = 24;
/// The most pieces a bot buys in a session.
pub const MAX_BUYS: u32 = 12;
/// A minted piece for a core slot (armor, the weapons of the build) counts this much more than
/// an accessory or a cloak: the tier is filled first.
pub const CORE_BIAS: f32 = 1.5;

/// How the operator configured the goal (`[gear]`, `bots.gm_bootstrap`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rules {
    /// Buy gear from merchants.
    pub buy: bool,
    /// Mint gear and gems with GM commands (`[gear] mint` and `bots.gm_bootstrap`).
    pub mint: bool,
    /// Socket lunagems.
    pub socket: bool,
    /// Regrade worn pieces (`[gear] regrade` and `bots.gm_bootstrap`).
    pub regrade: bool,
    /// The most a purchase may cost, in percent of the bot's money.
    pub max_spend_pct: u8,
    /// Unspent rank allowance available for a gear purchase.
    pub guild_budget: u32,
}

impl Rules {
    fn can_buy(&self, price: u32, money: i64) -> bool {
        (price != 0 && price <= self.guild_budget && i64::from(price) <= money)
            || affordable(price, money, self.max_spend_pct)
    }
}

/// A merchant that sells a piece.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vendor {
    /// The merchant's NPC template.
    pub npc: u32,
    /// Where it stands (its nearest spawn).
    pub pos: [f32; 3],
    /// How far that is from the bot (metres).
    pub dist: f32,
}

/// What the world offers the planner besides the item facts.
pub trait Catalog: Facts {
    /// The gear and gems merchants sell for copper.
    fn offers(&self) -> &[SoldGear];
    /// The nearest merchant of `npcs` within the trip limit.
    fn vendor(&self, npcs: &[u32]) -> Option<Vendor>;
    /// The price of one (copper).
    fn price(&self, template: u32) -> u32;
    /// Calls `f` with every gear template a GM may mint (no quest or test item, with a price).
    fn each_mintable(&self, f: &mut dyn FnMut(u32, GearItem));
    /// Calls `f` with every lunagem.
    fn each_gem(&self, f: &mut dyn FnMut(u32, Lunagem));
    /// The skill an item casts when it is used (0: none).
    fn use_skill(&self, item: u32) -> u32;
    /// The regrade odds of a grade.
    fn grade_facts(&self, grade: u8) -> GradeFacts;
    /// The plain regrade scroll of an item class (0: none).
    fn scroll(&self, class: GearClass) -> u32;
}

/// The bot as the planner sees it.
pub struct Me<'a> {
    /// Bot id (picks among equal pieces, so bots of a build do not all wear the same).
    pub id: u32,
    /// Character level.
    pub level: u8,
    /// Gender (1 male, 2 female).
    pub gender: u8,
    /// Money in copper.
    pub money: i64,
    /// How much the bot cares about gear, 0 to 1.
    pub gearhead: f32,
    /// What suits the build.
    pub fit: &'a Fit,
    /// What it wears.
    pub worn: &'a Worn,
    /// The gear, gems and scrolls in the bag.
    pub bag: &'a [BagPiece],
    /// Templates bought or minted this session (not fetched again).
    pub fetched: &'a [u32],
    /// Pieces minted and bought this session.
    pub minted: u32,
    /// Pieces bought this session.
    pub bought: u32,
}

impl Me<'_> {
    fn has(&self, template: u32) -> Option<&BagPiece> {
        self.bag.iter().find(|b| b.template == template)
    }

    fn wears(&self, template: u32) -> bool {
        self.worn
            .slots
            .iter()
            .flatten()
            .any(|p| p.template == template)
    }

    fn fresh(&self, template: u32) -> bool {
        !self.fetched.contains(&template) && self.has(template).is_none() && !self.wears(template)
    }
}

/// What one run of the goal does.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Job {
    /// Put on the bag piece of `template` (an operator asked for it: `forced`, even when worse).
    Equip {
        /// The template.
        template: u32,
        /// Put on even when it is worse.
        forced: bool,
    },
    /// Buy a piece from a merchant and put it on.
    Buy {
        /// The template.
        template: u32,
        /// The grade the merchant sells it at.
        grade: u8,
        /// The price (copper).
        price: u32,
        /// Where to buy it.
        vendor: Vendor,
        /// Put on even when it is worse.
        forced: bool,
    },
    /// Mint a piece with a GM command and put it on.
    Mint {
        /// The template.
        template: u32,
        /// The grade to mint it at.
        grade: u8,
        /// Put on even when it is worse.
        forced: bool,
    },
    /// Socket a gem into a worn piece (minting the gem first when `mint`).
    Socket {
        /// The worn piece.
        target: u64,
        /// Its equipment slot.
        slot: u8,
        /// The gem's template.
        gem: u32,
        /// The gem is minted first.
        mint: bool,
        /// The chance of the socket, per 10 000.
        chance: u16,
        /// Gems the piece holds before.
        held: u8,
    },
    /// Regrade a worn piece with a scroll (minted first when `mint`).
    Regrade {
        /// The worn piece.
        target: u64,
        /// Its equipment slot.
        slot: u8,
        /// The scroll's template.
        scroll: u32,
        /// The scroll is minted first.
        mint: bool,
        /// The piece's grade before.
        from: u8,
    },
}

impl Job {
    /// A word for the log and the web UI.
    pub fn name(&self) -> &'static str {
        match self {
            Job::Equip { .. } => "equip",
            Job::Buy { .. } => "buy",
            Job::Mint { .. } => "mint",
            Job::Socket { .. } => "socket",
            Job::Regrade { .. } => "regrade",
        }
    }
}

/// What the goal could do now, as the equip duty sees it at each check (the goal's score).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Want {
    /// How far the core slots are below the tier ([`eval::deficit`]).
    pub deficit: f32,
    /// A merchant sells a piece worth buying.
    pub buy: bool,
    /// Minting would fill a gap.
    pub mint: bool,
    /// A worn piece can take a gem.
    pub socket: bool,
    /// A worn piece can be regraded.
    pub regrade: bool,
}

impl Want {
    /// Whether there is anything to do.
    pub fn any(&self) -> bool {
        self.buy || self.mint || self.socket || self.regrade
    }
}

/// The minimum chance (per 10 000) a bot accepts for a gem, by its gearhead: 100 % below
/// [`CAUTIOUS`], down to 30 % for a bot that lives for its gear.
pub fn min_chance(gearhead: f32) -> u16 {
    if gearhead < CAUTIOUS {
        10_000
    } else {
        (10_000.0 - 7_000.0 * gearhead.clamp(0.0, 1.0)) as u16
    }
}

/// Whether a bot with `gearhead` sockets a gem into a piece holding `held` gems at `chance` (per
/// 10 000): always at a sure chance; never when a failure would wipe the gems of a cautious bot;
/// else when the chance is at least [`min_chance`].
pub fn may_socket(held: u8, chance: u16, gearhead: f32) -> bool {
    if chance >= 10_000 {
        return true;
    }
    if chance == 0 || (held > 0 && gearhead < CAUTIOUS) {
        return false;
    }
    chance >= min_chance(gearhead)
}

/// Whether a purchase of `price` copper fits the budget.
pub fn affordable(price: u32, money: i64, max_spend_pct: u8) -> bool {
    price > 0 && i64::from(price) * 100 <= money.max(0) * i64::from(max_spend_pct)
}

/// A stable mix of the bot id and a template (picks among equal pieces).
fn mix(bot: u32, template: u32) -> u32 {
    let mut x = (u64::from(bot) << 32 | u64::from(template)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 29;
    (x >> 16) as u32
}

/// What the goal could do now (cheap: the merchant offers and the worn pieces, no scan of every
/// item).
pub fn want(me: &Me<'_>, rules: &Rules, cat: &dyn Catalog) -> Want {
    let tier = Tier::of_level(me.level);
    let deficit = eval::deficit(me.worn, &tier, me.fit);
    Want {
        deficit,
        buy: rules.buy && best_offer(me, rules, cat).is_some(),
        mint: rules.mint && deficit > 0.0 && me.minted < MAX_MINTS,
        socket: rules.socket && socket_job(me, rules, cat, None).is_some(),
        regrade: rules.regrade && regrade_job(me, rules, cat).is_some(),
    }
}

/// The job of a run: an operator's `item`, else (in this order) a purchase, a minted piece, a gem,
/// a regrade. `Err` says why there is nothing to do.
pub fn choose(
    me: &Me<'_>,
    rules: &Rules,
    cat: &dyn Catalog,
    item: Option<u32>,
) -> Result<Job, &'static str> {
    if let Some(t) = item {
        return choose_item(me, rules, cat, t);
    }
    if rules.buy {
        if let Some(job) = best_offer(me, rules, cat) {
            return Ok(job);
        }
    }
    if rules.mint && me.minted < MAX_MINTS {
        if let Some(job) = best_mint(me, cat) {
            return Ok(job);
        }
    }
    if rules.socket {
        if let Some(job) = socket_job(me, rules, cat, None) {
            return Ok(job);
        }
    }
    if rules.regrade {
        if let Some(job) = regrade_job(me, rules, cat) {
            return Ok(job);
        }
    }
    Err("nothing to upgrade")
}

/// The job for an operator's item.
fn choose_item(me: &Me<'_>, rules: &Rules, cat: &dyn Catalog, t: u32) -> Result<Job, &'static str> {
    if me.wears(t) {
        return Err("already wears it");
    }
    if cat.gem(t).is_some() {
        return socket_job(me, rules, cat, Some(t)).ok_or("no worn piece takes that gem");
    }
    let item = cat.item(t).ok_or("not a piece of gear")?;
    if !item.wearable_by(me.level, me.gender) {
        return Err("above the bot's level");
    }
    if me.has(t).is_some() {
        return Ok(Job::Equip {
            template: t,
            forced: true,
        });
    }
    if rules.buy {
        for o in cat.offers().iter().filter(|o| o.item == t) {
            let price = cat.price(t);
            if !rules.can_buy(price, me.money) {
                continue;
            }
            if let Some(vendor) = cat.vendor(&o.npcs) {
                return Ok(Job::Buy {
                    template: t,
                    grade: o.grade,
                    price,
                    vendor,
                    forced: true,
                });
            }
        }
    }
    if rules.mint {
        let tier = Tier::of_level(me.level);
        return Ok(Job::Mint {
            template: t,
            grade: item.grade_at(tier.grade),
            forced: true,
        });
    }
    Err("no merchant sells it in reach and minting is off")
}

/// The best merchant offer: worth putting on, affordable, the merchant in reach.
fn best_offer(me: &Me<'_>, rules: &Rules, cat: &dyn Catalog) -> Option<Job> {
    if me.bought >= MAX_BUYS {
        return None;
    }
    let tier = Tier::of_level(me.level);
    let mut best: Option<(f32, Job)> = None;
    for o in cat.offers() {
        let Some(item) = cat.item(o.item) else {
            continue;
        };
        if !item.wearable_by(me.level, me.gender) || !me.fresh(o.item) {
            continue;
        }
        if rules.mint && item.level < tier.min_item_level {
            continue;
        }
        let affinity = me.fit.affinity(&item);
        let v = eval::value(&item, cat.grade_factor(o.grade), 0, affinity);
        let Some((_, gain, before)) = eval::gain_of(me.worn, &item, v) else {
            continue;
        };
        if v <= 0.0 || !eval::worth(gain, before) || best.is_some_and(|b| b.0 >= gain) {
            continue;
        }
        let price = cat.price(o.item);
        if !rules.can_buy(price, me.money) {
            continue;
        }
        let Some(vendor) = cat.vendor(&o.npcs) else {
            continue;
        };
        best = Some((
            gain,
            Job::Buy {
                template: o.item,
                grade: o.grade,
                price,
                vendor,
                forced: false,
            },
        ));
    }
    best.map(|b| b.1)
}

/// The best piece to mint: of a wanted slot, suiting the build, not above the bot's level nor
/// above the tier's grade (a piece of a fixed grade higher than that is not minted), a core slot
/// before the accessories ([`CORE_BIAS`]).
fn best_mint(me: &Me<'_>, cat: &dyn Catalog) -> Option<Job> {
    let tier = Tier::of_level(me.level);
    let wish = cat.grade_order(tier.grade);
    let wanted = eval::wanted_slots(me.fit, me.worn.two_handed());
    let core = |slot: u8| {
        eval::CORE_SLOTS.contains(&slot)
            || (slot == aa_world::gear::equip_slot::OFFHAND && me.fit.shield)
            || (slot == aa_world::gear::equip_slot::RANGED && me.fit.bow)
    };
    let mut best: Option<(f32, u32, u32, u8)> = None; // gain, mix, template, grade
    cat.each_mintable(&mut |t, item| {
        let affinity = me.fit.affinity(&item);
        if affinity < MINT_FIT
            || item.level > me.level
            || !item.wearable_by(me.level, me.gender)
            || !me.fresh(t)
            || !item.slots().iter().any(|s| wanted.contains(s))
        {
            return;
        }
        let grade = item.grade_at(tier.grade);
        if cat.grade_order(grade) > wish {
            return;
        }
        let v = eval::value(&item, cat.grade_factor(grade), 0, affinity);
        let Some((slot, gain, before)) = eval::gain_of(me.worn, &item, v) else {
            return;
        };
        if !wanted.contains(&slot) || !eval::worth(gain, before) {
            return;
        }
        // equal gains (rounded to a hundredth) go to the bot's own pick
        let bias = if core(slot) { CORE_BIAS } else { 1.0 };
        let g = (gain * bias * 100.0).round() / 100.0;
        let m = mix(me.id, t);
        if best.is_none_or(|b| g > b.0 || (g == b.0 && m > b.1)) {
            best = Some((g, m, t, grade));
        }
    });
    best.map(|(_, _, template, grade)| Job::Mint {
        template,
        grade,
        forced: false,
    })
}

/// A gem for a free socket of a worn piece: `only` restricts it to one gem template.
fn socket_job(me: &Me<'_>, rules: &Rules, cat: &dyn Catalog, only: Option<u32>) -> Option<Job> {
    for slot in 0..SLOTS as u8 {
        let Some(piece) = me.worn.get(slot) else {
            continue;
        };
        let (Some(item), Some(held)) = (piece.item, piece.gems) else {
            continue;
        };
        if held >= cat.sockets(item.slot_type, piece.grade) {
            continue;
        }
        let chance = cat.socket_chance(held);
        if !may_socket(held, chance, me.gearhead) {
            continue;
        }
        let fits = |t: u32, g: &Lunagem| {
            g.fits(item.slot_type, item.level)
                && cat.use_skill(t) != 0
                && only.is_none_or(|o| o == t)
        };
        // a gem of the bag first, the one for the highest item level
        let from_bag = me
            .bag
            .iter()
            .filter_map(|b| cat.gem(b.template).map(|g| (b.template, g)))
            .filter(|(t, g)| fits(*t, g))
            .max_by_key(|(t, g)| (g.min_level, mix(me.id, *t)))
            .map(|(t, _)| t);
        let gem = match from_bag {
            Some(t) => Some((t, false)),
            None if rules.mint => {
                let mut pick: Option<(u8, u32, u32)> = None;
                cat.each_gem(&mut |t, g| {
                    if fits(t, &g) {
                        let k = (g.min_level, mix(me.id, t), t);
                        if pick.is_none_or(|p| k > p) {
                            pick = Some(k);
                        }
                    }
                });
                pick.map(|p| (p.2, true))
            }
            None => None,
        };
        if let Some((gem, mint)) = gem {
            return Some(Job::Socket {
                target: piece.id,
                slot,
                gem,
                mint,
                chance,
                held,
            });
        }
    }
    None
}

/// A worn piece below the tier's grade that a scroll can raise without the risk of losing it.
fn regrade_job(me: &Me<'_>, rules: &Rules, cat: &dyn Catalog) -> Option<Job> {
    let tier = Tier::of_level(me.level);
    let wish = cat.grade_order(tier.grade);
    let mut best: Option<(u8, Job)> = None;
    for slot in eval::CORE_SLOTS {
        let Some(piece) = me.worn.get(slot) else {
            continue;
        };
        let Some(item) = piece.item else {
            continue;
        };
        let order = cat.grade_order(piece.grade);
        if !item.enchantable() || item.fixed_grade != NO_GRADE || order >= wish {
            continue;
        }
        let odds = cat.grade_facts(piece.grade);
        if odds.regrade_success == 0
            || odds.regrade_break > 0
            || (odds.regrade_downgrade > 0 && me.gearhead < RISKY)
        {
            continue;
        }
        let scroll = cat.scroll(item.class);
        if scroll == 0 || cat.use_skill(scroll) == 0 {
            continue;
        }
        let mint = me.has(scroll).is_none();
        if mint && !rules.mint {
            continue;
        }
        if best.as_ref().is_none_or(|b| order < b.0) {
            best = Some((
                order,
                Job::Regrade {
                    target: piece.id,
                    slot,
                    scroll,
                    mint,
                    from: piece.grade,
                },
            ));
        }
    }
    best.map(|b| b.1)
}
