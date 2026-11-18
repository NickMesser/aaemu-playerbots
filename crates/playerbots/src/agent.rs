//! The agent API: how an LLM (or any program) steers the bots more finely than "pick a goal"
//! (owned by the LLM and agent feature).
//!
//! The hub calls in here for the parts of `pb_web`'s agent API that need the bot runtime:
//!
//! * [`act_command`] turns the web UI's `act` command ([`pb_web::ActArgs`]) into a
//!   [`BotCmd::GoalWith`]: the goal runs once with its arguments, then the bot chooses again. A
//!   zone is a name or a key; [`ZoneNames::resolve`] turns a name into the key first
//!   ([`resolve_zones`]).
//! * [`write_note`] keeps a `note` command in the bot's journal.
//! * [`observe`] is one bot for an LLM to read; the part only the bot knows (the goals it may start
//!   now, the director's plan, what is around it) is the [`AgentSlot`] the brain fills in every
//!   [`AGENT_EVERY_MS`].
//! * [`mcp`] is the `playerbots mcp` bridge: an MCP server on stdin and stdout that forwards tool
//!   calls to the web API.

pub mod mcp;

use crate::bot::cmd::{BotCmd, GoalArgs, GoalKind, GoalMask};
use crate::hub::{unix_ms, Hub, SlotInner};
use crate::journal::Journal;

/// How often the brain refreshes its [`AgentSlot`], on the bot clock.
pub const AGENT_EVERY_MS: u64 = 15_000;
/// Event log lines an observation shows.
const OBSERVE_LOG: usize = 8;
/// Notes an observation shows (the newest).
const OBSERVE_NOTES: usize = 8;
/// Zones named in an "ambiguous" error.
const AMBIGUOUS_SHOWN: usize = 5;

/// What the agent API shows about a bot that only the bot itself knows, filled in by its brain
/// every [`AGENT_EVERY_MS`] (a field of the hub slot).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct AgentSlot {
    /// Unix milliseconds of the last refresh (0: never).
    pub at_unix_ms: u64,
    /// The goals the bot may start now (its own scoring gives them a chance), a bit per
    /// [`GoalKind::index`].
    pub eligible: GoalMask,
    /// Mana in percent.
    pub mp_pct: u8,
    /// A monster of a fair level is near.
    pub monsters_near: bool,
    /// Other characters within 60 m.
    pub characters_near: u16,
    /// Real players (not our bots) among them.
    pub players_near: u16,
    /// The LLM director's plan, the next step first.
    pub plan: Vec<String>,
    /// The bot's role in a configured guild.
    pub guild_work: Option<String>,
}

/// The goal names of `mask`, in [`GoalKind::ALL`] order.
pub fn goal_names(mask: GoalMask) -> Vec<String> {
    GoalKind::ALL
        .into_iter()
        .filter(|g| mask & (1 << g.index()) != 0)
        .map(|g| g.name().to_string())
        .collect()
}

// ---- zone names ------------------------------------------------------------------------------

/// The zone names an `act` may use, built once from the world when the hub is made.
#[derive(Debug, Default)]
pub struct ZoneNames {
    /// `(lower-case name, key, name)`, sorted by lower-case name, then key.
    rows: Vec<(Box<str>, u16, Box<str>)>,
}

impl ZoneNames {
    /// The table of every zone key the world names.
    pub fn from_world(world: &aa_world::World) -> ZoneNames {
        ZoneNames::from_pairs((1..=u16::MAX).filter_map(|k| world.zone_name(k).map(|n| (k, n))))
    }

    /// The table of `(key, name)` pairs; empty names are left out.
    pub fn from_pairs<'a>(pairs: impl IntoIterator<Item = (u16, &'a str)>) -> ZoneNames {
        let mut rows: Vec<(Box<str>, u16, Box<str>)> = pairs
            .into_iter()
            .map(|(k, n)| (k, n.trim()))
            .filter(|(_, n)| !n.is_empty())
            .map(|(k, n)| (n.to_lowercase().into(), k, n.into()))
            .collect();
        rows.sort();
        rows.dedup_by_key(|r| r.1);
        ZoneNames { rows }
    }

    /// Whether the table is empty (a world without zone names).
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The name of zone `key`.
    pub fn name(&self, key: u16) -> Option<&str> {
        self.rows.iter().find(|r| r.1 == key).map(|r| &*r.2)
    }

    /// Every zone, by name, for `GET /api/agent/zones`.
    pub fn entries(&self) -> Vec<pb_web::ZoneEntry> {
        self.rows
            .iter()
            .map(|(_, key, name)| pb_web::ZoneEntry {
                key: *key,
                name: name.to_string(),
            })
            .collect()
    }

    /// The zone key `text` names: a key, a name (any case) or the start of exactly one name. A
    /// name several keys share gives the lowest key. Without a table any number is taken as it
    /// is.
    pub fn resolve(&self, text: &str) -> Result<u16, String> {
        let t = text.trim();
        if t.is_empty() {
            return Err("the zone is empty".into());
        }
        if let Ok(key) = t.parse::<u16>() {
            return if self.rows.is_empty() || self.rows.iter().any(|r| r.1 == key) {
                Ok(key)
            } else {
                Err(format!("no zone has the key {key}"))
            };
        }
        let low = t.to_lowercase();
        if let Some(r) = self.rows.iter().find(|r| *r.0 == *low) {
            return Ok(r.1);
        }
        let mut names: Vec<&(Box<str>, u16, Box<str>)> = Vec::new();
        for r in self.rows.iter().filter(|r| r.0.starts_with(low.as_str())) {
            if names.last().is_none_or(|last| last.0 != r.0) {
                names.push(r);
            }
        }
        match names.as_slice() {
            [] => Err(format!(
                "unknown zone {t:?} (GET /api/agent/zones lists them)"
            )),
            [one] => Ok(one.1),
            many => {
                let shown: Vec<&str> = many.iter().take(AMBIGUOUS_SHOWN).map(|r| &*r.2).collect();
                let more = many.len().saturating_sub(AMBIGUOUS_SHOWN);
                Err(format!(
                    "zone {t:?} is ambiguous: {}{}",
                    shown.join(", "),
                    if more > 0 {
                        format!(" and {more} more")
                    } else {
                        String::new()
                    }
                ))
            }
        }
    }
}

/// `cmd` with the zone name of an `act` replaced by its key; every other command as it is.
pub fn resolve_zones(zones: &ZoneNames, cmd: pb_web::Command) -> Result<pb_web::Command, String> {
    match cmd {
        pb_web::Command::Act(mut a) => {
            if let Some(z) = a.zone.as_deref().map(str::trim).filter(|z| !z.is_empty()) {
                a.zone = Some(zones.resolve(z)?.to_string());
            }
            Ok(pb_web::Command::Act(a))
        }
        other => Ok(other),
    }
}

// ---- commands --------------------------------------------------------------------------------

/// The bot command for an `act` (the web layer checked the values; [`resolve_zones`] turned a zone
/// name into its key).
pub fn act_command(a: &pb_web::ActArgs) -> Result<BotCmd, String> {
    let kind = GoalKind::parse(&a.goal).ok_or_else(|| format!("unknown goal {:?}", a.goal))?;
    let zone = match a.zone.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(z) => Some(
            z.parse::<u16>()
                .map_err(|_| format!("unknown zone {z:?} (give the zone name or key)"))?,
        ),
    };
    let args = GoalArgs {
        dest: a.x.zip(a.y).map(|(x, y)| [x, y]),
        zone,
        item: a.item,
        target: a
            .target
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(Into::into),
        wild: a.wild,
        why: a
            .why
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(|t| format!("agent: {t}").into()),
    };
    Ok(BotCmd::GoalWith {
        kind,
        args: Box::new(args),
    })
}

/// Keeps a `note` command in the journal of bot `id` (filtered like a model's note: no command, no
/// link) and says so in the bot's log.
pub fn write_note(hub: &Hub, id: u32, text: &str) -> Result<(), String> {
    let slot = hub.slot(id).ok_or_else(|| format!("no bot {id}"))?;
    let now = unix_ms() / 1000;
    let kept = hub.journal.update(id, |j| j.notes.push_clean(now, text));
    match kept {
        Some(line) => {
            slot.push_log(format!("note: {line}"));
            Ok(())
        }
        None => {
            Err("the note was refused: it looks like a command, holds a link, or is empty".into())
        }
    }
}

// ---- the read side ---------------------------------------------------------------------------

/// Bot `id` as the agent API shows it, `None` if unknown.
pub fn observe(hub: &Hub, id: u32) -> Option<pb_web::Observation> {
    let slot = hub.slot(id)?;
    let now = unix_ms();
    let journal = hub.journal.read(id, |j| j.clone());
    Some(slot.with(|g| observation(g, &journal, now)))
}

/// The observation of one slot and its journal at unix time `now_ms` (the pure part of
/// [`observe`]). What the brain filled in is shown only while the bot is online.
pub fn observation(g: &SlotInner, journal: &Journal, now_ms: u64) -> pb_web::Observation {
    let mut o = pb_web::Observation::from_snapshot(&g.snap);
    o.log = g
        .log
        .iter()
        .rev()
        .take(OBSERVE_LOG)
        .rev()
        .map(|l| l.text.clone())
        .collect();
    o.profile = g.profile.clone();
    let a = &g.agent;
    if o.online && a.at_unix_ms > 0 {
        o.fresh_secs =
            Some((now_ms.saturating_sub(a.at_unix_ms) / 1000).min(u64::from(u32::MAX)) as u32);
        o.mp_pct = a.mp_pct;
        o.eligible = goal_names(a.eligible);
        o.plan = a.plan.clone();
        o.guild_work = a.guild_work.clone();
        o.nearby = pb_web::Nearby {
            monsters: a.monsters_near,
            characters: u32::from(a.characters_near),
            players: u32::from(a.players_near),
        };
    }
    let notes = &journal.notes.lines;
    o.notes = notes
        .iter()
        .skip(notes.len().saturating_sub(OBSERVE_NOTES))
        .map(|n| n.text.clone())
        .collect();
    o.zones_seen = journal.seen.zones.len() as u32;
    o.thief_history = journal.thief_history;
    o.fish_turn_ins = journal.fish_turn_ins;
    o.fish_sale_copper = journal.fish_sale_copper;
    o.relationships = journal
        .relationships
        .entries
        .iter()
        .map(|r| pb_web::agent::RelationshipView {
            character: r.character,
            shared_parties: r.shared_parties,
            trust: r.trust,
            preference: r.preference.map(|p| p.label().to_owned()),
        })
        .collect();
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zones() -> ZoneNames {
        ZoneNames::from_pairs([
            (129, "Solzreed Peninsula"),
            (130, "Solzreed Peninsula"),
            (133, "Gweonid Forest"),
            (140, "Solis Headlands"),
            (150, "  "),
            (151, "Two Crowns"),
        ])
    }

    #[test]
    fn an_act_becomes_a_goal_with_its_arguments() {
        let a = pb_web::ActArgs {
            goal: "explore".into(),
            x: Some(10.0),
            y: Some(20.0),
            zone: Some(" 12 ".into()),
            why: Some("new sights".into()),
            ..pb_web::ActArgs::default()
        };
        let BotCmd::GoalWith { kind, args } = act_command(&a).unwrap() else {
            panic!()
        };
        assert_eq!(kind, GoalKind::Explore);
        assert_eq!(args.dest, Some([10.0, 20.0]));
        assert_eq!(args.zone, Some(12));
        assert_eq!(args.why.as_deref(), Some("agent: new sights"));
        assert!(act_command(&pb_web::ActArgs {
            goal: "fly".into(),
            ..pb_web::ActArgs::default()
        })
        .is_err());
        assert!(act_command(&pb_web::ActArgs {
            goal: "explore".into(),
            zone: Some("Nowhere".into()),
            ..pb_web::ActArgs::default()
        })
        .is_err());
    }

    #[test]
    fn zone_names_resolve_by_key_name_and_unique_prefix() {
        let z = zones();
        assert_eq!(z.resolve("133"), Ok(133));
        assert!(z.resolve("999").unwrap_err().contains("999"));
        assert_eq!(z.resolve("gweonid forest"), Ok(133));
        assert_eq!(z.resolve("  GWEO "), Ok(133), "a unique prefix, any case");
        // a name several keys share is its lowest key
        assert_eq!(z.resolve("Solzreed Peninsula"), Ok(129));
        assert_eq!(z.resolve("solz"), Ok(129));
        let err = z.resolve("Sol").unwrap_err();
        assert!(
            err.contains("ambiguous")
                && err.contains("Solis Headlands")
                && err.contains("Solzreed"),
            "{err}"
        );
        assert!(z.resolve("Atlantis").unwrap_err().contains("unknown"));
        assert!(z.resolve("  ").is_err());
        // blank names are not zones
        assert_eq!(z.name(150), None);
        assert_eq!(z.name(151), Some("Two Crowns"));
        // without a table a number is taken as it is
        assert_eq!(ZoneNames::default().resolve("77"), Ok(77));
        assert!(ZoneNames::default().resolve("Solzreed").is_err());
        let names: Vec<String> = z.entries().into_iter().map(|e| e.name).collect();
        assert_eq!(names[0], "Gweonid Forest", "sorted by name");
        assert_eq!(z.entries().len(), 5);
    }

    #[test]
    fn an_act_with_a_zone_name_is_resolved_before_it_becomes_a_bot_command() {
        let cmd = pb_web::Command::Act(pb_web::ActArgs {
            goal: "explore".into(),
            zone: Some("gweonid".into()),
            ..pb_web::ActArgs::default()
        });
        let pb_web::Command::Act(a) = resolve_zones(&zones(), cmd).unwrap() else {
            panic!()
        };
        assert_eq!(a.zone.as_deref(), Some("133"));
        let BotCmd::GoalWith { args, .. } = act_command(&a).unwrap() else {
            panic!()
        };
        assert_eq!(args.zone, Some(133));
        // an unknown name is refused in words; other commands pass
        let bad = pb_web::Command::Act(pb_web::ActArgs {
            goal: "explore".into(),
            zone: Some("Atlantis".into()),
            ..pb_web::ActArgs::default()
        });
        assert!(resolve_zones(&zones(), bad).is_err());
        assert_eq!(
            resolve_zones(&zones(), pb_web::Command::Pause),
            Ok(pb_web::Command::Pause)
        );
    }

    #[test]
    fn an_observation_shows_what_the_brain_filled_in_only_while_online() {
        let mut g = SlotInner::default();
        g.snap.id = 3;
        g.snap.name = "Aria".into();
        g.snap.activity = pb_web::Activity::Questing;
        for i in 0..20 {
            g.log.push_back(pb_web::LogLine {
                t: i,
                text: format!("line {i}"),
            });
        }
        g.agent = AgentSlot {
            at_unix_ms: 10_000,
            eligible: (1 << GoalKind::Quest.index()) | (1 << GoalKind::Rest.index()),
            mp_pct: 40,
            monsters_near: true,
            characters_near: 3,
            players_near: 1,
            plan: vec!["explore Gweonid Forest".into()],
            guild_work: None,
        };
        let mut j = Journal::default();
        j.seen.add_zone(4);
        for i in 0..12 {
            j.notes.push(i, &format!("note {i}"));
        }
        let o = observation(&g, &j, 25_500);
        assert_eq!(o.fresh_secs, Some(15));
        assert_eq!(o.eligible, vec!["rest".to_string(), "quest".to_string()]);
        assert_eq!(o.plan, vec!["explore Gweonid Forest".to_string()]);
        assert_eq!((o.nearby.characters, o.nearby.players), (3, 1));
        assert_eq!(o.mp_pct, 40);
        assert_eq!(o.log.len(), OBSERVE_LOG);
        assert_eq!(o.log.last().unwrap(), "line 19");
        assert_eq!(o.notes.len(), OBSERVE_NOTES);
        assert_eq!(o.notes.last().unwrap(), "note 11");
        assert_eq!(o.zones_seen, 1);
        // offline: the bot's own view is stale and left out
        g.snap.activity = pb_web::Activity::Offline;
        let o = observation(&g, &j, 25_500);
        assert!(o.eligible.is_empty() && o.plan.is_empty() && o.fresh_secs.is_none());
        assert_eq!(o.notes.len(), OBSERVE_NOTES, "the journal is still there");
    }
}
