//! The unit requirements of a quest's start components (`unit_reqs` rows owned by a component of
//! the start kind): the server refuses to start a quest whose requirements the character does not
//! meet and answers nothing, so a bot checks the ones it can judge before it walks to the giver.
//!
//! Judged: level, maximum level, race, gender, mother faction, "item in the bag", "quest
//! completed" / "quest not completed" and "quest in progress" (`PreCompleteQuestContext`, what
//! quest 1137 needs of 1136). Everything else (buffs, zones, time of day, ...) is treated as met;
//! the server has the last word and a refused start follows the usual failure policy.
//! The semantics mirror the server's `UnitReqs.Validate` and `CanComponentRun`: all requirements of a
//! component must hold, or one of them when the component is flagged `or_unit_reqs`, and every
//! start component of the quest has to be open.

use anyhow::Result;
use rusqlite::Connection;
use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use crate::sql::{each_row, RowExt};

/// `unit_reqs.kind_id` values judged here.
mod kind {
    pub const LEVEL: u16 = 0x01;
    pub const RACE: u16 = 0x03;
    pub const GENDER: u16 = 0x04;
    pub const OWN_ITEM: u16 = 0x0A;
    pub const COMPLETE_QUEST: u16 = 0x1F;
    pub const EXCEPT_COMPLETE_QUEST: u16 = 0x24;
    /// The quest must be started and not finished (`PreCompleteQuestContext`): quest 1137
    /// (unit_reqs kind 0x25 value 1136) needs 1136 in progress.
    pub const PRE_COMPLETE_QUEST: u16 = 0x25;
    pub const MOTHER_FACTION: u16 = 0x2A;
    pub const MOTHER_FACTION_ONLY: u16 = 0x38;
    pub const MOTHER_FACTION_ONLY_NOT: u16 = 0x3B;
    pub const MAX_LEVEL: u16 = 0x45;
}

/// `quest_components.component_kind_id` of a start component.
const START_KIND: i64 = 2;

/// One requirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Req {
    kind: u16,
    v1: u32,
    v2: u32,
}

/// What the character is, as far as requirements are concerned.
pub struct Who<'a> {
    /// Character level.
    pub level: u8,
    /// Race id.
    pub race: u8,
    /// Gender id.
    pub gender: u8,
    /// `mother_id` of the character's faction (0 for none, see `World::mother_faction`).
    pub mother_faction: u32,
    /// Whether a quest is completed.
    pub done: &'a dyn Fn(u32) -> bool,
    /// Whether a quest is in the journal (started and not finished).
    pub in_journal: &'a dyn Fn(u32) -> bool,
    /// Whether the bag holds an item template.
    pub owns: &'a dyn Fn(u32) -> bool,
}

impl Req {
    /// Whether the character meets the requirement; `None` when it cannot be told.
    fn holds(&self, who: &Who<'_>) -> Option<bool> {
        let level = u32::from(who.level);
        Some(match self.kind {
            kind::LEVEL => level >= self.v1 && (self.v2 == 0 || level <= self.v2),
            kind::MAX_LEVEL => level <= self.v1,
            kind::RACE => u32::from(who.race) == self.v1,
            kind::GENDER => u32::from(who.gender) == self.v1,
            kind::OWN_ITEM => (who.owns)(self.v1),
            kind::COMPLETE_QUEST => (who.done)(self.v1),
            kind::EXCEPT_COMPLETE_QUEST => !(who.done)(self.v1),
            kind::PRE_COMPLETE_QUEST => !(who.done)(self.v1) && (who.in_journal)(self.v1),
            kind::MOTHER_FACTION | kind::MOTHER_FACTION_ONLY => who.mother_faction == self.v1,
            kind::MOTHER_FACTION_ONLY_NOT => who.mother_faction != self.v1,
            _ => return None,
        })
    }
}

/// The requirements of one start component.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Gate {
    or: bool,
    reqs: SmallVec<[Req; 4]>,
}

impl Gate {
    /// Whether the component may run for `who`. Requirements that cannot be judged count as met
    /// (for an "or" gate they open it: the server decides).
    pub fn open(&self, who: &Who<'_>) -> bool {
        if self.reqs.is_empty() {
            return true;
        }
        if self.or {
            self.reqs.iter().any(|r| r.holds(who).unwrap_or(true))
        } else {
            self.reqs.iter().all(|r| r.holds(who).unwrap_or(true))
        }
    }
}

/// The gates of all start components of every quest that has requirements.
#[derive(Debug, Default)]
pub struct StartGates {
    by_quest: FxHashMap<u32, Box<[Gate]>>,
}

impl StartGates {
    /// Whether every start component of `quest` is open for `who`.
    pub fn open(&self, quest: u32, who: &Who<'_>) -> bool {
        self.by_quest
            .get(&quest)
            .is_none_or(|gates| gates.iter().all(|g| g.open(who)))
    }

    /// Number of quests with gates.
    pub fn len(&self) -> usize {
        self.by_quest.len()
    }

    /// Whether no quest has gates.
    pub fn is_empty(&self) -> bool {
        self.by_quest.is_empty()
    }

    /// Reads the gates from `quest_components` and `unit_reqs`.
    pub fn load(conn: &Connection) -> Result<StartGates> {
        let mut gates: FxHashMap<u32, Vec<(i64, Gate)>> = FxHashMap::default();
        each_row(
            conn,
            &format!(
                "SELECT c.quest_context_id, c.id, c.or_unit_reqs, u.kind_id, u.value1, u.value2 \
                 FROM quest_components c \
                 JOIN unit_reqs u ON u.owner_type = 'QuestComponent' AND u.owner_id = c.id \
                 WHERE c.component_kind_id = {START_KIND} \
                 ORDER BY c.quest_context_id, c.id, u.id"
            ),
            |r| {
                let Ok(kind) = u16::try_from(r.int(3)) else {
                    return;
                };
                let (quest, component) = (r.u32(0), r.int(1));
                let req = Req {
                    kind,
                    v1: r.u32(4),
                    v2: r.u32(5),
                };
                let list = gates.entry(quest).or_default();
                match list.last_mut() {
                    Some((id, gate)) if *id == component => gate.reqs.push(req),
                    _ => {
                        let mut reqs = SmallVec::new();
                        reqs.push(req);
                        list.push((
                            component,
                            Gate {
                                or: r.flag(2),
                                reqs,
                            },
                        ));
                    }
                }
            },
        )?;
        Ok(StartGates {
            by_quest: gates
                .into_iter()
                .map(|(q, v)| (q, v.into_iter().map(|(_, g)| g).collect()))
                .collect(),
        })
    }

    /// Builds gates from literal data (tests): `(quest, or, [(kind, v1, v2)])` per component.
    #[cfg(test)]
    pub fn from_tables(
        gates: impl IntoIterator<Item = (u32, bool, Vec<(u16, u32, u32)>)>,
    ) -> StartGates {
        let mut by_quest: FxHashMap<u32, Vec<Gate>> = FxHashMap::default();
        for (quest, or, reqs) in gates {
            by_quest.entry(quest).or_default().push(Gate {
                or,
                reqs: reqs
                    .into_iter()
                    .map(|(kind, v1, v2)| Req { kind, v1, v2 })
                    .collect(),
            });
        }
        StartGates {
            by_quest: by_quest
                .into_iter()
                .map(|(q, v)| (q, v.into_boxed_slice()))
                .collect(),
        }
    }

    /// Heap bytes held.
    pub fn bytes(&self) -> usize {
        let gate = std::mem::size_of::<Gate>();
        self.by_quest.capacity() * (4 + std::mem::size_of::<Box<[Gate]>>() + 8)
            + self
                .by_quest
                .values()
                .map(|g| {
                    g.len() * gate
                        + g.iter()
                            .filter(|g| g.reqs.spilled())
                            .map(|g| g.reqs.capacity() * std::mem::size_of::<Req>())
                            .sum::<usize>()
                })
                .sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn who<'a>(level: u8, done: &'a dyn Fn(u32) -> bool) -> Who<'a> {
        Who {
            level,
            race: 1,
            gender: 2,
            mother_faction: 148,
            done,
            in_journal: &|_| false,
            owns: &|item| item == 2142,
        }
    }

    #[test]
    fn a_quest_without_requirements_is_open() {
        let g = StartGates::default();
        let none = |_: u32| false;
        assert!(g.open(5, &who(1, &none)));
    }

    #[test]
    fn requirements_must_all_hold() {
        // quest 6161: level 3, mother faction 148, quest 350 done
        let g = StartGates::from_tables([(
            6161,
            false,
            vec![
                (kind::LEVEL, 3, 0),
                (kind::MOTHER_FACTION, 148, 0),
                (kind::COMPLETE_QUEST, 350, 0),
            ],
        )]);
        let none = |_: u32| false;
        let done_350 = |q: u32| q == 350;
        assert!(!g.open(6161, &who(5, &none)));
        assert!(g.open(6161, &who(5, &done_350)));
        assert!(!g.open(6161, &who(2, &done_350)));
        let mut w = who(5, &done_350);
        w.mother_faction = 149;
        assert!(!g.open(6161, &w));
    }

    #[test]
    fn race_gender_and_level_range() {
        let g = StartGates::from_tables([(
            7,
            false,
            vec![
                (kind::RACE, 1, 0),
                (kind::GENDER, 2, 0),
                (kind::LEVEL, 2, 10),
            ],
        )]);
        let none = |_: u32| false;
        assert!(g.open(7, &who(10, &none)));
        assert!(!g.open(7, &who(11, &none)));
        let mut w = who(5, &none);
        w.race = 4;
        assert!(!g.open(7, &w));
        w.race = 1;
        w.gender = 1;
        assert!(!g.open(7, &w));
    }

    #[test]
    fn unknown_requirements_count_as_met() {
        // kind 0x0B (trained skill) cannot be judged
        let g = StartGates::from_tables([(8, false, vec![(0x0B, 1234, 0), (kind::LEVEL, 3, 0)])]);
        let none = |_: u32| false;
        assert!(g.open(8, &who(3, &none)));
        assert!(!g.open(8, &who(2, &none)));
    }

    #[test]
    fn an_or_gate_opens_when_one_holds_or_one_is_unknown() {
        let g = StartGates::from_tables([(
            9,
            true,
            vec![(kind::RACE, 4, 0), (kind::COMPLETE_QUEST, 50, 0)],
        )]);
        let none = |_: u32| false;
        let done_50 = |q: u32| q == 50;
        assert!(!g.open(9, &who(5, &none)));
        assert!(g.open(9, &who(5, &done_50)));
        let unknown = StartGates::from_tables([(9, true, vec![(kind::RACE, 4, 0), (0x0B, 1, 0)])]);
        assert!(unknown.open(9, &who(5, &none)));
    }

    #[test]
    fn a_precomplete_quest_must_be_in_progress() {
        // quest 1137: unit_reqs kind 0x25 value 1136 (quest 1136 started, not finished)
        let g = StartGates::from_tables([(1137, false, vec![(kind::PRE_COMPLETE_QUEST, 1136, 0)])]);
        let none = |_: u32| false;
        assert!(!g.open(1137, &who(20, &none)));
        let mut w = who(20, &none);
        w.in_journal = &|q| q == 1136;
        assert!(g.open(1137, &w));
        // completed is not "in progress" any more
        let done_1136 = |q: u32| q == 1136;
        let mut w = who(20, &done_1136);
        w.in_journal = &|q| q == 1136;
        assert!(!g.open(1137, &w));
    }

    #[test]
    fn an_item_requirement_looks_in_the_bag() {
        let g = StartGates::from_tables([(18, false, vec![(kind::OWN_ITEM, 2142, 0)])]);
        let none = |_: u32| false;
        assert!(g.open(18, &who(5, &none)));
        let w = Who {
            owns: &|_| false,
            ..who(5, &none)
        };
        assert!(!g.open(18, &w));
    }

    #[test]
    fn every_start_component_must_be_open() {
        let g = StartGates::from_tables([
            (10, false, vec![(kind::LEVEL, 3, 0)]),
            (10, false, vec![(kind::EXCEPT_COMPLETE_QUEST, 99, 0)]),
        ]);
        let none = |_: u32| false;
        let done_99 = |q: u32| q == 99;
        assert!(g.open(10, &who(4, &none)));
        assert!(!g.open(10, &who(4, &done_99)));
        assert!(!g.open(10, &who(2, &none)));
    }

    #[test]
    fn the_tables_are_read() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE quest_components (id INT, quest_context_id INT, component_kind_id INT, or_unit_reqs TEXT);
             CREATE TABLE unit_reqs (id INT, owner_id INT, owner_type TEXT, kind_id INT, value1 INT, value2 INT);
             INSERT INTO quest_components VALUES (26431, 6161, 2, 'f'), (500, 6161, 3, 'f'), (18735, 4292, 2, 't');
             INSERT INTO unit_reqs VALUES
               (45292, 26431, 'QuestComponent', 1, 3, 0),
               (45293, 26431, 'QuestComponent', 42, 148, 0),
               (45305, 26431, 'QuestComponent', 31, 350, 0),
               (1, 500, 'QuestComponent', 1, 40, 0),
               (35800, 18735, 'QuestComponent', 3, 1, 0),
               (45607, 18735, 'QuestComponent', 31, 354, 0),
               (2, 26431, 'Sphere', 1, 99, 0);",
        )
        .unwrap();
        let g = StartGates::load(&conn).unwrap();
        assert_eq!(g.len(), 2);
        assert!(g.bytes() > 0);
        let none = |_: u32| false;
        let done_350 = |q: u32| q == 350;
        // the progress component (kind 3) and the sphere row are ignored
        assert!(g.open(6161, &who(3, &done_350)));
        assert!(!g.open(6161, &who(3, &none)));
        // 4292 is an "or" gate: the race holds
        assert!(g.open(4292, &who(1, &none)));
    }
}
