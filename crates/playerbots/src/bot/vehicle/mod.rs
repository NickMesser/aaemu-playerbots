//! Vehicles: boats and other slaves the bot owns or rides (owned by the sailing feature).
//!
//! [`VehicleState`] is the bot's memory of its slave: the one it summoned, the seat it sits on and,
//! for a ship, its heading, speed and helm. [`BotState`](super::state::BotState) owns one
//! (`state.vehicle`) and forwards every slave event to [`VehicleState::on_event`], in arrival
//! order, from `BotState::apply` (never from a goal). The events are [`WorldEvent::SlaveCreated`],
//! [`WorldEvent::MySlave`], [`WorldEvent::SlaveBound`], [`WorldEvent::UnitAttached`],
//! [`WorldEvent::UnitDetached`], [`WorldEvent::SlaveDespawned`], [`WorldEvent::SlaveRemoved`] and
//! [`WorldEvent::ShipMoved`] (the last one is made from the same packet as a ship's
//! [`WorldEvent::UnitMoved`], see `session::decode_checked`).
//!
//! The per-bot budget is tight (`size_of::<Bot>() < 3200` is asserted, with 48 bytes to spare when
//! the travel hooks went in): [`VehicleState`] is one pointer, and the [`Sailor`] behind it is
//! allocated by the first sail goal (or the first summon) and kept for the life of the bot task,
//! so a bot that never sails costs eight bytes. Events of other characters' boats are only noted
//! once the sailor exists.
//!
//! A ship's `ShipMoved` arrives about 25 times a second, so [`VehicleState::on_event`] only copies
//! the sample ([`Track`]) and keeps a short yaw history for the yaw rate;
//! everything else (the helm, [`helm`]) runs once per brain tick in the goal.

pub mod follow;
pub mod helm;
pub mod naval;
#[cfg(test)]
pub(crate) mod sim;

use aa_proto::sc_travel::ShipMotion;
use aa_proto::sc_world::WorldEvent;

/// The most bytes [`VehicleState`] may take inline in every bot (checked by a test).
pub const MAX_INLINE_BYTES: usize = 16;
/// The yaw rate is measured over at least this many milliseconds of ship time (a few packets).
pub const RATE_SPAN_MS: u32 = 300;

/// What a bot knows about its boat. See the module docs.
#[derive(Debug, Default)]
pub struct VehicleState {
    sailor: Option<Box<Sailor>>,
}

/// The boat the bot summoned.
#[derive(Debug, Clone, PartialEq)]
pub struct Boat {
    /// Slave object id.
    pub obj: u32,
    /// Slave timeline id (what bind, discard and destroy name).
    pub tl: u16,
    /// Slave template (0 until the `MySlave` record arrives).
    pub template: u32,
    /// Hit points as the summoner's record says (real values).
    pub hp: i32,
    /// Maximum hit points.
    pub max_hp: i32,
    /// Last known position (the summoner's record or a ship movement).
    pub pos: [f32; 3],
    /// Character id the last `SCSlaveBound` of this boat named as its driver (0 = none yet).
    pub driver_char: u32,
    /// The server said the boat is going away (`SCSlaveDespawn`).
    pub despawning: bool,
}

/// A seat the bot occupies: the slave and the attach point (1 driver, 2.. passengers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Seat {
    /// Slave object id.
    pub parent: u32,
    /// `AttachPointKind`.
    pub point: u8,
}

/// A boat somebody else summoned that came into view (for boarding as a passenger).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Foreign {
    /// Object id of the summoner.
    pub owner_obj: u32,
    /// Slave timeline id.
    pub tl: u16,
    /// Slave object id.
    pub obj: u32,
    /// Event counter ([`Sailor::events`]) when it was noted.
    pub at_event: u32,
}

/// Samples of the yaw history a [`Track`] keeps (0.64 s at the server's 25 Hz).
const HIST: usize = 16;

/// The newest sample of a ship the bot owns or sits on and its recent yaw history.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Track {
    /// Newest sample.
    pub cur: ShipMotion,
    /// Samples received.
    pub rx: u32,
    /// `(ship time, yaw)` of the recent samples, a ring ending before `head`.
    hist: [(u32, f32); HIST],
    head: u8,
    len: u8,
}

impl Track {
    /// A track that starts with `m`.
    pub fn new(m: &ShipMotion) -> Track {
        let mut t = Track {
            cur: *m,
            rx: 0,
            hist: [(0, 0.0); HIST],
            head: 0,
            len: 0,
        };
        t.push(m);
        t
    }

    /// Takes a new sample.
    pub fn push(&mut self, m: &ShipMotion) {
        self.cur = *m;
        self.rx = self.rx.wrapping_add(1);
        self.hist[usize::from(self.head)] = (m.time, m.yaw);
        self.head = ((usize::from(self.head) + 1) % HIST) as u8;
        self.len = (self.len + 1).min(HIST as u8);
    }

    /// Yaw rate in rad/s (positive = counter-clockwise): the change of heading since the newest
    /// sample that is at least [`RATE_SPAN_MS`] older than the current one (the oldest kept when
    /// none is); 0 when there is no older sample.
    pub fn yaw_rate(&self) -> f32 {
        let n = usize::from(self.len);
        let mut older = None;
        for k in 1..n {
            let (time, yaw) = self.hist[(usize::from(self.head) + HIST - 1 - k) % HIST];
            let dt = self.cur.time.wrapping_sub(time);
            if dt > 10_000 {
                break;
            }
            older = Some((dt, yaw));
            if dt >= RATE_SPAN_MS {
                break;
            }
        }
        match older {
            Some((dt, yaw)) if dt > 0 => wrap_pi(self.cur.yaw - yaw) / (dt as f32 / 1000.0),
            _ => 0.0,
        }
    }
}

/// Wraps an angle to `(-π, π]`.
pub fn wrap_pi(a: f32) -> f32 {
    let tau = std::f32::consts::TAU;
    let mut r = a % tau;
    if r > std::f32::consts::PI {
        r -= tau;
    } else if r <= -std::f32::consts::PI {
        r += tau;
    }
    r
}

/// Everything the sailing feature remembers about the bot's boats, behind [`VehicleState`].
#[derive(Debug, Default)]
pub struct Sailor {
    /// The boat the bot summoned, until the server removes it.
    pub boat: Option<Boat>,
    /// The seat the bot occupies (its own boat or another's).
    pub seat: Option<Seat>,
    /// Samples of the bot's own boat, or of the ship it sits on.
    pub track: Option<Track>,
    /// The reason of the last `SCUnitDetached` about the bot, with the event counter it came at.
    pub detached: Option<(u8, u32)>,
    /// Object id of the last boat the server removed (a late `MySlave` must not bring it back).
    pub gone: u32,
    /// Another character's boat in view.
    pub foreign: Option<Foreign>,
    /// Counts the events that changed the state (so a goal can tell what is new).
    pub events: u32,
    /// A sail goal is running (the bot task then ticks fast while the bot is on a boat).
    pub active: bool,
    /// Bot clock (ms) the last sailing trip ended at; 0 = never.
    pub last_trip_end_ms: u64,
    /// Trips ended.
    pub trips: u16,
    pub naval: Option<Box<naval::Sight>>,
}

impl Sailor {
    /// The bot's boat, when it exists and is not going away.
    pub fn live_boat(&self) -> Option<&Boat> {
        self.boat.as_ref().filter(|b| !b.despawning)
    }

    /// The attach point of the bot on its own boat (0 = not seated on it).
    pub fn my_seat_on_boat(&self) -> u8 {
        match (&self.boat, &self.seat) {
            (Some(b), Some(s)) if s.parent == b.obj => s.point,
            _ => 0,
        }
    }

    fn note(&mut self) {
        self.events = self.events.wrapping_add(1);
    }

    fn sample(&mut self, m: &ShipMotion) {
        let mine = self.boat.as_ref().is_some_and(|b| b.obj == m.obj)
            || self.seat.is_some_and(|s| s.parent == m.obj);
        if !mine {
            return;
        }
        if let Some(b) = self.boat.as_mut().filter(|b| b.obj == m.obj) {
            b.pos = [m.pos.0, m.pos.1, m.pos.2];
        }
        match &mut self.track {
            Some(t) if t.cur.obj == m.obj => t.push(m),
            _ => self.track = Some(Track::new(m)),
        }
    }
}

impl VehicleState {
    pub fn observe_naval(&mut self, event: &WorldEvent) {
        if let Some(sight) = self.sailor.as_mut().and_then(|s| s.naval.as_deref_mut()) {
            sight.observe(event);
        }
    }

    /// A slave event of the server (see the module docs for which); `my_obj` is the bot's own
    /// object id (0 before it spawned).
    pub fn on_event(&mut self, ev: &WorldEvent, my_obj: u32) {
        match ev {
            WorldEvent::SlaveCreated {
                owner_obj,
                tl,
                slave_obj,
                ..
            } => {
                if my_obj != 0 && *owner_obj == my_obj {
                    let s = self.sailor_mut();
                    s.boat = Some(Boat {
                        obj: *slave_obj,
                        tl: *tl,
                        template: 0,
                        hp: 0,
                        max_hp: 0,
                        pos: [0.0; 3],
                        driver_char: 0,
                        despawning: false,
                    });
                    s.track = None;
                    s.gone = 0;
                    s.note();
                } else if let Some(s) = self.sailor.as_mut() {
                    s.foreign = Some(Foreign {
                        owner_obj: *owner_obj,
                        tl: *tl,
                        obj: *slave_obj,
                        at_event: s.events,
                    });
                    s.note();
                }
            }
            WorldEvent::MySlave(info) => {
                let s = self.sailor_mut();
                if info.obj == s.gone {
                    return;
                }
                let boat = s.boat.get_or_insert(Boat {
                    obj: info.obj,
                    tl: info.tl,
                    template: info.template,
                    hp: info.hp,
                    max_hp: info.max_hp,
                    pos: [0.0; 3],
                    driver_char: 0,
                    despawning: false,
                });
                if boat.obj != info.obj {
                    // a different slave than the one noted: the server's record wins
                    *boat = Boat {
                        obj: info.obj,
                        tl: info.tl,
                        template: info.template,
                        hp: info.hp,
                        max_hp: info.max_hp,
                        pos: [0.0; 3],
                        driver_char: 0,
                        despawning: false,
                    };
                }
                boat.tl = info.tl;
                boat.template = info.template;
                boat.hp = info.hp;
                boat.max_hp = info.max_hp;
                // (the summoner's record is stale while the boat sails: ShipMoved is newer)
                if s.track.is_none() {
                    boat.pos = [info.pos.0, info.pos.1, info.pos.2];
                }
                s.note();
            }
            WorldEvent::SlaveBound {
                master_id,
                slave_obj,
            } => {
                if let Some(b) = self
                    .sailor
                    .as_mut()
                    .and_then(|s| s.boat.as_mut())
                    .filter(|b| b.obj == *slave_obj)
                {
                    b.driver_char = *master_id;
                }
                if let Some(s) = self.sailor.as_mut() {
                    s.note();
                }
            }
            WorldEvent::UnitAttached {
                child,
                parent,
                point,
                ..
            } => {
                if my_obj != 0 && *child == my_obj {
                    let s = self.sailor_mut();
                    s.seat = Some(Seat {
                        parent: *parent,
                        point: *point,
                    });
                    s.note();
                }
            }
            WorldEvent::UnitDetached { obj, reason } => {
                if my_obj != 0 && *obj == my_obj {
                    if let Some(s) = self.sailor.as_mut() {
                        s.seat = None;
                        s.note();
                        s.detached = Some((*reason, s.events));
                    }
                }
            }
            WorldEvent::SlaveDespawned { obj } => {
                if let Some(s) = self.sailor.as_mut() {
                    if let Some(b) = s.boat.as_mut().filter(|b| b.obj == *obj) {
                        b.despawning = true;
                    }
                    s.note();
                }
            }
            WorldEvent::SlaveRemoved { obj, tl } => {
                if let Some(s) = self.sailor.as_mut() {
                    if s.boat
                        .as_ref()
                        .is_some_and(|b| b.tl == *tl || b.obj == *obj)
                    {
                        s.gone = s.boat.take().map_or(0, |b| b.obj);
                        if s.seat.is_some_and(|seat| seat.parent == s.gone) {
                            s.seat = None;
                        }
                        s.track = None;
                    }
                    if s.foreign.is_some_and(|f| f.tl == *tl || f.obj == *obj) {
                        s.foreign = None;
                    }
                    s.note();
                }
            }
            WorldEvent::ShipMoved(m) => {
                if let Some(s) = self.sailor.as_mut() {
                    s.sample(m);
                }
            }
            _ => {}
        }
    }

    /// The sailor, if the bot ever sailed or summoned.
    pub fn sailor(&self) -> Option<&Sailor> {
        self.sailor.as_deref()
    }

    /// The sailor, created (and kept) on the first call.
    pub fn sailor_mut(&mut self) -> &mut Sailor {
        self.sailor.get_or_insert_with(Box::default)
    }

    /// The bot's boat, if it has one.
    pub fn boat(&self) -> Option<&Boat> {
        self.sailor.as_ref()?.boat.as_ref()
    }

    /// The seat the bot occupies.
    pub fn seat(&self) -> Option<Seat> {
        self.sailor.as_ref()?.seat
    }

    /// Whether the bot task should tick fast: a sail goal runs and the bot has a boat or a seat
    /// (steering by a 1.5 s tick would be useless).
    pub fn fast_tick(&self) -> bool {
        self.sailor
            .as_ref()
            .is_some_and(|s| s.active && (s.boat.is_some() || s.seat.is_some()))
    }

    /// When the last sailing trip ended and how many trips there were: what a new connection
    /// keeps (see [`VehicleState::restore_trip_log`]).
    pub fn trip_log(&self) -> (u64, u16) {
        self.sailor
            .as_ref()
            .map_or((0, 0), |s| (s.last_trip_end_ms, s.trips))
    }

    /// Puts [`VehicleState::trip_log`] back into a fresh state (allocates only when a trip was
    /// made).
    pub fn restore_trip_log(&mut self, (last_end_ms, trips): (u64, u16)) {
        if last_end_ms != 0 || trips != 0 {
            let s = self.sailor_mut();
            s.last_trip_end_ms = last_end_ms;
            s.trips = trips;
        }
    }

    /// Heap bytes held (for the memory report).
    pub fn heap_bytes(&self) -> usize {
        self.sailor
            .as_ref()
            .map_or(0, |_| std::mem::size_of::<Sailor>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aa_proto::sc_travel::MySlaveInfo;

    const ME: u32 = 0x400;

    fn motion(obj: u32, time: u32, yaw: f32) -> ShipMotion {
        ShipMotion {
            obj,
            time,
            pos: (100.0 + time as f32 / 100.0, 200.0, 100.5),
            velocity: [0.0, 5.0, 0.0],
            speed: 5.0,
            yaw,
            ang_vel: [0.0; 3],
            steering: 0,
            throttle: 100,
            zone: 1,
            stuck: false,
        }
    }

    fn created(owner: u32, tl: u16, obj: u32) -> WorldEvent {
        WorldEvent::SlaveCreated {
            owner_obj: owner,
            tl,
            slave_obj: obj,
            hide_effect: false,
            creator: "x".into(),
        }
    }

    #[test]
    fn the_state_stays_within_its_share_of_the_bot() {
        assert!(std::mem::size_of::<VehicleState>() <= MAX_INLINE_BYTES);
        // one pointer: a bot that never sails pays eight bytes
        assert_eq!(
            std::mem::size_of::<VehicleState>(),
            std::mem::size_of::<usize>()
        );
    }

    #[test]
    fn the_trip_log_survives_a_new_connection() {
        let mut v = VehicleState::default();
        v.restore_trip_log(v.trip_log());
        assert!(v.sailor().is_none(), "nothing to keep, nothing allocated");
        let s = v.sailor_mut();
        s.last_trip_end_ms = 5000;
        s.trips = 2;
        s.boat = Some(Boat {
            obj: 1,
            tl: 1,
            template: 1,
            hp: 1,
            max_hp: 1,
            pos: [0.0; 3],
            driver_char: 0,
            despawning: false,
        });
        let log = v.trip_log();
        let mut fresh = VehicleState::default();
        fresh.restore_trip_log(log);
        assert_eq!(fresh.trip_log(), (5000, 2));
        assert!(fresh.boat().is_none() && fresh.seat().is_none());
    }

    #[test]
    fn a_bot_that_never_sails_allocates_nothing() {
        let mut v = VehicleState::default();
        // somebody else's boat, ships moving, a seat of another unit: nothing to keep
        v.on_event(&created(0x999, 7, 0x500), ME);
        v.on_event(
            &WorldEvent::ShipMoved(Box::new(motion(0x500, 100, 0.0))),
            ME,
        );
        v.on_event(
            &WorldEvent::UnitAttached {
                child: 0x999,
                point: 1,
                parent: 0x500,
                reason: 6,
            },
            ME,
        );
        v.on_event(&WorldEvent::SlaveDespawned { obj: 0x500 }, ME);
        assert!(v.sailor().is_none());
        assert!(!v.fast_tick());
        assert_eq!(v.heap_bytes(), 0);
    }

    #[test]
    fn the_bots_own_boat_is_followed_from_summon_to_removal() {
        let mut v = VehicleState::default();
        v.sailor_mut().active = true;
        v.on_event(&created(ME, 7, 0x500), ME);
        let b = v.boat().unwrap();
        assert_eq!((b.obj, b.tl), (0x500, 7));
        assert!(v.fast_tick(), "a boat while a sail goal runs");
        v.on_event(
            &WorldEvent::MySlave(Box::new(MySlaveInfo {
                obj: 0x500,
                tl: 7,
                name: "Rowboat".into(),
                template: 15,
                hp: 840,
                max_hp: 1000,
                pos: (100.0, 200.0, 100.0),
            })),
            ME,
        );
        let b = v.boat().unwrap();
        assert_eq!((b.template, b.hp, b.max_hp), (15, 840, 1000));
        assert_eq!(b.pos, [100.0, 200.0, 100.0]);
        // sit down
        v.on_event(
            &WorldEvent::UnitAttached {
                child: ME,
                point: 1,
                parent: 0x500,
                reason: 6,
            },
            ME,
        );
        v.on_event(
            &WorldEvent::SlaveBound {
                master_id: 42,
                slave_obj: 0x500,
            },
            ME,
        );
        assert_eq!(
            v.seat(),
            Some(Seat {
                parent: 0x500,
                point: 1
            })
        );
        assert_eq!(v.sailor().unwrap().my_seat_on_boat(), 1);
        assert_eq!(v.boat().unwrap().driver_char, 42);
        assert!(v.fast_tick());
        // the ship reports its movement: position and samples follow
        v.on_event(
            &WorldEvent::ShipMoved(Box::new(motion(0x500, 1000, 0.1))),
            ME,
        );
        v.on_event(
            &WorldEvent::ShipMoved(Box::new(motion(0x777, 1000, 0.1))),
            ME,
        );
        let s = v.sailor().unwrap();
        assert_eq!(s.track.unwrap().cur.obj, 0x500);
        assert_eq!(v.boat().unwrap().pos[0], 110.0);
        // a stale summoner record does not move the boat back
        v.on_event(
            &WorldEvent::MySlave(Box::new(MySlaveInfo {
                obj: 0x500,
                tl: 7,
                name: "Rowboat".into(),
                template: 15,
                hp: 800,
                max_hp: 1000,
                pos: (100.0, 200.0, 100.0),
            })),
            ME,
        );
        assert_eq!(v.boat().unwrap().pos[0], 110.0);
        // leave, despawn, removal
        v.on_event(&WorldEvent::UnitDetached { obj: ME, reason: 5 }, ME);
        assert_eq!(v.seat(), None);
        assert_eq!(v.sailor().unwrap().detached.map(|d| d.0), Some(5));
        v.on_event(&WorldEvent::SlaveDespawned { obj: 0x500 }, ME);
        assert!(v.boat().unwrap().despawning);
        assert!(v.sailor().unwrap().live_boat().is_none());
        v.on_event(&WorldEvent::SlaveRemoved { obj: ME, tl: 7 }, ME);
        assert!(v.boat().is_none());
        assert!(v.sailor().unwrap().track.is_none());
        // a late summoner record of the removed boat is ignored
        v.on_event(
            &WorldEvent::MySlave(Box::new(MySlaveInfo {
                obj: 0x500,
                tl: 7,
                name: "Rowboat".into(),
                template: 15,
                hp: 800,
                max_hp: 1000,
                pos: (100.0, 200.0, 100.0),
            })),
            ME,
        );
        assert!(v.boat().is_none());
    }

    #[test]
    fn other_boats_are_noted_once_the_bot_sails() {
        let mut v = VehicleState::default();
        v.sailor_mut();
        v.on_event(&created(0x999, 9, 0x501), ME);
        let f = v.sailor().unwrap().foreign.unwrap();
        assert_eq!((f.owner_obj, f.tl, f.obj), (0x999, 9, 0x501));
        // seated on it as a passenger: its movement is tracked
        v.on_event(
            &WorldEvent::UnitAttached {
                child: ME,
                point: 2,
                parent: 0x501,
                reason: 0,
            },
            ME,
        );
        v.on_event(
            &WorldEvent::ShipMoved(Box::new(motion(0x501, 500, 0.0))),
            ME,
        );
        assert_eq!(v.sailor().unwrap().track.unwrap().cur.obj, 0x501);
        assert_eq!(
            v.sailor().unwrap().my_seat_on_boat(),
            0,
            "not the bot's boat"
        );
        v.on_event(&WorldEvent::SlaveRemoved { obj: 0x999, tl: 9 }, ME);
        assert!(v.sailor().unwrap().foreign.is_none());
    }

    #[test]
    fn the_yaw_rate_spans_a_few_samples() {
        let mut v = VehicleState::default();
        v.sailor_mut();
        v.on_event(&created(ME, 7, 0x500), ME);
        // 25 Hz samples turning at 0.5 rad/s counter-clockwise
        for i in 0..40u32 {
            let t = i * 40;
            v.on_event(
                &WorldEvent::ShipMoved(Box::new(motion(0x500, t, 0.5 * t as f32 / 1000.0))),
                ME,
            );
        }
        let tr = v.sailor().unwrap().track.unwrap();
        assert!((tr.yaw_rate() - 0.5).abs() < 0.01, "{}", tr.yaw_rate());
        assert_eq!(tr.rx, 40);
    }

    #[test]
    fn the_yaw_rate_wraps_at_a_half_turn() {
        let mut t = Track::new(&motion(1, 0, 3.0));
        t.push(&motion(1, 500, -3.0));
        // -3.0 - 3.0 = -6.0 wraps to +0.283 rad over 0.5 s
        assert!((t.yaw_rate() - 0.566).abs() < 0.01, "{}", t.yaw_rate());
        assert!((wrap_pi(7.0) - (7.0 - std::f32::consts::TAU)).abs() < 1e-5);
        assert!((wrap_pi(-7.0) - (-7.0 + std::f32::consts::TAU)).abs() < 1e-5);
    }
}
