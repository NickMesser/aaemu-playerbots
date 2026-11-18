//! Types shared between the web UI and whatever implements [`BotHub`].
//!
//! The web crate knows nothing about how bots work: the bot runtime publishes plain snapshots
//! through a [`BotHub`] and receives [`Command`]s back. Everything here is `serde`-friendly and is
//! exactly what the JSON API sends or accepts.

use serde::{Deserialize, Serialize};

use crate::population::{
    Alliance, Continent, DirectorView, EconomyView, PolicyPatch, SpawnFilter, WorldView,
};

/// What a bot is doing right now. Serialized as `snake_case` (`"trade_run"`).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    #[default]
    Offline,
    Connecting,
    Idle,
    Traveling,
    Questing,
    Grinding,
    Fighting,
    Dueling,
    Trading,
    TradeRun,
    Housing,
    Resting,
    Dead,
}

impl Activity {
    /// Every activity, in the order the UI lists them.
    pub const ALL: [Activity; 13] = [
        Activity::Offline,
        Activity::Connecting,
        Activity::Idle,
        Activity::Traveling,
        Activity::Questing,
        Activity::Grinding,
        Activity::Fighting,
        Activity::Dueling,
        Activity::Trading,
        Activity::TradeRun,
        Activity::Housing,
        Activity::Resting,
        Activity::Dead,
    ];

    /// The `snake_case` name used in the JSON API.
    pub fn as_str(self) -> &'static str {
        match self {
            Activity::Offline => "offline",
            Activity::Connecting => "connecting",
            Activity::Idle => "idle",
            Activity::Traveling => "traveling",
            Activity::Questing => "questing",
            Activity::Grinding => "grinding",
            Activity::Fighting => "fighting",
            Activity::Dueling => "dueling",
            Activity::Trading => "trading",
            Activity::TradeRun => "trade_run",
            Activity::Housing => "housing",
            Activity::Resting => "resting",
            Activity::Dead => "dead",
        }
    }
}

/// One bot as the map and the bot list show it. Published by the bot runtime about once a second.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct BotSnapshot {
    pub id: u32,
    pub name: String,
    pub race: String,
    pub class: String,
    pub level: u8,
    /// 0..=100.
    pub hp_pct: u8,
    /// World position in metres. `x`/`y` are the map plane, `z` is height.
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub zone: Option<String>,
    /// Mother faction id of the bot's faction (0 = unknown); the alliance comes from it.
    #[serde(default)]
    pub mother_faction: u32,
    /// The side the bot plays (by its race until it is in the world).
    #[serde(default)]
    pub alliance: Alliance,
    /// The continent the bot stands on (`unknown` while offline).
    #[serde(default)]
    pub continent: Continent,
    /// Zone key of the position (0 = none).
    #[serde(default)]
    pub zone_key: u16,
    pub activity: Activity,
    /// Short goal name ("quest", "grind", …).
    pub goal: String,
    /// One line of detail, e.g. `Quest 330 A Friendly Reminder: talk to Maude`.
    pub detail: String,
    /// The other bot of a duel or a trade.
    pub partner: Option<u32>,
    /// Money carried. Shown as-is (with thousands separators); the unit is the hub's choice.
    pub gold: i64,
    pub has_house: bool,
    pub online_secs: u32,
    pub paused: bool,
    /// The guild (expedition) the bot belongs to, with its members; none for a bot in no guild.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guild: Option<Box<GuildInfo>>,
    /// The party of real players the bot is in (a companion), with its members; none for a bot
    /// in no party or in a party of bots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub party: Option<Box<PartyInfo>>,
}

/// The guild of a bot as its web snapshot shows it.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct GuildInfo {
    /// The guild id.
    pub id: u32,
    pub name: String,
    /// The bot's own rank: "General" (the owner), "Commander", "Officer", "Member", "Apprentice".
    pub role: String,
    /// The members the bot knows, the owner first.
    pub members: Vec<GuildMember>,
}

/// A member of a guild.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct GuildMember {
    pub name: String,
    /// Rank, as in [`GuildInfo::role`].
    pub role: String,
    pub level: u8,
    pub online: bool,
    /// One of our bots (else a real player).
    pub bot: bool,
}

/// The party of a bot as its web snapshot shows it.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct PartyInfo {
    /// The server's team id.
    pub id: u32,
    /// Name of the leader (the owner of the team).
    pub leader: String,
    /// What the bot does when nothing special is going on: `follow` or `stay`.
    pub mode: String,
    /// The members the bot knows, the bot itself included.
    pub members: Vec<PartyMember>,
}

/// A member of a party.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct PartyMember {
    pub name: String,
    /// `tank`, `healer`, `attacker` or empty (no role chosen).
    pub role: String,
    pub level: u8,
    pub leader: bool,
    pub online: bool,
    /// One of our bots (else a real player).
    pub bot: bool,
}

/// One line of a bot's event log.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct LogLine {
    /// Unix time in milliseconds.
    pub t: u64,
    pub text: String,
}

/// One quest of a bot.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct QuestLine {
    pub id: u32,
    pub name: String,
    /// Free text, e.g. `active`, `ready`, `done`, `quarantined`.
    pub status: String,
}

/// One crop of a bot (the farming goal).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PlotLine {
    /// Where it stands.
    pub x: f32,
    pub y: f32,
    /// What grows there (the crop's name).
    pub crop: String,
    /// `growing`, `ripe`, `due` (worth a look) or `out of view`, with the time left when known.
    pub state: String,
    /// The seeds were bought: the harvest may be sold.
    pub honest: bool,
    /// Where it is: `own land` or the kind of public farm.
    pub land: String,
}

/// Everything the detail panel shows about one bot.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct BotDetail {
    pub snapshot: BotSnapshot,
    /// Event log; the UI sorts it newest first.
    pub log: Vec<LogLine>,
    pub quests: Vec<QuestLine>,
    /// Extra key/value rows (persona, tick rate, …), shown in order.
    pub extra: Vec<(String, String)>,
    /// The last chat lines the bot heard or said, oldest first (`t` is unix milliseconds).
    #[serde(default)]
    pub chat: Vec<LogLine>,
    /// The route the bot is walking, as map-plane `[x, y]` points in metres, the bot first and at
    /// most 32 points (empty when it is not moving; two points for a straight line). Filled by
    /// the motion controller; the UI draws it for the selected bot.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path: Vec<[f32; 2]>,
    /// The bot's crops (the farming goal), empty when it has none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plots: Vec<PlotLine>,
    /// What the bot wears (the gear feature), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gear: Option<crate::gear::GearView>,
    /// The bot's personality (the personality feature).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<crate::persona::ProfileView>,
    /// What the bot remembers between runs (the journal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journal: Option<crate::journal::JournalView>,
}

/// Token-budget numbers of the optional LLM.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LlmStats {
    pub requests: u64,
    pub failures: u64,
    pub tokens_today: u64,
    /// Tokens left today; `u64::MAX` (or anything above 10^15) means "no budget".
    pub budget_left: u64,
    /// Requests refused by a local limit or the endpoint (they cost nothing).
    #[serde(default)]
    pub rate_limited: u64,
    /// Chat replies the model wrote and the bots sent.
    #[serde(default)]
    pub chat_replies: u64,
    /// Goals the model chose and the bots started.
    #[serde(default)]
    pub director_picks: u64,
    /// Director plans the model made (each one or more steps).
    #[serde(default)]
    pub plans: u64,
    /// Steps in those plans, all together.
    #[serde(default)]
    pub plan_steps: u64,
    /// Director answers that came with a note the bot keeps.
    #[serde(default)]
    pub notes: u64,
}

/// Process-wide numbers for the header of the UI.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct HubStats {
    pub bots_total: u32,
    pub bots_online: u32,
    pub rss_bytes: u64,
    pub bytes_per_bot: u64,
    pub packets_in_per_sec: f32,
    pub packets_out_per_sec: f32,
    /// Bytes received from the game server per second, all bots together.
    #[serde(default)]
    pub bytes_in_per_sec: f32,
    /// Bytes sent to the game server per second, all bots together.
    #[serde(default)]
    pub bytes_out_per_sec: f32,
    pub uptime_secs: u64,
    pub llm: Option<LlmStats>,
    /// Serialized as `[["idle", 12], ["questing", 40], …]`.
    pub activity_counts: Vec<(Activity, u32)>,
    /// Server issues the bots collected that nobody has dealt with (state "open").
    #[serde(default)]
    pub open_issues: u32,
    /// Server issues whose entry was written to the team's issue queue.
    #[serde(default)]
    pub queued_issues: u32,
    /// Navigation (path finding and walking) counters of the whole process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nav: Option<NavStats>,
}

/// Counters of the navigation motion, all bots together, since the process started. Latencies
/// are in microseconds.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct NavStats {
    /// Path finding is on (`behavior.navmesh`).
    pub enabled: bool,
    /// Searches run on the bot's own task (short trips).
    pub queries_inline: u64,
    /// Searches run on the planner threads (long trips).
    pub queries_pooled: u64,
    /// Searches that found a route to the goal.
    pub found: u64,
    /// Searches that hit the expansion cap (the bot walks the route found and asks again).
    pub partial: u64,
    /// Searches that proved the goal unreachable.
    pub unreachable: u64,
    /// Searches whose start or goal is not on the mesh.
    pub no_mesh: u64,
    /// Searches made to continue a partial or very long route.
    pub replans: u64,
    /// Searches made because a bot did not get anywhere.
    pub stuck_replans: u64,
    /// Times the stuck detector fired.
    pub stuck_events: u64,
    /// Trips that fell back to the straight line.
    pub legacy_fallbacks: u64,
    /// Planner answers that did not come within the wait (the bot walked the straight line).
    pub plan_timeouts: u64,
    /// Requests the planner queue refused because it was full.
    pub queue_full: u64,
    /// Planner answers that arrived for a trip that was over.
    pub stale_results: u64,
    /// Median / 99th percentile of the time a search took.
    pub compute_p50_us: u64,
    pub compute_p99_us: u64,
    /// Median / 99th percentile of queue wait plus search for pooled requests.
    pub wait_p50_us: u64,
    pub wait_p99_us: u64,
    /// Requests waiting for a planner thread now, and the most so far.
    pub queue_depth: u32,
    pub queue_max: u32,
    /// Trips of 15 m or more, and how they ended.
    pub trips: u64,
    pub trips_arrived: u64,
    pub trips_failed: u64,
    pub trips_cancelled: u64,
    /// Walked distance over the straight distance, arrived trips only (1.0 = straight).
    pub detour_ratio: f32,
}

/// What a map marker stands for. Serialized as `snake_case`.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[serde(rename_all = "snake_case")]
pub enum MarkerKind {
    House,
    DuelFlag,
    QuestGiver,
    TradeOutlet,
}

/// A static point of interest drawn on the map.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Marker {
    pub kind: MarkerKind,
    pub x: f32,
    pub y: f32,
    pub label: String,
}

/// The world rectangle covered by the map image.
///
/// Pixel `(0, 0)` is `(min_x, max_y)`: `u = (x - min_x) / (max_x - min_x)` and
/// `v = 1 - (y - min_y) / (max_y - min_y)`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct WorldRect {
    pub min_x: f32,
    pub min_y: f32,
    pub max_x: f32,
    pub max_y: f32,
}

/// Calibration of the map image.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct MapInfo {
    /// `Some("/map.png")` when [`BotHub::map_image`] has an image; `None` draws a plain grid. The
    /// web layer appends a `?v=<hash>` cache-busting query to `/map.png` in `/api/map`.
    pub image_url: Option<String>,
    /// Pixel size of the image.
    pub width: u32,
    pub height: u32,
    pub world: WorldRect,
}

/// Something the operator asks a bot (or all bots) to do. JSON: `{"type":"goal","goal":"quest"}`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    /// Pause one bot, or every bot when no bot is given.
    Pause,
    Resume,
    /// Force a goal (see [`Command::GOALS`]); `auto` hands the choice back to the bot.
    Goal {
        goal: String,
    },
    /// Make the bot challenge the bot `other` to a duel.
    DuelWith {
        other: u32,
    },
    /// Make the bot trade with the bot `other`.
    TradeWith {
        other: u32,
    },
    /// Make the bot say a line in local chat.
    Say {
        text: String,
    },
    /// Make the bot whisper a line to the character `to`.
    Whisper {
        to: String,
        text: String,
    },
    /// Global: log in `count` more bots.
    Spawn {
        count: u32,
    },
    /// Global: log out `count` bots.
    Despawn {
        count: u32,
    },
    /// Global: log in `count` more bots that match the filter (their race decides the side and
    /// the home continent). The roster grows with matching races when too few are free.
    SpawnWhere {
        count: u32,
        #[serde(default)]
        filter: SpawnFilter,
    },
    /// Global: log out up to `count` bots that match the filter, idle and resting ones first; a bot
    /// that trades, duels, runs a dungeon or meets a buyer is left alone.
    DespawnWhere {
        count: u32,
        #[serde(default)]
        filter: SpawnFilter,
    },
    /// Start a goal once, with arguments (see [`crate::agent`]): `{"type":"act","goal":"explore",
    /// "zone":"..."}`. Without a bot it goes to every bot.
    Act(crate::agent::ActArgs),
    /// Change the bot's personality (see [`crate::persona`]): `{"type":"profile","chatty":0.9}`.
    Profile(crate::persona::ProfilePatch),
    /// Write a short memory into the bot's journal (see [`crate::agent`]):
    /// `{"type":"note","text":"Bob owes us a duel"}`. The LLM director shows it to the model as
    /// the bot's own notes, quoted as data.
    Note {
        text: String,
    },
}

impl Command {
    /// Values accepted by [`Command::Goal`].
    pub const GOALS: [&'static str; 22] = [
        "quest",
        "grind",
        "duel",
        "trade",
        "trade_run",
        "housing",
        "dungeon",
        "sail",
        "auction",
        "market",
        "guild",
        "gank",
        "warzone",
        "event",
        "companion",
        "farm",
        "explore",
        "gear",
        "work",
        "rest",
        "wander",
        "auto",
    ];
    /// Largest `count` of a spawn or despawn command.
    pub const MAX_COUNT: u32 = 1000;
    /// Longest `Say` / `Whisper` text, in characters.
    pub const MAX_SAY_CHARS: usize = 200;
    /// Longest whisper target, in characters.
    pub const MAX_NAME_CHARS: usize = 32;

    /// Cheap sanity checks the web layer applies before the hub sees a command.
    ///
    /// `bot` is the target from the URL (`None` for `POST /api/command`).
    pub fn validate(&self, bot: Option<u32>) -> Result<(), String> {
        match self {
            Command::Pause | Command::Resume => Ok(()),
            Command::Goal { goal } => {
                if Command::GOALS.contains(&goal.as_str()) {
                    Ok(())
                } else {
                    Err(format!(
                        "unknown goal {goal:?}; expected one of {}",
                        Command::GOALS.join(", ")
                    ))
                }
            }
            Command::DuelWith { other } | Command::TradeWith { other } => match bot {
                None => Err("this command needs a bot: POST /api/bots/<id>/command".into()),
                Some(b) if b == *other => Err("a bot cannot pair with itself".into()),
                Some(_) => Ok(()),
            },
            Command::Say { text } | Command::Whisper { text, .. } => {
                if bot.is_none() {
                    return Err("this command needs a bot: POST /api/bots/<id>/command".into());
                }
                if let Command::Whisper { to, .. } = self {
                    let to = to.trim();
                    if to.is_empty() || to.chars().count() > Command::MAX_NAME_CHARS {
                        return Err(format!(
                            "the whisper target must be a character name of 1..={} characters",
                            Command::MAX_NAME_CHARS
                        ));
                    }
                    if !to
                        .chars()
                        .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
                    {
                        return Err("the whisper target is not a plain character name".into());
                    }
                }
                let text = text.trim();
                if text.is_empty() {
                    Err("text is empty".into())
                } else if text.chars().count() > Command::MAX_SAY_CHARS {
                    Err(format!(
                        "text is longer than {} characters",
                        Command::MAX_SAY_CHARS
                    ))
                } else if text.starts_with('/') {
                    Err("text starting with '/' would run as a chat command; not allowed".into())
                } else if text.chars().any(char::is_control) {
                    Err("text contains control characters".into())
                } else {
                    Ok(())
                }
            }
            Command::Spawn { count } | Command::Despawn { count } => {
                if bot.is_some() {
                    Err("spawn/despawn are global: POST /api/command".into())
                } else if *count == 0 || *count > Command::MAX_COUNT {
                    Err(format!("count must be 1..={}", Command::MAX_COUNT))
                } else {
                    Ok(())
                }
            }
            Command::SpawnWhere { count, filter } | Command::DespawnWhere { count, filter } => {
                if bot.is_some() {
                    Err("spawn/despawn are global: POST /api/command".into())
                } else if *count == 0 || *count > Command::MAX_COUNT {
                    Err(format!("count must be 1..={}", Command::MAX_COUNT))
                } else {
                    filter.validate()
                }
            }
            Command::Act(args) => args.validate(),
            Command::Profile(patch) => {
                if bot.is_none() {
                    return Err("this command needs a bot: POST /api/bots/<id>/command".into());
                }
                patch.validate()
            }
            Command::Note { text } => {
                if bot.is_none() {
                    return Err("this command needs a bot: POST /api/bots/<id>/command".into());
                }
                crate::agent::validate_note(text)
            }
        }
    }
}

/// One row of the issue list: a problem with the server that the bots ran into, deduplicated.
/// All the words are `snake_case` strings (the hub owns the vocabulary).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct IssueSummary {
    /// Stable id of the issue (`pb:` and 8 hex digits); the marker in the team's issue queue.
    pub fingerprint: String,
    /// What went wrong: `quest_credit_missing`, `kicked`, ...
    pub kind: String,
    /// One line.
    pub title: String,
    /// Who is probably at fault: `server`, `data`, `bot`, `unknown`.
    pub suspect: String,
    /// `low`, `medium`, `high`.
    pub severity: String,
    /// Occurrences.
    pub count: u64,
    /// Different bots that met it.
    pub bots: u32,
    /// Unix milliseconds of the first and the latest occurrence.
    pub first_seen: u64,
    pub last_seen: u64,
    /// `open`, `queued`, `ignored`, `resolved`.
    pub state: String,
    /// Whether the hub's policy would tell the team about it (bot-suspect issues never are).
    pub reportable: bool,
}

/// One kept observation of an issue (the first and the latest two are kept).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct IssueSample {
    /// Unix milliseconds.
    pub at: u64,
    /// Name of the reporting bot.
    pub bot: String,
    pub expected: String,
    pub actual: String,
    /// `bot Aria (level 12 Elf Archery) doing quest (...) at (x, y, z) in Solzreed`.
    pub context: String,
    /// `(name, value)` details.
    pub facts: Vec<(String, String)>,
    /// The packets before it, oldest first, one readable line each (and the triggering packet's head).
    pub trail: Vec<String>,
    /// Server log lines around it.
    pub server_log: Vec<String>,
    /// The bot's own log lines.
    pub bot_log: Vec<String>,
}

/// Everything about one issue.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct IssueDetail {
    pub summary: IssueSummary,
    /// The entry as it is (or would be) written to the team's issue queue, CRLF line breaks included.
    pub entry: String,
    /// Server area hint (a path in the server repository; not verified).
    pub area: Option<String>,
    /// The evidence folder on the machine the bots run on, once written.
    pub evidence: Option<String>,
    /// Unix milliseconds at which the entry was queued, in state `queued`.
    pub queued_at: Option<u64>,
    pub samples: Vec<IssueSample>,
}

/// What the operator can do with an issue: `{"action":"queue"}`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IssueAction {
    /// Write a queue entry now, whatever the policy and the rate limits say.
    Queue,
    /// Never queue it by itself.
    Ignore,
    /// It is fixed.
    Resolve,
    /// Back to open.
    Reopen,
}

/// The JSON body of `POST /api/issues/{fingerprint}/action`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct IssueActionRequest {
    pub action: IssueAction,
}

/// What the web UI needs from the bot runtime. Implemented by the `playerbots` binary.
///
/// All methods are called from async handlers, so they must return quickly and never block on
/// network I/O: read published snapshots, enqueue commands.
pub trait BotHub: Send + Sync + 'static {
    /// One snapshot per bot (also offline ones), in any order.
    fn snapshots(&self) -> Vec<BotSnapshot>;
    /// Detail of one bot, `None` if the id is unknown.
    fn detail(&self, id: u32) -> Option<BotDetail>;
    fn stats(&self) -> HubStats;
    fn markers(&self) -> Vec<Marker>;
    fn map_info(&self) -> MapInfo;
    /// PNG bytes of the world map, served at `/map.png`. Called at most every few minutes by the
    /// web layer, but the hub should still cache the encoded bytes.
    fn map_image(&self) -> Option<Vec<u8>>;
    /// Run a command. `bot` is `None` for global commands (spawn/despawn, pause all, …). The error
    /// text is shown to the operator.
    fn command(&self, bot: Option<u32>, cmd: Command) -> Result<(), String>;

    /// The server issues the bots collected, in any order (the UI sorts). Empty by default.
    fn issues(&self) -> Vec<IssueSummary> {
        Vec::new()
    }
    /// One issue by its fingerprint (`pb:abcd1234`), `None` if unknown.
    fn issue(&self, _fingerprint: &str) -> Option<IssueDetail> {
        None
    }
    /// Applies an operator action to an issue. The error text is shown to the operator.
    fn issue_action(&self, _fingerprint: &str, _action: IssueAction) -> Result<(), String> {
        Err("issue reports are not available".into())
    }

    /// Players and bots by continent and side, zones, heat, and what the game server says.
    fn world(&self) -> WorldView {
        WorldView::default()
    }
    /// Scarce items, prices and supply.
    fn economy(&self) -> EconomyView {
        EconomyView::default()
    }
    /// The population director: policy, targets, health and the log of its actions.
    fn director(&self) -> DirectorView {
        DirectorView::default()
    }
    /// Changes the director's policy (only the fields present change). The error text is shown to
    /// the operator.
    fn set_policy(&self, _patch: PolicyPatch) -> Result<(), String> {
        Err("the population director is not available".into())
    }

    /// One bot as the agent API shows it (`GET /api/agent/observe/{id}`), `None` if the id is
    /// unknown. The default takes what the snapshot has.
    fn observe(&self, id: u32) -> Option<crate::agent::Observation> {
        self.detail(id).map(|d| {
            let mut o = crate::agent::Observation::from_snapshot(&d.snapshot);
            o.log = d
                .log
                .iter()
                .rev()
                .take(8)
                .rev()
                .map(|l| l.text.clone())
                .collect();
            o.profile = d.profile;
            if let Some(j) = d.journal {
                o.notes = j.notes;
                o.zones_seen = j.zones_seen;
            }
            o
        })
    }

    /// The zones an `act` may name, by name (`GET /api/agent/zones`). Empty by default.
    fn zones(&self) -> Vec<crate::agent::ZoneEntry> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_names_match_serde() {
        let mut seen = std::collections::HashSet::new();
        for a in Activity::ALL {
            let json = serde_json::to_string(&a).unwrap();
            assert_eq!(json, format!("\"{}\"", a.as_str()));
            assert!(seen.insert(a), "{a:?} listed twice");
        }
        assert_eq!(seen.len(), 13);
    }

    #[test]
    fn command_json_shapes() {
        let cases = [
            (r#"{"type":"pause"}"#, Command::Pause),
            (r#"{"type":"resume"}"#, Command::Resume),
            (
                r#"{"type":"goal","goal":"trade_run"}"#,
                Command::Goal {
                    goal: "trade_run".into(),
                },
            ),
            (
                r#"{"type":"duel_with","other":7}"#,
                Command::DuelWith { other: 7 },
            ),
            (
                r#"{"type":"trade_with","other":9}"#,
                Command::TradeWith { other: 9 },
            ),
            (
                r#"{"type":"say","text":"hello"}"#,
                Command::Say {
                    text: "hello".into(),
                },
            ),
            (
                r#"{"type":"whisper","to":"Bob","text":"hello"}"#,
                Command::Whisper {
                    to: "Bob".into(),
                    text: "hello".into(),
                },
            ),
            (r#"{"type":"spawn","count":5}"#, Command::Spawn { count: 5 }),
            (
                r#"{"type":"despawn","count":2}"#,
                Command::Despawn { count: 2 },
            ),
        ];
        for (json, want) in cases {
            let got: Command = serde_json::from_str(json).unwrap();
            assert_eq!(got, want, "{json}");
            // and back, so the mock UI and the tests agree on the wire format
            let again: Command =
                serde_json::from_str(&serde_json::to_string(&got).unwrap()).unwrap();
            assert_eq!(again, want);
        }
        assert!(serde_json::from_str::<Command>(r#"{"type":"nope"}"#).is_err());
        assert!(serde_json::from_str::<Command>(r#"{"type":"spawn"}"#).is_err());
    }

    #[test]
    fn validation_rules() {
        let g = |s: &str| Command::Goal { goal: s.into() };
        assert!(g("auto").validate(Some(1)).is_ok());
        assert!(g("auto").validate(None).is_ok());
        assert!(g("fly").validate(Some(1)).is_err());

        assert!(Command::DuelWith { other: 2 }.validate(Some(1)).is_ok());
        assert!(Command::DuelWith { other: 1 }.validate(Some(1)).is_err());
        assert!(Command::TradeWith { other: 2 }.validate(None).is_err());

        let say = |s: &str| Command::Say { text: s.into() };
        assert!(say("hi there").validate(Some(1)).is_ok());
        assert!(say("   ").validate(Some(1)).is_err());
        assert!(say("/gold add self 1").validate(Some(1)).is_err());
        assert!(say("a\nb").validate(Some(1)).is_err());
        assert!(say(&"x".repeat(201)).validate(Some(1)).is_err());
        assert!(say("hi").validate(None).is_err());

        let whisper = |to: &str, text: &str| Command::Whisper {
            to: to.into(),
            text: text.into(),
        };
        assert!(whisper("Bob", "hi there").validate(Some(1)).is_ok());
        assert!(whisper("Bob", "hi there").validate(None).is_err());
        assert!(whisper("", "hi").validate(Some(1)).is_err());
        assert!(whisper("Bo b", "hi").validate(Some(1)).is_err());
        assert!(whisper("/Bob", "hi").validate(Some(1)).is_err());
        assert!(whisper(&"B".repeat(33), "hi").validate(Some(1)).is_err());
        assert!(whisper("Bob", "/gold add self 1")
            .validate(Some(1))
            .is_err());
        assert!(whisper("Bob", "  ").validate(Some(1)).is_err());
        assert!(g("wander").validate(Some(1)).is_ok());
        assert!(g("dungeon").validate(Some(1)).is_ok());
        assert!(g("sail").validate(Some(1)).is_ok());
        assert!(g("auction").validate(Some(1)).is_ok());
        assert!(g("market").validate(Some(1)).is_ok());
        for name in ["guild", "gank", "warzone", "event", "companion", "farm"] {
            assert!(g(name).validate(Some(1)).is_ok(), "{name}");
        }

        assert!(Command::Spawn { count: 10 }.validate(None).is_ok());
        assert!(Command::Spawn { count: 0 }.validate(None).is_err());
        assert!(Command::Despawn { count: 1001 }.validate(None).is_err());
        assert!(Command::Spawn { count: 1 }.validate(Some(3)).is_err());
    }

    #[test]
    fn stats_json_uses_pairs_for_counts() {
        let stats = HubStats {
            activity_counts: vec![(Activity::Idle, 3), (Activity::TradeRun, 1)],
            ..HubStats::default()
        };
        let v = serde_json::to_value(&stats).unwrap();
        assert_eq!(v["activity_counts"][0][0], "idle");
        assert_eq!(v["activity_counts"][1][0], "trade_run");
        assert_eq!(v["activity_counts"][1][1], 1);
        assert!(v["llm"].is_null());
    }

    #[test]
    fn stats_carry_the_traffic_rates_and_older_json_without_them_parses() {
        let stats = HubStats {
            bytes_in_per_sec: 12_345.5,
            bytes_out_per_sec: 678.0,
            ..HubStats::default()
        };
        let v = serde_json::to_value(&stats).unwrap();
        assert_eq!(v["bytes_in_per_sec"], 12_345.5);
        assert_eq!(v["bytes_out_per_sec"], 678.0);
        let old: HubStats = serde_json::from_str(
            r#"{"bots_total":1,"bots_online":1,"rss_bytes":2,"bytes_per_bot":3,
                "packets_in_per_sec":4.0,"packets_out_per_sec":5.0,"uptime_secs":6,
                "llm":null,"activity_counts":[]}"#,
        )
        .unwrap();
        assert_eq!((old.bytes_in_per_sec, old.bytes_out_per_sec), (0.0, 0.0));
        assert_eq!(old.packets_in_per_sec, 4.0);
    }

    #[test]
    fn issue_json_shapes() {
        let req: IssueActionRequest = serde_json::from_str(r#"{"action":"queue"}"#).unwrap();
        assert_eq!(req.action, IssueAction::Queue);
        for (word, want) in [
            ("ignore", IssueAction::Ignore),
            ("resolve", IssueAction::Resolve),
            ("reopen", IssueAction::Reopen),
        ] {
            let req: IssueActionRequest =
                serde_json::from_str(&format!(r#"{{"action":"{word}"}}"#)).unwrap();
            assert_eq!(req.action, want);
        }
        assert!(serde_json::from_str::<IssueActionRequest>(r#"{"action":"delete"}"#).is_err());
        assert!(serde_json::from_str::<IssueActionRequest>(r#"{}"#).is_err());

        let s = IssueSummary {
            fingerprint: "pb:6f3b2e1c".into(),
            kind: "kicked".into(),
            count: 3,
            bots: 2,
            reportable: true,
            ..IssueSummary::default()
        };
        let d = IssueDetail {
            summary: s.clone(),
            entry: "[t] x (playerbots)".into(),
            ..IssueDetail::default()
        };
        let back: IssueDetail = serde_json::from_str(&serde_json::to_string(&d).unwrap()).unwrap();
        assert_eq!(back, d);
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["summary"]["fingerprint"], "pb:6f3b2e1c");
        assert!(v["evidence"].is_null() && v["samples"].as_array().unwrap().is_empty());
    }

    #[test]
    fn a_hub_without_issue_support_answers_politely() {
        struct Bare;
        impl BotHub for Bare {
            fn snapshots(&self) -> Vec<BotSnapshot> {
                Vec::new()
            }
            fn detail(&self, _: u32) -> Option<BotDetail> {
                None
            }
            fn stats(&self) -> HubStats {
                HubStats::default()
            }
            fn markers(&self) -> Vec<Marker> {
                Vec::new()
            }
            fn map_info(&self) -> MapInfo {
                MapInfo::default()
            }
            fn map_image(&self) -> Option<Vec<u8>> {
                None
            }
            fn command(&self, _: Option<u32>, _: Command) -> Result<(), String> {
                Ok(())
            }
        }
        assert!(Bare.issues().is_empty());
        assert!(Bare.observe(1).is_none() && Bare.zones().is_empty());
        assert!(Bare.issue("pb:00000000").is_none());
        assert!(Bare
            .issue_action("pb:00000000", IssueAction::Queue)
            .is_err());
    }

    #[test]
    fn older_json_without_the_chat_and_llm_extras_still_parses() {
        let stats: LlmStats =
            serde_json::from_str(r#"{"requests":1,"failures":0,"tokens_today":5,"budget_left":9}"#)
                .unwrap();
        assert_eq!(
            (stats.rate_limited, stats.chat_replies, stats.director_picks),
            (0, 0, 0)
        );
        assert_eq!((stats.plans, stats.plan_steps, stats.notes), (0, 0, 0));
        let detail: BotDetail = serde_json::from_str(
            r#"{"snapshot":{"id":1,"name":"A","race":"","class":"","level":1,"hp_pct":1,"x":0,"y":0,"z":0,"zone":null,"activity":"idle","goal":"","detail":"","partner":null,"gold":0,"has_house":false,"online_secs":0,"paused":false},"log":[],"quests":[],"extra":[]}"#,
        )
        .unwrap();
        assert!(detail.chat.is_empty());
        assert!(detail.path.is_empty());
    }

    #[test]
    fn the_path_of_a_detail_is_left_out_when_empty_and_round_trips_otherwise() {
        let mut d = BotDetail {
            snapshot: BotSnapshot::default(),
            log: Vec::new(),
            quests: Vec::new(),
            extra: Vec::new(),
            chat: Vec::new(),
            path: Vec::new(),
            plots: Vec::new(),
            gear: None,
            profile: None,
            journal: None,
        };
        let v = serde_json::to_value(&d).unwrap();
        assert!(v.get("path").is_none(), "{v}");
        for key in ["gear", "profile", "journal"] {
            assert!(v.get(key).is_none(), "{key} in {v}");
        }
        assert!(v.get("plots").is_none(), "{v}");
        d.path = vec![[1.5, 2.0], [3.0, -4.25]];
        let json = serde_json::to_string(&d).unwrap();
        assert!(json.contains(r#""path":[[1.5,2.0],[3.0,-4.25]]"#), "{json}");
        let back: BotDetail = serde_json::from_str(&json).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn the_plots_of_a_detail_are_left_out_when_empty_and_round_trip_otherwise() {
        let mut d = BotDetail {
            snapshot: BotSnapshot::default(),
            log: Vec::new(),
            quests: Vec::new(),
            extra: Vec::new(),
            chat: Vec::new(),
            path: Vec::new(),
            plots: Vec::new(),
            gear: None,
            profile: None,
            journal: None,
        };
        d.plots.push(PlotLine {
            x: 10.5,
            y: -3.0,
            crop: "Potato".into(),
            state: "ripe".into(),
            honest: true,
            land: "farm".into(),
        });
        let json = serde_json::to_string(&d).unwrap();
        let back: BotDetail = serde_json::from_str(&json).unwrap();
        assert_eq!(back, d);
        // a detail from before the field existed still reads
        let mut v = serde_json::to_value(&d).unwrap();
        v.as_object_mut().unwrap().remove("plots");
        let old: BotDetail = serde_json::from_value(v).unwrap();
        assert!(old.plots.is_empty());
    }
}
