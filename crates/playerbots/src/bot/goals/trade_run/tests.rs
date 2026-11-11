//! State-machine tests: the run is driven against a fake [`Env`] with synthetic server events.

use std::cell::RefCell;

use aa_proto::sc::ServerEvent;
use aa_proto::sc_activity::{mail_type, ActivityEvent, MailBodyInfo, MailHeader};
use aa_proto::sc_world::{unit_type, DoodadInfo, ItemChange, ItemTaskInfo, UnitInfo, WorldEvent};
use rustc_hash::FxHashMap;
use smallvec::{smallvec, SmallVec};

use super::*;
use crate::bot::motion::MoveFailure;

const CRAFT: u32 = 6226;
const PACK: u32 = 31855;
const BENCH_TPL: u32 = 4233;
const BUYER_TPL: u32 = 14766;
const BENCH_OBJ: u32 = 0x777;
const BUYER_OBJ: u32 = 0x888;
const PACK_OBJ: u32 = 0x999;
const ME_OBJ: u32 = 0x400;
const BACKPACK_SLOT: u8 = 26;
const PACK_DOODAD: u32 = 6059;

struct Fake {
    now: u64,
    st: BotState,
    pos: [f32; 3],
    motion: MotionStatus,
    gm: bool,
    teleport: bool,
    crafts: FxHashMap<u32, Craft>,
    new_plan: Option<Plan>,
    carry_plan: Option<Plan>,
    out: Vec<Out>,
    gm_lines: Vec<String>,
    travels: Vec<(f32, f32, f32)>,
    said: RefCell<Vec<String>>,
}

fn craft() -> Craft {
    Craft {
        id: CRAFT,
        name: "Marianople Sweeteners".into(),
        skill_id: 16766,
        labor: 60,
        product: (PACK, 1),
        materials: vec![(4747, 1), (30901, 50)].into_boxed_slice(),
        is_pack: true,
        req_doodad: BENCH_TPL,
        benches: Box::new([BENCH_TPL]),
        req_level: 10,
        actability_group: 31,
        actability_limit: 0,
    }
}

fn plan() -> Plan {
    Plan {
        craft: CRAFT,
        item: PACK,
        bench_tpl: BENCH_TPL,
        buyer_tpl: BUYER_TPL,
        bench: [100.0, 0.0],
        buyer: [300.0, 0.0],
        price: 40_000,
        to_bench_m: 100.0,
        carry_m: 200.0,
    }
}

impl Fake {
    fn new(gm: bool) -> Fake {
        let mut st = BotState::new();
        st.me.obj = ME_OBJ;
        st.me.spawned = true;
        st.me.labor = 50;
        st.me.level = 10;
        st.me.hp = 1_000;
        st.me.hp_max = 1_000;
        let mut crafts = FxHashMap::default();
        crafts.insert(CRAFT, craft());
        Fake {
            now: 10_000,
            st,
            pos: [0.0, 0.0, 100.0],
            motion: MotionStatus::Idle,
            gm,
            teleport: false,
            crafts,
            new_plan: Some(plan()),
            carry_plan: None,
            out: Vec::new(),
            gm_lines: Vec::new(),
            travels: Vec::new(),
            said: RefCell::new(Vec::new()),
        }
    }

    fn feed(&mut self, ev: WorldEvent) {
        self.st.now_ms = self.now;
        self.st.apply(&ServerEvent::World(ev));
    }

    fn feed_activity(&mut self, ev: ActivityEvent) {
        self.st.now_ms = self.now;
        self.st.apply(&ServerEvent::Activity(ev));
    }

    /// The bot walks to where the last trip led.
    fn arrive(&mut self) {
        let (x, y, _) = *self.travels.last().expect("a trip was started");
        self.pos = [x, y, 100.0];
        self.motion = MotionStatus::Arrived;
    }

    fn give_craft_inputs(&mut self) {
        self.feed(bag_item(0, 4747, 1));
        self.feed(bag_item(1, 30901, 50));
        self.feed(WorldEvent::LaborChanged {
            delta: 80,
            actability: 0,
            point: 0,
            step: 0,
        });
    }

    fn show_bench(&mut self) {
        self.feed(WorldEvent::DoodadsCreated(smallvec![DoodadInfo {
            obj: BENCH_OBJ,
            template: BENCH_TPL,
            pos: (100.0, 0.0, 100.0),
            owner_obj: 0,
            parent_obj: 0,
            phase: 1,
            owner_id: 0,
            quest_glow: 0,
            has_loot: false,
        }]));
    }

    fn show_buyer(&mut self) {
        self.feed(WorldEvent::UnitState(Box::new(npc(
            BUYER_OBJ, BUYER_TPL, 300.0, 0.0,
        ))));
    }

    fn show_pack(&mut self, x: f32, y: f32) {
        self.feed(WorldEvent::DoodadsCreated(smallvec![DoodadInfo {
            obj: PACK_OBJ,
            template: PACK_DOODAD,
            pos: (x, y, 100.0),
            owner_obj: 0,
            parent_obj: 0,
            phase: 1,
            owner_id: 0,
            quest_glow: 0,
            has_loot: false,
        }]));
    }

    fn equip_pack(&mut self) {
        self.feed(WorldEvent::ItemTask(Box::new(ItemTaskInfo {
            reason: 29,
            changes: smallvec![ItemChange::Added {
                action: 5,
                slot_type: 1,
                slot: BACKPACK_SLOT,
                template: PACK,
                id: 9001,
                grade: 0,
                count: 1,
            }],
            force_removed: SmallVec::new(),
        })));
    }

    fn sell_pack(&mut self) {
        self.feed(WorldEvent::ItemTask(Box::new(ItemTaskInfo {
            reason: 104,
            changes: smallvec![ItemChange::Removed {
                slot_type: 1,
                slot: BACKPACK_SLOT,
                id: 9001,
                template: PACK,
            }],
            force_removed: SmallVec::new(),
        })));
    }

    fn error(&mut self, err: i16) {
        self.feed(WorldEvent::ErrorMsg {
            err,
            err2: err,
            kind: 0,
            notify: false,
        });
    }

    fn opcodes(&self) -> Vec<u16> {
        self.out.iter().map(|o| o.opcode).collect()
    }

    fn said_any(&self, needle: &str) -> bool {
        self.said.borrow().iter().any(|s| s.contains(needle))
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
    fn state_mut(&mut self) -> &mut BotState {
        &mut self.st
    }
    fn motion(&self) -> MotionStatus {
        self.motion
    }
    fn gm_allowed(&self) -> bool {
        self.gm
    }
    fn would_teleport(&self, _dist: f32) -> bool {
        self.teleport
    }
    fn craft(&self, id: u32) -> Option<&Craft> {
        self.crafts.get(&id)
    }
    fn is_pack(&self, item: u32) -> bool {
        item == PACK
    }
    fn is_pack_doodad(&self, template: u32) -> bool {
        template == PACK_DOODAD
    }
    fn doodad_use_skill(&self, _template: u32) -> u32 {
        0
    }
    fn min_pack_level(&self) -> u8 {
        10
    }
    fn name(&self, kind: Named, id: u32) -> String {
        format!("{kind:?}{id}")
    }
    fn plan_new(&mut self) -> Option<Plan> {
        self.new_plan
    }
    fn plan_carry(&mut self, item: u32) -> Option<Plan> {
        self.carry_plan.filter(|p| p.item == item)
    }
    fn send(&mut self, out: Out) {
        self.out.push(out);
    }
    fn gm(&mut self, line: &str) -> bool {
        if self.gm {
            // every line the goal writes must plan into a form that can only hit the bot itself
            // (the live `Ctx::gm` does exactly this)
            let wire = crate::gm::plan(line, "Tester")
                .unwrap_or_else(|e| panic!("the goal wrote {line:?}: {e}"));
            assert!(wire.is_self_only("Tester"), "{line:?} -> {wire:?}");
            self.gm_lines.push(line.to_owned());
        }
        self.gm
    }
    fn travel(&mut self, x: f32, y: f32, radius: f32) {
        self.travels.push((x, y, radius));
        self.motion = MotionStatus::Moving;
    }
    fn stop(&mut self) {
        if self.motion == MotionStatus::Moving {
            self.motion = MotionStatus::Idle;
        }
    }
    fn face(&mut self, _x: f32, _y: f32) {}
    fn say(&self, text: &str) {
        self.said.borrow_mut().push(text.to_owned());
    }
}

fn bag_item(slot: u8, template: u32, count: i32) -> WorldEvent {
    WorldEvent::ItemTask(Box::new(ItemTaskInfo {
        reason: 0,
        changes: smallvec![ItemChange::Added {
            action: 5,
            slot_type: 2,
            slot,
            template,
            id: 100 + u64::from(slot),
            grade: 0,
            count,
        }],
        force_removed: SmallVec::new(),
    }))
}

fn npc(obj: u32, template: u32, x: f32, y: f32) -> UnitInfo {
    UnitInfo {
        obj,
        unit_type: unit_type::NPC,
        id: template,
        name: "".into(),
        owner_name: "".into(),
        owner_id: 0,
        tl_id: 0,
        pos: (x, y, 100.0),
        level: 10,
        hp: 100,
        mp: 0,
        dead: false,
        has_loot: false,
        faction_id: 1,
        expedition_id: 0,
        yaw: 0,
        skills: Box::new([]),
        buffs: Box::new([]),
        backpack: 0,
        complete: true,
    }
}

fn run() -> TradeRun {
    TradeRun {
        end_ms: u64::MAX,
        ..TradeRun::default()
    }
}

/// Steps the goal `n` times, half a second apart.
fn steps(g: &mut TradeRun, f: &mut Fake, n: u32) -> GoalStatus {
    let mut last = GoalStatus::Running;
    for _ in 0..n {
        last = g.step(f);
        f.now += 500;
        if last != GoalStatus::Running {
            break;
        }
    }
    last
}

fn failure(s: GoalStatus) -> &'static str {
    match s {
        GoalStatus::Failed(FailReason::Other(why)) => why,
        other => panic!("expected a failure with a reason, got {other:?}"),
    }
}

// ---- the whole run --------------------------------------------------------------------------

#[test]
fn a_gm_run_crafts_carries_sells_and_waits_for_the_payment() {
    use aa_proto::opcodes::cs as op;
    let mut f = Fake::new(true);
    let mut g = run();

    // plan -> supply: exactly the missing materials and labor are given
    assert_eq!(steps(&mut g, &mut f, 2), GoalStatus::Running);
    assert_eq!(g.phase, Phase::Supply);
    assert_eq!(
        f.gm_lines,
        vec![
            "/item add self 4747 1",
            "/item add self 30901 50",
            "/labor 74", // 60 craft + 60 sale + 4 margin - 50
        ]
    );
    // nothing is sent twice while the supply is on its way
    steps(&mut g, &mut f, 3);
    assert_eq!(f.gm_lines.len(), 3);
    assert_eq!(g.phase, Phase::Supply);

    // the supply lands: walk to the bench
    f.give_craft_inputs();
    steps(&mut g, &mut f, 1);
    assert_eq!(g.phase, Phase::ToBench);
    steps(&mut g, &mut f, 1);
    assert_eq!(*f.travels.last().unwrap(), (100.0, 0.0, BENCH_STOP_M));
    assert!(
        g.text.starts_with("Trade run: going to Doodad4233"),
        "{}",
        g.text
    );

    // at the spawn, the bench object is not in view yet
    f.arrive();
    steps(&mut g, &mut f, 3);
    assert_eq!(g.phase, Phase::ToBench);
    f.show_bench();
    steps(&mut g, &mut f, 2);
    assert_eq!(g.phase, Phase::Crafting);
    assert_eq!(
        f.out.last().unwrap(),
        &cs::execute_craft(CRAFT, BENCH_OBJ, 1)
    );
    assert_eq!(g.text.as_ref(), "Trade run: crafting Item31855");

    // the pack lands in the backpack slot
    steps(&mut g, &mut f, 3);
    assert_eq!(g.phase, Phase::Crafting);
    f.equip_pack();
    steps(&mut g, &mut f, 2);
    assert_eq!(g.phase, Phase::ToBuyer);
    assert_eq!(*f.travels.last().unwrap(), (300.0, 0.0, NPC_STOP_M));
    assert!(g
        .text
        .starts_with("Trade run: carrying Item31855 to Npc14766"));

    // the buyer is in view when the bot gets there
    f.arrive();
    f.show_buyer();
    steps(&mut g, &mut f, 3);
    assert_eq!(g.phase, Phase::Selling);
    assert_eq!(f.out.last().unwrap(), &cs::sell_backpack_goods(BUYER_OBJ));

    // the pack leaves the backpack: sold, payment pending
    f.sell_pack();
    steps(&mut g, &mut f, 1);
    assert_eq!(g.phase, Phase::Payment);
    assert_eq!(f.st.econ.awaiting(), 1);
    assert_eq!(steps(&mut g, &mut f, 3), GoalStatus::Running);

    // the payment mail arrives and the background duty collects it
    f.feed_activity(got_mail(55));
    let mut out = Vec::new();
    f.st.econ.tick(f.now, &mut out);
    assert_eq!(out.last().unwrap().opcode, op::CSReadMailPacket);
    f.feed_activity(mail_body(55, 30_000));
    f.st.econ.tick(f.now + 100, &mut out);
    assert_eq!(out.last().unwrap().opcode, op::CSTakeAttachmentSequentially);
    f.feed_activity(ActivityEvent::AttachmentTaken {
        mail_id: 55,
        money: true,
        aa_point: false,
        take_sequentially: true,
        items: Box::new([]),
    });
    assert_eq!(steps(&mut g, &mut f, 1), GoalStatus::Done);
    assert_eq!(f.st.econ.earned, 30_000);
}

fn got_mail(id: i64) -> ActivityEvent {
    ActivityEvent::MailReceived {
        mail: Box::new(MailHeader {
            mail_id: id,
            mail_type: mail_type::SYS_SELL_BACKPACK,
            status: 0,
            title: "".into(),
            sender: "".into(),
            attachments: 1,
            receiver: "".into(),
            open_date: 0,
            returned: false,
            extra: 0,
        }),
        unread: [0, 1, 0, 0],
        is_cancel: false,
        body: None,
    }
}

fn mail_body(id: i64, money: i32) -> ActivityEvent {
    ActivityEvent::MailBody {
        is_prepare: false,
        is_sent: false,
        body: Box::new(MailBodyInfo {
            mail_id: id,
            mail_type: mail_type::SYS_SELL_BACKPACK,
            receiver: "".into(),
            title: "".into(),
            text: "".into(),
            money,
            billing: 0,
            money2: 0,
            send_date: 0,
            recv_date: 0,
            open_date: 0,
            items: Vec::new(),
        }),
        open_date_modified: true,
        unread: [0; 4],
    }
}

#[test]
fn without_gm_a_bot_that_cannot_pay_for_the_craft_fails_with_a_reason() {
    let mut f = Fake::new(false);
    let mut g = run();
    let s = steps(&mut g, &mut f, 4);
    assert_eq!(failure(s), "not enough labor");
    assert!(f.gm_lines.is_empty());
    assert!(f.said_any("trade run failed: not enough labor"));
}

#[test]
fn trade_packs_need_level_10_and_a_gm_bot_levels_up_first() {
    // without the GM a level 1 bot does not even plan a craft
    let mut f = Fake::new(false);
    f.st.me.level = 1;
    let mut g = run();
    assert_eq!(
        failure(steps(&mut g, &mut f, 2)),
        "trade packs need a higher character level"
    );
    assert!(f.new_plan.is_some(), "no plan was drawn");
    // a bot that carries a pack sells it at any level
    let (mut f, mut g) = at_the_buyer();
    f.st.me.level = 1;
    g.phase = Phase::Plan;
    assert_eq!(steps(&mut g, &mut f, 2), GoalStatus::Running);
    assert!(matches!(g.phase, Phase::ToBuyer | Phase::Sell));
    // the GM bot asks for the level together with the rest
    let mut f = Fake::new(true);
    f.st.me.level = 1;
    f.give_craft_inputs();
    let mut g = run();
    steps(&mut g, &mut f, 3);
    assert_eq!(g.phase, Phase::Supply);
    assert_eq!(f.gm_lines, vec!["/level 10"]);
    f.feed(WorldEvent::LevelChanged {
        obj: ME_OBJ,
        level: 10,
    });
    steps(&mut g, &mut f, 1);
    assert_eq!(g.phase, Phase::ToBench);
}

#[test]
fn without_gm_a_bot_with_everything_goes_straight_to_the_bench() {
    let mut f = Fake::new(false);
    f.give_craft_inputs();
    let mut g = run();
    steps(&mut g, &mut f, 3);
    assert_eq!(g.phase, Phase::ToBench);
    assert!(f.gm_lines.is_empty());
}

#[test]
fn no_route_fails_with_no_target() {
    let mut f = Fake::new(true);
    f.new_plan = None;
    let mut g = run();
    assert_eq!(
        steps(&mut g, &mut f, 2),
        GoalStatus::Failed(FailReason::NoTarget)
    );
}

#[test]
fn gm_supply_that_never_lands_gives_up_after_two_asks() {
    let mut f = Fake::new(true);
    let mut g = run();
    let s = steps(&mut g, &mut f, 40);
    assert_eq!(failure(s), "the GM supply did not arrive");
    assert_eq!(f.gm_lines.len(), 6);
}

// ---- crafting -------------------------------------------------------------------------------

/// A fake that is standing at the bench with everything, one step from sending the craft.
fn at_the_bench() -> (Fake, TradeRun) {
    let mut f = Fake::new(true);
    f.give_craft_inputs();
    f.show_bench();
    f.pos = [99.0, 0.0, 100.0];
    let mut g = run();
    // plan, supply (nothing missing), to bench (already there), craft
    steps(&mut g, &mut f, 4);
    assert_eq!(g.phase, Phase::Crafting, "{:?}", g.phase);
    (f, g)
}

#[test]
fn a_refused_craft_is_retried_closer_and_finally_quarantined() {
    let (mut f, mut g) = at_the_bench();
    for attempt in 1..=MAX_RETRIES {
        f.error(ledger::ERR_CRAFT_STOPPED);
        let s = steps(&mut g, &mut f, 1);
        if attempt < MAX_RETRIES {
            // back off, walk closer, try again
            assert_eq!(s, GoalStatus::Running);
            assert_eq!(g.phase, Phase::ToBench);
            assert!(g.close);
            f.now += 20_000;
            f.pos = [99.0, 0.0, 100.0];
            f.motion = MotionStatus::Idle;
            steps(&mut g, &mut f, 1);
            assert_eq!(*f.travels.last().unwrap(), (100.0, 0.0, BENCH_CLOSE_M));
            f.arrive();
            steps(&mut g, &mut f, 3);
            assert_eq!(g.phase, Phase::Crafting);
            assert_eq!(f.out.len(), usize::from(attempt) + 1);
        } else {
            assert_eq!(failure(s), "the workbench refused the craft");
        }
    }
    assert!(f.st.econ.skipped(SkipKind::Craft, CRAFT, f.now));
    assert!(f
        .st
        .econ
        .skipped(SkipKind::Bench, spot_key(100.0, 0.0), f.now));
}

#[test]
fn a_craft_that_gives_no_answer_counts_as_refused() {
    let (mut f, mut g) = at_the_bench();
    f.now += CRAFT_WAIT_MS + 1_000;
    steps(&mut g, &mut f, 1);
    assert_eq!(g.phase, Phase::ToBench);
    assert_eq!(g.retries, 1);
}

#[test]
fn spent_labor_sends_a_gm_bot_back_to_supply() {
    let (mut f, mut g) = at_the_bench();
    // another craft ate the labor
    f.feed(WorldEvent::LaborChanged {
        delta: -100,
        actability: 0,
        point: 0,
        step: 0,
    });
    f.error(ledger::ERR_CRAFT_STOPPED);
    steps(&mut g, &mut f, 1);
    assert_eq!(g.phase, Phase::Supply);
    f.gm_lines.clear();
    steps(&mut g, &mut f, 1);
    assert_eq!(f.gm_lines, vec!["/labor 94"]);
}

#[test]
fn a_bench_that_never_shows_up_is_quarantined() {
    let mut f = Fake::new(true);
    f.give_craft_inputs();
    let mut g = run();
    steps(&mut g, &mut f, 3);
    assert_eq!(g.phase, Phase::ToBench);
    f.arrive();
    let s = steps(&mut g, &mut f, 40);
    assert_eq!(s, GoalStatus::Failed(FailReason::NoTarget));
    assert!(f
        .st
        .econ
        .skipped(SkipKind::Bench, spot_key(100.0, 0.0), f.now));
    assert!(f.said_any("the workbench is not there"));
}

// ---- carrying and selling -------------------------------------------------------------------

/// A fake that carries the pack and stands next to the buyer, one step from selling.
fn at_the_buyer() -> (Fake, TradeRun) {
    let mut f = Fake::new(true);
    f.equip_pack();
    f.carry_plan = Some(Plan {
        craft: 0,
        bench_tpl: 0,
        to_bench_m: 0.0,
        ..plan()
    });
    f.feed(WorldEvent::LaborChanged {
        delta: 20,
        actability: 0,
        point: 0,
        step: 0,
    });
    f.show_buyer();
    f.pos = [299.0, 0.0, 100.0];
    let mut g = run();
    steps(&mut g, &mut f, 4);
    assert_eq!(g.phase, Phase::Selling, "{:?}", g.phase);
    (f, g)
}

#[test]
fn a_hurt_bot_rests_before_crafting() {
    let mut f = Fake::new(true);
    f.st.me.hp = 500; // 50 %
    let mut g = run();
    steps(&mut g, &mut f, 3);
    assert_eq!(g.phase, Phase::Plan);
    assert!(f.new_plan.is_some(), "no plan was drawn while hurt");
    assert!(f.said_any("resting first (50% hit points)"));
    // healthy again: the run is planned
    f.st.me.hp = 1_000;
    steps(&mut g, &mut f, 2);
    assert_eq!(g.phase, Phase::Supply);
}

#[test]
fn a_dropped_pack_is_fetched_and_the_run_goes_on() {
    use aa_proto::opcodes::cs as op;
    let mut f = Fake::new(true);
    f.st.econ
        .note_death_with_pack(PACK, [100.0, 0.0, 100.0], 0, f.now);
    f.carry_plan = Some(Plan {
        craft: 0,
        bench_tpl: 0,
        to_bench_m: 0.0,
        ..plan()
    });
    f.pos = [0.0, 0.0, 100.0];
    let mut g = run();
    // the death spot is far: walk back there first
    steps(&mut g, &mut f, 2);
    assert_eq!(g.phase, Phase::Recover);
    assert_eq!(
        *f.travels.last().unwrap(),
        (100.0, 0.0, ledger::PACK_RETURN_M)
    );
    assert!(g
        .text
        .starts_with("Trade run: going back for the dropped pack"));
    f.arrive();
    // nothing in view yet: wait, then the doodad appears
    steps(&mut g, &mut f, 2);
    assert_eq!(g.phase, Phase::Recover);
    f.show_pack(100.0, 0.0);
    steps(&mut g, &mut f, 1);
    assert_eq!(
        f.out.last().unwrap(),
        &cs::start_skill_doodad(PACK_RECOVER_SKILL, ME_OBJ, PACK_OBJ)
    );
    assert_eq!(f.out.last().unwrap().opcode, op::CSStartSkillPacket);
    // the pack is in the back slot: the run carries it to the buyer
    f.equip_pack();
    steps(&mut g, &mut f, 3);
    assert!(f.st.econ.lost_pack().is_none());
    assert!(f.said_any("picked the dropped pack back up"));
    assert_eq!(g.phase, Phase::ToBuyer);
    assert_eq!(*f.travels.last().unwrap(), (300.0, 0.0, NPC_STOP_M));
}

#[test]
fn a_dropped_pack_in_view_from_afar_is_walked_up_to_before_the_pickup() {
    let mut f = Fake::new(true);
    f.st.econ
        .note_death_with_pack(PACK, [100.0, 0.0, 100.0], 0, f.now);
    f.carry_plan = Some(Plan {
        craft: 0,
        bench_tpl: 0,
        to_bench_m: 0.0,
        ..plan()
    });
    // 20 m short of the death spot: inside the return radius, the doodad already in view
    f.pos = [80.0, 0.0, 100.0];
    f.show_pack(100.0, 0.0);
    let mut g = run();
    steps(&mut g, &mut f, 2);
    assert_eq!(g.phase, Phase::Recover);
    // no skill is sent from out of reach: the bot walks to the doodad first
    assert!(
        !f.out
            .iter()
            .any(|o| o.opcode == aa_proto::opcodes::cs::CSStartSkillPacket),
        "the pickup was tried from 20 m away"
    );
    assert_eq!(
        *f.travels.last().expect("a walk to the doodad"),
        (100.0, 0.0, PACK_STOP_M)
    );
    // at the doodad the pickup is sent
    f.arrive();
    steps(&mut g, &mut f, 2);
    assert_eq!(
        f.out.last().unwrap(),
        &cs::start_skill_doodad(PACK_RECOVER_SKILL, ME_OBJ, PACK_OBJ)
    );
}

#[test]
fn a_dropped_pack_that_is_gone_is_logged_and_counted() {
    let mut f = Fake::new(true);
    f.st.econ
        .note_death_with_pack(PACK, [100.0, 0.0, 100.0], 0, f.now);
    f.pos = [110.0, 0.0, 100.0]; // at the death spot
    let mut g = run();
    steps(&mut g, &mut f, 2);
    assert_eq!(g.phase, Phase::Recover);
    // the doodad never shows up
    f.now += ledger::PACK_APPEAR_MS + 1_000;
    let s = steps(&mut g, &mut f, 1);
    assert_eq!(s, GoalStatus::Failed(FailReason::NoTarget));
    assert!(f.said_any("the dropped pack is gone"));
    assert!(f.said_any("pack lost (Item31855; taken or decayed)"));
    assert_eq!(f.st.econ.packs_lost, 1);
    assert!(f.st.econ.lost_pack().is_none());
}

#[test]
fn a_bot_that_already_carries_a_pack_skips_the_craft() {
    let (f, g) = at_the_buyer();
    assert_eq!(f.opcodes().len(), 1);
    assert!(f.gm_lines.is_empty());
    assert!(g.plan.unwrap().craft == 0);
    assert!(f.said_any("carrying Item31855"));
}

#[test]
fn a_sale_that_is_too_far_is_retried_closer() {
    let (mut f, mut g) = at_the_buyer();
    f.error(ERR_TOO_FAR_AWAY);
    steps(&mut g, &mut f, 1);
    assert_eq!(g.phase, Phase::ToBuyer);
    assert!(g.close);
    steps(&mut g, &mut f, 1);
    assert_eq!(*f.travels.last().unwrap(), (300.0, 0.0, NPC_CLOSE_M));
    f.arrive();
    steps(&mut g, &mut f, 3);
    assert_eq!(g.phase, Phase::Selling);
    f.sell_pack();
    steps(&mut g, &mut f, 1);
    assert_eq!(g.phase, Phase::Payment);
}

#[test]
fn a_buyer_that_refuses_the_pack_is_quarantined() {
    let (mut f, mut g) = at_the_buyer();
    f.error(ERR_CANT_SELL_HERE);
    let s = steps(&mut g, &mut f, 1);
    assert_eq!(failure(s), "the buyer refused the pack");
    assert!(f.st.econ.skipped(SkipKind::Buyer, BUYER_TPL, f.now));
}

#[test]
fn a_sale_that_gets_no_answer_is_tried_again_then_given_up() {
    let (mut f, mut g) = at_the_buyer();
    let mut last = GoalStatus::Running;
    for _ in 0..4 {
        f.now += SELL_WAIT_MS + 1_000;
        last = steps(&mut g, &mut f, 3);
        if last != GoalStatus::Running {
            break;
        }
    }
    assert_eq!(last, GoalStatus::Failed(FailReason::Timeout));
    assert!(f.st.econ.skipped(SkipKind::Buyer, BUYER_TPL, f.now));
    assert!(f.opcodes().len() >= 2);
}

#[test]
fn a_lost_pack_ends_the_run() {
    let (mut f, mut g) = at_the_buyer();
    // the pack is gone while the bot is still walking to the buyer
    f.sell_pack();
    g.phase = Phase::ToBuyer;
    let s = steps(&mut g, &mut f, 1);
    assert_eq!(failure(s), "the trade pack is gone");
}

#[test]
fn a_gm_bot_tops_up_the_labor_for_the_sale() {
    let mut f = Fake::new(true);
    f.equip_pack();
    f.carry_plan = Some(Plan {
        craft: 0,
        bench_tpl: 0,
        to_bench_m: 0.0,
        ..plan()
    });
    f.show_buyer();
    f.pos = [299.0, 0.0, 100.0];
    f.st.me.labor = 10;
    let mut g = run();
    steps(&mut g, &mut f, 4);
    assert_eq!(g.phase, Phase::Sell);
    assert_eq!(f.gm_lines, vec!["/labor 54"]);
    f.feed(WorldEvent::LaborChanged {
        delta: 54,
        actability: 0,
        point: 0,
        step: 0,
    });
    steps(&mut g, &mut f, 2);
    assert_eq!(g.phase, Phase::Selling);
}

#[test]
fn the_payment_phase_ends_after_a_while_and_leaves_the_mail_to_the_background() {
    let (mut f, mut g) = at_the_buyer();
    f.sell_pack();
    steps(&mut g, &mut f, 1);
    assert_eq!(g.phase, Phase::Payment);
    assert_eq!(steps(&mut g, &mut f, 10), GoalStatus::Running);
    f.now += PAYMENT_WAIT_MS;
    assert_eq!(steps(&mut g, &mut f, 1), GoalStatus::Done);
    assert_eq!(f.st.econ.awaiting(), 1, "the payment is still outstanding");
    assert!(f.said_any("will be collected when it arrives"));
}

// ---- walking --------------------------------------------------------------------------------

#[test]
fn long_walks_are_split_into_legs() {
    let mut f = Fake::new(true);
    f.give_craft_inputs();
    f.new_plan = Some(Plan {
        bench: [1_000.0, 0.0],
        to_bench_m: 1_000.0,
        ..plan()
    });
    let mut g = run();
    steps(&mut g, &mut f, 3);
    assert_eq!(g.phase, Phase::ToBench);
    let (x, y, r) = *f.travels.last().unwrap();
    assert!(
        (x - LEG_M).abs() < 0.5 && y.abs() < 0.01 && r == 1.0,
        "{x} {y} {r}"
    );
    // the leg ends; the next one continues from there
    f.arrive();
    steps(&mut g, &mut f, 1);
    let (x2, _, _) = *f.travels.last().unwrap();
    assert!((x2 - 2.0 * LEG_M).abs() < 1.0, "{x2}");
    // while a leg is under way nothing new starts
    let n = f.travels.len();
    steps(&mut g, &mut f, 5);
    assert_eq!(f.travels.len(), n);
}

#[test]
fn a_gm_move_covers_the_whole_way_at_once() {
    let mut f = Fake::new(true);
    f.teleport = true;
    f.give_craft_inputs();
    f.new_plan = Some(Plan {
        bench: [5_000.0, 0.0],
        to_bench_m: 5_000.0,
        ..plan()
    });
    let mut g = run();
    steps(&mut g, &mut f, 3);
    assert_eq!(*f.travels.last().unwrap(), (5_000.0, 0.0, BENCH_STOP_M));
}

#[test]
fn a_trip_that_keeps_failing_ends_the_run_and_quarantines_the_bench() {
    let mut f = Fake::new(true);
    f.give_craft_inputs();
    let mut g = run();
    steps(&mut g, &mut f, 3);
    assert_eq!(g.phase, Phase::ToBench);
    let mut last = GoalStatus::Running;
    for _ in 0..6 {
        f.motion = MotionStatus::Failed(MoveFailure::Stuck);
        last = steps(&mut g, &mut f, 1);
        if last != GoalStatus::Running {
            break;
        }
    }
    assert_eq!(last, GoalStatus::Failed(FailReason::Stuck));
    assert!(f
        .st
        .econ
        .skipped(SkipKind::Bench, spot_key(100.0, 0.0), f.now));
}

#[test]
fn the_run_times_out() {
    let mut f = Fake::new(true);
    let mut g = TradeRun {
        end_ms: f.now + 1_000,
        ..TradeRun::default()
    };
    assert_eq!(steps(&mut g, &mut f, 1), GoalStatus::Running);
    f.now += 2_000;
    assert_eq!(
        steps(&mut g, &mut f, 1),
        GoalStatus::Failed(FailReason::Timeout)
    );
}

#[test]
fn money_is_formatted_in_gold_silver_copper() {
    assert_eq!(coins(0), "0c");
    assert_eq!(coins(48), "48c");
    assert_eq!(coins(3_048), "30s 48c");
    assert_eq!(coins(33_048), "3g 30s 48c");
    assert_eq!(length(640.0), "640 m");
    assert_eq!(length(1_234.0), "1.2 km");
}

#[test]
fn sizes_stay_small() {
    use std::mem::size_of;
    assert!(
        size_of::<Ledger>() <= 96,
        "Ledger is {}",
        size_of::<Ledger>()
    );
    assert!(
        size_of::<TradeRun>() <= 112,
        "TradeRun is {}",
        size_of::<TradeRun>()
    );
}

#[test]
fn the_carried_pack_is_found_in_the_equipment() {
    let slot = |slot: u8, template: u32| aa_proto::sc_world::EquipSlot {
        slot,
        template,
        id: 1,
        grade: 0,
    };
    let is_pack = |t: u32| t == PACK;
    assert_eq!(carried_pack(&[], is_pack), None);
    assert_eq!(carried_pack(&[slot(2, 77), slot(3, 78)], is_pack), None);
    assert_eq!(
        carried_pack(&[slot(2, 77), slot(BACKPACK_SLOT, PACK)], is_pack),
        Some(PACK)
    );
}
