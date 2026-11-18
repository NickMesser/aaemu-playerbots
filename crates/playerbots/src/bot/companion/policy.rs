//! The invitation policy: may a bot join the party of a real player (or invite one) right now?
//!
//! Pure: [`judge`] looks at a [`Facts`] and says yes or why not ([`Why`]). The server never
//! expires a team invitation and a pending one blocks every later invitation to the same
//! character, so a bot answers each invitation it gets within seconds, with a yes or a no; this is
//! the no.

use crate::bot::cmd::GoalKind;

/// What the decision looks at.
#[derive(Debug, Clone, Copy)]
pub struct Facts {
    /// The goal the bot is running.
    pub goal: Option<GoalKind>,
    /// The bot is in a team already (a second membership is never accepted: the server would add
    /// the bot to the new team without checking).
    pub in_team: bool,
    /// The bot is in an instance or loading one.
    pub away: bool,
    /// The bot is dead or not in the world yet.
    pub down: bool,
    /// A trade window is open.
    pub trading: bool,
    /// The bot is in a duel.
    pub dueling: bool,
    /// The bot is on a portal journey, a boat or a seat.
    pub travelling: bool,
    /// The bot's level.
    pub level: u8,
    /// The level of the player, when the bot can see them.
    pub other_level: Option<u8>,
    /// `party.max_level_gap`.
    pub max_gap: u8,
    /// Bots of this process in parties of real players right now.
    pub companions: usize,
    /// `party.max_companions`.
    pub max_companions: usize,
    /// Bots of this process in this player's party.
    pub with_player: usize,
    /// `party.max_per_player`.
    pub max_per_player: usize,
}

/// Why a bot says no.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// The bot is in a team already.
    InTeam,
    /// The bot is in the middle of something it should not drop.
    Busy,
    /// The bot is in a dungeon, loading, dead or not in the world.
    Away,
    /// The levels are too far apart.
    LevelGap,
    /// This process already has as many bots in player parties as it allows.
    TooMany,
    /// This player's party has as many of our bots as it should.
    FullParty,
    /// The character has repeatedly attacked this bot.
    Distrusted,
}

impl Why {
    /// A word for the log.
    pub fn word(self) -> &'static str {
        match self {
            Why::InTeam => "in a team",
            Why::Busy => "busy",
            Why::Away => "away",
            Why::LevelGap => "level gap",
            Why::TooMany => "too many companions",
            Why::FullParty => "party full of bots",
            Why::Distrusted => "I don't trust that player",
        }
    }
}

/// Goals a bot does not drop for a party: a dungeon run, a duel or a trade with another bot, a
/// meeting with a buyer, a guild founding, a gank squad, a war, a rift, a trip with a trade pack,
/// the auction house and a boat.
pub fn busy_goal(kind: GoalKind) -> bool {
    matches!(
        kind,
        GoalKind::Dungeon
            | GoalKind::Duel
            | GoalKind::Trade
            | GoalKind::TradeRun
            | GoalKind::Market
            | GoalKind::Auction
            | GoalKind::Guild
            | GoalKind::Gank
            | GoalKind::Warzone
            | GoalKind::Event
            | GoalKind::Sail
    )
}

/// Goals the companion goal replaces when the bot joins a party (everything that is not
/// [`busy_goal`], and not the companion goal itself).
pub fn preemptible(kind: GoalKind) -> bool {
    kind != GoalKind::Companion && !busy_goal(kind)
}

/// Whether the level `other` is within `gap` levels of `own`.
pub fn level_fits(own: u8, other: u8, gap: u8) -> bool {
    own.abs_diff(other) <= gap
}

/// The decision. `Ok(())` accepts.
pub fn judge(f: &Facts) -> Result<(), Why> {
    if f.in_team {
        return Err(Why::InTeam);
    }
    if f.down || f.away {
        return Err(Why::Away);
    }
    if f.trading || f.dueling || f.travelling || f.goal.is_some_and(busy_goal) {
        return Err(Why::Busy);
    }
    if f.other_level
        .is_some_and(|l| !level_fits(f.level, l, f.max_gap))
    {
        return Err(Why::LevelGap);
    }
    if f.companions >= f.max_companions {
        return Err(Why::TooMany);
    }
    if f.with_player >= f.max_per_player {
        return Err(Why::FullParty);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready() -> Facts {
        Facts {
            goal: Some(GoalKind::Grind),
            in_team: false,
            away: false,
            down: false,
            trading: false,
            dueling: false,
            travelling: false,
            level: 20,
            other_level: Some(24),
            max_gap: 10,
            companions: 0,
            max_companions: 8,
            with_player: 0,
            max_per_player: 4,
        }
    }

    #[test]
    fn an_idle_bot_of_a_fitting_level_accepts() {
        assert_eq!(judge(&ready()), Ok(()));
        // no goal yet, or a small one, is idle enough
        for goal in [
            None,
            Some(GoalKind::Wander),
            Some(GoalKind::Rest),
            Some(GoalKind::Quest),
            Some(GoalKind::Grind),
            Some(GoalKind::Housing),
            Some(GoalKind::Farm),
        ] {
            assert_eq!(judge(&Facts { goal, ..ready() }), Ok(()), "{goal:?}");
        }
        // an unknown level is no reason to say no
        assert_eq!(
            judge(&Facts {
                other_level: None,
                ..ready()
            }),
            Ok(())
        );
    }

    #[test]
    fn a_busy_bot_says_no() {
        for kind in [
            GoalKind::Dungeon,
            GoalKind::Duel,
            GoalKind::Trade,
            GoalKind::TradeRun,
            GoalKind::Market,
            GoalKind::Auction,
            GoalKind::Guild,
            GoalKind::Gank,
            GoalKind::Warzone,
            GoalKind::Event,
            GoalKind::Sail,
        ] {
            assert_eq!(
                judge(&Facts {
                    goal: Some(kind),
                    ..ready()
                }),
                Err(Why::Busy),
                "{kind:?}"
            );
            assert!(busy_goal(kind) && !preemptible(kind));
        }
        for f in [
            Facts {
                trading: true,
                ..ready()
            },
            Facts {
                dueling: true,
                ..ready()
            },
            Facts {
                travelling: true,
                ..ready()
            },
        ] {
            assert_eq!(judge(&f), Err(Why::Busy));
        }
        assert!(preemptible(GoalKind::Quest) && preemptible(GoalKind::Rest));
        assert!(!preemptible(GoalKind::Companion));
    }

    #[test]
    fn a_bot_in_a_team_or_away_says_no() {
        assert_eq!(
            judge(&Facts {
                in_team: true,
                ..ready()
            }),
            Err(Why::InTeam)
        );
        for f in [
            Facts {
                away: true,
                ..ready()
            },
            Facts {
                down: true,
                ..ready()
            },
        ] {
            assert_eq!(judge(&f), Err(Why::Away));
        }
    }

    #[test]
    fn the_level_gap_and_the_caps_decide() {
        assert!(level_fits(20, 30, 10) && level_fits(30, 20, 10));
        assert!(!level_fits(20, 31, 10) && !level_fits(31, 20, 10));
        assert_eq!(
            judge(&Facts {
                other_level: Some(35),
                ..ready()
            }),
            Err(Why::LevelGap)
        );
        assert_eq!(
            judge(&Facts {
                other_level: Some(30),
                ..ready()
            }),
            Ok(())
        );
        assert_eq!(
            judge(&Facts {
                companions: 8,
                ..ready()
            }),
            Err(Why::TooMany)
        );
        assert_eq!(
            judge(&Facts {
                companions: 7,
                ..ready()
            }),
            Ok(())
        );
        assert_eq!(
            judge(&Facts {
                with_player: 4,
                ..ready()
            }),
            Err(Why::FullParty)
        );
        for why in [
            Why::InTeam,
            Why::Busy,
            Why::Away,
            Why::LevelGap,
            Why::TooMany,
            Why::FullParty,
        ] {
            assert!(!why.word().is_empty());
        }
    }
}
