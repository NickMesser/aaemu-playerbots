//! Where a sale is handed over: a town (at the auctioneer) or Mirage Isle (at the mailbox).
//!
//! The choice is a pure function of the configuration and of what the seller knows about the
//! buyer: a buyer of the same faction near the same town meets in town; one of another faction,
//! one who is far away and one nobody can place (a player who is not in view) meets on Mirage
//! Isle, which every character can enter. The towns' auctioneers stand next to the island's
//! portal doodads, so the portal spawn nearest to a bot is also the nearest town meeting point.

use crate::bot::market::ads::lines::Meet;
use crate::config::MeetAt;

/// A town meeting is only offered when both sides are within this many metres of its point.
pub const NEAR_TOWN_M: f32 = 350.0;

/// What is known about one side.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Side {
    /// Mother faction (0 = unknown).
    pub faction: u32,
    /// Map position.
    pub pos: [f32; 2],
}

fn dist(a: [f32; 2], b: [f32; 2]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt()
}

/// A town meeting both sides can reach and whose faction is known.
pub fn local_town(me: Side, peer: Option<Side>, town: Option<[f32; 2]>) -> Option<Meet> {
    let peer = peer?;
    let point = town?;
    (me.faction != 0
        && me.faction == peer.faction
        && dist(me.pos, point) <= NEAR_TOWN_M
        && dist(peer.pos, point) <= NEAR_TOWN_M)
        .then_some(Meet::Town {
            x: point[0],
            y: point[1],
        })
}

/// The meeting place for a sale between `me` and `peer`.
///
/// `town` is the town meeting point nearest to `me` (the auctioneer's spot), `mirage` whether the
/// island is available. `None` when the configuration asks for something that is not there.
pub fn choose(
    cfg: MeetAt,
    me: Side,
    peer: Option<Side>,
    town: Option<[f32; 2]>,
    mirage: bool,
) -> Option<Meet> {
    let town_meet = |t: [f32; 2]| Meet::Town { x: t[0], y: t[1] };
    match cfg {
        MeetAt::Mirage => mirage.then_some(Meet::Mirage),
        MeetAt::Town => town.map(town_meet),
        MeetAt::Auto => local_town(me, peer, town)
            .or_else(|| mirage.then_some(Meet::Mirage))
            .or_else(|| town.map(town_meet)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOWN: [f32; 2] = [1000.0, 1000.0];

    fn at(faction: u32, x: f32, y: f32) -> Side {
        Side {
            faction,
            pos: [x, y],
        }
    }

    #[test]
    fn the_same_faction_near_the_same_town_meets_in_town() {
        let m = choose(
            MeetAt::Auto,
            at(1, 1010.0, 1000.0),
            Some(at(1, 1100.0, 1100.0)),
            Some(TOWN),
            true,
        );
        assert_eq!(
            m,
            Some(Meet::Town {
                x: 1000.0,
                y: 1000.0
            })
        );
    }

    #[test]
    fn another_faction_or_a_far_or_unknown_buyer_meets_on_the_island() {
        let me = at(1, 1010.0, 1000.0);
        let cases = [
            Some(at(2, 1020.0, 1000.0)),
            Some(at(1, 5000.0, 5000.0)),
            Some(at(0, 1020.0, 1000.0)),
            None,
        ];
        for peer in cases {
            assert_eq!(
                choose(MeetAt::Auto, me, peer, Some(TOWN), true),
                Some(Meet::Mirage),
                "{peer:?}"
            );
        }
        // a seller far from every town cannot offer one either
        assert_eq!(
            choose(
                MeetAt::Auto,
                at(1, 9000.0, 9000.0),
                Some(at(1, 9010.0, 9000.0)),
                Some(TOWN),
                true
            ),
            Some(Meet::Mirage)
        );
    }

    #[test]
    fn without_the_island_the_town_is_the_only_place() {
        let me = at(1, 9000.0, 9000.0);
        assert_eq!(
            choose(MeetAt::Auto, me, None, Some(TOWN), false),
            Some(Meet::Town {
                x: 1000.0,
                y: 1000.0
            })
        );
        assert_eq!(choose(MeetAt::Auto, me, None, None, false), None);
    }

    #[test]
    fn the_configuration_can_insist() {
        let me = at(1, 1010.0, 1000.0);
        let peer = Some(at(2, 1020.0, 1000.0));
        assert_eq!(
            choose(MeetAt::Mirage, me, peer, Some(TOWN), true),
            Some(Meet::Mirage)
        );
        assert_eq!(choose(MeetAt::Mirage, me, peer, Some(TOWN), false), None);
        assert_eq!(
            choose(MeetAt::Town, me, peer, Some(TOWN), true),
            Some(Meet::Town {
                x: 1000.0,
                y: 1000.0
            })
        );
        assert_eq!(choose(MeetAt::Town, me, peer, None, true), None);
    }
}
