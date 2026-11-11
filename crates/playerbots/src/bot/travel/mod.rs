//! Portal travel: the teleport book, recall points and gates (owned by the portals feature).
//!
//! [`TravelState`] is the bot's memory of what the server told it about portals: the private
//! teleport book, the visited recall districts and the bound one, the entrance gates in view, and
//! the bookkeeping of the casts it made. [`BotState`](super::state::BotState) owns one
//! (`state.travel`) and forwards the portal events to [`TravelState::on_event`], in arrival order,
//! from `BotState::apply` (never from a goal): [`WorldEvent::PortalBook`],
//! [`WorldEvent::ReturnDistricts`], [`WorldEvent::PortalSaved`], [`WorldEvent::PortalDeleted`],
//! [`WorldEvent::PortalUsed`] and the answers to the four portal skills
//! ([`WorldEvent::SkillStarted`]). `BotState::apply_unit_state` tells it about entrance gates
//! ([`TravelState::note_gate`]).
//!
//! # What other code uses
//!
//! * [`Journey`]: a far trip by portal any goal can drive (`Journey::new(dest)`, `tick(ctx)`,
//!   [`Journey::follow_gate`] for the gate of a party member).
//! * [`far_trip`]: the hook of `Ctx::travel`. A trip of more than [`plan::FAR_TRIP_M`] metres is
//!   given to an implicit journey when a portal route beats walking; goals need no change. While a
//!   journey casts or waits at a gate the brain keeps the goal waiting ([`duty`]).
//! * [`duty`]: called by the brain on every tick. It records Teleport Book entries at useful
//!   places, lets an idle bot hop through the gate of another bot now and then and watches the
//!   casts for symptoms the server may be to blame for.
//!
//! # Memory
//!
//! The per-bot budget is tight (`size_of::<Bot>() < 3200` is asserted): [`TravelState`] is one
//! pointer, the data lives in a lazily created `Box` ([`Inner`], a few hundred bytes with a full
//! book) that only bots with a portal book or a gate in view ever allocate.

pub mod book;
pub mod drive;
pub mod journey;
pub mod places;
pub mod plan;

use aa_proto::sc_world::WorldEvent;
use smallvec::SmallVec;

use book::Book;
pub use drive::{duty, far_trip, has_route, on_goal_end, stats, Stats};
pub use journey::{Dest, Journey, JourneyFail, JourneyStatus};

use journey::{GateView, SkillNote};

/// The most bytes [`TravelState`] may take inline in every bot (checked by a test).
pub const MAX_INLINE_BYTES: usize = 16;

/// Entrance gates remembered at once (the server removes them after 30 s).
const MAX_GATES: usize = 6;

/// What a bot knows about portals. See the module docs.
#[derive(Debug, Default)]
pub struct TravelState {
    inner: Option<Box<Inner>>,
}

/// The state of the recording duty.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Rec {
    /// Nothing under way.
    #[default]
    Idle,
    /// A `CSDeletePortal` was sent to make room in the book.
    Delete {
        /// The entry that goes.
        id: u32,
        /// Bot clock when it was sent.
        sent_ms: u64,
    },
    /// The record cast was sent.
    Cast {
        /// Bot clock when it was sent.
        sent_ms: u64,
        /// Where the bot stood.
        at: [f32; 3],
        /// The zone key the world data puts the spot in.
        zone: u16,
        /// Memory Ink in the bag when the cast was sent.
        ink: i64,
    },
}

/// A spot the bot recorded: the book entry it got, and where the bot stood (the way to learn the
/// origin cell of the entry's zone).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Recorded {
    /// The new entry.
    pub id: u32,
    /// Where the bot stood.
    pub at: [f32; 3],
}

/// A world position that failed to be reached by portal; the same trip is walked for a while.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Block {
    /// Destination, rounded to 64 m cells.
    pub cell: (i32, i32),
    /// Bot clock until which portal routes to it are not tried.
    pub until_ms: u64,
}

/// Everything behind [`TravelState`]'s pointer.
#[derive(Debug, Default)]
pub struct Inner {
    /// The teleport book.
    pub book: Book,
    /// Entrance gates seen (the live ones are the ones still in the unit view).
    pub gates: SmallVec<[GateView; MAX_GATES]>,
    /// The server's last answer to a portal skill of the bot.
    pub skill: Option<SkillNote>,
    /// The last `SCUnitPortalUsed`: `(gate, bot clock)`.
    pub used: Option<(u32, u64)>,
    /// The recording duty.
    pub rec: Rec,
    /// A record that succeeded and waits for the duty to learn the zone cell from it.
    pub recorded: Option<Recorded>,
    /// The implicit journey of `Ctx::travel` (or a hop).
    pub auto: Option<Journey>,
    /// Bot clock of the last step of `auto`.
    pub auto_step_ms: u64,
    /// Destinations that failed by portal lately.
    pub blocked: SmallVec<[Block; 4]>,
    /// The duty has been set up (first-record delay chosen).
    pub started: bool,
    /// Do not look for a place to record before this bot clock.
    pub next_probe_ms: u64,
    /// Do not record before this bot clock.
    pub next_record_ms: u64,
    /// Do not hop through another bot's gate before this bot clock.
    pub next_hop_ms: u64,
    /// The open skill is ready at this bot clock (cast time plus cooldown after our own cast).
    pub open_ready_ms: u64,
    /// The recall is ready at this bot clock.
    pub recall_ready_ms: u64,
    /// No portal gate is opened before this bot clock (the last attempts failed).
    pub open_off_until_ms: u64,
    /// No recall before this bot clock (the last attempt failed).
    pub recall_off_until_ms: u64,
    /// The gate the hop dice were last thrown for.
    pub rolled_gate: u32,
    /// Sub zones the bot told the server it entered (`CSNotifySubZone`), the last few.
    pub notified: SmallVec<[u32; 6]>,
    /// Do not look for a recall district to visit before this bot clock.
    pub next_visit_ms: u64,
    /// Places where the server refused the record cast (a peace-zone buff lets only listed skills
    /// through): `(x, y, until)`, the last few. No new record is tried near them until then.
    pub no_record: SmallVec<[(f32, f32, u64); 4]>,
    /// The mount and glider feature's memory (kept here so a bot pays no extra pointer).
    pub mount: crate::bot::mount::Mem,
}

impl TravelState {
    /// A portal event of the server (see the module docs for which). `me_obj` is the bot's object
    /// id and `now_ms` the bot clock.
    pub fn on_event(&mut self, ev: &WorldEvent, me_obj: u32, now_ms: u64) {
        match ev {
            WorldEvent::PortalBook(list) => {
                let inner = self.inner_mut();
                let added = inner.book.merge(list);
                // a record we cast: the one new entry is ours
                if let Rec::Cast { at, .. } = inner.rec {
                    if let Some(&id) = added.first() {
                        inner.recorded = Some(Recorded { id, at });
                        inner.rec = Rec::Idle;
                        drive::STATS.record_ok();
                    }
                }
            }
            WorldEvent::PortalSaved(entry) => {
                self.inner_mut().book.merge(std::slice::from_ref(&**entry));
            }
            WorldEvent::ReturnDistricts { portals, bound } => {
                self.inner_mut().book.set_districts(portals, *bound);
            }
            WorldEvent::PortalDeleted { kind, id } => {
                let inner = self.inner_mut();
                inner.book.remove(*kind, *id);
                if let Rec::Delete { id: want, .. } = inner.rec {
                    if u32::try_from(*id).is_ok_and(|i| i == want) {
                        inner.rec = Rec::Idle;
                    }
                }
            }
            WorldEvent::PortalUsed { gate } => {
                self.inner_mut().used = Some((*gate, now_ms));
            }
            WorldEvent::MateSpawned(mate) => {
                self.inner_mut()
                    .mount
                    .on_spawned(mate.tl, mate.item_id, now_ms);
            }
            WorldEvent::SkillStarted {
                caster_obj,
                skill,
                result,
                ..
            } if *caster_obj == me_obj && me_obj != 0 => {
                self.inner_mut().skill = Some(SkillNote {
                    skill: *skill,
                    result: *result,
                    at_ms: now_ms,
                });
            }
            _ => {}
        }
    }

    /// An entrance gate (NPC 3891) came into view; `owner` is the opener's character id and
    /// `mine` says it is the bot's own.
    pub fn note_gate(&mut self, obj: u32, owner: u32, mine: bool, pos: [f32; 3], now_ms: u64) {
        let inner = self.inner_mut();
        if let Some(g) = inner.gates.iter_mut().find(|g| g.obj == obj) {
            g.owner = owner;
            g.pos = pos;
            return;
        }
        if mine {
            drive::STATS.gate_seen();
        }
        if inner.gates.len() >= MAX_GATES {
            // the oldest has long expired (gates live 30 s)
            inner.gates.remove(0);
        }
        inner.gates.push(GateView {
            obj,
            owner,
            pos,
            seen_ms: now_ms,
        });
    }

    /// Whether the bot has anything to do with portals this tick that wants a fast brain tick
    /// (a journey or a record under way).
    pub fn busy(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|i| i.auto.as_ref().is_some_and(Journey::holds) || i.rec != Rec::Idle)
    }

    /// The teleport book, `None` before the bot knows anything about portals.
    pub fn book(&self) -> Option<&Book> {
        self.inner.as_deref().map(|i| &i.book)
    }

    /// The gates in view that character `owner` opened, from the unit view `live`.
    pub fn gate_of<'a>(
        &'a self,
        owner: u32,
        live: impl Fn(u32) -> bool + 'a,
    ) -> impl Iterator<Item = &'a GateView> + 'a {
        self.inner
            .iter()
            .flat_map(|i| i.gates.iter())
            .filter(move |g| g.owner == owner && live(g.obj))
    }

    pub(crate) fn inner_mut(&mut self) -> &mut Inner {
        self.inner.get_or_insert_with(Default::default)
    }

    /// The mount and glider memory, created on first use.
    pub(crate) fn mount_mut(&mut self) -> &mut crate::bot::mount::Mem {
        &mut self.inner_mut().mount
    }

    /// The mount and glider memory, when it exists.
    pub fn mount_ref(&self) -> Option<&crate::bot::mount::Mem> {
        self.inner.as_deref().map(|i| &i.mount)
    }

    /// The last skill the server answered for the bot.
    pub fn skill_note(&self) -> Option<SkillNote> {
        self.inner.as_deref().and_then(|i| i.skill)
    }

    /// An attach/detach of the bot (the mate seat; a boat seat is ignored by the mount code).
    pub fn on_vehicle_event(&mut self, ev: &WorldEvent, me_obj: u32, now_ms: u64) {
        let Some(inner) = self.inner.as_deref_mut() else {
            return;
        };
        match ev {
            WorldEvent::UnitAttached {
                child,
                parent,
                point,
                ..
            } if *child == me_obj => {
                inner.mount.on_attached(*parent, *point, now_ms);
            }
            WorldEvent::UnitDetached { obj, .. } if *obj == me_obj => {
                inner.mount.on_detached(now_ms);
            }
            _ => {}
        }
    }

    /// The mate's unit state came into view (its object id and position).
    pub fn note_mate_state(
        &mut self,
        obj: u32,
        tl: u16,
        owner_id: u32,
        me_char_id: u32,
        pos: [f32; 3],
    ) {
        if let Some(inner) = self.inner.as_deref_mut() {
            inner
                .mount
                .on_mate_state(obj, tl, owner_id, me_char_id, pos);
        }
    }

    /// A movement update of a unit; only the mate is kept.
    pub fn note_mate_moved(&mut self, obj: u32, pos: [f32; 3]) {
        if let Some(inner) = self.inner.as_deref_mut() {
            inner.mount.on_moved(obj, pos);
        }
    }

    #[cfg(test)]
    pub(crate) fn inner_ref(&self) -> Option<&Inner> {
        self.inner.as_deref()
    }

    /// Takes the data out for a tick (put back with [`TravelState::put`]), creating it when asked.
    pub(crate) fn take(&mut self, create: bool) -> Option<Box<Inner>> {
        match self.inner.take() {
            Some(i) => Some(i),
            None if create => Some(Box::default()),
            None => None,
        }
    }

    pub(crate) fn put(&mut self, inner: Box<Inner>) {
        self.inner = Some(inner);
    }
}

#[cfg(test)]
mod tests {
    use super::book::tests::wire;
    use super::*;
    use aa_proto::sc_travel::PortalEntry;

    const ME: u32 = 0x400;

    #[test]
    fn the_state_stays_within_its_share_of_the_bot() {
        assert!(std::mem::size_of::<TravelState>() <= MAX_INLINE_BYTES);
        // a bot that never hears of a portal allocates nothing
        let s = TravelState::default();
        assert!(s.book().is_none() && !s.busy());
        assert!(s.inner_ref().is_none());
    }

    #[test]
    fn the_book_follows_the_events() {
        let mut t = TravelState::default();
        let book = |list: Vec<PortalEntry>| WorldEvent::PortalBook(list.into_boxed_slice());
        t.on_event(&book(vec![wire(4100, 0, (1.0, 2.0))]), ME, 10);
        t.on_event(&book(vec![wire(4101, 0, (3.0, 4.0))]), ME, 20);
        let ids = |t: &TravelState| -> Vec<u32> {
            t.book().unwrap().private.iter().map(|p| p.id).collect()
        };
        assert_eq!(
            ids(&t),
            [4100, 4101],
            "a new entry arrives alone and is merged"
        );
        // a rename answers with SCPortalInfoSaved
        let mut renamed = wire(4100, 0, (1.0, 2.0));
        renamed.name = "keep".into();
        t.on_event(&WorldEvent::PortalSaved(Box::new(renamed)), ME, 30);
        assert_eq!(&*t.book().unwrap().private[0].name, "keep");
        t.on_event(
            &WorldEvent::ReturnDistricts {
                portals: vec![wire(243, 179, (0.0, 0.0))].into_boxed_slice(),
                bound: 243,
            },
            ME,
            40,
        );
        assert_eq!(t.book().unwrap().bound, 243);
        assert_eq!(t.book().unwrap().districts.len(), 1);
        t.on_event(&WorldEvent::PortalDeleted { kind: 2, id: 4100 }, ME, 50);
        assert_eq!(ids(&t), [4101]);
        t.on_event(&WorldEvent::PortalDeleted { kind: 1, id: 243 }, ME, 60);
        assert!(t.book().unwrap().districts.is_empty());
    }

    #[test]
    fn a_new_entry_after_a_record_cast_is_ours() {
        let mut t = TravelState::default();
        t.on_event(
            &WorldEvent::PortalBook(vec![wire(4100, 7, (1.0, 2.0))].into()),
            ME,
            1,
        );
        t.inner_mut().rec = Rec::Cast {
            sent_ms: 100,
            at: [50.0, 60.0, 70.0],
            zone: 7,
            ink: 2,
        };
        // a re-sent entry that is not new does not count
        t.on_event(
            &WorldEvent::PortalBook(vec![wire(4100, 7, (1.0, 2.0))].into()),
            ME,
            150,
        );
        assert!(matches!(t.inner_ref().unwrap().rec, Rec::Cast { .. }));
        t.on_event(
            &WorldEvent::PortalBook(vec![wire(4101, 7, (9.0, 9.0))].into()),
            ME,
            5_300,
        );
        let i = t.inner_ref().unwrap();
        assert_eq!(i.rec, Rec::Idle);
        assert_eq!(
            i.recorded,
            Some(Recorded {
                id: 4101,
                at: [50.0, 60.0, 70.0]
            })
        );
    }

    #[test]
    fn a_delete_for_the_pending_one_ends_the_wait() {
        let mut t = TravelState::default();
        t.on_event(
            &WorldEvent::PortalBook(vec![wire(4100, 7, (1.0, 2.0))].into()),
            ME,
            1,
        );
        t.inner_mut().rec = Rec::Delete {
            id: 4100,
            sent_ms: 5,
        };
        t.on_event(&WorldEvent::PortalDeleted { kind: 2, id: 4999 }, ME, 20);
        assert!(matches!(t.inner_ref().unwrap().rec, Rec::Delete { .. }));
        t.on_event(&WorldEvent::PortalDeleted { kind: 2, id: 4100 }, ME, 30);
        assert_eq!(t.inner_ref().unwrap().rec, Rec::Idle);
    }

    #[test]
    fn only_the_bots_own_portal_skills_are_noted() {
        let mut t = TravelState::default();
        let started = |caster: u32, skill: u32, result: u8| WorldEvent::SkillStarted {
            caster_obj: caster,
            skill,
            tl: 1,
            target_obj: 0,
            result,
        };
        t.on_event(&started(0x999, 11216, 0), ME, 10);
        assert!(t.inner_ref().is_none_or(|i| i.skill.is_none()));
        t.on_event(&started(ME, 11216, 0x0F), ME, 20);
        assert_eq!(
            t.inner_ref().unwrap().skill,
            Some(SkillNote {
                skill: 11216,
                result: 0x0F,
                at_ms: 20
            })
        );
        t.on_event(&WorldEvent::PortalUsed { gate: 0x77 }, ME, 30);
        assert_eq!(t.inner_ref().unwrap().used, Some((0x77, 30)));
    }

    #[test]
    fn gates_are_remembered_per_object_and_bounded() {
        let mut t = TravelState::default();
        t.note_gate(1, 500, false, [1.0, 2.0, 3.0], 100);
        t.note_gate(1, 500, false, [1.5, 2.0, 3.0], 900);
        let g: Vec<_> = t.gate_of(500, |_| true).collect();
        assert_eq!(g.len(), 1);
        assert_eq!((g[0].seen_ms, g[0].pos[0]), (100, 1.5), "first seen stays");
        assert_eq!(t.gate_of(500, |_| false).count(), 0, "gone from the view");
        assert_eq!(t.gate_of(501, |_| true).count(), 0);
        for obj in 2..20 {
            t.note_gate(obj, 600, false, [0.0; 3], 1_000);
        }
        assert!(t.inner_ref().unwrap().gates.len() <= MAX_GATES);
    }
}
