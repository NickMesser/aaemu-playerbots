//! Bounty cases based on observed deaths of trade pack carriers.

use std::collections::VecDeque;

use parking_lot::Mutex;

use crate::bot::state::Unit;

const MAX_CASES: usize = 32;
const LIFETIME_MS: u64 = 30 * 60_000;

#[derive(Debug, Clone)]
pub struct Case {
    pub target: u32,
    pub name: Box<str>,
    pub alliance: u32,
    pub group: u16,
    pub last_seen: [f32; 2],
    victim: u32,
    requested: bool,
    expires: u64,
}

#[derive(Debug, Default)]
pub struct Board(Mutex<VecDeque<Case>>);

impl Board {
    pub fn report(
        &self,
        now: u64,
        attacker: &Unit,
        victim: u32,
        alliance: u32,
        group: u16,
        automatic: bool,
    ) {
        let Some(name) = attacker.name.clone() else {
            return;
        };
        let target = attacker.id;
        let mut cases = self.0.lock();
        cases.retain(|c| c.expires > now);
        let duplicate = cases
            .iter()
            .position(|c| c.target == target && c.alliance == alliance && c.victim == victim);
        let previous = duplicate.and_then(|index| cases.remove(index));
        let requested = automatic || previous.is_some_and(|c| c.requested);
        cases.push_front(Case {
            target,
            name,
            alliance,
            group,
            last_seen: [attacker.pos[0], attacker.pos[1]],
            victim,
            requested,
            expires: now + LIFETIME_MS,
        });
        cases.truncate(MAX_CASES);
    }

    pub fn request(&self, now: u64, alliance: u32, victim: u32, target_name: &str) -> Option<Case> {
        let mut cases = self.0.lock();
        let index = cases.iter().position(|c| {
            c.expires > now
                && c.alliance == alliance
                && c.victim == victim
                && c.name.eq_ignore_ascii_case(target_name)
        })?;
        let mut case = cases.remove(index)?;
        case.requested = true;
        cases.push_front(case.clone());
        Some(case)
    }

    pub fn find(&self, now: u64, alliance: u32, matches: impl Fn(&Case) -> bool) -> Option<Case> {
        self.0
            .lock()
            .iter()
            .find(|c| c.requested && c.alliance == alliance && c.expires > now && matches(c))
            .cloned()
    }

    /// Only the server's kill notification can close a case.
    pub fn close(&self, now: u64, alliance: u32, target: u32) -> bool {
        let mut cases = self.0.lock();
        let same_target = |c: &Case| c.alliance == alliance && c.target == target;
        if !cases
            .iter()
            .any(|c| same_target(c) && c.requested && c.expires > now)
        {
            return false;
        }
        cases.retain(|c| !same_target(c));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raider(x: f32, y: f32) -> Unit {
        Unit {
            kind: crate::bot::state::UnitKind::Character,
            id: 70,
            pos: [x, y, 100.0],
            faction: 149,
            hp: 100,
            level: 40,
            flags: 0,
            target: 0,
            name: Some("Raider".into()),
        }
    }

    #[test]
    fn reports_refresh_by_character_and_close_only_for_the_victims_alliance() {
        let board = Board::default();
        board.report(200, &raider(2.0, 3.0), 30, 148, 14, true);
        board.report(300, &raider(5.0, 7.0), 30, 148, 18, true);
        let case = board.find(301, 148, |_| true).unwrap();
        assert_eq!(case.group, 18);
        assert_eq!(case.last_seen, [5.0, 7.0]);
        assert!(!board.close(301, 149, 70));
        assert!(board.close(301, 148, 70));
        assert!(!board.close(301, 148, 70));
        board.report(400, &raider(5.0, 7.0), 30, 148, 18, true);
        assert!(board.find(400 + LIFETIME_MS, 148, |_| true).is_none());
    }

    #[test]
    fn a_player_contract_requires_the_witnessed_victim_and_only_pays_once() {
        let board = Board::default();
        board.report(200, &raider(2.0, 3.0), 30, 148, 14, false);
        board.report(201, &raider(2.0, 3.0), 31, 148, 14, false);
        assert!(board.find(202, 148, |_| true).is_none());
        assert!(!board.close(202, 148, 70));
        assert!(board.request(202, 148, 32, "Raider").is_none());
        assert!(board.request(202, 149, 30, "Raider").is_none());
        assert!(board.request(202, 148, 30, "raider").is_some());
        assert!(board.request(203, 148, 31, "Raider").is_some());
        board.report(204, &raider(5.0, 7.0), 30, 148, 18, false);
        assert_eq!(board.find(205, 148, |_| true).unwrap().group, 18);
        assert!(board.close(206, 148, 70));
        assert!(!board.close(207, 148, 70));
    }
}
