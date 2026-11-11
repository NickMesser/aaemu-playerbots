//! Tests of the auction goal: stack choice, and whole visits against a scripted auction house.

use std::collections::{HashMap, HashSet};

use aa_proto::auction::{self as wire, AuctionEvent, Lot};
use aa_proto::cs::Out;
use aa_proto::opcodes::cs as op;
use aa_proto::sc::ServerEvent;
use aa_proto::sc_activity::ActivityEvent;
use aa_proto::PacketStream;
use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use super::*;
use crate::bot::market::auction::{OwnLot, PRICE_TTL_MS};
use crate::bot::state::{Item, Unit, UnitKind};
use crate::issues::IssueKind;

const ME: u32 = 5;
const NPC_OBJ: u32 = 0x0001_0203;
const NPC_TEMPLATE: u32 = 872;
const ORE: u32 = 3411;

fn facts(price: u32, refund: u32) -> Facts {
    Facts { price, refund }
}

fn item(id: u64, template: u32, count: i32) -> Item {
    Item {
        id,
        template,
        count,
    }
}

fn bag_of(items: &[(u16, Item)]) -> FxHashMap<u16, Item> {
    items.iter().copied().collect()
}

// ---- which stacks are listed -------------------------------------------------------------------

#[test]
fn the_biggest_whole_stack_within_the_surplus_is_listed() {
    let bag = bag_of(&[
        (0, item(10, ORE, 20)),
        (1, item(11, ORE, 8)),
        (2, item(12, 99, 5)),
    ]);
    let f = |t: u32| match t {
        ORE => Some(facts(40, 20)),
        99 => Some(facts(500, 300)),
        _ => None,
    };
    // 25 honest ore: the stack of 20 (the stack of 8 stays for the next visit)
    let got = pick_stacks([(ORE, 25)].into_iter(), &bag, f, |_| false, 4);
    assert_eq!(
        got.as_slice(),
        [Stack {
            item_id: 10,
            template: ORE,
            count: 20
        }]
    );
    // 19 honest: only the stack of 8 fits
    let got = pick_stacks([(ORE, 19)].into_iter(), &bag, f, |_| false, 4);
    assert_eq!(got[0].item_id, 11);
    // 5 honest: no whole stack is honest
    assert!(pick_stacks([(ORE, 5)].into_iter(), &bag, f, |_| false, 4).is_empty());
}

#[test]
fn stacks_are_sorted_by_value_and_cut() {
    let bag = bag_of(&[
        (0, item(10, ORE, 20)), // 400
        (1, item(12, 99, 5)),   // 1500
        (2, item(13, 98, 1)),   // 120
        (3, item(14, 97, 1)),   // 50: too little
        (4, item(15, 96, 100)), // refund 0
    ]);
    let f = |t: u32| match t {
        ORE => Some(facts(40, 20)),
        99 => Some(facts(500, 300)),
        98 => Some(facts(200, 120)),
        97 => Some(facts(100, 50)),
        96 => Some(facts(0, 0)),
        _ => None,
    };
    let all = [(ORE, 20), (99, 5), (98, 1), (97, 1), (96, 100), (7, 3)];
    let got = pick_stacks(all.into_iter(), &bag, f, |_| false, 4);
    assert_eq!(
        got.iter().map(|s| s.template).collect::<Vec<_>>(),
        [99, ORE, 98]
    );
    let got = pick_stacks(all.into_iter(), &bag, f, |_| false, 2);
    assert_eq!(got.len(), 2);
    // skipped templates are left out
    let got = pick_stacks(all.into_iter(), &bag, f, |t| t == 99, 4);
    assert_eq!(
        got.iter().map(|s| s.template).collect::<Vec<_>>(),
        [ORE, 98]
    );
    assert_eq!(
        stacks_value(&got, f),
        20 * 20 + 120,
        "a vendor pays 400 and 120"
    );
}

#[test]
fn a_bigger_stack_wins_the_tie_by_id() {
    let bag = bag_of(&[(0, item(10, ORE, 20)), (1, item(11, ORE, 20))]);
    let f = |_t: u32| Some(facts(40, 20));
    let got = pick_stacks([(ORE, 40)].into_iter(), &bag, f, |_| false, 4);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].item_id, 11);
}

#[test]
fn a_bot_that_cannot_pay_the_fees_does_not_go() {
    // 20 ore at 40 / 20: asked halfway (30 each) = 600 copper, 4 % for 48 hours = 24 copper
    let bag = bag_of(&[(0, item(10, ORE, 20)), (1, item(12, 99, 5))]);
    let f = |t: u32| match t {
        ORE => Some(facts(40, 20)),
        99 => Some(facts(500, 300)),
        _ => None,
    };
    let both = pick_stacks([(ORE, 20), (99, 5)].into_iter(), &bag, f, |_| false, 4);
    assert_eq!(both.len(), 2);
    assert_eq!(
        estimated_fee(&both[1], facts(40, 20)),
        24,
        "the ore costs 24 copper to list"
    );
    assert!(
        affordable(both.clone(), 0, f).is_empty(),
        "no gold, no trip"
    );
    // the bigger stack (5 x 400 = 2000 copper, a fee of 80) first, the ore (24) after it
    let fees = |m| {
        affordable(both.clone(), m, f)
            .iter()
            .map(|s| s.template)
            .collect::<Vec<_>>()
    };
    assert_eq!(fees(23), Vec::<u32>::new());
    assert_eq!(fees(24), [ORE]);
    assert_eq!(fees(80), [99]);
    assert_eq!(fees(104), [99, ORE]);
    assert_eq!(fees(1_000_000), [99, ORE]);
}

#[test]
fn bigger_goods_weigh_more_and_merchants_like_the_house() {
    let t = Temperament::NEUTRAL;
    assert!(weight_for(300, &t) < weight_for(3_000, &t));
    assert_eq!(weight_for(3_000, &t), weight_for(30_000, &t));
    let merchant = Temperament {
        merchant: 1.0,
        ..Temperament::NEUTRAL
    };
    let homebody = Temperament {
        merchant: 0.0,
        ..Temperament::NEUTRAL
    };
    assert!(weight_for(1_000, &merchant) > weight_for(1_000, &homebody));
    assert!(weight_for(0, &homebody) > 0.0);
    assert_eq!(base_weight(&BehaviorCfg::default(), &t), 0.0);
}

#[test]
fn spot_keys_never_meet_item_templates() {
    for (x, y) in [
        (0.0, 0.0),
        (11240.0, 12110.0),
        (19404.0, 8355.0),
        (-5.0, 3.0),
    ] {
        assert!(machine::spot_key(x, y) >= 0x8000_0000);
    }
    assert_eq!(
        machine::spot_key(11240.0, 12110.0),
        machine::spot_key(11243.0, 12111.0)
    );
    assert_ne!(
        machine::spot_key(11240.0, 12110.0),
        machine::spot_key(11260.0, 12110.0)
    );
}

// ---- a scripted auction house ----------------------------------------------------------------------

/// How the scripted server treats a post.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PostMode {
    /// SCAuctionPosted, the fee taken, the item gone.
    Accept,
    /// The fee taken and the item gone, no answer.
    TakeSilently,
    /// No answer, nothing changes.
    Ignore,
    /// An error message.
    Error(i16),
    /// SCAuctionPosted, but the lot is never listed.
    AcceptUnlisted,
}

/// How the scripted server treats a search.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SearchMode {
    Answer,
    Ignore,
    /// Error 388 for the first n searches, then answers.
    TooFar(u8),
}

struct Fake {
    now: u64,
    pos: [f32; 3],
    st: BotState,
    mem: AuctionMemory,
    sent: Vec<Out>,
    log: Vec<Out>,
    said: Vec<String>,
    reports: Vec<IssueReport>,
    rng: fastrand::Rng,
    facts: HashMap<u32, Facts>,
    names: HashMap<u32, String>,
    useful: HashSet<u32>,
    /// Templates the bot must not trade (quest items, bound or unsellable ones).
    untradable: HashSet<u32>,
    spot: Option<Spot>,
    stacks: SmallVec<[Stack; MAX_LISTINGS]>,
    budget: Option<i64>,
    err: Option<(u64, i16)>,
    dest: Option<[f32; 3]>,
    // the server
    market: Vec<Lot>,
    own: Vec<Lot>,
    post: PostMode,
    search: SearchMode,
    bid_answers: bool,
    cancel_answers: bool,
    next_lot: u64,
}

impl Fake {
    fn new() -> Fake {
        let mut st = BotState::default();
        st.me.char_id = ME;
        st.me.name = "Seller".into();
        st.me.money = 100_000;
        st.me.spawned = true;
        st.units.insert(
            NPC_OBJ,
            Unit {
                kind: UnitKind::Npc,
                id: NPC_TEMPLATE,
                pos: [1000.0, 2000.0, 10.0],
                hp: 100,
                faction: 0,
                level: 1,
                flags: 0,
                target: 0,
                name: None,
            },
        );
        Fake {
            now: 10 * 60_000,
            pos: [1000.0, 2000.0 - 150.0, 10.0],
            st,
            mem: AuctionMemory::default(),
            sent: Vec::new(),
            log: Vec::new(),
            said: Vec::new(),
            reports: Vec::new(),
            rng: fastrand::Rng::with_seed(11),
            facts: HashMap::from([(ORE, facts(40, 20))]),
            names: HashMap::from([(ORE, "Copper Ore".to_owned())]),
            useful: HashSet::new(),
            untradable: HashSet::new(),
            spot: Some(Spot {
                template: NPC_TEMPLATE,
                pos: [1000.0, 2000.0, 10.0],
            }),
            stacks: SmallVec::new(),
            budget: None,
            err: None,
            dest: None,
            market: Vec::new(),
            own: Vec::new(),
            post: PostMode::Accept,
            search: SearchMode::Answer,
            bid_answers: true,
            cancel_answers: true,
            next_lot: 1000,
        }
    }

    /// The bot holds 20 copper ore in one stack and wants to list it.
    fn with_ore(mut self) -> Fake {
        self.st.bag.insert(0, item(10, ORE, 20));
        self.stacks.push(Stack {
            item_id: 10,
            template: ORE,
            count: 20,
        });
        self
    }

    fn lot(&mut self, template: u32, count: i32, buyout: i32, client: u32) -> Lot {
        self.next_lot += 1;
        Lot {
            id: self.next_lot,
            duration: 3,
            template,
            item_id: 5000 + self.next_lot,
            count,
            world: 1,
            client_id: client,
            client_name: if client == ME { "Seller" } else { "Other" }.into(),
            start: buyout * 9 / 10,
            buyout,
            post_date: 1_700_000_000,
            bid_world: 255,
            ..Lot::default()
        }
    }

    fn market_lot(&mut self, template: u32, count: i32, buyout: i32) {
        let l = self.lot(template, count, buyout, 77);
        self.market.push(l);
    }

    fn event(&mut self, ev: AuctionEvent) {
        let ev = ServerEvent::Activity(ActivityEvent::Auction(Box::new(ev)));
        self.mem.observe(&ev, self.now);
    }

    /// The server answers what the bot sent during the last step.
    fn serve(&mut self) {
        let sent: Vec<Out> = std::mem::take(&mut self.sent);
        for out in sent {
            self.log.push(out.clone());
            let mut s = PacketStream::from_bytes(out.body.clone());
            match out.opcode {
                op::CSAuctionSearchPacket => self.serve_search(&mut s),
                op::CSAuctionPostPacket => self.serve_post(&mut s),
                op::CSBidAuctionPacket => self.serve_bid(&mut s),
                op::CSCancelAuctionPacket => self.serve_cancel(&mut s),
                _ => {}
            }
        }
    }

    fn serve_search(&mut self, s: &mut PacketStream) {
        let (_, _) = (s.read_bc().unwrap(), s.read_bc().unwrap());
        let keyword = s.read_string().unwrap();
        let exact = s.read_bool().unwrap();
        let _grade = s.read_u8().unwrap();
        let _cats = [
            s.read_u8().unwrap(),
            s.read_u8().unwrap(),
            s.read_u8().unwrap(),
        ];
        let page = s.read_i32().unwrap();
        let client = s.read_u32().unwrap();
        let _filter = s.read_i32().unwrap();
        let _world = s.read_u8().unwrap();
        let _levels = [s.read_u8().unwrap(), s.read_u8().unwrap()];
        let sort = [s.read_u8().unwrap(), s.read_u8().unwrap()];
        match self.search {
            SearchMode::Ignore => return,
            SearchMode::TooFar(n) if n > 0 => {
                self.search = SearchMode::TooFar(n - 1);
                self.err = Some((self.now, wire::ERR_AUCTIONEER_TOO_FAR));
                return;
            }
            _ => {}
        }
        let mut lots: Vec<Lot> = self
            .market
            .iter()
            .chain(self.own.iter())
            .filter(|l| client == 0 || l.client_id == client)
            .filter(|l| {
                if keyword.trim().is_empty() {
                    return true;
                }
                let name = self.names.get(&l.template).map(|n| n.to_lowercase());
                let k = keyword.to_lowercase();
                name.is_some_and(|n| if exact { n == k } else { n.contains(&k) })
            })
            .cloned()
            .collect();
        if sort[0] == wire::sort_kind::DIRECT_PRICE {
            lots.sort_by_key(|l| l.buyout);
        }
        let pages = lots.len().div_ceil(wire::PAGE_SIZE);
        let lots: Vec<Lot> = if page >= 0 && (page as usize) < pages {
            lots.into_iter()
                .skip(page as usize * wire::PAGE_SIZE)
                .take(wire::PAGE_SIZE)
                .collect()
        } else {
            Vec::new()
        };
        self.event(AuctionEvent::Searched {
            page,
            lots,
            error: 0,
            server_time: 1_700_000_000,
        });
    }

    fn serve_post(&mut self, s: &mut PacketStream) {
        let (_, _) = (s.read_bc().unwrap(), s.read_bc().unwrap());
        let item_id = s.read_u64().unwrap();
        let start = s.read_i32().unwrap();
        let buyout = s.read_i32().unwrap();
        let dur = s.read_u8().unwrap();
        assert_eq!(dur, 3, "48 hours");
        assert!(start <= buyout && start > 0);
        let Some((slot, it)) = self
            .st
            .bag
            .iter()
            .find(|(_, i)| i.id == item_id)
            .map(|(s, i)| (*s, *i))
        else {
            panic!("the bot posted an item it does not have: {item_id}");
        };
        let fee = i64::from(wire::listing_fee(buyout, dur));
        match self.post {
            PostMode::Ignore => {}
            PostMode::Error(code) => self.err = Some((self.now, code)),
            PostMode::TakeSilently => {
                self.st.bag.remove(&slot);
                self.st.me.money -= fee;
            }
            PostMode::Accept | PostMode::AcceptUnlisted => {
                self.st.bag.remove(&slot);
                self.st.me.money -= fee;
                let mut lot = self.lot(it.template, it.count, buyout, ME);
                lot.item_id = item_id;
                lot.start = start;
                if self.post == PostMode::Accept {
                    self.own.push(lot.clone());
                }
                self.event(AuctionEvent::Posted(Box::new(lot)));
            }
        }
    }

    fn serve_bid(&mut self, s: &mut PacketStream) {
        let (_, _) = (s.read_bc().unwrap(), s.read_bc().unwrap());
        let lot_id = s.read_u64().unwrap();
        // skip the rest of the lot to the bid: the bid starts with the lot id again
        let rest = s.unread().to_vec();
        let at = rest
            .windows(8)
            .rposition(|w| w == lot_id.to_le_bytes())
            .expect("the bid repeats the lot id");
        let mut b = PacketStream::from_bytes(rest[at..].to_vec());
        assert_eq!(b.read_u64().unwrap(), lot_id);
        let _world = b.read_u8().unwrap();
        let bidder = b.read_u32().unwrap();
        assert_eq!(bidder, ME);
        let _name = b.read_string().unwrap();
        let money = b.read_i32().unwrap();
        let Some(i) = self.market.iter().position(|l| l.id == lot_id) else {
            return;
        };
        if !self.bid_answers {
            // the money is taken, nothing is said
            self.st.me.money -= i64::from(money);
            return;
        }
        let lot = self.market.remove(i);
        self.st.me.money -= i64::from(money);
        self.event(AuctionEvent::Bid {
            lot_id,
            bidder_id: ME,
            bidder_name: "Seller".into(),
            money,
            buyout: money >= lot.buyout,
            template: lot.template,
        });
    }

    fn serve_cancel(&mut self, s: &mut PacketStream) {
        let (_, _) = (s.read_bc().unwrap(), s.read_bc().unwrap());
        let lot_id = s.read_u64().unwrap();
        if !self.cancel_answers {
            return;
        }
        if let Some(i) = self.own.iter().position(|l| l.id == lot_id) {
            let lot = self.own.remove(i);
            self.event(AuctionEvent::Canceled(Box::new(lot)));
        }
    }

    /// The movement controller: 40 m a step, stopping `radius` short.
    fn walk_a_step(&mut self) {
        let Some([x, y, radius]) = self.dest else {
            return;
        };
        let d = ((x - self.pos[0]).powi(2) + (y - self.pos[1]).powi(2)).sqrt();
        let step = (d - radius).clamp(0.0, 40.0);
        if d > 0.0 {
            self.pos[0] += (x - self.pos[0]) / d * step;
            self.pos[1] += (y - self.pos[1]) / d * step;
        }
        if d - step <= radius + 0.01 {
            self.dest = None;
        }
    }

    fn packets(&self, opcode: u16) -> Vec<&Out> {
        self.log.iter().filter(|o| o.opcode == opcode).collect()
    }

    fn reported(&self, kind: IssueKind) -> Vec<&IssueReport> {
        self.reports.iter().filter(|r| r.kind == kind).collect()
    }
}

impl Env for Fake {
    fn now_ms(&self) -> u64 {
        self.now
    }
    fn pos(&self) -> [f32; 3] {
        self.pos
    }
    fn state(&self) -> &BotState {
        &self.st
    }
    fn mem(&mut self) -> &mut AuctionMemory {
        &mut self.mem
    }
    fn motion(&self) -> MotionStatus {
        if self.dest.is_some() {
            MotionStatus::Moving
        } else {
            MotionStatus::Idle
        }
    }
    fn travel(&mut self, x: f32, y: f32, radius: f32) {
        self.dest = Some([x, y, radius]);
    }
    fn leg_m(&self) -> f32 {
        1_000.0
    }
    fn stop(&mut self) {
        self.dest = None;
    }
    fn face(&mut self, _x: f32, _y: f32) {}
    fn send(&mut self, out: Out) {
        self.sent.push(out);
    }
    fn rng(&mut self) -> &mut fastrand::Rng {
        &mut self.rng
    }
    fn say(&self, _text: &str) {}
    fn report(&mut self, report: IssueReport) {
        self.reports.push(report);
    }
    fn facts(&self, template: u32) -> Option<Facts> {
        self.facts.get(&template).copied()
    }
    fn tradable(&self, template: u32) -> bool {
        !self.untradable.contains(&template)
    }
    fn item_name(&self, template: u32) -> Option<String> {
        self.names.get(&template).cloned()
    }
    fn useful(&self, template: u32) -> bool {
        self.useful.contains(&template)
    }
    fn auctioneer(&mut self) -> Option<Spot> {
        self.spot
    }
    fn sell_stacks(&mut self, max: usize) -> SmallVec<[Stack; MAX_LISTINGS]> {
        self.stacks.iter().copied().take(max).collect()
    }
    fn buy_budget(&self) -> Option<i64> {
        self.budget
    }
    fn error_since(&self, since_ms: u64) -> Option<(i16, u32)> {
        self.err.filter(|e| e.0 >= since_ms).map(|e| (e.1, 0))
    }
}

/// Runs the visit until it ends (or `max_steps` of a quarter second pass).
fn run_visit(f: &mut Fake, max_steps: u32) -> (Run, GoalStatus) {
    let mut run = Run::new(f.now);
    let mut status = GoalStatus::Running;
    for _ in 0..max_steps {
        f.now += 250;
        f.walk_a_step();
        status = run.step(f);
        f.serve();
        if status != GoalStatus::Running {
            break;
        }
    }
    (run, status)
}

// ---- visits -------------------------------------------------------------------------------------

#[test]
fn an_auctioneer_the_bot_died_on_the_way_to_is_left_alone() {
    let mut f = Fake::new().with_ore();
    let mut run = Run::new(f.now);
    // walk until the bot is on its way (the fake walks one step per tick)
    for _ in 0..4 {
        f.now += 250;
        f.walk_a_step();
        assert_eq!(run.step(&mut f), GoalStatus::Running);
    }
    assert!(run.walking(), "the auctioneer is far enough to walk to");
    let spot = f.spot.unwrap();
    let key = machine::spot_key(spot.pos[0], spot.pos[1]);
    assert!(!f.mem.skipped(key, f.now));
    run.died(&mut f.mem, f.now);
    assert!(f.mem.skipped(key, f.now + 1));
    assert!(f.mem.skipped(key, f.now + 29 * 60_000));
    assert!(!f.mem.skipped(key, f.now + 31 * 60_000));
    // a bot that died anywhere else leaves the spot alone
    let mut f = Fake::new().with_ore();
    let (run, status) = run_visit(&mut f, 2_000);
    assert_eq!(status, GoalStatus::Done);
    run.died(&mut f.mem, f.now);
    assert!(!f.mem.skipped(key, f.now));
}

#[test]
fn a_visit_walks_surveys_prices_posts_verifies_and_lingers() {
    let mut f = Fake::new().with_ore();
    for buyout in [600, 640, 700] {
        f.market_lot(ORE, 20, buyout); // 30, 32 and 35 each
    }
    let (run, status) = run_visit(&mut f, 2_000);
    assert_eq!(status, GoalStatus::Done, "{:?}", f.said);
    assert!(run.reached_the_house());

    // the packets: own lots, the market by exact name sorted by buyout, the post, the check
    let searches = f.packets(op::CSAuctionSearchPacket);
    assert_eq!(searches.len(), 3);
    let q = |o: &Out| {
        let mut s = PacketStream::from_bytes(o.body.clone());
        s.read_bc().unwrap();
        s.read_bc().unwrap();
        let kw = s.read_string().unwrap();
        let exact = s.read_bool().unwrap();
        s.read_u8().unwrap();
        s.read_bytes(3).unwrap();
        let page = s.read_i32().unwrap();
        let client = s.read_u32().unwrap();
        s.read_i32().unwrap();
        s.read_u8().unwrap();
        s.read_bytes(2).unwrap();
        let sort = s.read_u8().unwrap();
        (kw, exact, page, client, sort)
    };
    assert_eq!(q(searches[0]), (String::new(), false, 0, ME, 0));
    assert_eq!(
        q(searches[1]),
        (
            "Copper Ore".to_owned(),
            true,
            0,
            0,
            wire::sort_kind::DIRECT_PRICE
        )
    );
    assert_eq!(q(searches[2]), (String::new(), false, 0, ME, 0));
    // both ids are the NPC's object id
    assert_eq!(&searches[0].body[..6], &[3, 2, 1, 3, 2, 1]);

    // the post: 20 ore, the market median is 32 each, 95..=99% of it, 48 hours
    let posts = f.packets(op::CSAuctionPostPacket);
    assert_eq!(posts.len(), 1);
    let mut s = PacketStream::from_bytes(posts[0].body.clone());
    s.read_bc().unwrap();
    s.read_bc().unwrap();
    assert_eq!(s.read_u64().unwrap(), 10);
    let (start, buyout, dur) = (
        s.read_i32().unwrap(),
        s.read_i32().unwrap(),
        s.read_u8().unwrap(),
    );
    assert!((608..=634).contains(&buyout), "{buyout}");
    assert_eq!((start, dur), (buyout * 9 / 10, 3));

    // the memory
    let m = &f.mem;
    assert_eq!(m.stats.posted, 1);
    assert_eq!(m.stats.visits, 1, "a visit is counted when the bot arrives");
    assert_eq!(m.stats.fees, i64::from(wire::listing_fee(buyout, 3)));
    assert_eq!(m.own().len(), 1);
    assert!(m.own()[0].posted_here);
    let note = m.price(ORE, f.now).expect("the price is remembered");
    assert_eq!(note.quote.map(|q| q.unit), Some(32));
    assert!(m.price(ORE, f.now + PRICE_TTL_MS).is_none());
    assert_eq!(f.st.me.money, 100_000 - m.stats.fees);
    assert!(f.reports.is_empty(), "{:?}", f.reports);
    // nothing was cancelled, bought or bid
    assert!(f.packets(op::CSCancelAuctionPacket).is_empty());
    assert!(f.packets(op::CSBidAuctionPacket).is_empty());
}

#[test]
fn without_a_market_the_price_comes_from_the_item_facts() {
    let mut f = Fake::new().with_ore();
    let (_, status) = run_visit(&mut f, 2_000);
    assert_eq!(status, GoalStatus::Done);
    let posts = f.packets(op::CSAuctionPostPacket);
    assert_eq!(posts.len(), 1);
    let mut s = PacketStream::from_bytes(posts[0].body.clone());
    s.read_bc().unwrap();
    s.read_bc().unwrap();
    s.read_u64().unwrap();
    s.read_i32().unwrap();
    // 20 each between the vendor prices 20 and 40: 30
    assert_eq!(s.read_i32().unwrap(), 600);
    assert_eq!(f.mem.price(ORE, f.now).unwrap().quote, None);
}

#[test]
fn a_market_below_what_a_vendor_pays_leaves_the_stack_alone() {
    let mut f = Fake::new().with_ore();
    for buyout in [400, 420, 440] {
        f.market_lot(ORE, 20, buyout); // 20 to 22 each: no better than the vendor
    }
    let (_, status) = run_visit(&mut f, 2_000);
    assert_eq!(status, GoalStatus::Done);
    assert!(f.packets(op::CSAuctionPostPacket).is_empty());
    assert!(f.mem.skipped(ORE, f.now), "left alone for a while");
    assert_eq!(f.mem.stats.posted, 0);
    assert_eq!(f.st.bag.len(), 1);
}

#[test]
fn a_visit_without_goods_or_gold_does_nothing() {
    let mut f = Fake::new();
    let (run, status) = run_visit(&mut f, 100);
    assert_eq!(status, GoalStatus::Failed(FailReason::NoTarget));
    assert!(!run.reached_the_house());
    assert!(f.log.is_empty());
}

#[test]
fn no_auctioneer_in_reach_fails_the_visit() {
    let mut f = Fake::new().with_ore();
    f.spot = None;
    let (_, status) = run_visit(&mut f, 100);
    assert_eq!(status, GoalStatus::Failed(FailReason::NoTarget));
}

#[test]
fn an_auctioneer_that_is_not_there_is_reported_and_avoided() {
    let mut f = Fake::new().with_ore();
    f.st.units.clear();
    let (run, status) = run_visit(&mut f, 2_000);
    assert_eq!(status, GoalStatus::Failed(FailReason::NoTarget));
    assert!(!run.reached_the_house());
    let r = f.reported(IssueKind::NpcMissing);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].key, format!("template={NPC_TEMPLATE}"));
    assert!(f.mem.skipped(machine::spot_key(1000.0, 2000.0), f.now));
}

#[test]
fn too_far_makes_the_bot_interact_step_up_and_ask_again() {
    let mut f = Fake::new().with_ore();
    f.search = SearchMode::TooFar(1);
    let (_, status) = run_visit(&mut f, 3_000);
    assert_eq!(status, GoalStatus::Done, "{:?}", f.reports);
    let interact = f.packets(op::CSInteractNPCPacket);
    assert_eq!(interact.len(), 1);
    assert_eq!(&interact[0].body[..3], &[3, 2, 1]);
    // the first search was refused, the second (after stepping up) was answered
    assert!(f.packets(op::CSAuctionSearchPacket).len() >= 4);
    assert_eq!(f.mem.stats.posted, 1);
    assert!(f.reports.is_empty(), "{:?}", f.reports);
}

#[test]
fn too_far_again_after_stepping_up_is_reported_and_the_house_avoided() {
    let mut f = Fake::new().with_ore();
    f.search = SearchMode::TooFar(9);
    let (_, status) = run_visit(&mut f, 3_000);
    assert!(matches!(status, GoalStatus::Failed(_)), "{status:?}");
    assert_eq!(f.reported(IssueKind::Other).len(), 1);
    assert!(f.reported(IssueKind::Other)[0]
        .key
        .starts_with("auction-refused-in-range"));
    assert!(f.mem.skipped(machine::spot_key(1000.0, 2000.0), f.now));
}

#[test]
fn a_search_nobody_answers_is_tried_twice_and_reported() {
    let mut f = Fake::new().with_ore();
    f.search = SearchMode::Ignore;
    let (_, status) = run_visit(&mut f, 3_000);
    assert_eq!(status, GoalStatus::Failed(FailReason::Timeout));
    assert_eq!(f.packets(op::CSAuctionSearchPacket).len(), 2);
    assert_eq!(f.reported(IssueKind::NoResponse).len(), 1);
    assert_eq!(f.mem.stats.timeouts, 2);
}

#[test]
fn the_fee_error_stops_the_listing_and_nothing_else_is_asked() {
    let mut f = Fake::new().with_ore();
    f.st.bag.insert(1, item(11, 99, 3));
    f.facts.insert(99, facts(1_000, 500));
    f.names.insert(99, "Gold Bar".to_owned());
    f.stacks.push(Stack {
        item_id: 11,
        template: 99,
        count: 3,
    });
    f.post = PostMode::Error(wire::ERR_CANNOT_PUT_UP);
    let (_, status) = run_visit(&mut f, 3_000);
    assert_eq!(status, GoalStatus::Done);
    assert_eq!(
        f.packets(op::CSAuctionPostPacket).len(),
        1,
        "no second post"
    );
    assert_eq!(f.mem.stats.posted, 0);
    assert_eq!(f.mem.stats.refused, 1);
    assert!(f.reports.is_empty(), "241 is expected: {:?}", f.reports);
}

#[test]
fn a_bot_that_cannot_afford_the_fee_does_not_post() {
    let mut f = Fake::new().with_ore();
    f.st.me.money = 3;
    let (_, status) = run_visit(&mut f, 3_000);
    assert_eq!(status, GoalStatus::Done);
    assert!(f.packets(op::CSAuctionPostPacket).is_empty());
}

#[test]
fn a_post_that_takes_item_and_fee_without_an_answer_is_a_server_report() {
    let mut f = Fake::new().with_ore();
    f.post = PostMode::TakeSilently;
    let (_, status) = run_visit(&mut f, 3_000);
    assert_eq!(status, GoalStatus::Done);
    let r = f.reported(IssueKind::NoResponse);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].key, "auction-post-no-posted-packet");
    assert_eq!(r[0].suspect, crate::issues::Suspect::Server);
    assert_eq!(f.mem.stats.posted, 0);
    assert!(f.mem.skipped(ORE, f.now));
}

#[test]
fn a_silent_refusal_stays_a_local_note_and_stops_after_two() {
    let mut f = Fake::new().with_ore();
    for (slot, id, t) in [(1u16, 11u64, 98u32), (2, 12, 97)] {
        f.st.bag.insert(slot, item(id, t, 2));
        f.facts.insert(t, facts(600, 300));
        f.names.insert(t, format!("Thing {t}"));
        f.stacks.push(Stack {
            item_id: id,
            template: t,
            count: 2,
        });
    }
    f.post = PostMode::Ignore;
    let (_, status) = run_visit(&mut f, 5_000);
    assert_eq!(status, GoalStatus::Done);
    assert_eq!(
        f.packets(op::CSAuctionPostPacket).len(),
        2,
        "two, then it stops"
    );
    let r = f.reported(IssueKind::Other);
    assert!(!r.is_empty());
    assert!(r.iter().all(|r| r.suspect == crate::issues::Suspect::Bot));
    assert_eq!(f.mem.stats.timeouts, 2);
}

#[test]
fn a_posted_lot_the_house_does_not_list_is_reported() {
    let mut f = Fake::new().with_ore();
    f.post = PostMode::AcceptUnlisted;
    let (_, status) = run_visit(&mut f, 3_000);
    assert_eq!(status, GoalStatus::Done);
    let r = f.reported(IssueKind::Other);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].key, "auction-posted-lot-not-listed");
    assert!(f.mem.own().is_empty(), "forgotten without counting as sold");
    assert_eq!(f.mem.lots_gone, 0);
}

#[test]
fn lots_that_vanished_since_the_last_visit_are_counted_for_the_mail_detector() {
    let mut f = Fake::new();
    f.stacks.clear();
    f.budget = None;
    // an item to list so the visit has a reason
    f = f.with_ore();
    f.mem.note_own(OwnLot {
        id: 77,
        template: 55,
        count: 3,
        buyout: 900,
        posted_ms: 0,
        posted_here: true,
        seen: true,
    });
    let (_, status) = run_visit(&mut f, 3_000);
    assert_eq!(status, GoalStatus::Done);
    assert_eq!(f.mem.lots_gone, 1);
    assert_eq!(f.mem.missing_seller_mails(), 1);
}

// ---- buying ---------------------------------------------------------------------------------------

fn buyer() -> Fake {
    let mut f = Fake::new();
    f.st.me.money = 50_000;
    f.budget = Some(10_000);
    f
}

#[test]
fn a_bargain_in_the_list_is_bought_out_and_its_mail_expected() {
    let mut f = buyer();
    f.facts.insert(98, facts(500, 300));
    f.names.insert(98, "Silk".to_owned());
    // 5 silk: a vendor pays 1500; 900 is 60%
    let cheap = f.lot(98, 5, 900, 77);
    let id = cheap.id;
    f.market.push(cheap);
    // a fair one is left alone
    let fair = f.lot(98, 5, 1_600, 78);
    f.market.push(fair);
    let (_, status) = run_visit(&mut f, 3_000);
    assert_eq!(status, GoalStatus::Done, "{:?}", f.reports);
    let bids = f.packets(op::CSBidAuctionPacket);
    assert_eq!(bids.len(), 1);
    // the bid names the lot and the buyout price
    let body = &bids[0].body;
    assert!(body.windows(8).any(|w| w == id.to_le_bytes()));
    assert_eq!(f.st.me.money, 50_000 - 900);
    assert_eq!((f.mem.stats.bought, f.mem.stats.spent), (1, 900));
    assert_eq!(f.mem.waiting_wins(), 1, "the win mail is due");
    assert!(f.reports.is_empty(), "{:?}", f.reports);
}

#[test]
fn something_the_bot_can_use_is_bought_when_no_dearer_than_a_shop() {
    let mut f = buyer();
    f.facts.insert(60, facts(100, 30));
    f.names.insert(60, "Healing Potion".to_owned());
    f.useful.insert(60);
    let lot = f.lot(60, 2, 190, 77); // a shop asks 200
    f.market.push(lot);
    let (_, status) = run_visit(&mut f, 3_000);
    assert_eq!(status, GoalStatus::Done);
    assert_eq!(f.packets(op::CSBidAuctionPacket).len(), 1);
    // the same lot of an item the bot cannot use stays
    let mut g = buyer();
    g.facts.insert(60, facts(100, 30));
    g.names.insert(60, "Healing Potion".to_owned());
    let lot = g.lot(60, 2, 190, 77);
    g.market.push(lot);
    let (_, status) = run_visit(&mut g, 3_000);
    assert_eq!(status, GoalStatus::Done);
    assert!(g.packets(op::CSBidAuctionPacket).is_empty());
}

#[test]
fn a_bot_that_may_not_buy_never_browses_or_bids() {
    let mut f = Fake::new().with_ore();
    f.budget = None;
    f.facts.insert(98, facts(500, 300));
    f.names.insert(98, "Silk".to_owned());
    let cheap = f.lot(98, 5, 100, 77);
    f.market.push(cheap);
    let (_, status) = run_visit(&mut f, 3_000);
    assert_eq!(status, GoalStatus::Done);
    assert!(f.packets(op::CSBidAuctionPacket).is_empty());
    // own lots, the price of the ore, the check: no general browse (a search with no keyword
    // and no client id)
    let browses = f
        .packets(op::CSAuctionSearchPacket)
        .into_iter()
        .filter(|o| {
            let mut s = PacketStream::from_bytes(o.body.clone());
            s.read_bc().unwrap();
            s.read_bc().unwrap();
            let kw = s.read_string().unwrap();
            s.read_bool().unwrap();
            s.read_bytes(4).unwrap();
            s.read_i32().unwrap();
            let client = s.read_u32().unwrap();
            kw.is_empty() && client == 0
        })
        .count();
    assert_eq!(browses, 0);
}

#[test]
fn a_bargain_seen_while_pricing_my_own_goods_is_bought() {
    let mut f = Fake::new().with_ore();
    f.st.me.money = 50_000;
    f.budget = Some(10_000);
    // 20 ore at 5 each: a vendor pays 400, 100 is 25%
    let cheap = f.lot(ORE, 20, 100, 77);
    f.market.push(cheap);
    let (_, status) = run_visit(&mut f, 3_000);
    assert_eq!(status, GoalStatus::Done);
    assert_eq!(f.packets(op::CSBidAuctionPacket).len(), 1);
    // and the bot's own ore is not listed under that price: the market (5 each) is below the
    // vendor's 20
    assert!(f.packets(op::CSAuctionPostPacket).is_empty());
}

#[test]
fn a_lot_of_an_item_the_bot_may_not_trade_is_never_bought() {
    // a quest item (or a bound, or an unsellable one) priced at 60 % of what a vendor would
    // pay, and a potion the bot could use at the price of a shop: neither is bought
    let mut f = buyer();
    f.facts.insert(98, facts(500, 300));
    f.names.insert(98, "Dark Shadow Talisman".to_owned());
    f.facts.insert(60, facts(100, 30));
    f.names.insert(60, "Healing Potion".to_owned());
    f.useful.insert(60);
    f.untradable.extend([98, 60]);
    let cheap = f.lot(98, 5, 900, 77);
    f.market.push(cheap);
    let potion = f.lot(60, 2, 190, 77);
    f.market.push(potion);
    let (_, status) = run_visit(&mut f, 3_000);
    assert_eq!(status, GoalStatus::Done, "{:?}", f.reports);
    assert!(f.packets(op::CSBidAuctionPacket).is_empty());
    assert_eq!(f.st.me.money, 50_000);
    assert_eq!(f.mem.stats.bought, 0);
    // a tradable lot beside them is still found
    let ok = f.lot(98, 5, 900, 78);
    f.untradable.remove(&98);
    f.market.push(ok);
    let (_, status) = run_visit(&mut f, 3_000);
    assert_eq!(status, GoalStatus::Done, "{:?}", f.reports);
    assert_eq!(f.packets(op::CSBidAuctionPacket).len(), 1);
}

#[test]
fn a_bargain_among_my_own_goods_is_not_bought_when_the_item_may_not_be_traded() {
    let mut f = Fake::new().with_ore();
    f.st.me.money = 50_000;
    f.budget = Some(10_000);
    // 20 ore at 5 each, a vendor pays 400: a bargain, but the ore is not tradable here (and so
    // it is not listed either, whatever the stack list says)
    f.untradable.insert(ORE);
    let cheap = f.lot(ORE, 20, 100, 77);
    f.market.push(cheap);
    let (_, status) = run_visit(&mut f, 3_000);
    assert_eq!(status, GoalStatus::Done);
    assert!(f.packets(op::CSBidAuctionPacket).is_empty());
    assert!(f.packets(op::CSAuctionPostPacket).is_empty());
}

#[test]
fn a_buyout_that_takes_the_money_without_an_answer_is_reported() {
    let mut f = buyer();
    f.facts.insert(98, facts(500, 300));
    f.names.insert(98, "Silk".to_owned());
    let cheap = f.lot(98, 5, 900, 77);
    f.market.push(cheap);
    f.bid_answers = false;
    let (_, status) = run_visit(&mut f, 3_000);
    assert_eq!(status, GoalStatus::Done);
    let r = f.reported(IssueKind::NoResponse);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].key, "auction-bid-no-bid-packet");
    assert_eq!(f.mem.waiting_wins(), 1, "the item is still due");
}

#[test]
fn the_bot_never_buys_its_own_lot() {
    let mut f = buyer();
    f.facts.insert(98, facts(500, 300));
    f.names.insert(98, "Silk".to_owned());
    let mine = f.lot(98, 5, 100, ME);
    f.market.push(mine);
    let (_, status) = run_visit(&mut f, 3_000);
    assert_eq!(status, GoalStatus::Done);
    assert!(f.packets(op::CSBidAuctionPacket).is_empty());
}

// ---- cancelling -----------------------------------------------------------------------------------

#[test]
fn an_old_overpriced_lot_is_cancelled_and_its_mail_expected() {
    let mut f = Fake::new();
    f.st.bag.insert(1, item(11, ORE, 2));
    // something to list so the visit goes ahead: another item with no market
    f.facts.insert(98, facts(600, 300));
    f.names.insert(98, "Silk".to_owned());
    f.st.bag.insert(2, item(12, 98, 2));
    f.stacks.push(Stack {
        item_id: 12,
        template: 98,
        count: 2,
    });
    // my old lot of ore asks 50 each, the market 25
    let mine = f.lot(ORE, 20, 1_000, ME);
    let id = mine.id;
    f.own.push(mine);
    f.mem.note_own(OwnLot {
        id,
        template: ORE,
        count: 20,
        buyout: 1_000,
        posted_ms: 0,
        posted_here: true,
        seen: true,
    });
    for buyout in [500, 520, 540] {
        f.market_lot(ORE, 20, buyout);
    }
    // (find a seed whose roll decides to look at the lot)
    let mut cancelled = false;
    for seed in 0..40 {
        let mut g = Fake::new();
        g.now = 3 * 3600 * 1000;
        g.rng = fastrand::Rng::with_seed(seed);
        g.st = BotState::default();
        g.st.me = f.st.me.clone();
        g.st.units = f.st.units.clone();
        g.st.bag = f.st.bag.clone();
        g.facts = f.facts.clone();
        g.names = f.names.clone();
        g.stacks = f.stacks.clone();
        g.own = f.own.clone();
        g.market = f.market.clone();
        g.mem.note_own(f.mem.own()[0]);
        let (_, status) = run_visit(&mut g, 5_000);
        assert_eq!(status, GoalStatus::Done);
        if !g.packets(op::CSCancelAuctionPacket).is_empty() {
            assert_eq!(g.mem.stats.cancelled, 1);
            assert!(
                !g.own.iter().any(|l| l.template == ORE),
                "the old lot is gone"
            );
            assert_eq!(g.mem.lots_gone, 1, "the item comes back by mail");
            cancelled = true;
            break;
        }
    }
    assert!(cancelled, "a bot that rolls low cancels the overpriced lot");
}

#[test]
fn a_fairly_priced_old_lot_is_left_alone() {
    for seed in 0..20 {
        let mut f = Fake::new();
        f.now = 3 * 3600 * 1000;
        f.rng = fastrand::Rng::with_seed(seed);
        f.facts.insert(98, facts(600, 300));
        f.names.insert(98, "Silk".to_owned());
        f.st.bag.insert(2, item(12, 98, 2));
        f.stacks.push(Stack {
            item_id: 12,
            template: 98,
            count: 2,
        });
        let mine = f.lot(ORE, 20, 600, ME);
        let id = mine.id;
        f.own.push(mine);
        f.mem.note_own(OwnLot {
            id,
            template: ORE,
            count: 20,
            buyout: 600,
            posted_ms: 0,
            posted_here: true,
            seen: true,
        });
        for buyout in [580, 600, 620] {
            f.market_lot(ORE, 20, buyout);
        }
        let (_, status) = run_visit(&mut f, 5_000);
        assert_eq!(status, GoalStatus::Done);
        assert!(
            f.packets(op::CSCancelAuctionPacket).is_empty(),
            "seed {seed}"
        );
    }
}

// ---- memory bounds --------------------------------------------------------------------------------

#[test]
fn many_visits_do_not_grow_the_memory() {
    let mut f = Fake::new();
    for round in 0..30u32 {
        f.st.bag.clear();
        f.own.clear();
        f.market.clear();
        let t = 1_000 + round;
        f.facts.insert(t, facts(600, 300));
        f.names.insert(t, format!("Thing {round}"));
        f.st.bag.insert(0, item(u64::from(round) + 1, t, 2));
        f.stacks.clear();
        f.stacks.push(Stack {
            item_id: u64::from(round) + 1,
            template: t,
            count: 2,
        });
        let (_, status) = run_visit(&mut f, 3_000);
        assert_eq!(status, GoalStatus::Done, "round {round}");
    }
    assert_eq!(f.mem.stats.posted, 30);
    assert_eq!(f.mem.stats.visits, 30);
    assert!(f.mem.prices().len() <= crate::bot::market::auction::MAX_PRICES);
    assert!(f.mem.own().len() <= crate::bot::market::auction::MAX_OWN);
    // the packets of 30 visits are all answered (no queue is left behind)
    assert!(f.sent.is_empty());
}

#[test]
fn the_goal_is_small() {
    assert_eq!(
        std::mem::size_of::<Auction>(),
        std::mem::size_of::<usize>(),
        "one pointer while idle"
    );
}

// ---- the mail duty -------------------------------------------------------------------------------------

fn mail_event(id: i64, ty: u8, attachments: u8) -> ServerEvent {
    use aa_proto::sc_activity::MailHeader;
    ServerEvent::Activity(ActivityEvent::MailReceived {
        mail: Box::new(MailHeader {
            mail_id: id,
            mail_type: ty,
            status: 0,
            title: "".into(),
            sender: "".into(),
            attachments,
            receiver: "".into(),
            open_date: 0,
            returned: false,
            extra: 0,
        }),
        unread: [0, 1, 0, 0],
        is_cancel: false,
        body: None,
    })
}

#[test]
fn the_duty_collects_a_sale_mail_and_reports_what_the_detectors_find() {
    use crate::bot::market::auction::{SALE_MAIL_WAIT_MS, WIN_MAIL_WAIT_MS};
    let mut mem = AuctionMemory::default();
    let mut out = Vec::new();
    // nothing to do
    assert_eq!(duty_step(&mut mem, 1_000, &mut out), DutyOutcome::default());
    assert!(out.is_empty());
    // a sale mail comes: read, take, settled
    mem.observe(&mail_event(5, 14, 0), 2_000);
    duty_step(&mut mem, 2_000, &mut out);
    assert_eq!(out.pop().unwrap().opcode, op::CSReadMailPacket);
    let body = ServerEvent::Activity(ActivityEvent::MailBody {
        is_prepare: false,
        is_sent: false,
        body: Box::new(aa_proto::sc_activity::MailBodyInfo {
            mail_id: 5,
            mail_type: 14,
            receiver: "".into(),
            title: "".into(),
            text: "".into(),
            money: 540,
            billing: 0,
            money2: 0,
            send_date: 0,
            recv_date: 0,
            open_date: 0,
            items: Vec::new(),
        }),
        open_date_modified: true,
        unread: [0; 4],
    });
    mem.observe(&body, 2_100);
    duty_step(&mut mem, 2_100, &mut out);
    assert_eq!(out.pop().unwrap().opcode, op::CSTakeAttachmentSequentially);
    mem.observe(
        &ServerEvent::Activity(ActivityEvent::AttachmentTaken {
            mail_id: 5,
            money: true,
            aa_point: false,
            take_sequentially: true,
            items: Box::new([]),
        }),
        2_200,
    );
    let found = duty_step(&mut mem, 2_200, &mut out);
    let settled = found.settled.expect("collected");
    assert_eq!((settled.kind, settled.money), (Kind::Sold, 540));
    assert_eq!(mem.mail.counts.earned, 540);
    assert_eq!(duty_step(&mut mem, 2_300, &mut out).settled, None);

    // a buyout whose item never comes
    mem.expect_win(9, ORE, 10_000);
    assert_eq!(
        duty_step(&mut mem, 10_000 + WIN_MAIL_WAIT_MS - 1, &mut out).late_win,
        None
    );
    let late = duty_step(&mut mem, 10_000 + WIN_MAIL_WAIT_MS, &mut out).late_win;
    assert_eq!(late.map(|w| (w.lot_id, w.template)), Some((9, ORE)));
    assert_eq!(
        duty_step(&mut mem, 10_000 + WIN_MAIL_WAIT_MS + 1, &mut out).late_win,
        None
    );

    // a lot that is gone without a mail (the sale above had its lot)
    mem.lots_gone += 2;
    out.clear();
    let t0 = 1_000_000;
    assert_eq!(duty_step(&mut mem, t0, &mut out).overdue, None);
    assert_eq!(
        duty_step(&mut mem, t0 + SALE_MAIL_WAIT_MS, &mut out).overdue,
        None
    );
    assert_eq!(
        duty_step(&mut mem, t0 + SALE_MAIL_WAIT_MS + 1_000, &mut out).overdue,
        Some(1)
    );
    assert!(
        out.iter().any(|o| o.opcode == op::CSListMailPacket),
        "the mailbox was listed first"
    );
    // the issue reports of the detectors
    let r = report::sale_mail_missing(1);
    assert_eq!(r.kind, IssueKind::PaymentMissing);
    assert_eq!(r.suspect, crate::issues::Suspect::Server);
    let r = report::win_mail_missing(late.unwrap(), "Copper Ore");
    assert!(r.title.contains("never arrived") && r.actual.contains("Copper Ore"));
    let r = report::mail_fault(Fault::Unanswered(Kind::Sold));
    assert_eq!(r.kind, IssueKind::PaymentMissing);
}
