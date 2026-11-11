//! The death line and the per-session death counters.
//!
//! The journal of 2026-11-11 had 1,407 `died` lines and nothing else: no killer, no level, no
//! place, so the causes could not be told apart. [`death_line`] builds one line from what the
//! combat state already watched: the killer's template, name and level (`SCUnitDeathPacket`), the
//! aggro list at the moment of death, the hit points and mana the fight started with, the goal,
//! the zone and whether the bot was running. [`DeathCounters`] counts the session's deaths by the
//! kind of killer for the bot's stats.

use super::state::UnitKind;

/// What killed the bot, as far as the server and the view say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    /// A monster (an NPC).
    Monster,
    /// Another character (a player, or a bot in PvP).
    Character,
    /// Nothing in view names the killer (a fall, the environment, a unit that despawned).
    Unknown,
}

impl Cause {
    /// The cause of a death to a killer of `kind` (`None`: no killer in view).
    pub fn of(kind: Option<UnitKind>) -> Cause {
        match kind {
            Some(UnitKind::Npc) => Cause::Monster,
            Some(UnitKind::Character) => Cause::Character,
            _ => Cause::Unknown,
        }
    }

    /// The word the stats use.
    pub fn word(self) -> &'static str {
        match self {
            Cause::Monster => "monster",
            Cause::Character => "character",
            Cause::Unknown => "unknown",
        }
    }
}

/// The deaths of one session, by cause (counters of a few bytes: the per-bot budget is tight).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DeathCounters {
    /// Deaths to a monster.
    pub monsters: u16,
    /// Deaths to a character.
    pub characters: u16,
    /// Deaths without a killer in view.
    pub unknown: u16,
}

impl DeathCounters {
    /// Counts one death of `cause`.
    pub fn add(&mut self, cause: Cause) {
        match cause {
            Cause::Monster => self.monsters = self.monsters.saturating_add(1),
            Cause::Character => self.characters = self.characters.saturating_add(1),
            Cause::Unknown => self.unknown = self.unknown.saturating_add(1),
        }
    }

    /// All deaths of the session.
    pub fn total(self) -> u32 {
        u32::from(self.monsters) + u32::from(self.characters) + u32::from(self.unknown)
    }

    /// The line the bot's stats show.
    pub fn summary(self) -> String {
        format!(
            "{} (monster {}, character {}, unknown {})",
            self.total(),
            self.monsters,
            self.characters,
            self.unknown
        )
    }
}

/// The killer as the bot sees it at the moment of death.
#[derive(Debug, Clone, Copy, Default)]
pub struct Killer<'a> {
    /// Object id from the death packet (0 = none).
    pub obj: u32,
    /// NPC template id when the killer is a monster.
    pub template: Option<u32>,
    /// Character id when the killer is a character.
    pub character: Option<u32>,
    /// Display name, when the unit was in view.
    pub name: Option<&'a str>,
    /// Level, when known.
    pub level: Option<u8>,
    /// What kind of unit it is.
    pub kind: Option<UnitKind>,
}

/// Everything [`death_line`] writes down.
#[derive(Debug, Clone, Copy)]
pub struct DeathFacts<'a> {
    /// The bot's level.
    pub own_level: u8,
    /// The killer.
    pub killer: Killer<'a>,
    /// Hostile units that targeted the bot when it died.
    pub attackers: u8,
    /// Hit points in percent when the fight started (0 = the bot was not fighting).
    pub hp_pct: u8,
    /// Mana in percent when the fight started.
    pub mp_pct: u8,
    /// Whether `hp_pct`/`mp_pct` are the fight's start (else the last values seen alive).
    pub at_engage: bool,
    /// The goal the bot was running.
    pub goal: &'a str,
    /// The zone name, when the position is on the map.
    pub zone: Option<&'a str>,
    /// Where the bot died.
    pub pos: [f32; 3],
    /// Whether the bot was running from a fight.
    pub fleeing: bool,
}

/// One line an operator (and the issue tracker) can read: who killed the bot, how far above it
/// the killer stood, how many attacked, what the bot had when the fight began, what it was doing
/// and where.
pub fn death_line(f: &DeathFacts<'_>) -> String {
    let mut out = String::with_capacity(180);
    out.push_str("died to ");
    match f.killer.kind {
        Some(UnitKind::Npc) => {
            out.push_str(f.killer.name.unwrap_or("a monster"));
            if let Some(t) = f.killer.template {
                out.push_str(&format!(" (npc {t}"));
                if let Some(level) = f.killer.level {
                    out.push_str(&format!(
                        ", level {level}, {:+}",
                        i16::from(level) - i16::from(f.own_level)
                    ));
                }
                out.push(')');
            } else if let Some(level) = f.killer.level {
                out.push_str(&format!(
                    " (level {level}, {:+})",
                    i16::from(level) - i16::from(f.own_level)
                ));
            }
        }
        Some(UnitKind::Character) => {
            out.push_str(f.killer.name.unwrap_or("a character"));
            let id = f
                .killer
                .character
                .map_or_else(|| f.killer.obj.to_string(), |c| c.to_string());
            out.push_str(&format!(" (character {id}"));
            if let Some(level) = f.killer.level {
                out.push_str(&format!(
                    ", level {level}, {:+}",
                    i16::from(level) - i16::from(f.own_level)
                ));
            }
            out.push(')');
        }
        _ => {
            if f.killer.obj != 0 {
                out.push_str(&format!("unit {} (not in view)", f.killer.obj));
            } else {
                out.push_str("an unknown cause");
            }
        }
    }
    if let Some(zone) = f.zone {
        out.push_str(&format!(" in {zone} ({:.0}, {:.0})", f.pos[0], f.pos[1]));
    } else {
        out.push_str(&format!(" at ({:.0}, {:.0})", f.pos[0], f.pos[1]));
    }
    out.push_str(&format!("; goal {}", f.goal));
    if f.fleeing {
        out.push_str(" (fleeing)");
    }
    out.push_str(&format!("; attackers {}", f.attackers));
    let when = if f.at_engage {
        "at the fight's start"
    } else {
        "last seen alive"
    };
    out.push_str(&format!("; hp {}% mp {}% {when}", f.hp_pct, f.mp_pct));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts<'a>(killer: Killer<'a>) -> DeathFacts<'a> {
        DeathFacts {
            own_level: 20,
            killer,
            attackers: 2,
            hp_pct: 55,
            mp_pct: 70,
            at_engage: true,
            goal: "grind",
            zone: Some("Tigerspine Mountains"),
            pos: [12345.0, 6789.0, 120.0],
            fleeing: false,
        }
    }

    #[test]
    fn a_monster_death_names_the_template_level_gap_and_the_fight() {
        let line = death_line(&facts(Killer {
            obj: 0x401,
            template: Some(1234),
            character: None,
            name: Some("Bloodspike Boar"),
            level: Some(24),
            kind: Some(UnitKind::Npc),
        }));
        assert!(
            line.starts_with("died to Bloodspike Boar (npc 1234, level 24, +4)"),
            "{line}"
        );
        assert!(
            line.contains("in Tigerspine Mountains (12345, 6789)"),
            "{line}"
        );
        assert!(line.contains("goal grind"), "{line}");
        assert!(line.contains("attackers 2"), "{line}");
        assert!(
            line.contains("hp 55% mp 70% at the fight's start"),
            "{line}"
        );
        assert!(!line.contains("fleeing"), "{line}");
    }

    #[test]
    fn a_character_death_names_the_character() {
        let line = death_line(&facts(Killer {
            obj: 0x501,
            template: None,
            character: Some(77),
            name: Some("Ganker"),
            level: Some(18),
            kind: Some(UnitKind::Character),
        }));
        assert!(
            line.contains("died to Ganker (character 77, level 18, -2)"),
            "{line}"
        );
    }

    #[test]
    fn an_unknown_killer_is_said_so_and_fleeing_and_the_last_live_values_are_kept() {
        let mut f = facts(Killer::default());
        f.fleeing = true;
        f.at_engage = false;
        f.zone = None;
        let line = death_line(&f);
        assert!(
            line.starts_with("died to an unknown cause at (12345, 6789)"),
            "{line}"
        );
        assert!(line.contains("(fleeing)"), "{line}");
        assert!(line.contains("hp 55% mp 70% last seen alive"), "{line}");
        // a killer that left the view keeps its object id
        let mut f = facts(Killer {
            obj: 0x999,
            ..Killer::default()
        });
        f.killer.kind = None;
        assert!(death_line(&f).contains("unit 2457 (not in view)"));
    }

    #[test]
    fn the_session_counters_count_every_cause() {
        let mut c = DeathCounters::default();
        c.add(Cause::Monster);
        c.add(Cause::Monster);
        c.add(Cause::Character);
        c.add(Cause::Unknown);
        assert_eq!(c.total(), 4);
        assert_eq!(c.summary(), "4 (monster 2, character 1, unknown 1)");
        assert_eq!(Cause::of(Some(UnitKind::Npc)), Cause::Monster);
        assert_eq!(Cause::of(Some(UnitKind::Character)), Cause::Character);
        assert_eq!(Cause::of(Some(UnitKind::Slave)), Cause::Unknown);
        assert_eq!(Cause::of(None), Cause::Unknown);
    }
}
