//! When a bot may start a fight, when it gives one up and runs, and how long it rests after a
//! resurrection.
//!
//! The rules are pure decisions over the facts the goals and the combat controller already have
//! ([`PullFacts`], the hit point percentage), configured in `[survival]`; the callers only gather
//! the facts and act on the verdict. The defaults come from the 2026-11-11 death journal: bots
//! kept pulling at low hit points, pulled packs and elites, fought on below 40 % and went back to
//! work right after a resurrection.

use crate::config::SurvivalCfg;

/// Why a bot does not pull a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullBlocker {
    /// The bot is too hurt to start a fight.
    LowHp,
    /// The bot has too little mana to start a fight.
    LowMp,
    /// The monster is too many levels above the bot.
    TooHighLevel,
    /// The monster is an elite or a boss.
    Elite,
    /// Too many hostile monsters stand around the target.
    TooManyHostiles,
}

impl PullBlocker {
    /// The word the debug line uses.
    pub fn word(self) -> &'static str {
        match self {
            PullBlocker::LowHp => "low hit points",
            PullBlocker::LowMp => "low mana",
            PullBlocker::TooHighLevel => "too high level",
            PullBlocker::Elite => "elite",
            PullBlocker::TooManyHostiles => "too many hostiles",
        }
    }
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
    /// The target's grade (1 = normal, higher = elite or boss).
    pub target_grade: u8,
    /// Hostile monsters within [`SurvivalCfg::pack_radius_m`] of the target, the target included.
    pub hostiles: u8,
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
    if cfg.max_grade != 0 && f.target_grade > cfg.max_grade {
        return Some(PullBlocker::Elite);
    }
    if cfg.max_pack != 0 && f.hostiles > cfg.max_pack {
        return Some(PullBlocker::TooManyHostiles);
    }
    None
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
    fn elites_and_bosses_are_not_pulled() {
        let cfg = SurvivalCfg::default();
        let elite = PullFacts {
            target_grade: cfg.max_grade + 1,
            ..facts()
        };
        assert_eq!(pull_blocker(&cfg, &elite), Some(PullBlocker::Elite));
        let normal = PullFacts {
            target_grade: cfg.max_grade,
            ..facts()
        };
        assert_eq!(pull_blocker(&cfg, &normal), None);
    }

    #[test]
    fn a_pack_larger_than_the_bot_can_take_is_not_pulled() {
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
