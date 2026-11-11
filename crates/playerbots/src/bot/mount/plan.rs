//! The pure decisions of the mount and glider feature: when to summon, ride, leave and dismiss
//! a mate, when to put a glider on, and when a ledge is worth gliding down.
//!
//! Nothing here reads a clock or sends a packet: the caller fills a [`Facts`] and gets at most
//! two [`Act`]s, and every wait has a deadline and a back-off. The glue that turns the acts into
//! packets and motion is [`super::duty`].

use smallvec::SmallVec;

use crate::bot::travel::journey::SkillNote;

/// How close the bot must be to its mate to mount it: the server refuses a mount further than
/// 6 m (`systems/vehicles/mates.rs:331`, `MOUNT_REACH`); the bot stays a metre inside that.
pub const MOUNT_REACH_M: f32 = 5.0;
/// How long the mate's unit state may take after `SCMateSpawned` before it is considered lost.
pub const MATE_OBJ_WAIT_MS: u64 = 3_000;
/// How long a summon cast may take to produce `SCMateSpawned` (the skill starts after 100 ms;
/// this leaves room for a slow connection).
pub const SUMMON_WAIT_MS: u64 = 8_000;
/// How long a granted item may take to appear in the bag (the GM command answers with an item
/// task).
pub const ITEM_WAIT_MS: u64 = 5_000;
/// How long an equip request may take to show on the character.
pub const EQUIP_WAIT_MS: u64 = 4_000;
/// How long `CSMountMate` may take to answer `SCUnitAttached`.
pub const MOUNT_WAIT_MS: u64 = 5_000;
/// How long `CSUnMountMate` may take to answer `SCUnitDetached`.
pub const DISMOUNT_WAIT_MS: u64 = 5_000;
/// After a dismount the server did not answer, the next one is not sent for this long.
pub const DISMOUNT_RETRY_MS: u64 = 30_000;
/// How long `CSRemoveMate` may take before the mate is forgotten (the server sends no answer).
pub const DESPAWN_WAIT_MS: u64 = 3_000;
/// After a refusal the same request is not sent again for this long.
pub const REFUSE_MS: u64 = 2 * 60_000;
/// Without an item and with no GM rights no summon is tried for this long.
pub const NO_ITEM_MS: u64 = 30 * 60_000;
/// After a mount or dismount timed out, the next try waits this long.
pub const RETRY_MS: u64 = 60_000;
/// After a dismissed mate, a new summon waits this long (the server's mate record may still be
/// settling).
pub const SUMMON_RETRY_MS: u64 = 30_000;
/// After a glide, no new ledge is taken for this long.
pub const GLIDE_GAP_MS: u64 = 5_000;
/// A glider cast or deploy refusal stops the glider for this long.
pub const GLIDE_REFUSE_MS: u64 = 2 * 60_000;
/// A glide is only started with at least this much trip left (metres).
pub const GLIDE_MIN_REMAINING_M: f32 = 20.0;
/// A glide is only started when the edge is this close ahead (metres): the bot runs the last
/// steps along the ground and glides when the ground falls away, so a cliff 30 m ahead is no
/// reason to start (the glide would end on the level ground in front of it).
pub const GLIDE_START_EDGE_M: f32 = 6.0;
/// Two samples of the terrain closer than this below the step before are a cliff, not a slope
/// (metres per [`LEDGE_STEP_M`]).
const LEDGE_STEP_M: f32 = 2.0;
const LEDGE_DROP_PER_SAMPLE_M: f32 = 0.8;
/// The ground just before the drop must still be this close to the start height (metres).
const LEDGE_RUN_M: f32 = 1.5;
/// A rise this much above the start height blocks the line (metres).
const LEDGE_WALL_M: f32 = 1.0;
/// Terrain below this is sea: a glide may not land in it (`motion::SWIM_THRESHOLD`).
pub const DRY_LAND_M: f32 = 99.0;

/// The settings the decisions use (from `[mounts]`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rules {
    /// Bots summon and ride mounts (`[mounts] mounts`).
    pub mounts: bool,
    /// Bots equip a glider and glide (`[mounts] gliders`).
    pub gliders: bool,
    /// The shortest trip a mount is summoned for, metres.
    pub summon_over_m: f32,
    /// A mounted bot dismounts when the trip left is this short, metres.
    pub dismount_under_m: f32,
    /// Dismiss a standing mate after this long without a trip, ms (0 = never).
    pub idle_despawn_ms: u64,
    /// A drop at least this deep may be glided, metres.
    pub glide_drop_m: f32,
    /// How far ahead the ground is sampled, metres.
    pub glide_lookahead_m: f32,
}

/// One item the bot has: instance, template, the use skill and the bag slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemRef {
    /// Item instance id.
    pub item_id: u64,
    /// Item template.
    pub template: u32,
    /// The use skill from the game data (0 when unknown).
    pub skill: u32,
    /// Bag slot the item sits in.
    pub bag_slot: u16,
}

/// A ledge found ahead: where to go and how far the ground falls.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ledge {
    /// Unit direction of the glide (x, y).
    pub dir: (f32, f32),
    /// Ground height at the first sample past the edge.
    pub ground: f32,
    /// The fall from the bot's height (metres).
    pub drop_m: f32,
    /// Distance along the heading to the first sample past the edge (metres).
    pub edge_m: f32,
}

/// What the caller knows this tick.
#[derive(Debug, Clone, Copy)]
pub struct Facts {
    /// Bot clock, ms.
    pub now_ms: u64,
    /// The settings.
    pub rules: Rules,
    /// The bot's position.
    pub me_pos: [f32; 3],
    /// The bot is in the world.
    pub spawned: bool,
    /// The bot is dead.
    pub dead: bool,
    /// A trip is under way.
    pub moving: bool,
    /// Metres left on the trip, `None` when standing.
    pub remaining: Option<f32>,
    /// Fighting or threatened.
    pub combat: bool,
    /// A boat, a portal journey, a trade, an instance or a load blocks riding.
    pub busy: bool,
    /// `bots.gm_bootstrap`: items may be granted.
    pub gm: bool,
    /// A rideable mount summon item in the bag (configured or any).
    pub mount_item: Option<ItemRef>,
    /// A glider in the bag that is not worn.
    pub glider_item: Option<ItemRef>,
    /// The back slot holds something that is not a glider (a carried trade pack): a glider may
    /// not be put on.
    pub back_blocked: bool,
    /// The glider in the back slot.
    pub glider_equipped: Option<ItemRef>,
    /// The last skill the server answered for this bot.
    pub skill_note: Option<SkillNote>,
    /// A ledge worth gliding down (computed with [`find_ledge`]).
    pub ledge: Option<Ledge>,
    /// The mount template to grant when the bag holds none (0/None = no mount data).
    pub default_mount: Option<u32>,
    /// The glider template to grant when the bag holds none.
    pub default_glider: Option<u32>,
}

impl Facts {
    /// A fact set for a standing, spawned bot with no items.
    pub fn idle(now_ms: u64, rules: Rules) -> Facts {
        Facts {
            now_ms,
            rules,
            me_pos: [0.0; 3],
            spawned: true,
            dead: false,
            moving: false,
            remaining: None,
            combat: false,
            busy: false,
            gm: false,
            mount_item: None,
            glider_item: None,
            back_blocked: false,
            glider_equipped: None,
            skill_note: None,
            ledge: None,
            default_mount: None,
            default_glider: None,
        }
    }
}

/// The state of one bot's mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    /// No request waits, no mate is out.
    #[default]
    Idle,
    /// A GM grant was sent; the item is awaited.
    NeedItem {
        /// The summon item template.
        template: u32,
        /// Bot clock when the GM command was sent.
        sent_ms: u64,
    },
    /// The summon skill was cast; `SCMateSpawned` is awaited.
    Summoning {
        /// The summon item instance.
        item_id: u64,
        /// The summon item template.
        template: u32,
        /// The use skill.
        skill: u32,
        /// Bot clock when the cast was sent.
        sent_ms: u64,
    },
    /// The mate exists; it is walked to and mounted.
    MateOut {
        /// Bot clock the mate appeared.
        since_ms: u64,
    },
    /// `CSMountMate` was sent; the attach is awaited.
    Mounting {
        /// Bot clock when the request was sent.
        since_ms: u64,
    },
    /// The bot sits on the mate.
    Mounted {
        /// Bot clock of the attach.
        since_ms: u64,
    },
    /// `CSUnMountMate` was sent; the detach is awaited.
    Dismounting {
        /// Bot clock when the request was sent.
        since_ms: u64,
    },
    /// `CSRemoveMate` was sent; the mate is forgotten after a short wait.
    Despawning {
        /// Bot clock when the request was sent.
        since_ms: u64,
    },
}

/// A glider item the bot wears or carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GliderPending {
    /// The item instance (0 for a GM grant that has not landed).
    pub item_id: u64,
    /// The item template.
    pub template: u32,
    /// Bot clock when the request was sent.
    pub sent_ms: u64,
    /// The request was a GM `/item add`, not a `CSSwapItems`.
    pub grant: bool,
}

/// Everything the feature remembers about one bot. Kept behind the travel state's one pointer.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Mem {
    /// The mount request in progress.
    pub phase: Phase,
    /// The bag mount item last used.
    pub mount_item: Option<ItemRef>,
    /// The mate's timeline id (0 = no mate out).
    pub mate_tl: u16,
    /// The mate's object id (from its unit state; 0 = not seen yet).
    pub mate_obj: u32,
    /// The last position of the mate.
    pub mate_pos: Option<[f32; 3]>,
    /// No summon or mount before this bot clock.
    pub next_try_ms: u64,
    /// A refusal backs off every mount attempt until this bot clock.
    pub refused_until_ms: u64,
    /// No dismount request before this bot clock (an unanswered one backs off, it is not re-sent
    /// every few seconds).
    pub dismount_retry_ms: u64,
    /// Bot clock the bot last moved (or stopped); the idle despawn counts from here.
    pub idle_since_ms: u64,
    /// The glider the bot knows (worn or being equipped).
    pub glider: Option<ItemRef>,
    /// An equip or grant in progress.
    pub glider_pending: Option<GliderPending>,
    /// No glider work before this bot clock.
    pub glider_off_until_ms: u64,
    /// Bot clock of the last glide started.
    pub last_glide_ms: u64,
    /// No new glide before this bot clock (a refused deploy).
    pub glide_off_until_ms: u64,
    /// The skill of the last glider deploy cast.
    pub glide_skill: u32,
    /// Bot clock of the last glider deploy cast.
    pub glide_sent_ms: u64,
}

impl Mem {
    /// Whether the bot sits on its mate.
    pub fn mounted(&self) -> bool {
        matches!(self.phase, Phase::Mounted { .. })
    }

    /// Whether a mate is out (mounted or standing).
    pub fn mate_out(&self) -> bool {
        self.mate_tl != 0 && !matches!(self.phase, Phase::Despawning { .. })
    }

    /// The bot clock a request we await an answer for was sent at, if any.
    pub fn awaiting_since(&self) -> Option<u64> {
        match self.phase {
            Phase::NeedItem { sent_ms, .. }
            | Phase::Summoning { sent_ms, .. }
            | Phase::Mounting { since_ms: sent_ms }
            | Phase::Dismounting { since_ms: sent_ms } => Some(sent_ms),
            Phase::Despawning { since_ms } => Some(since_ms),
            Phase::Idle | Phase::MateOut { .. } | Phase::Mounted { .. } => None,
        }
    }

    /// `SCMateSpawned`: the mate exists. Only a spawn this feature asked for is taken over: a
    /// battle pet or a quest mate of the same owner must not be mounted or dismissed.
    pub fn on_spawned(&mut self, tl: u16, item_id: u64, now_ms: u64) {
        if !matches!(self.phase, Phase::Summoning { .. }) {
            return;
        }
        self.mate_tl = tl;
        self.mate_obj = 0;
        self.mate_pos = None;
        self.phase = Phase::MateOut { since_ms: now_ms };
        self.idle_since_ms = now_ms;
        self.next_try_ms = 0;
        if let Some(item) = self.mount_item.as_mut() {
            if item_id != 0 && item.item_id != item_id {
                item.item_id = item_id;
            }
        }
    }

    /// The mate's unit state gives its object id and position.
    pub fn on_mate_state(
        &mut self,
        obj: u32,
        tl: u16,
        owner_id: u32,
        me_char_id: u32,
        pos: [f32; 3],
    ) {
        if self.mate_tl != 0 && tl == self.mate_tl && owner_id != 0 && owner_id == me_char_id {
            self.mate_obj = obj;
            self.mate_pos = Some(pos);
        }
    }

    /// A movement update of the mate.
    pub fn on_moved(&mut self, obj: u32, pos: [f32; 3]) {
        if self.mate_obj != 0 && obj == self.mate_obj {
            self.mate_pos = Some(pos);
        }
    }

    /// `SCUnitAttached` of the bot. Only the driver seat on the known mate counts: a boat seat
    /// must not make the bot think it is riding.
    pub fn on_attached(&mut self, parent: u32, point: u8, now_ms: u64) {
        if point != aa_proto::sc_travel::attach_point::DRIVER {
            return;
        }
        if self.mate_obj == 0 || parent != self.mate_obj {
            return;
        }
        self.phase = Phase::Mounted { since_ms: now_ms };
        self.idle_since_ms = now_ms;
        self.next_try_ms = 0;
        self.refused_until_ms = 0;
    }

    /// `SCUnitDetached` of the bot while it was riding.
    pub fn on_detached(&mut self, now_ms: u64) {
        match self.phase {
            Phase::Mounted { .. } | Phase::Mounting { .. } | Phase::Dismounting { .. } => {
                self.phase = Phase::MateOut { since_ms: now_ms };
                self.idle_since_ms = now_ms;
                // A second mount right after a dismount would flap; wait a moment.
                self.next_try_ms = now_ms + 2_000;
            }
            _ => {}
        }
    }

    /// The server refused or ignored the request we were waiting for.
    pub fn on_refused(&mut self, now_ms: u64) {
        match self.phase {
            Phase::NeedItem { .. } | Phase::Summoning { .. } => {
                self.phase = Phase::Idle;
                self.refused_until_ms = now_ms + REFUSE_MS;
            }
            Phase::Mounting { .. } => {
                self.phase = Phase::MateOut { since_ms: now_ms };
                self.next_try_ms = now_ms + RETRY_MS;
                self.refused_until_ms = now_ms + REFUSE_MS;
            }
            Phase::Dismounting { .. } => {
                // It may still be mounted; let the next phase learn the truth.
                self.phase = Phase::Mounted { since_ms: now_ms };
                self.refused_until_ms = now_ms + REFUSE_MS;
            }
            Phase::Despawning { .. } => {
                self.forget_mate();
                self.refused_until_ms = now_ms + REFUSE_MS;
            }
            Phase::Idle | Phase::MateOut { .. } | Phase::Mounted { .. } => {}
        }
    }

    /// Forgets the mate (dismissed, died, instance load).
    pub fn forget_mate(&mut self) {
        self.phase = Phase::Idle;
        self.mate_tl = 0;
        self.mate_obj = 0;
        self.mate_pos = None;
        self.mount_item = None;
    }

    /// The bot died or left the world: a mate cannot survive that.
    pub fn on_gone(&mut self) {
        self.forget_mate();
        self.glider_pending = None;
        self.phase = Phase::Idle;
    }
}

/// What the caller should do.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Act {
    /// `/item add self <template> 1` for the mount.
    GrantMount {
        /// Summon item template.
        template: u32,
    },
    /// Cast the summon item's use skill.
    Summon {
        /// Item instance.
        item_id: u64,
        /// Item template.
        template: u32,
        /// Use skill.
        skill: u32,
    },
    /// `CSMountMate` on the mate.
    Mount {
        /// Mate timeline id.
        tl: u16,
    },
    /// `CSUnMountMate`.
    Dismount {
        /// Mate timeline id.
        tl: u16,
    },
    /// `CSRemoveMate`.
    Despawn {
        /// Mate timeline id.
        tl: u16,
    },
    /// `/item add self <template> 1` for the glider.
    GrantGlider {
        /// Glider item template.
        template: u32,
    },
    /// Put the glider in the back slot.
    EquipGlider {
        /// Item instance.
        item_id: u64,
        /// Item template.
        template: u32,
        /// Bag slot it is taken from.
        bag_slot: u16,
    },
    /// Deploy the glider and start the flight.
    Glide {
        /// Item instance.
        item_id: u64,
        /// Item template.
        template: u32,
        /// The deploy skill.
        skill: u32,
        /// Unit direction of the glide.
        dir: (f32, f32),
    },
}

/// The acts this tick; at most one mount act and one glider act, and never a glide together
/// with another act.
pub type Acts = SmallVec<[Act; 2]>;

/// One step of the feature. Pure: `mem` is the only mutable input.
pub fn step(mem: &mut Mem, f: &Facts) -> Acts {
    let mut acts: Acts = SmallVec::new();
    if !f.spawned || f.dead {
        return acts;
    }
    if f.moving || mem.idle_since_ms == 0 {
        mem.idle_since_ms = f.now_ms;
    }
    if f.rules.gliders {
        if let Some(a) = glider_step(mem, f) {
            acts.push(a);
        }
    }
    if f.rules.mounts {
        if let Some(a) = mount_step(mem, f) {
            acts.push(a);
        }
    }
    // (a glide only when nothing else happened: a mount or a glider swap comes first)
    if acts.is_empty() && f.rules.gliders {
        if let Some(a) = glide_step(mem, f) {
            acts.push(a);
        }
    }
    acts
}

/// Whether the bot wants to be on a mount now.
pub fn wants_mount(mem: &Mem, f: &Facts) -> bool {
    f.moving
        && !f.combat
        && !f.busy
        && f.remaining.is_some_and(|r| r >= f.rules.summon_over_m)
        && f.now_ms >= mem.next_try_ms
        && f.now_ms >= mem.refused_until_ms
}

/// Whether a mounted bot must get off now: a fight, a blocking situation, a stop or the end of
/// the trip (the talk, harvest or dungeon door at the destination).
pub fn wants_dismount(mem: &Mem, f: &Facts) -> bool {
    mem.mounted()
        && (f.combat
            || f.busy
            || !f.moving
            || f.remaining.is_none_or(|r| r <= f.rules.dismount_under_m))
}

/// The glider equip step.
fn glider_step(mem: &mut Mem, f: &Facts) -> Option<Act> {
    if f.busy || f.combat || f.back_blocked {
        return None;
    }
    if let Some(g) = f.glider_equipped {
        mem.glider = Some(g);
        mem.glider_pending = None;
        return None;
    }
    if f.now_ms < mem.glider_off_until_ms {
        return None;
    }
    if let Some(p) = mem.glider_pending {
        // The item landed after a grant: equip it now.
        if p.grant {
            if let Some(g) = f.glider_item {
                mem.glider = Some(g);
                mem.glider_pending = Some(GliderPending {
                    item_id: g.item_id,
                    template: g.template,
                    sent_ms: f.now_ms,
                    grant: false,
                });
                return Some(Act::EquipGlider {
                    item_id: g.item_id,
                    template: g.template,
                    bag_slot: g.bag_slot,
                });
            }
        }
        if f.now_ms < p.sent_ms + if p.grant { ITEM_WAIT_MS } else { EQUIP_WAIT_MS } {
            return None;
        }
        // It did not land: leave it alone for a while.
        mem.glider_pending = None;
        mem.glider_off_until_ms = f.now_ms + REFUSE_MS;
        return None;
    }
    if let Some(g) = f.glider_item {
        mem.glider = Some(g);
        mem.glider_pending = Some(GliderPending {
            item_id: g.item_id,
            template: g.template,
            sent_ms: f.now_ms,
            grant: false,
        });
        return Some(Act::EquipGlider {
            item_id: g.item_id,
            template: g.template,
            bag_slot: g.bag_slot,
        });
    }
    if f.gm {
        let Some(template) = f.default_glider else {
            mem.glider_off_until_ms = f.now_ms + NO_ITEM_MS;
            return None;
        };
        mem.glider_pending = Some(GliderPending {
            item_id: 0,
            template,
            sent_ms: f.now_ms,
            grant: true,
        });
        return Some(Act::GrantGlider { template });
    }
    mem.glider_off_until_ms = f.now_ms + NO_ITEM_MS;
    None
}

/// The glide step: a ledge, a worn glider and a trip long enough.
fn glide_step(mem: &mut Mem, f: &Facts) -> Option<Act> {
    if mem.mounted()
        || f.combat
        || f.busy
        || !f.moving
        || f.remaining.is_none_or(|r| r < GLIDE_MIN_REMAINING_M)
    {
        return None;
    }
    let g = f.glider_equipped?;
    if f.now_ms < mem.glide_off_until_ms
        || f.now_ms < mem.last_glide_ms.saturating_add(GLIDE_GAP_MS)
    {
        return None;
    }
    let ledge = f.ledge?;
    if ledge.edge_m > GLIDE_START_EDGE_M {
        // (still walking up to it; the ground ahead is looked at again next tick)
        return None;
    }
    mem.last_glide_ms = f.now_ms;
    mem.glide_skill = g.skill;
    mem.glide_sent_ms = f.now_ms;
    Some(Act::Glide {
        item_id: g.item_id,
        template: g.template,
        skill: g.skill,
        dir: ledge.dir,
    })
}

/// The mount state machine.
fn mount_step(mem: &mut Mem, f: &Facts) -> Option<Act> {
    let now = f.now_ms;
    match mem.phase {
        Phase::NeedItem { sent_ms, .. } => {
            if let Some(item) = f.mount_item {
                return Some(begin_summon(mem, item, now));
            }
            if now < sent_ms.saturating_add(ITEM_WAIT_MS) {
                return None;
            }
            mem.phase = Phase::Idle;
            mem.refused_until_ms = now + NO_ITEM_MS;
            None
        }
        Phase::Summoning { skill, sent_ms, .. } => {
            if let Some(n) = f.skill_note {
                if n.skill == skill && n.at_ms >= sent_ms && n.result != 0 {
                    mem.phase = Phase::Idle;
                    mem.refused_until_ms = now + REFUSE_MS;
                    return None;
                }
            }
            if now < sent_ms.saturating_add(SUMMON_WAIT_MS) {
                return None;
            }
            mem.phase = Phase::Idle;
            mem.refused_until_ms = now + REFUSE_MS;
            None
        }
        Phase::MateOut { since_ms } => {
            if mem.mate_obj == 0 && now >= since_ms.saturating_add(MATE_OBJ_WAIT_MS) {
                // The unit state never came; drop the mate so the next summon starts clean.
                mem.phase = Phase::Despawning { since_ms: now };
                return Some(Act::Despawn { tl: mem.mate_tl });
            }
            if wants_mount(mem, f) {
                if mate_far(mem, f) {
                    mem.phase = Phase::Despawning { since_ms: now };
                    return Some(Act::Despawn { tl: mem.mate_tl });
                }
                if mem.mate_obj != 0 {
                    mem.phase = Phase::Mounting { since_ms: now };
                    return Some(Act::Mount { tl: mem.mate_tl });
                }
            }
            if idle_due(mem, f) {
                mem.phase = Phase::Despawning { since_ms: now };
                return Some(Act::Despawn { tl: mem.mate_tl });
            }
            None
        }
        Phase::Mounting { since_ms } => {
            if now < since_ms.saturating_add(MOUNT_WAIT_MS) {
                return None;
            }
            mem.phase = Phase::MateOut { since_ms: now };
            mem.next_try_ms = now + RETRY_MS;
            None
        }
        Phase::Mounted { .. } => {
            if wants_dismount(mem, f) && now >= mem.dismount_retry_ms {
                mem.phase = Phase::Dismounting { since_ms: now };
                return Some(Act::Dismount { tl: mem.mate_tl });
            }
            None
        }
        Phase::Dismounting { since_ms } => {
            if now >= since_ms.saturating_add(DISMOUNT_WAIT_MS) {
                mem.phase = Phase::Mounted { since_ms: now };
                mem.refused_until_ms = now + REFUSE_MS;
                // (a server that never answers is not asked again every five seconds)
                mem.dismount_retry_ms = now + DISMOUNT_RETRY_MS;
            }
            None
        }
        Phase::Despawning { since_ms } => {
            if now >= since_ms.saturating_add(DESPAWN_WAIT_MS) {
                mem.forget_mate();
                mem.next_try_ms = now + SUMMON_RETRY_MS;
            }
            None
        }
        Phase::Idle => {
            if mem.mate_tl != 0 {
                if wants_mount(mem, f) {
                    if mate_far(mem, f) {
                        mem.phase = Phase::Despawning { since_ms: now };
                        return Some(Act::Despawn { tl: mem.mate_tl });
                    }
                    if mem.mate_obj != 0 {
                        mem.phase = Phase::Mounting { since_ms: now };
                        return Some(Act::Mount { tl: mem.mate_tl });
                    }
                }
                if idle_due(mem, f) {
                    mem.phase = Phase::Despawning { since_ms: now };
                    return Some(Act::Despawn { tl: mem.mate_tl });
                }
                return None;
            }
            if !wants_mount(mem, f) {
                return None;
            }
            if let Some(item) = f.mount_item {
                return Some(begin_summon(mem, item, now));
            }
            if f.gm {
                let Some(template) = f.default_mount else {
                    mem.refused_until_ms = now + NO_ITEM_MS;
                    return None;
                };
                mem.phase = Phase::NeedItem {
                    template,
                    sent_ms: now,
                };
                return Some(Act::GrantMount { template });
            }
            mem.refused_until_ms = now + NO_ITEM_MS;
            None
        }
    }
}

fn begin_summon(mem: &mut Mem, item: ItemRef, now: u64) -> Act {
    mem.mount_item = Some(item);
    mem.phase = Phase::Summoning {
        item_id: item.item_id,
        template: item.template,
        skill: item.skill,
        sent_ms: now,
    };
    Act::Summon {
        item_id: item.item_id,
        template: item.template,
        skill: item.skill,
    }
}

/// The mate stands further than the server's mount reach: dismiss and summon again.
fn mate_far(mem: &Mem, f: &Facts) -> bool {
    let Some(pos) = mem.mate_pos else {
        // The unit state names its spawn position; without one there is no reason to think it
        // is far (and a wrong despawn would only cost a summon).
        return false;
    };
    distance(f.me_pos, pos) > MOUNT_REACH_M
}

/// Whether the idle despawn is due.
fn idle_due(mem: &Mem, f: &Facts) -> bool {
    if f.moving || f.rules.idle_despawn_ms == 0 || mem.mate_tl == 0 {
        return false;
    }
    f.now_ms.saturating_sub(mem.idle_since_ms) >= f.rules.idle_despawn_ms
}

/// Finds a ledge under the heading `yaw` (radians, 0 = +Y, positive counter-clockwise): the
/// ground falls at least `drop` within `lookahead` and the fall is abrupt (a cliff, not a slope),
/// the line before the edge has no wall and the landing is dry. `None` when the ground is
/// unknown, the bot is over water, or nothing falls far enough.
pub fn find_ledge(
    ground: impl Fn(f32, f32) -> Option<f32>,
    pos: [f32; 3],
    yaw: f32,
    lookahead: f32,
    drop: f32,
) -> Option<Ledge> {
    if !lookahead.is_finite() || !drop.is_finite() || lookahead < LEDGE_STEP_M || drop <= 0.0 {
        return None;
    }
    let start = ground(pos[0], pos[1])?;
    if start < DRY_LAND_M {
        return None;
    }
    let dir = (-yaw.sin(), yaw.cos());
    if !dir.0.is_finite() || !dir.1.is_finite() || dir.0.hypot(dir.1) < 0.9 {
        return None;
    }
    let steps = (lookahead / LEDGE_STEP_M).floor() as usize;
    let mut prev = start;
    for i in 1..=steps {
        let d = i as f32 * LEDGE_STEP_M;
        let g = ground(pos[0] + dir.0 * d, pos[1] + dir.1 * d)?;
        if g > start + LEDGE_WALL_M {
            // the way ahead rises: a glide would hit terrain
            return None;
        }
        let fallen = start - g;
        if g < prev - LEDGE_DROP_PER_SAMPLE_M && prev >= start - LEDGE_RUN_M && fallen >= drop {
            if g < DRY_LAND_M {
                return None;
            }
            return Some(Ledge {
                dir,
                ground: g,
                drop_m: fallen,
                edge_m: d,
            });
        }
        prev = g;
    }
    None
}

/// The horizontal distance between two points.
pub fn distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}
