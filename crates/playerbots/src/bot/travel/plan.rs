//! Which way to go: walk, open a gate to a book entry, or recall to the bound point.
//!
//! Pure functions over numbers, so the choice is tested without a world. The costs are rough
//! seconds: walking is `distance / run speed`, a gate costs the cast and use time plus the walk
//! from the exit to the destination, a recall the same with its own cast.

use super::book::{dist, Target};

/// A trip shorter than this is walked (a gate costs more than it saves).
pub const FAR_TRIP_M: f32 = 1200.0;
/// An entry (or the recall point) farther than this from the destination is no help.
pub const NEAR_ENTRY_M: f32 = 400.0;
/// A portal route must save at least this many seconds over walking.
pub const MIN_SAVING_S: f32 = 45.0;
/// Seconds a gate costs besides the walk afterwards: the 10 s cast, the wait for the gate and the
/// step through.
pub const GATE_OVERHEAD_S: f32 = 28.0;
/// Seconds a recall costs besides the walk afterwards (the 10 s cast and the landing).
pub const RECALL_OVERHEAD_S: f32 = 16.0;
/// Run speed assumed for the costs, in m/s.
pub const RUN_MPS: f32 = 5.4;
/// A recall that is no more than this many seconds slower than the gate is preferred: it needs no
/// reagents.
pub const RECALL_BIAS_S: f32 = 12.0;

/// A book entry the planner may open a gate to, with what the reagents look like for it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cand {
    /// The entry.
    pub target: Target,
    /// The bot has the stones for it now.
    pub afford: bool,
    /// What a GM top-up would add when it does not: `(item, amount)`.
    pub topup: (u32, u32),
}

/// What the planner needs to know.
#[derive(Debug, Clone, Copy)]
pub struct PlanIn<'a> {
    /// The bot's world `(x, y)`.
    pub me: [f32; 2],
    /// Where the trip ends.
    pub dest: [f32; 2],
    /// Entries to consider (the adapter passes the few nearest to `dest`).
    pub cands: &'a [Cand],
    /// The bound recall point's `(x, y)` when a recall is possible right now (bound, off
    /// cooldown, not failed before).
    pub recall_at: Option<[f32; 2]>,
    /// The bot owns a Teleport Book, or may mint one.
    pub can_open: bool,
    /// The bot may top up missing stones (GM allowed).
    pub can_topup: bool,
    /// No buff on the bot blocks the open-gate skill (a carried trade pack blocks it too, see
    /// [`portal_allowed`]).
    pub gate_ok: bool,
    /// No buff on the bot blocks the recall skill.
    pub recall_ok: bool,
}

/// Whether a portal may be planned at all: a carried trade pack makes the server refuse every
/// gate (`SCErrorMsg` 610), and a buff on the bot blocks the skill it names. The trip is walked
/// from the start instead of failing at a gate.
pub fn portal_allowed(laden: bool, gate_ok: bool, recall_ok: bool) -> bool {
    !laden && (gate_ok || recall_ok)
}

/// The way to travel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Route {
    /// No portal route beats walking.
    Walk,
    /// Open a gate to this entry.
    Gate(Cand),
    /// Recall to the bound point.
    Recall,
}

/// Seconds to walk `metres`.
pub fn walk_secs(metres: f32) -> f32 {
    metres / RUN_MPS
}

/// The cheapest route for the trip in `p`; [`Route::Walk`] when it is short or a portal route does
/// not save [`MIN_SAVING_S`].
pub fn choose(p: &PlanIn<'_>) -> Route {
    let trip = dist(p.me, p.dest);
    if !trip.is_finite() || trip <= FAR_TRIP_M {
        return Route::Walk;
    }
    let walk = walk_secs(trip);
    let gate = if p.can_open && p.gate_ok {
        p.cands
            .iter()
            .filter(|c| c.afford || p.can_topup)
            .map(|c| (dist(c.target.at, p.dest), c))
            .filter(|(d, _)| *d <= NEAR_ENTRY_M)
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(d, c)| (GATE_OVERHEAD_S + walk_secs(d), *c))
    } else {
        None
    };
    let recall = if p.recall_ok {
        p.recall_at
            .map(|r| dist(r, p.dest))
            .filter(|d| *d <= NEAR_ENTRY_M)
            .map(|d| RECALL_OVERHEAD_S + walk_secs(d))
    } else {
        None
    };
    let best = match (gate, recall) {
        (Some((g, _)), Some(r)) if r <= g + RECALL_BIAS_S => Some((r, Route::Recall)),
        (Some((g, c)), _) => Some((g, Route::Gate(c))),
        (None, Some(r)) => Some((r, Route::Recall)),
        (None, None) => None,
    };
    match best {
        Some((secs, route)) if walk - secs >= MIN_SAVING_S => route,
        _ => Route::Walk,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(id: u32, at: [f32; 2], afford: bool) -> Cand {
        Cand {
            target: Target {
                kind: 2,
                id,
                zone: 1,
                at,
            },
            afford,
            topup: (4052, 3),
        }
    }

    fn input<'a>(dest: [f32; 2], cands: &'a [Cand]) -> PlanIn<'a> {
        PlanIn {
            me: [0.0, 0.0],
            dest,
            cands,
            recall_at: None,
            can_open: true,
            can_topup: false,
            gate_ok: true,
            recall_ok: true,
        }
    }

    #[test]
    fn short_trips_are_walked_whatever_the_book_says() {
        let c = [cand(1, [1000.0, 0.0], true)];
        assert_eq!(choose(&input([1100.0, 0.0], &c)), Route::Walk);
        // just over the line a gate next to the destination wins
        let c = [cand(1, [1500.0, 0.0], true)];
        assert_eq!(choose(&input([1500.0, 0.0], &c)), Route::Gate(c[0]));
    }

    #[test]
    fn the_entry_nearest_the_destination_wins_within_reach() {
        let c = [
            cand(1, [3000.0, 300.0], true),
            cand(2, [3000.0, 100.0], true),
            cand(3, [3000.0, 900.0], true),
        ];
        match choose(&input([3000.0, 0.0], &c)) {
            Route::Gate(g) => assert_eq!(g.target.id, 2),
            other => panic!("{other:?}"),
        }
        // every entry farther than NEAR_ENTRY_M from the destination: walk
        let far = [cand(3, [3000.0, 900.0], true)];
        assert_eq!(choose(&input([3000.0, 0.0], &far)), Route::Walk);
    }

    #[test]
    fn stones_decide_unless_a_top_up_is_allowed() {
        let c = [cand(1, [3000.0, 0.0], false)];
        let mut p = input([3000.0, 0.0], &c);
        assert_eq!(choose(&p), Route::Walk);
        p.can_topup = true;
        assert_eq!(choose(&p), Route::Gate(c[0]));
        // no book, no gate, however good
        p.can_open = false;
        assert_eq!(choose(&p), Route::Walk);
    }

    #[test]
    fn a_recall_is_preferred_when_it_is_nearly_as_good_as_the_gate() {
        let c = [cand(1, [3000.0, 0.0], true)];
        let mut p = input([3000.0, 0.0], &c);
        p.recall_at = Some([3000.0, 60.0]);
        assert_eq!(choose(&p), Route::Recall);
        // a recall point far from the destination is no help; the gate stays
        p.recall_at = Some([3000.0, 380.0]);
        assert_eq!(choose(&p), Route::Gate(c[0]));
        p.recall_at = Some([3000.0, 700.0]);
        assert_eq!(choose(&p), Route::Gate(c[0]));
        // with no gate the recall alone does it
        p.recall_at = Some([3000.0, 60.0]);
        p.cands = &[];
        assert_eq!(choose(&p), Route::Recall);
    }

    #[test]
    fn a_carried_pack_or_a_blocking_buff_keeps_the_trip_on_foot() {
        let c = [cand(1, [3000.0, 0.0], true)];
        // a pack blocks every gate; nothing is planned even with a perfect book
        assert!(!portal_allowed(true, true, true));
        assert!(portal_allowed(false, true, true));
        assert!(!portal_allowed(false, false, false));
        let mut p = input([3000.0, 0.0], &c);
        p.gate_ok = false;
        p.recall_ok = false;
        assert_eq!(choose(&p), Route::Walk);
        // the recall is still there when only the gate is blocked ...
        p.recall_at = Some([3000.0, 0.0]);
        p.recall_ok = true;
        assert_eq!(choose(&p), Route::Recall);
        // ... and the gate when only the recall is blocked
        p.recall_ok = false;
        p.gate_ok = true;
        assert_eq!(choose(&p), Route::Gate(c[0]));
    }

    #[test]
    fn a_route_must_save_enough_time() {
        // 1250 m: walking takes 231 s; a gate right at the destination costs 28 s: saves plenty
        let c = [cand(1, [1250.0, 0.0], true)];
        assert!(matches!(choose(&input([1250.0, 0.0], &c)), Route::Gate(_)));
        // an entry 390 m before the destination of a 1300 m trip: 28 + 72 = 100 s of 240 s
        let c = [cand(1, [910.0, 0.0], true)];
        assert!(matches!(choose(&input([1300.0, 0.0], &c)), Route::Gate(_)));
        // not a number: walk
        assert_eq!(choose(&input([f32::NAN, 0.0], &c)), Route::Walk);
    }
}
