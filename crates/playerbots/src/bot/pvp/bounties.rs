//! Recent sightings reported by bots that lost a trade pack to a hostile character.

use std::collections::VecDeque;

use parking_lot::Mutex;

const MAX_CASES: usize = 32;
const LIFETIME_MS: u64 = 30 * 60_000;

#[derive(Debug, Clone)]
pub struct Case {
    pub target: u32,
    pub name: Box<str>,
    pub alliance: u32,
    pub group: u16,
    pub last_seen: [f32; 2],
    pub expires: u64,
}

#[derive(Debug, Default)]
pub struct Board(Mutex<VecDeque<Case>>);

impl Board {
    pub fn report(
        &self,
        now: u64,
        target: u32,
        name: Box<str>,
        alliance: u32,
        group: u16,
        last_seen: [f32; 2],
    ) {
        let mut cases = self.0.lock();
        cases.retain(|c| c.expires > now && (c.target != target || c.alliance != alliance));
        cases.push_front(Case {
            target,
            name,
            alliance,
            group,
            last_seen,
            expires: now + LIFETIME_MS,
        });
        cases.truncate(MAX_CASES);
    }

    pub fn find(&self, now: u64, alliance: u32, matches: impl Fn(&Case) -> bool) -> Option<Case> {
        self.0
            .lock()
            .iter()
            .find(|c| c.alliance == alliance && c.expires > now && matches(c))
            .cloned()
    }

    /// Only the server's kill notification can close a case.
    pub fn close(&self, now: u64, alliance: u32, target: u32) -> bool {
        let mut cases = self.0.lock();
        let Some(index) = cases
            .iter()
            .position(|c| c.alliance == alliance && c.target == target && c.expires > now)
        else {
            return false;
        };
        cases.remove(index);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_refresh_by_character_and_close_only_for_the_victims_alliance() {
        let board = Board::default();
        board.report(200, 70, "Raider".into(), 148, 14, [2.0, 3.0]);
        board.report(300, 70, "Raider".into(), 148, 18, [5.0, 7.0]);
        let case = board.find(301, 148, |_| true).unwrap();
        assert_eq!(case.group, 18);
        assert_eq!(case.last_seen, [5.0, 7.0]);
        assert!(!board.close(301, 149, 70));
        assert!(board.close(301, 148, 70));
        assert!(!board.close(301, 148, 70));
        board.report(400, 70, "Raider".into(), 148, 18, [5.0, 7.0]);
        assert!(board.find(400 + LIFETIME_MS, 148, |_| true).is_none());
    }
}
