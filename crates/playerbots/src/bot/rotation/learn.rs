//! Skill learning and ability unlocks.
//!
//! A character starts with one ability (class tree) and its first skill. As the level grows the
//! bot adds the archetype's second and third ability (`CSSwapAbility` into an empty slot, at the
//! archetype's unlock levels) and learns the skills of its `skill_learn_order` whose ability is
//! active and whose ability level the character has reached (`CSLearnSkill`).
//!
//! The server answers a successful learn with `SCSkillLearned` and a refusal with silence (not
//! enough skill points, ability level too low), so a request that is not answered within
//! [`ANSWER_WAIT_MS`] counts as refused and the skill is left alone until the character has
//! gained a level. One request at a time, [`STEP_GAP_MS`] apart, and only out of combat.

use aa_proto::cs::{self, Out};
use smallvec::SmallVec;

use super::eval::SkillBook;
use crate::archetype::Archetype;

/// `ability` value of an empty ability slot.
pub const NO_ABILITY: u8 = 11;
/// Pause between two requests.
const STEP_GAP_MS: u64 = 1_200;
/// How long a learn request waits for `SCSkillLearned`.
const ANSWER_WAIT_MS: u64 = 3_000;

/// The next thing to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Put ability `new` into the empty slot (`old` is [`NO_ABILITY`]).
    Swap {
        /// Ability being replaced.
        old: u8,
        /// Ability being added.
        new: u8,
    },
    /// Learn this skill.
    Learn(u32),
}

/// What the plan looks at.
#[derive(Debug, Clone, Copy)]
pub struct PlanInput<'a> {
    /// Character level.
    pub level: u8,
    /// The active abilities.
    pub abilities: [u8; 3],
    /// Learned skills.
    pub known: &'a [(u32, u8)],
    /// The archetype's learn order.
    pub order: &'a [u32],
    /// The archetype's three abilities in slot order.
    pub required: &'a [u8],
    /// Level for the second ability.
    pub unlock_second: u8,
    /// Level for the third ability.
    pub unlock_third: u8,
}

/// The next step for a character, or `None` when there is nothing to do now. `failed` lists
/// `(skill, level it was refused at)`.
pub fn next_step(p: &PlanInput<'_>, book: &dyn SkillBook, failed: &[(u32, u8)]) -> Option<Step> {
    // Abilities first: a new tree opens new skills.
    for (slot, unlock) in [(1usize, p.unlock_second), (2, p.unlock_third)] {
        let Some(&want) = p.required.get(slot) else {
            continue;
        };
        if unlock == 0 || p.level < unlock {
            continue;
        }
        if p.abilities[slot] == NO_ABILITY
            && !p.abilities.contains(&want)
            && (slot == 1 || p.abilities[1] != NO_ABILITY)
        {
            return Some(Step::Swap {
                old: NO_ABILITY,
                new: want,
            });
        }
    }
    for &skill in p.order {
        if p.known.iter().any(|k| k.0 == skill) {
            continue;
        }
        let Some(f) = book.facts(skill) else { continue };
        if !f.need_learn
            || f.ability_id == 0
            || !p.abilities.contains(&f.ability_id)
            || f.ability_level > p.level
        {
            continue;
        }
        if failed.iter().any(|x| x.0 == skill && x.1 >= p.level) {
            continue;
        }
        return Some(Step::Learn(skill));
    }
    None
}

/// What [`Learner::tick`] needs.
pub struct LearnCtx<'a> {
    /// Bot clock in ms.
    pub now_ms: u64,
    /// Character level.
    pub level: u8,
    /// Own object id.
    pub me_obj: u32,
    /// The abilities the server reported at login.
    pub abilities: [u8; 3],
    /// Learned skills.
    pub known: &'a [(u32, u8)],
    /// Skill data.
    pub book: &'a dyn SkillBook,
    /// The build.
    pub arch: &'a Archetype,
}

/// Per-bot learning state.
#[derive(Debug, Clone, Default)]
pub struct Learner {
    next_ms: u64,
    pending: Option<(u32, u64)>,
    /// One entry per refused skill in the archetype's learn order.
    failed: SmallVec<[(u32, u8); 4]>,
    /// The bot's abilities, kept up to date by its own swaps (`None` until the first tick).
    abilities: Option<[u8; 3]>,
    learned: u16,
    refused: u16,
}

impl Learner {
    /// The bot's abilities as far as it knows (the login state plus its own swaps).
    pub fn abilities(&self) -> Option<[u8; 3]> {
        self.abilities
    }

    /// Skills learned and requests the server ignored since the bot started.
    pub fn counts(&self) -> (u16, u16) {
        (self.learned, self.refused)
    }

    /// One step; returns the packet to send, if any.
    pub fn tick(&mut self, c: &LearnCtx<'_>) -> Option<Out> {
        if c.me_obj == 0 || c.abilities[0] == 0 || c.abilities[0] == NO_ABILITY {
            return None;
        }
        let abilities = *self.abilities.get_or_insert(c.abilities);
        if let Some((skill, sent)) = self.pending {
            if c.known.iter().any(|k| k.0 == skill) {
                self.pending = None;
                self.learned = self.learned.saturating_add(1);
                self.next_ms = c.now_ms + STEP_GAP_MS / 2;
            } else if c.now_ms.saturating_sub(sent) > ANSWER_WAIT_MS {
                self.pending = None;
                self.refused = self.refused.saturating_add(1);
                self.failed.retain(|f| f.0 != skill);
                self.failed.push((skill, c.level));
            } else {
                return None;
            }
        }
        if c.now_ms < self.next_ms {
            return None;
        }
        let plan = PlanInput {
            level: c.level,
            abilities,
            known: c.known,
            order: &c.arch.skill_learn_order,
            required: &c.arch.required_abilities,
            unlock_second: c.arch.level_to_unlock_second,
            unlock_third: c.arch.level_to_unlock_third,
        };
        let step = next_step(&plan, c.book, &self.failed)?;
        self.next_ms = c.now_ms + STEP_GAP_MS;
        Some(match step {
            Step::Swap { old, new } => {
                let slot = abilities.iter().position(|a| *a == old).unwrap_or(1);
                let mut next = abilities;
                next[slot] = new;
                self.abilities = Some(next);
                cs::swap_ability(c.me_obj, old, new)
            }
            Step::Learn(skill) => {
                self.pending = Some((skill, c.now_ms));
                cs::learn_skill(skill)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::eval::SkillFacts;
    use super::*;
    use aa_proto::opcodes::cs as op;
    use std::collections::HashMap;

    struct Book(HashMap<u32, SkillFacts>);

    impl SkillBook for Book {
        fn facts(&self, skill: u32) -> Option<SkillFacts> {
            self.0.get(&skill).copied()
        }
    }

    fn skill(ability: u8, level: u8, need_learn: bool) -> SkillFacts {
        SkillFacts {
            ability_id: ability,
            ability_level: level,
            need_learn,
            ..SkillFacts::default()
        }
    }

    fn book() -> Book {
        Book(HashMap::from([
            (100, skill(1, 1, true)),
            (101, skill(1, 3, true)),
            (102, skill(1, 10, true)),
            (200, skill(3, 1, true)),
            (300, skill(4, 5, true)),
            (400, skill(1, 1, false)),
            (500, skill(0, 1, true)),
        ]))
    }

    fn plan<'a>(level: u8, abilities: [u8; 3], known: &'a [(u32, u8)]) -> PlanInput<'a> {
        PlanInput {
            level,
            abilities,
            known,
            order: &[400, 100, 101, 102, 200, 300, 500],
            required: &[1, 3, 4],
            unlock_second: 5,
            unlock_third: 10,
        }
    }

    #[test]
    fn skills_are_learned_in_order_when_the_level_allows() {
        let b = book();
        let known = [(100, 1)];
        // level 1: 100 known, 101 needs level 3, 102 level 10, 200's ability is not active
        assert_eq!(next_step(&plan(1, [1, 11, 11], &known), &b, &[]), None);
        assert_eq!(
            next_step(&plan(3, [1, 11, 11], &known), &b, &[]),
            Some(Step::Learn(101))
        );
        // not learnable skills (need_learn off) and skills without an ability are skipped
        let known = [(100, 1), (101, 1)];
        assert_eq!(next_step(&plan(4, [1, 11, 11], &known), &b, &[]), None);
    }

    #[test]
    fn abilities_are_unlocked_at_the_archetype_levels_one_after_the_other() {
        let b = book();
        let known = [(100, 1), (101, 1)];
        assert_eq!(next_step(&plan(4, [1, 11, 11], &known), &b, &[]), None);
        assert_eq!(
            next_step(&plan(5, [1, 11, 11], &known), &b, &[]),
            Some(Step::Swap { old: 11, new: 3 })
        );
        // with the second ability in, its skills are learnable and the third waits for level 10
        assert_eq!(
            next_step(&plan(5, [1, 3, 11], &known), &b, &[]),
            Some(Step::Learn(200))
        );
        let known = [(100, 1), (101, 1), (200, 1)];
        assert_eq!(next_step(&plan(9, [1, 3, 11], &known), &b, &[]), None);
        assert_eq!(
            next_step(&plan(10, [1, 3, 11], &known), &b, &[]),
            Some(Step::Swap { old: 11, new: 4 })
        );
        // the third never comes before the second
        assert_eq!(
            next_step(&plan(12, [1, 11, 11], &known), &b, &[]),
            Some(Step::Swap { old: 11, new: 3 })
        );
        // an ability already in a slot is not added twice
        assert_eq!(
            next_step(&plan(12, [1, 4, 3], &known), &b, &[]),
            Some(Step::Learn(102))
        );
    }

    #[test]
    fn refused_skills_wait_for_the_next_level() {
        let b = book();
        let known = [(100, 1)];
        let failed = [(101, 3)];
        assert_eq!(next_step(&plan(3, [1, 11, 11], &known), &b, &failed), None);
        assert_eq!(
            next_step(&plan(4, [1, 11, 11], &known), &b, &failed),
            Some(Step::Learn(101))
        );
    }

    fn arch() -> Archetype {
        let a = crate::archetype::Archetypes::builtin().unwrap();
        (**a.get("Abolisher").unwrap()).clone()
    }

    fn ctx<'a>(
        now: u64,
        level: u8,
        known: &'a [(u32, u8)],
        book: &'a Book,
        arch: &'a Archetype,
    ) -> LearnCtx<'a> {
        LearnCtx {
            now_ms: now,
            level,
            me_obj: 7,
            abilities: [1, 11, 11],
            known,
            book,
            arch,
        }
    }

    #[test]
    fn the_learner_paces_requests_and_notices_answers() {
        let a = arch();
        // Abolisher order starts with 18132; give it one learnable skill.
        let b = Book(HashMap::from([
            (18132, skill(1, 1, true)),
            (10399, skill(3, 1, true)),
        ]));
        let mut l = Learner::default();
        let known: Vec<(u32, u8)> = Vec::new();
        let out = l.tick(&ctx(1000, 1, &known, &b, &a)).expect("a request");
        assert_eq!(out.opcode, op::CSLearnSkillPacket);
        assert_eq!(out.body, 18132u32.to_le_bytes());
        // waiting for the answer
        assert!(l.tick(&ctx(2000, 1, &known, &b, &a)).is_none());
        // the answer arrives
        let known = vec![(18132, 1)];
        assert!(l.tick(&ctx(2100, 1, &known, &b, &a)).is_none(), "paced");
        assert_eq!(l.counts(), (1, 0));
        // the second ability is not due before level 5
        assert!(l.tick(&ctx(5000, 4, &known, &b, &a)).is_none());
        let out = l.tick(&ctx(6000, 5, &known, &b, &a)).expect("a swap");
        assert_eq!(out.opcode, op::CSSwapAbilityPacket);
        // after the swap the new ability's skill is learnable
        let out = l.tick(&ctx(8000, 5, &known, &b, &a)).expect("a request");
        assert_eq!(out.opcode, op::CSLearnSkillPacket);
        assert_eq!(out.body, 10399u32.to_le_bytes());
    }

    #[test]
    fn an_ignored_request_is_not_repeated_until_the_level_changes() {
        let a = arch();
        let b = Book(HashMap::from([(18132, skill(1, 1, true))]));
        let mut l = Learner::default();
        let known: Vec<(u32, u8)> = Vec::new();
        assert!(l.tick(&ctx(1000, 1, &known, &b, &a)).is_some());
        assert!(l.tick(&ctx(2000, 1, &known, &b, &a)).is_none());
        // no answer within 3 s: refused, and not asked again at this level
        assert!(l.tick(&ctx(4500, 1, &known, &b, &a)).is_none());
        assert!(l.tick(&ctx(9000, 1, &known, &b, &a)).is_none());
        assert_eq!(l.counts(), (0, 1));
        // a level later it tries again
        let out = l.tick(&ctx(9100, 2, &known, &b, &a)).expect("retry");
        assert_eq!(out.opcode, op::CSLearnSkillPacket);
    }

    #[test]
    fn more_than_four_ignored_requests_do_not_cycle() {
        let mut a = arch();
        a.skill_learn_order = vec![100, 101, 102, 103, 104];
        let b = Book(
            a.skill_learn_order
                .iter()
                .map(|&id| (id, skill(1, 1, true)))
                .collect(),
        );
        let mut learner = Learner::default();
        let mut now = 1_000;
        for &id in &a.skill_learn_order {
            let request = learner.tick(&ctx(now, 1, &[], &b, &a)).unwrap();
            assert_eq!(request.body, id.to_le_bytes());
            now += ANSWER_WAIT_MS + 1;
        }
        assert!(learner.tick(&ctx(now, 1, &[], &b, &a)).is_none());
        assert_eq!(learner.counts(), (0, 5));
        let retry = learner.tick(&ctx(now + 1, 2, &[], &b, &a)).unwrap();
        assert_eq!(retry.body, 100u32.to_le_bytes());
    }

    #[test]
    fn nothing_happens_before_the_character_is_known() {
        let a = arch();
        let b = Book(HashMap::new());
        let mut l = Learner::default();
        let mut c = ctx(1000, 1, &[], &b, &a);
        c.me_obj = 0;
        assert!(l.tick(&c).is_none());
        let mut c = ctx(1000, 1, &[], &b, &a);
        c.abilities = [0, 0, 0];
        assert!(l.tick(&c).is_none());
    }
}
