//! When a bot may start a fight, when it gives one up and runs, and how long it rests after a
//! resurrection.
//!
//! The rules are pure decisions over the facts the goals and the combat controller already have
//! ([`PullFacts`], the hit point percentage), configured in `[survival]`; the callers only gather
//! the facts and act on the verdict. The defaults come from the 2026-11-11 death journal: bots
//! kept pulling at low hit points, pulled packs and elites, fought on below 40 % and went back to
//! work right after a resurrection.

use crate::config::{GradeLimit, SurvivalCfg};

/// Why a bot does not pull a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullBlocker {
    /// The bot is too hurt to start a fight.
    LowHp,
    /// The bot has too little mana to start a fight.
    LowMp,
    /// The monster is too many levels above the bot.
    TooHighLevel,
    /// The monster's grade is above the pull limit (an elite, strong, or a boss).
    Elite,
    /// Too many hostile monsters stand around the target.
    TooManyHostiles,
}

/// What [`pull_blocker`] looks at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PullFacts {
    /// The bot's hit points in percent.
    pub hp_pct: u8,
    /// The bot's mana in percent.
    pub mp_pct: u8,
    /// The bot's level.
    pub level: u8,
    /// The target's level.
    pub target_level: u8,
    /// The target's grade (`NpcGradeType`: 1 Normal, 2 Elite, 3-5/8 Boss, 6 Weak, 7 Strong).
    pub target_grade: u8,
    /// Hostile monsters within [`SurvivalCfg::pack_radius_m`] of the target that would aggro
    /// (or already do), the target included.
    pub hostiles: u8,
    /// Party members in range, the bot included (1 = solo).
    pub party_members: u8,
}

/// `NpcGradeType` ids the grade rule knows (`D:/aa-re/.../NpcGradeType.cs` and `npcs` in
/// compact.sqlite3): 1 Normal, 2 Elite, 3 BossA, 4 BossB, 5 BossC, 6 Weak, 7 Strong, 8 BossS.
pub const GRADE_ELITE: u8 = 2;
/// The lowest boss grade.
pub const GRADE_BOSS_A: u8 = 3;
/// A weak monster; weaker than normal, never blocked.
pub const GRADE_WEAK: u8 = 6;
/// A strong (veteran) monster; as strong as an elite.
pub const GRADE_STRONG: u8 = 7;
/// The world-boss grade.
pub const GRADE_BOSS_S: u8 = 8;

/// Whether grade `grade` is one of the bosses (BossA, BossB, BossC, BossS).
pub fn is_boss(grade: u8) -> bool {
    matches!(grade, GRADE_BOSS_A..=5 | GRADE_BOSS_S)
}

/// The word the reason line uses for a grade.
pub fn grade_word(grade: u8) -> &'static str {
    match grade {
        1 => "normal",
        2 => "elite",
        3 => "boss A",
        4 => "boss B",
        5 => "boss C",
        6 => "weak",
        7 => "strong",
        8 => "boss S",
        _ => "unknown grade",
    }
}

/// Why the bot must not pull the target, with the facts it was decided on, for the reason line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PullBlock {
    /// The rule that refused the pull.
    pub reason: PullBlocker,
    /// The facts the rule saw.
    pub facts: PullFacts,
}

/// Whether a monster near the target joins the pull: it is aggressive on its own, linked to the
/// target by `npc_aggro_links`, or already targets the bot. A passive, unlinked bystander does
/// not count towards the pack.
pub fn joins_pull(aggressive: bool, linked: bool, targets_bot: bool) -> bool {
    aggressive || linked || targets_bot
}

/// The pack a bot (or its party) may pull into.
pub fn pack_limit(cfg: &SurvivalCfg, f: &PullFacts) -> u8 {
    let members = f.party_members.max(1);
    cfg.max_pack.saturating_mul(members)
}

/// Whether the grade rule refuses `grade` for this party.
fn grade_blocked(cfg: &SurvivalCfg, f: &PullFacts) -> bool {
    match f.target_grade {
        // normal, weak and an unknown grade are always fine
        0 | 1 | GRADE_WEAK => false,
        // elite and strong are the default limit
        GRADE_ELITE | GRADE_STRONG => cfg.max_grade == GradeLimit::Normal,
        // bosses only from a big enough party, and never with `max_grade = normal`
        grade if is_boss(grade) => {
            let party_may = cfg.party_boss_min != 0 && f.party_members >= cfg.party_boss_min;
            cfg.max_grade != GradeLimit::Boss && (cfg.max_grade == GradeLimit::Normal || !party_may)
        }
        _ => false,
    }
}

/// The reason line an operator reads: `too strong: level +N`, `grade X` or `pack N`, or the
/// resource that is too low.
pub fn pull_reason(cfg: &SurvivalCfg, f: &PullFacts, reason: PullBlocker) -> String {
    match reason {
        PullBlocker::LowHp => format!(
            "low hit points: {}% (pull at {}%)",
            f.hp_pct, cfg.pull_min_hp_pct
        ),
        PullBlocker::LowMp => format!("low mana: {}% (pull at {}%)", f.mp_pct, cfg.pull_min_mp_pct),
        PullBlocker::TooHighLevel => format!(
            "too strong: level {:+} ({} vs {})",
            i16::from(f.target_level) - i16::from(f.level),
            f.target_level,
            f.level
        ),
        PullBlocker::Elite => format!(
            "too strong: grade {} ({})",
            f.target_grade,
            grade_word(f.target_grade)
        ),
        PullBlocker::TooManyHostiles => format!(
            "too strong: pack {} (limit {})",
            f.hostiles,
            pack_limit(cfg, f)
        ),
    }
}

/// Why the bot must not pull the target, `None` when it may. The first reason wins, so the
/// debug line names the most immediate danger.
pub fn pull_blocker(cfg: &SurvivalCfg, f: &PullFacts) -> Option<PullBlocker> {
    if !cfg.enabled {
        return None;
    }
    if f.hp_pct < cfg.pull_min_hp_pct {
        return Some(PullBlocker::LowHp);
    }
    if f.mp_pct < cfg.pull_min_mp_pct {
        return Some(PullBlocker::LowMp);
    }
    if f.target_level > f.level.saturating_add(cfg.max_level_gap) {
        return Some(PullBlocker::TooHighLevel);
    }
    if grade_blocked(cfg, f) {
        return Some(PullBlocker::Elite);
    }
    if cfg.max_pack != 0 && f.hostiles > pack_limit(cfg, f) {
        return Some(PullBlocker::TooManyHostiles);
    }
    None
}

/// [`pull_blocker`] with the facts kept, for the reason line.
pub fn pull_block(cfg: &SurvivalCfg, f: &PullFacts) -> Option<PullBlock> {
    pull_blocker(cfg, f).map(|reason| PullBlock { reason, facts: *f })
}

/// Whether the bot gives up a monster fight and runs: below the hit point line, or with at least
/// [`SurvivalCfg::flee_adds`] attackers while below [`SurvivalCfg::flee_adds_hp_pct`].
pub fn should_flee(cfg: &SurvivalCfg, hp_pct: u8, attackers: u8) -> bool {
    if !cfg.enabled {
        return false;
    }
    if cfg.flee_hp_pct != 0 && hp_pct < cfg.flee_hp_pct {
        return true;
    }
    cfg.flee_adds != 0 && attackers >= cfg.flee_adds && hp_pct < cfg.flee_adds_hp_pct
}

/// How long after a resurrection still counts as "just revived" (the rest that follows it is the
/// extended one).
pub const POST_REZ_WINDOW_MS: u64 = 15_000;

/// Whether the bot was resurrected so recently that the next rest is the post-resurrection one.
pub fn after_resurrection(now_ms: u64, last_resurrect_ms: u64) -> bool {
    last_resurrect_ms != 0 && now_ms.saturating_sub(last_resurrect_ms) <= POST_REZ_WINDOW_MS
}

/// The hit point percentage a rest has to reach: the post-resurrection one after a death, else
/// the ordinary 95 %.
pub fn rest_hp_target(cfg: &SurvivalCfg, post_rez: bool) -> u8 {
    if cfg.enabled && post_rez {
        cfg.post_rez_hp_pct
    } else {
        95
    }
}

/// The end of a rest: the ordinary random end, or the longer post-resurrection one after a
/// death.
pub fn rest_deadline(now_ms: u64, ordinary_end_ms: u64, cfg: &SurvivalCfg, post_rez: bool) -> u64 {
    if cfg.enabled && post_rez {
        ordinary_end_ms.max(now_ms.saturating_add(cfg.post_rez_rest_ms))
    } else {
        ordinary_end_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> PullFacts {
        PullFacts {
            hp_pct: 90,
            mp_pct: 90,
            level: 20,
            target_level: 20,
            target_grade: 1,
            hostiles: 1,
            party_members: 1,
        }
    }

    #[test]
    fn a_healthy_bot_may_pull_a_fair_target() {
        let cfg = SurvivalCfg::default();
        assert_eq!(pull_blocker(&cfg, &facts()), None);
    }

    #[test]
    fn a_hurt_or_drained_bot_does_not_pull() {
        let cfg = SurvivalCfg::default();
        let hurt = PullFacts {
            hp_pct: cfg.pull_min_hp_pct - 1,
            ..facts()
        };
        assert_eq!(pull_blocker(&cfg, &hurt), Some(PullBlocker::LowHp));
        let edge = PullFacts {
            hp_pct: cfg.pull_min_hp_pct,
            ..facts()
        };
        assert_eq!(pull_blocker(&cfg, &edge), None, "the line itself may pull");
        let drained = PullFacts {
            mp_pct: cfg.pull_min_mp_pct - 1,
            ..facts()
        };
        assert_eq!(pull_blocker(&cfg, &drained), Some(PullBlocker::LowMp));
    }

    #[test]
    fn a_monster_too_far_above_the_bot_is_not_pulled() {
        let cfg = SurvivalCfg::default();
        let fair = PullFacts {
            target_level: 20 + cfg.max_level_gap,
            ..facts()
        };
        assert_eq!(pull_blocker(&cfg, &fair), None, "the gap itself is fine");
        let high = PullFacts {
            target_level: 20 + cfg.max_level_gap + 1,
            ..facts()
        };
        assert_eq!(pull_blocker(&cfg, &high), Some(PullBlocker::TooHighLevel));
        let low = PullFacts {
            target_level: 5,
            ..facts()
        };
        assert_eq!(pull_blocker(&cfg, &low), None, "low monsters are fine");
    }

    #[test]
    fn normal_elite_weak_and_strong_monsters_are_pulled_but_bosses_are_not_solo() {
        let cfg = SurvivalCfg::default();
        assert_eq!(cfg.max_level_gap, 3, "1.2 quest targets are 2-3 above");
        for grade in [1, GRADE_ELITE, GRADE_WEAK, GRADE_STRONG] {
            let f = PullFacts {
                target_grade: grade,
                ..facts()
            };
            assert_eq!(pull_blocker(&cfg, &f), None, "grade {grade}");
        }
        for grade in [GRADE_BOSS_A, 4, 5, GRADE_BOSS_S] {
            let f = PullFacts {
                target_grade: grade,
                ..facts()
            };
            assert_eq!(
                pull_blocker(&cfg, &f),
                Some(PullBlocker::Elite),
                "boss grade {grade} solo"
            );
        }
        // a normal-only limit refuses elites and strong monsters too
        let strict = SurvivalCfg {
            max_grade: GradeLimit::Normal,
            ..cfg
        };
        let elite = PullFacts {
            target_grade: GRADE_ELITE,
            ..facts()
        };
        assert_eq!(pull_blocker(&strict, &elite), Some(PullBlocker::Elite));
        // a big enough party may take a boss; the same party with max_grade = normal may not
        let party = PullFacts {
            target_grade: GRADE_BOSS_A,
            party_members: cfg.party_boss_min,
            ..facts()
        };
        assert_eq!(pull_blocker(&cfg, &party), None);
        assert_eq!(pull_blocker(&strict, &party), Some(PullBlocker::Elite));
        // 0 turns bosses off for every party size
        let never = SurvivalCfg {
            party_boss_min: 0,
            ..cfg
        };
        assert_eq!(pull_blocker(&never, &party), Some(PullBlocker::Elite));
    }

    #[test]
    fn only_monsters_that_would_aggro_belong_to_the_pack() {
        assert!(joins_pull(true, false, false), "aggressive");
        assert!(joins_pull(false, true, false), "linked by npc_aggro_links");
        assert!(joins_pull(false, false, true), "already on the bot");
        assert!(!joins_pull(false, false, false), "a passive bystander");
    }

    #[test]
    fn a_pack_larger_than_the_bot_or_its_party_can_take_is_not_pulled() {
        let cfg = SurvivalCfg::default();
        let pack = PullFacts {
            hostiles: cfg.max_pack,
            ..facts()
        };
        assert_eq!(pull_blocker(&cfg, &pack), None, "the pack itself is fine");
        let crowd = PullFacts {
            hostiles: cfg.max_pack + 1,
            ..facts()
        };
        assert_eq!(
            pull_blocker(&cfg, &crowd),
            Some(PullBlocker::TooManyHostiles)
        );
        // two members may pull twice the pack; three, three times
        let two = PullFacts {
            hostiles: cfg.max_pack * 2,
            party_members: 2,
            ..facts()
        };
        assert_eq!(pull_blocker(&cfg, &two), None);
        let three = PullFacts {
            hostiles: cfg.max_pack * 3,
            party_members: 3,
            ..facts()
        };
        assert_eq!(pull_blocker(&cfg, &three), None);
        let too_many = PullFacts {
            hostiles: cfg.max_pack * 3 + 1,
            party_members: 3,
            ..facts()
        };
        assert_eq!(
            pull_blocker(&cfg, &too_many),
            Some(PullBlocker::TooManyHostiles)
        );
    }

    #[test]
    fn the_party_level_is_the_one_the_level_rule_compares_with() {
        let cfg = SurvivalCfg::default();
        let solo = PullFacts {
            target_level: 24,
            ..facts()
        };
        assert_eq!(pull_blocker(&cfg, &solo), Some(PullBlocker::TooHighLevel));
        // a level 22 member (the highest in range) turns the same target into a fair one
        let party = PullFacts {
            level: 22,
            target_level: 24,
            party_members: 2,
            ..facts()
        };
        assert_eq!(pull_blocker(&cfg, &party), None);
    }

    #[test]
    fn a_block_carries_a_reason_an_operator_can_read() {
        let cfg = SurvivalCfg::default();
        let high = PullBlock {
            reason: PullBlocker::TooHighLevel,
            facts: PullFacts {
                level: 20,
                target_level: 25,
                ..facts()
            },
        };
        assert_eq!(
            pull_reason(&cfg, &high.facts, high.reason),
            "too strong: level +5 (25 vs 20)"
        );
        let boss = PullBlock {
            reason: PullBlocker::Elite,
            facts: PullFacts {
                target_grade: GRADE_BOSS_A,
                ..facts()
            },
        };
        assert_eq!(
            pull_reason(&cfg, &boss.facts, boss.reason),
            "too strong: grade 3 (boss A)"
        );
        let pack = PullBlock {
            reason: PullBlocker::TooManyHostiles,
            facts: PullFacts {
                hostiles: 4,
                party_members: 1,
                ..facts()
            },
        };
        assert_eq!(
            pull_reason(&cfg, &pack.facts, pack.reason),
            "too strong: pack 4 (limit 2)"
        );
        assert!(pull_reason(
            &cfg,
            &PullFacts {
                hp_pct: 12,
                ..facts()
            },
            PullBlocker::LowHp
        )
        .contains("12%"));
    }

    #[test]
    fn the_switch_turns_every_pull_rule_off() {
        let cfg = SurvivalCfg {
            enabled: false,
            ..SurvivalCfg::default()
        };
        let awful = PullFacts {
            hp_pct: 0,
            mp_pct: 0,
            level: 1,
            target_level: 60,
            target_grade: 6,
            hostiles: 20,
            party_members: 1,
        };
        assert_eq!(pull_blocker(&cfg, &awful), None);
        assert!(!should_flee(&cfg, 1, 20));
    }

    #[test]
    fn a_bot_below_the_flee_line_or_swarmed_runs() {
        let cfg = SurvivalCfg::default();
        assert!(
            !should_flee(&cfg, cfg.flee_hp_pct, 0),
            "the line itself is firm"
        );
        assert!(should_flee(&cfg, cfg.flee_hp_pct - 1, 0));
        // a crowd while still somewhat healthy is no reason to run ...
        assert!(!should_flee(&cfg, cfg.flee_adds_hp_pct, cfg.flee_adds));
        // ... until the bot is worn down
        assert!(should_flee(&cfg, cfg.flee_adds_hp_pct - 1, cfg.flee_adds));
        assert!(!should_flee(
            &cfg,
            cfg.flee_adds_hp_pct - 1,
            cfg.flee_adds - 1
        ));
        let never = SurvivalCfg {
            flee_hp_pct: 0,
            flee_adds: 0,
            ..cfg
        };
        assert!(!should_flee(&never, 1, 20));
    }

    #[test]
    fn a_rest_after_a_resurrection_is_longer_and_waits_for_more_hit_points() {
        let cfg = SurvivalCfg::default();
        assert!(after_resurrection(10_000, 10_000 - 5_000));
        assert!(!after_resurrection(10_000 + POST_REZ_WINDOW_MS + 1, 10_000));
        assert!(!after_resurrection(10_000, 0), "no death yet");
        assert_eq!(rest_hp_target(&cfg, false), 95);
        assert_eq!(rest_hp_target(&cfg, true), cfg.post_rez_hp_pct);
        assert_eq!(rest_deadline(10_000, 20_000, &cfg, false), 20_000);
        assert_eq!(
            rest_deadline(10_000, 20_000, &cfg, true),
            10_000 + cfg.post_rez_rest_ms
        );
        // the ordinary rest is never cut short by the post-resurrection deadline
        assert_eq!(
            rest_deadline(10_000, 10_000 + cfg.post_rez_rest_ms + 5, &cfg, true),
            10_000 + cfg.post_rez_rest_ms + 5
        );
    }
}
