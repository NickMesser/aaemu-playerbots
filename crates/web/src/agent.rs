//! The agent API: how an LLM agent outside the process (or any program) reads and steers the
//! bots more finely than "pick a goal" (owned by the LLM and agent feature).
//!
//! **Steering** is the `act` command: start a goal once, with arguments. It is what an LLM agent,
//! the operator or the in-process LLM director uses:
//!
//! ```json
//! {"type": "act", "goal": "explore", "zone": "Solzreed", "why": "never been there"}
//! {"type": "act", "goal": "wander", "x": 11380.0, "y": 11900.0}
//! {"type": "act", "goal": "farm", "wild": true}
//! ```
//!
//! The bot runs the goal once and then chooses again as before; an operator's forced goal comes
//! back after it. A zone is a name (case-insensitive; a unique prefix is enough) or a zone key,
//! see `GET /api/agent/zones`. The `note` command writes a line into the bot's journal:
//! `{"type": "note", "text": "Bob owes us a duel"}`.
//!
//! **Reading** is made for a model's context window:
//!
//! ```text
//! GET /api/agent/tools          [Tool]         every action an agent may take, with a JSON schema
//! GET /api/agent/bots           [AgentBot]     one short row per bot
//! GET /api/agent/observe/{id}   Observation    one bot in a few dozen lines (404 if unknown)
//! GET /api/agent/zones          [ZoneEntry]    zone names and keys
//! ```
//!
//! [`tools`] is also the catalog of the `playerbots mcp` bridge, which offers the same actions to
//! an MCP client and forwards each call to the route its [`ToolHttp`] names.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::types::{Activity, BotSnapshot, Command};

/// Longest zone name, character name or reason, in characters.
pub const MAX_ARG_CHARS: usize = 80;
/// Longest note, in characters.
pub const MAX_NOTE_CHARS: usize = 160;

/// What `Command::Act` asks a bot to do. Every field but `goal` is optional; a goal reads only the
/// ones it understands.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct ActArgs {
    /// The goal, a [`Command::GOALS`] name other than `auto`.
    pub goal: String,
    /// A point (x and y together, world metres): where to walk, explore or work.
    pub x: Option<f32>,
    pub y: Option<f32>,
    /// A zone by name (as the World view and the bot detail show it; case-insensitive, a unique
    /// prefix is enough) or by zone key.
    pub zone: Option<String>,
    /// An item template the goal is about (a seed, a piece of gear, goods).
    pub item: Option<u32>,
    /// A character the goal is about (a duel partner, a player to follow, a buyer).
    pub target: Option<String>,
    /// Farm on unprotected land (a wild, "illegal" farm).
    pub wild: bool,
    /// Why, for the bot's log.
    pub why: Option<String>,
}

impl ActArgs {
    /// Cheap checks the web layer applies before the hub sees the command.
    pub fn validate(&self) -> Result<(), String> {
        let goal = self.goal.as_str();
        if goal == "auto" || !Command::GOALS.contains(&goal) {
            return Err(format!(
                "act needs a goal: one of {} except auto",
                Command::GOALS.join(", ")
            ));
        }
        match (self.x, self.y) {
            (None, None) => {}
            (Some(x), Some(y)) if x.is_finite() && y.is_finite() => {}
            _ => return Err("x and y go together and must be numbers".into()),
        }
        for (name, text) in [
            ("zone", self.zone.as_deref()),
            ("target", self.target.as_deref()),
            ("why", self.why.as_deref()),
        ] {
            if let Some(t) = text {
                if t.chars().count() > MAX_ARG_CHARS {
                    return Err(format!("{name} is longer than {MAX_ARG_CHARS} characters"));
                }
                if t.chars().any(char::is_control) {
                    return Err(format!("{name} contains control characters"));
                }
            }
        }
        Ok(())
    }
}

/// Checks the text of a `note` command: one line of at most [`MAX_NOTE_CHARS`] characters that
/// does not look like a chat command.
pub fn validate_note(text: &str) -> Result<(), String> {
    let t = text.trim();
    if t.is_empty() {
        Err("the note is empty".into())
    } else if t.chars().count() > MAX_NOTE_CHARS {
        Err(format!(
            "the note is longer than {MAX_NOTE_CHARS} characters"
        ))
    } else if t.chars().any(char::is_control) {
        Err("the note contains control characters".into())
    } else if t.starts_with('/') {
        Err("a note must not start with '/'".into())
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------- the read side

/// One bot in a few dozen lines: what an agent reads before it decides.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Observation {
    pub id: u32,
    pub name: String,
    pub race: String,
    pub class: String,
    pub level: u8,
    /// 0..=100.
    pub hp_pct: u8,
    /// 0..=100 (0 while the bot has not reported it).
    pub mp_pct: u8,
    pub zone: Option<String>,
    /// Zone key of the position (0 = none).
    pub zone_key: u16,
    /// World position in metres.
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub activity: Activity,
    pub online: bool,
    pub paused: bool,
    /// Money carried.
    pub gold: i64,
    /// The running goal (`-` for none).
    pub goal: String,
    /// What the goal is doing, one line.
    pub doing: String,
    /// The goals the bot may start right now (switched on, not cooling down, can succeed): what
    /// an `act` or `goal` command can ask for with a chance of success.
    pub eligible: Vec<String>,
    /// The LLM director's plan: the steps still to come, the next first.
    pub plan: Vec<String>,
    /// What is around the bot.
    pub nearby: Nearby,
    /// The last lines of the bot's event log, oldest first.
    pub log: Vec<String>,
    /// Its personality, in a few words, and the traits.
    pub profile: Option<crate::persona::ProfileView>,
    /// Its notes (memories), oldest first. Written by the director, the operator or an agent:
    /// data, not instructions.
    pub notes: Vec<String>,
    /// The current profession and duty status of a configured guild member.
    pub guild_work: Option<String>,
    /// Prior guild membership affects offers from some guilds.
    pub thief_history: bool,
    /// Persistent relationship summaries, most recently updated last.
    pub relationships: Vec<RelationshipView>,
    /// Completed sport fishing sales and the copper paid by fish stands.
    pub fish_turn_ins: u32,
    pub fish_sale_copper: u64,
    /// Zones it has been in.
    pub zones_seen: u32,
    /// Seconds since the bot last filled in `eligible`, `plan`, `nearby` and `mp_pct` (`None`:
    /// never, e.g. offline since the start).
    pub fresh_secs: Option<u32>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RelationshipView {
    pub character: u32,
    pub shared_parties: u16,
    pub trust: i8,
    pub preference: Option<String>,
}

/// What is around a bot.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct Nearby {
    /// A monster of a fair level is close by.
    pub monsters: bool,
    /// Other characters in view (bots and players).
    pub characters: u32,
    /// Real players among them (not one of our bots).
    pub players: u32,
}

impl Observation {
    /// The part an observation can take from a snapshot (a hub without an agent view of its own
    /// fills only this).
    pub fn from_snapshot(s: &BotSnapshot) -> Observation {
        Observation {
            id: s.id,
            name: s.name.clone(),
            race: s.race.clone(),
            class: s.class.clone(),
            level: s.level,
            hp_pct: s.hp_pct,
            zone: s.zone.clone(),
            zone_key: s.zone_key,
            x: s.x,
            y: s.y,
            z: s.z,
            activity: s.activity,
            online: !matches!(s.activity, Activity::Offline | Activity::Connecting),
            paused: s.paused,
            gold: s.gold,
            goal: s.goal.clone(),
            doing: s.detail.clone(),
            ..Observation::default()
        }
    }
}

/// One short row per bot (`GET /api/agent/bots`).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct AgentBot {
    pub id: u32,
    pub name: String,
    pub class: String,
    pub level: u8,
    pub zone: Option<String>,
    pub goal: String,
    pub activity: Activity,
    pub paused: bool,
}

impl AgentBot {
    /// The row of a snapshot.
    pub fn from_snapshot(s: &BotSnapshot) -> AgentBot {
        AgentBot {
            id: s.id,
            name: s.name.clone(),
            class: s.class.clone(),
            level: s.level,
            zone: s.zone.clone(),
            goal: s.goal.clone(),
            activity: s.activity,
            paused: s.paused,
        }
    }
}

/// A zone an `act` may name (`GET /api/agent/zones`).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct ZoneEntry {
    /// The zone key (what `act` accepts besides the name).
    pub key: u16,
    pub name: String,
}

// ---------------------------------------------------------------- the tool catalog

/// One action an agent may take: a name, what it does, the JSON schema of its input and the HTTP
/// route that does it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Tool {
    pub name: String,
    pub description: String,
    /// JSON schema of the arguments (an object).
    pub input_schema: Value,
    pub http: ToolHttp,
}

/// How a tool call becomes a request of this API.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ToolHttp {
    /// `GET` or `POST`.
    pub method: String,
    /// The path; `{bot}` stands for the `bot` argument. A `POST` with a `bot` argument goes to
    /// `/api/bots/{bot}/command`, one without to `/api/command`.
    pub path: String,
    /// For a `POST`: the command `type`; the body is the arguments without `bot`, plus the type.
    pub command: Option<String>,
}

fn get(path: &str) -> ToolHttp {
    ToolHttp {
        method: "GET".into(),
        path: path.into(),
        command: None,
    }
}

fn post(command: &str) -> ToolHttp {
    ToolHttp {
        method: "POST".into(),
        path: "/api/command".into(),
        command: Some(command.into()),
    }
}

fn tool(name: &str, description: &str, input_schema: Value, http: ToolHttp) -> Tool {
    Tool {
        name: name.into(),
        description: description.into(),
        input_schema,
        http,
    }
}

/// `{"type":"object","properties":...,"required":...}` without extra properties.
fn object(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn bot_arg(what: &str) -> Value {
    json!({"type": "integer", "minimum": 0, "description": what})
}

/// Every action an agent may take, in the order an agent would learn them.
pub fn tools() -> Vec<Tool> {
    let goals: Vec<&str> = Command::GOALS.to_vec();
    let act_goals: Vec<&str> = goals.iter().copied().filter(|g| *g != "auto").collect();
    let trait_arg = json!({"type": "number", "minimum": 0.0, "maximum": 1.0});
    let filter = json!({
        "type": "object",
        "description": "Only bots of this side, race or continent (all optional).",
        "properties": {
            "alliance": {"type": "string", "enum": ["nuia", "haranya"]},
            "race": {"type": "string", "enum": crate::population::SpawnFilter::RACES},
            "continent": {"type": "string", "enum": ["west", "east", "auroria", "sea"]},
        },
        "additionalProperties": false,
    });
    let count = json!({"type": "integer", "minimum": 1, "maximum": Command::MAX_COUNT});
    let text = json!({"type": "string", "minLength": 1, "maxLength": Command::MAX_SAY_CHARS});
    vec![
        tool(
            "list_bots",
            "List every bot in one short row each: id, name, class, level, zone, goal, activity, paused.",
            object(json!({}), &[]),
            get("/api/agent/bots"),
        ),
        tool(
            "observe",
            "Look at one bot: level, health, zone, position, what it does, the goals it may start \
right now (eligible), the director's plan, what is around it, its last log lines, personality \
and notes. Read this before you act.",
            object(json!({"bot": bot_arg("The bot id.")}), &["bot"]),
            get("/api/agent/observe/{bot}"),
        ),
        tool(
            "zones",
            "List the zone names and keys an act may name.",
            object(json!({}), &[]),
            get("/api/agent/zones"),
        ),
        tool(
            "stats",
            "Process-wide numbers: bots online, memory, traffic, the LLM budget.",
            object(json!({}), &[]),
            get("/api/stats"),
        ),
        tool(
            "act",
            "Make a bot start a goal once, with details: explore a zone, wander to a point, farm \
wild land, duel a named character. Afterwards the bot chooses again by itself. Pick a goal from the \
bot's eligible list (observe); a goal that cannot succeed just fails. Without bot: every bot.",
            object(
                json!({
                    "bot": bot_arg("The bot id; leave out to send it to every bot."),
                    "goal": {"type": "string", "enum": act_goals},
                    "zone": {"type": "string", "maxLength": MAX_ARG_CHARS, "description": "A zone name (a unique prefix is enough) or key, for explore."},
                    "x": {"type": "number", "description": "A point (with y), world metres: where to walk or work."},
                    "y": {"type": "number"},
                    "item": {"type": "integer", "minimum": 0, "description": "An item template the goal is about."},
                    "target": {"type": "string", "maxLength": MAX_ARG_CHARS, "description": "A character the goal is about."},
                    "wild": {"type": "boolean", "description": "farm: plant on unprotected land."},
                    "why": {"type": "string", "maxLength": MAX_ARG_CHARS, "description": "Why, for the bot's log."},
                }),
                &["goal"],
            ),
            post("act"),
        ),
        tool(
            "set_goal",
            "Keep a bot on one goal until told otherwise (auto hands the choice back to the bot). \
Without bot: every bot.",
            object(
                json!({
                    "bot": bot_arg("The bot id; leave out for every bot."),
                    "goal": {"type": "string", "enum": goals},
                }),
                &["goal"],
            ),
            post("goal"),
        ),
        tool(
            "say",
            "Make a bot say a line in local chat (plain text, no commands).",
            object(json!({"bot": bot_arg("The bot id."), "text": text}), &["bot", "text"]),
            post("say"),
        ),
        tool(
            "whisper",
            "Make a bot whisper a line to a character.",
            object(
                json!({
                    "bot": bot_arg("The bot id."),
                    "to": {"type": "string", "minLength": 1, "maxLength": Command::MAX_NAME_CHARS},
                    "text": text,
                }),
                &["bot", "to", "text"],
            ),
            post("whisper"),
        ),
        tool(
            "note",
            "Write a short memory into a bot's journal; the LLM director sees it as the bot's own notes.",
            object(
                json!({
                    "bot": bot_arg("The bot id."),
                    "text": {"type": "string", "minLength": 1, "maxLength": MAX_NOTE_CHARS},
                }),
                &["bot", "text"],
            ),
            post("note"),
        ),
        tool(
            "profile",
            "Change a bot's personality (each trait 0..1; a trait left out keeps its value).",
            object(
                json!({
                    "bot": bot_arg("The bot id."),
                    "chatty": trait_arg, "crude": trait_arg, "joker": trait_arg,
                    "explorer": trait_arg, "outlaw": trait_arg, "gearhead": trait_arg,
                    "dedication": trait_arg,
                    "chronotype": {"type": "string", "enum": crate::persona::CHRONOTYPES},
                }),
                &["bot"],
            ),
            post("profile"),
        ),
        tool(
            "pause",
            "Pause a bot (it stays logged in and does nothing). Without bot: every bot.",
            object(json!({"bot": bot_arg("The bot id; leave out for every bot.")}), &[]),
            post("pause"),
        ),
        tool(
            "resume",
            "Resume a paused bot. Without bot: every bot.",
            object(json!({"bot": bot_arg("The bot id; leave out for every bot.")}), &[]),
            post("resume"),
        ),
        tool(
            "spawn",
            "Log in more bots, optionally of one side, race or continent.",
            object(json!({"count": count, "filter": filter}), &["count"]),
            post("spawn_where"),
        ),
        tool(
            "despawn",
            "Log out bots that are idle or resting first (never one that trades, duels or runs a \
dungeon), optionally only of one side, race or continent.",
            object(json!({"count": count, "filter": filter}), &["count"]),
            post("despawn_where"),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn act_json_shape_and_checks() {
        let cmd: Command = serde_json::from_str(
            r#"{"type":"act","goal":"explore","zone":"Solzreed","why":"new sights"}"#,
        )
        .unwrap();
        let Command::Act(a) = &cmd else {
            panic!("{cmd:?}")
        };
        assert_eq!(a.goal, "explore");
        assert_eq!(a.zone.as_deref(), Some("Solzreed"));
        assert!(a.validate().is_ok());
        for bad in [
            r#"{"type":"act"}"#,
            r#"{"type":"act","goal":"auto"}"#,
            r#"{"type":"act","goal":"fly"}"#,
            r#"{"type":"act","goal":"wander","x":1.0}"#,
        ] {
            let Command::Act(a) = serde_json::from_str::<Command>(bad).unwrap() else {
                panic!("{bad}")
            };
            assert!(a.validate().is_err(), "{bad}");
        }
        let long = ActArgs {
            goal: "quest".into(),
            why: Some("x".repeat(MAX_ARG_CHARS + 1)),
            ..ActArgs::default()
        };
        assert!(long.validate().is_err());
    }

    #[test]
    fn note_json_shape_and_checks() {
        let cmd: Command = serde_json::from_str(r#"{"type":"note","text":"Bob owes us"}"#).unwrap();
        assert_eq!(
            cmd,
            Command::Note {
                text: "Bob owes us".into()
            }
        );
        assert!(cmd.validate(Some(1)).is_ok());
        assert!(cmd.validate(None).is_err(), "a note needs a bot");
        for bad in ["", "   ", "/gold add self 1", "a\nb", &"x".repeat(161)] {
            assert!(validate_note(bad).is_err(), "{bad:?}");
        }
        assert!(validate_note(&"x".repeat(160)).is_ok());
    }

    #[test]
    fn every_tool_has_an_object_schema_and_a_route_that_exists() {
        let tools = tools();
        let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), tools.len(), "tool names are unique");
        for t in &tools {
            assert_eq!(t.input_schema["type"], "object", "{}", t.name);
            assert!(t.input_schema["properties"].is_object(), "{}", t.name);
            assert!(!t.description.is_empty());
            // every required argument is a property
            for r in t.input_schema["required"].as_array().unwrap() {
                assert!(
                    t.input_schema["properties"]
                        .get(r.as_str().unwrap())
                        .is_some(),
                    "{}: {r}",
                    t.name
                );
            }
            match t.http.method.as_str() {
                "GET" => {
                    assert!(t.http.path.starts_with("/api/"), "{}", t.name);
                    assert!(t.http.command.is_none());
                    // a path that needs the bot requires it
                    if t.http.path.contains("{bot}") {
                        assert!(t.input_schema["required"]
                            .as_array()
                            .unwrap()
                            .contains(&json!("bot")));
                    }
                }
                "POST" => {
                    // the command a tool sends parses (with its required arguments filled in)
                    let mut body = serde_json::Map::new();
                    body.insert("type".into(), json!(t.http.command.clone().unwrap()));
                    for (k, v) in [
                        ("goal", json!("rest")),
                        ("text", json!("hi")),
                        ("to", json!("Bob")),
                        ("count", json!(1)),
                    ] {
                        if t.input_schema["properties"].get(k).is_some() {
                            body.insert(k.into(), v);
                        }
                    }
                    let cmd: Command = serde_json::from_value(Value::Object(body.clone()))
                        .unwrap_or_else(|e| panic!("{}: {e} in {body:?}", t.name));
                    let bot = t.input_schema["properties"].get("bot").map(|_| 1);
                    assert!(cmd.validate(bot).is_ok(), "{}: {cmd:?}", t.name);
                }
                other => panic!("{}: method {other}", t.name),
            }
        }
        for want in [
            "observe",
            "act",
            "note",
            "say",
            "spawn",
            "zones",
            "list_bots",
        ] {
            assert!(names.contains(&want), "{want}");
        }
        // the catalog is plain JSON
        let v = serde_json::to_value(&tools).unwrap();
        assert_eq!(v[0]["http"]["method"], "GET");
    }

    #[test]
    fn an_observation_takes_what_a_snapshot_has() {
        let s = BotSnapshot {
            id: 4,
            name: "Aria".into(),
            level: 12,
            activity: Activity::Questing,
            goal: "quest".into(),
            detail: "talk to Maude".into(),
            zone: Some("Solzreed".into()),
            ..BotSnapshot::default()
        };
        let o = Observation::from_snapshot(&s);
        assert_eq!((o.id, o.level, o.online), (4, 12, true));
        assert_eq!(o.doing, "talk to Maude");
        assert!(o.eligible.is_empty() && o.fresh_secs.is_none());
        let v = serde_json::to_value(&o).unwrap();
        assert_eq!(v["activity"], "questing");
        assert_eq!(v["nearby"]["players"], 0);
        assert!(!Observation::from_snapshot(&BotSnapshot::default()).online);
        let row = AgentBot::from_snapshot(&s);
        assert_eq!(
            serde_json::to_value(&row).unwrap()["zone"],
            json!("Solzreed")
        );
    }
}
