//! Configuration of the `playerbots` binary (`playerbots.toml`).
//!
//! Every table and every key has a default, so an empty file is a valid config. Unknown keys are
//! rejected (a typo in a key would otherwise be silently ignored). [`Config::load`] validates the
//! values; [`Config::example`] is the commented example shipped as `playerbots.example.toml`.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context};
use serde::Deserialize;

/// Environment variable consulted when `server.password` is empty.
pub const PASSWORD_ENV: &str = "PLAYERBOTS_PASSWORD";
/// The password bots created by earlier versions use (the account password used to default to
/// it). Never used for new bots, and `gm_bootstrap` refuses it.
pub const DEFAULT_PASSWORD: &str = "playerbots";
/// Most bots one process accepts (a sanity cap, not a performance claim).
pub const MAX_BOTS: u32 = 5000;

/// The whole `playerbots.toml`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Login server and account naming.
    pub server: ServerCfg,
    /// Where the static game data lives.
    pub data: DataCfg,
    /// Number, pacing and movement of the bots.
    pub bots: BotsCfg,
    /// Goal weights.
    pub behavior: BehaviorCfg,
    /// Instance dungeons: the party size aim and the server's entry limit.
    pub dungeons: DungeonsCfg,
    /// Sailing: which boat the bots sail.
    pub sail: SailCfg,
    /// Mounts and gliders: summon a ride and glide down ledges.
    pub mounts: MountsCfg,
    /// Selling: the auction house, trade-chat advertisements, buyers.
    pub market: MarketCfg,
    /// Guilds (expeditions): bots found, join and run them.
    pub guild: GuildCfg,
    /// Player versus player: defending, bot against bot, attacking players, ganks, warzones.
    pub pvp: PvpCfg,
    /// World events such as rifts.
    pub events: EventsCfg,
    /// Parties with real players.
    pub party: PartyCfg,
    /// The population director of the web UI: starts and stops bots by itself.
    pub population: PopulationCfg,
    /// Farming: planting and harvesting crops.
    pub farm: FarmCfg,
    /// Gear: equipping, socketing and upgrading what suits the level.
    pub gear: GearCfg,
    /// Personalities: chat style, exploring, shady business, gear risk, play time.
    pub personality: PersonalityCfg,
    /// Presence: bots log in and out on schedules of their own, so the world looks played.
    pub presence: PresenceCfg,
    /// Exploring: bots go and see places they have not been.
    pub explore: ExploreCfg,
    /// Operator web UI (`bind`, `token`).
    pub web: pb_web::WebConfig,
    /// Optional LLM add-on.
    pub llm: pb_llm::LlmConfig,
    /// Chat: replies to players and bot-to-bot chatter.
    pub social: SocialCfg,
    /// Server issue reports: what the bots tell the team about the server.
    pub issues: IssuesCfg,
}

/// `[server]`: how to reach the login server and name the accounts.
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerCfg {
    /// Login server address (`host:port`), the public client port.
    pub login: String,
    /// Game server (world) id to enter; `0` takes the first active world the login server lists.
    pub world_id: u8,
    /// Accounts are `<prefix><bot id>` (`pb1`, `pb2`, ...). The login server creates unknown
    /// accounts on first login (`AutoAccount`).
    pub account_prefix: String,
    /// Password of every bot account. Empty: the environment variable `PLAYERBOTS_PASSWORD`, then
    /// the generated password in the secrets file next to the roster (see [`crate::secrets`]).
    /// Never logged.
    pub password: String,
}

impl Default for ServerCfg {
    fn default() -> Self {
        Self {
            login: "127.0.0.1:1237".into(),
            world_id: 0,
            account_prefix: "pb".into(),
            password: String::new(),
        }
    }
}

impl fmt::Debug for ServerCfg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerCfg")
            .field("login", &self.login)
            .field("world_id", &self.world_id)
            .field("account_prefix", &self.account_prefix)
            .field("password", &"<hidden>")
            .finish()
    }
}

/// `[data]`: static data locations (all read-only).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DataCfg {
    /// The server's `compact.sqlite3`.
    pub compact: PathBuf,
    /// The server's `app/Data` directory (`Worlds/<world>/*.json`, `CharTemplates.json`).
    pub server_data: PathBuf,
    /// The directory written by `playerbots bake`; without it bots run on flat terrain and the
    /// map has no image.
    pub baked: Option<PathBuf>,
    /// World name, `main_world` for the 1.2 world.
    pub world: String,
}

impl Default for DataCfg {
    fn default() -> Self {
        Self {
            compact: "compact.sqlite3".into(),
            server_data: "Data".into(),
            baked: None,
            world: "main_world".into(),
        }
    }
}

/// `[bots]`: how many bots there are and how they move.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BotsCfg {
    /// Bots started at launch (more can be started from the web UI).
    pub count: u32,
    /// Delay between two bot logins (spreads the load on the servers).
    pub ramp_ms: u64,
    /// The persistent roster (accounts, character names, archetypes, personas).
    pub roster: PathBuf,
    /// The bots' journal: what each bot remembers between runs (places seen, wild plots, play
    /// sessions, notes). Empty: `journal.json` next to the roster.
    pub journal: PathBuf,
    /// Races new characters may have: `nuian`, `elf`, `hariharan`, `ferre` (what the 1.2 data
    /// lets a player create), or a race id.
    pub races: Vec<String>,
    /// Bots may use GM chat commands (`/item add`, `/gold add`, `/move`, ...). Needs access level
    /// 100 on the bot accounts, see `playerbots grant-gm-sql`.
    pub gm_bootstrap: bool,
    /// Brain tick while a bot moves or fights.
    pub tick_ms: u64,
    /// Brain tick while a bot stands still.
    pub idle_tick_ms: u64,
    /// Milliseconds between two movement updates of a walking bot that a real player can see
    /// (the 1.2 client sends about seven a second, the server's NPCs ten). Between brain ticks
    /// the bot only moves on and sends the update.
    pub move_update_ms: u64,
    /// Run speed in m/s (the client's base run speed is about 5.4).
    pub move_speed: f32,
    /// Walk speed in m/s.
    pub walk_speed: f32,
    /// With `gm_bootstrap`, trips longer than this many metres use `/move` instead of walking;
    /// 0 never teleports.
    pub long_travel_teleport_m: f32,
    /// First wait after a failed connection.
    pub reconnect_min_secs: u64,
    /// Longest wait between two connection attempts (the wait doubles up to this).
    pub reconnect_max_secs: u64,
}

impl Default for BotsCfg {
    fn default() -> Self {
        Self {
            count: 10,
            ramp_ms: 750,
            roster: "roster.json".into(),
            journal: PathBuf::new(),
            races: ["nuian", "elf", "hariharan", "ferre"]
                .map(String::from)
                .to_vec(),
            gm_bootstrap: false,
            tick_ms: 250,
            idle_tick_ms: 1500,
            move_update_ms: 125,
            move_speed: 5.4,
            walk_speed: 1.8,
            long_travel_teleport_m: 0.0,
            reconnect_min_secs: 5,
            reconnect_max_secs: 120,
        }
    }
}

impl BotsCfg {
    /// Where the journal is kept: `journal`, else `journal.json` next to the roster.
    pub fn journal_path(&self) -> PathBuf {
        if self.journal.as_os_str().is_empty() {
            self.roster.with_file_name("journal.json")
        } else {
            self.journal.clone()
        }
    }

    /// The configured races as ids (`races` was checked by [`Config::validate`]).
    pub fn race_ids(&self) -> Vec<u8> {
        self.races.iter().filter_map(|r| race_id(r)).collect()
    }
}

/// `[behavior]`: relative weights of the goals. A weight of 0 switches a goal off. Each bot also
/// has a persona (temperament) that scales the weights; `persona_spread` says how strongly.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BehaviorCfg {
    /// Do quests.
    pub quest: f32,
    /// Kill monsters.
    pub grind: f32,
    /// Duel another bot.
    pub duel: f32,
    /// Trade with another bot.
    pub trade: f32,
    /// Craft, carry and sell trade packs.
    pub trade_run: f32,
    /// Build a house.
    pub housing: f32,
    /// Stand and recover.
    pub rest: f32,
    /// Walk around.
    pub wander: f32,
    /// 0 gives every bot the same temperament, 1 the full spread of the roster.
    pub persona_spread: f32,
    /// Trade with real players too: the bots answer a trade request of a character that is not one
    /// of ours. Off by default: a bot then only trades with other bots, and a real player can
    /// neither farm the bots' (GM-minted) goods and gold nor hold them in a trade window.
    pub trade_with_players: bool,
    /// Bots may travel by portal: the teleport book, recall points and gates (the portals
    /// feature). Off: no bot uses a portal.
    pub portals: bool,
    /// Bots may form parties and enter instance dungeons (the dungeons feature, goal `dungeon`).
    /// Off: no bot picks the goal or joins a party for it.
    pub dungeons: bool,
    /// A bot below the level range of a dungeon may raise its level to the range's minimum with
    /// the GM `/level` command (only with `bots.gm_bootstrap`; the raise is logged). Off: only
    /// bots whose level fits a dungeon run it.
    pub dungeon_level_up: bool,
    /// Bots may summon boats and sail (the sailing feature, goal `sail`). Off: no bot picks the
    /// goal or boards a boat.
    pub sailing: bool,
    /// Bots walk routes on the navigation mesh (`nav_<world>.bin` in the baked data) instead of
    /// straight lines. Off: every trip is the straight line, as before the mesh existed.
    pub navmesh: bool,
    /// Bots move like people: jumps, emotes and idle mannerisms (the life-like movement feature).
    /// Off: they only walk and stand still.
    pub lifelike: bool,
    /// How often bots jump while walking, as a multiplier of the default rate; 0 = never.
    pub jump_rate: f32,
    /// Bots use emotes (a wave, a bow, a sit) in life-like moments. Off: no emotes.
    pub emotes: bool,
}

impl Default for BehaviorCfg {
    fn default() -> Self {
        Self {
            quest: 1.0,
            grind: 1.0,
            duel: 0.3,
            trade: 0.2,
            trade_run: 0.3,
            housing: 0.2,
            rest: 0.4,
            wander: 0.5,
            persona_spread: 0.6,
            trade_with_players: false,
            portals: true,
            dungeons: true,
            dungeon_level_up: false,
            sailing: true,
            navmesh: true,
            lifelike: true,
            jump_rate: 1.0,
            emotes: true,
        }
    }
}

/// `[dungeons]`: how dungeon parties are formed and whether the server's entry limit is counted.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DungeonsCfg {
    /// Count the server's entry limit (`enter_count` entries per zone group and four hours) and
    /// skip a dungeon whose entries are used up. Off by default: the 1.2 server has no dungeon
    /// entry limit (`Dungeons.EntryLimit`). A refusal for the limit is answered either way.
    pub entry_limit: bool,
    /// Seconds a wish waits for a full party (the dungeon's `max_players`, at most five) before
    /// it goes with a smaller one. At most
    /// [`MAX_RELAX_SECS`](crate::bot::coordinator::MAX_RELAX_SECS); the bot waits up to
    /// [`GATHER_MS`](crate::bot::goals::dungeon::machine::GATHER_MS) for an order.
    pub relax_after_secs: u64,
}

impl Default for DungeonsCfg {
    fn default() -> Self {
        Self {
            entry_limit: false,
            relax_after_secs: crate::bot::party::RELAX_AFTER_SECS,
        }
    }
}

/// Which boat a bot sails (`[sail] ship`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShipChoice {
    /// The rowboat (3.2 m/s, one passenger).
    #[default]
    Rowboat,
    /// The clipper (6.5 m/s, driver only).
    Clipper,
    /// The Eznan cutter (6.3 m/s, 8 cannons, 4 pack racks).
    Cutter,
    /// The Lutesong junk (the cutter's hull).
    Junk,
    /// The Growling yawl (9.8 m/s).
    Yawl,
    /// The Merchant Schooner (6.5 m/s, 2 cannons, 20 pack racks).
    Schooner,
    /// A bot sails the boat whose scroll it holds, else each bot has a boat of its own: mostly
    /// rowboats and clippers, some cutters, junks and schooners (merchants like the schooner).
    Auto,
}

impl ShipChoice {
    /// The hull family of a fixed choice; `None` for `auto`.
    pub fn family(self) -> Option<aa_world::travel::ShipFamily> {
        use aa_world::travel::ShipFamily as F;
        match self {
            ShipChoice::Rowboat => Some(F::Rowboat),
            ShipChoice::Clipper => Some(F::Clipper),
            ShipChoice::Cutter => Some(F::Cutter),
            ShipChoice::Junk => Some(F::Junk),
            ShipChoice::Yawl => Some(F::Yawl),
            ShipChoice::Schooner => Some(F::Schooner),
            ShipChoice::Auto => None,
        }
    }
}

/// `[sail]`: the boats of the sailing goal (switched on and off by `[behavior] sailing`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SailCfg {
    /// The boat the bots sail: `rowboat` (default), `clipper`, `cutter`, `junk`, `yawl`,
    /// `schooner` or `auto`. With `bots.gm_bootstrap` a bot gets the scroll with the GM
    /// `/item add` command; without, it needs the scroll in its bag.
    pub ship: ShipChoice,
}

/// `[mounts]`: the owned mount and glider a bot uses on long trips (switched on and off by
/// `[mounts] enabled`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MountsCfg {
    /// Master switch: summon mounts, equip and fly gliders.
    pub enabled: bool,
    /// Bots may summon and ride a mount (the rideable mate of a summon item they own).
    pub mounts: bool,
    /// Bots equip a glider and glide down a ledge they are walking over.
    pub gliders: bool,
    /// The shortest trip a bot summons its mount for, in metres.
    pub summon_over_m: f32,
    /// A mounted bot dismounts when the trip left is this short (metres).
    pub dismount_under_m: f32,
    /// Mounted run speed in m/s (the server does not validate character movement; the client
    /// shows the mount and other clients interpolate at this speed).
    pub speed: f32,
    /// The summon item template a bot grants itself with `bots.gm_bootstrap` when its bag holds
    /// none; `0` takes the lowest-id rideable mount of its level from the game data (4177, the
    /// Lilyut horse, in the 1.2 data).
    pub item: u32,
    /// The glider item template a bot grants itself with `bots.gm_bootstrap` when its bag holds
    /// none; `0` takes the lowest-id glider of its level from the game data (16385, Tooloop's old
    /// glider, in the 1.2 data).
    pub glider_item: u32,
    /// A mounted mate is dismissed (despawned) after standing unused this long, in seconds;
    /// `0` keeps it.
    pub idle_despawn_secs: u32,
    /// A drop at least this deep (metres) under the way ahead may be glided down.
    pub glide_drop_m: f32,
    /// How far ahead the terrain is sampled for a ledge, in metres.
    pub glide_lookahead_m: f32,
    /// Glide horizontal speed in m/s.
    pub glide_speed: f32,
    /// Glide descent in m/s (the 1.2 client's glide falls slowly; this is the bot's own
    /// approximation, not a server value).
    pub glide_descent: f32,
}

impl Default for MountsCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            mounts: true,
            gliders: true,
            summon_over_m: 250.0,
            dismount_under_m: 40.0,
            speed: 10.0,
            item: 0,
            glider_item: 0,
            idle_despawn_secs: 120,
            glide_drop_m: 8.0,
            glide_lookahead_m: 30.0,
            glide_speed: 9.0,
            glide_descent: 3.0,
        }
    }
}

/// Where a bot meets the buyer of something it advertised.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MeetAt {
    /// The bot picks: Mirage Isle when it can get there, else the town it is in.
    #[default]
    Auto,
    /// Mirage Isle (the market island).
    Mirage,
    /// The town the bot is in.
    Town,
}

/// `[market]`: selling what the bots gather. Auction listings, trade-chat advertisements and
/// sales to real players, all of them only with goods the bots came by honestly (never GM-made
/// ones, see `bot::market::ledger`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MarketCfg {
    /// Master switch of the market features (goals `auction` and `market`). Off: bots neither list
    /// nor advertise nor sell.
    pub enabled: bool,
    /// Bots list surplus goods at the auction house (goal `auction`).
    pub auction: bool,
    /// Bots advertise surplus goods in trade chat (goal `market`).
    pub chat_ads: bool,
    /// Bots sell advertised goods to real players (characters that are not ours) in a trade. Off
    /// by default; ignored while `bots.gm_bootstrap` is on, see [`Config::may_sell_to_players`].
    pub sell_to_players: bool,
    /// Where a bot meets a buyer, see [`MeetAt`].
    pub meet_at: MeetAt,
    /// All bots together post at most this many trade-chat advertisements per hour (0 = none).
    pub max_ads_per_hour: u32,
    /// One bot posts an advertisement at most this often, in seconds (at least 60).
    pub ad_interval_secs: u64,
}

impl Default for MarketCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            auction: true,
            chat_ads: true,
            sell_to_players: false,
            meet_at: MeetAt::Auto,
            max_ads_per_hour: 12,
            ad_interval_secs: 900,
        }
    }
}

/// `[guild]`: bots in guilds (expeditions). Everything is off until `enabled` is set.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GuildCfg {
    /// Master switch of the guild features (goal `guild`). Off: no bot founds, joins or runs a
    /// guild.
    pub enabled: bool,
    /// Bots may found guilds. Off: they only join existing ones.
    pub found: bool,
    /// A bot founds or joins a guild from this level on.
    pub min_level: u8,
    /// Bots that found a guild together (the party that signs the charter).
    pub party_size: u8,
    /// This process founds at most this many guilds (0 = none).
    pub max_guilds: u32,
    /// Bots accept the guild invitations of real players.
    pub accept_player_invites: bool,
    /// How many times an hour a bot that has been in a bot guild for a quarter of an hour leaves
    /// it, on average (0 = never). An owner hands the guild to another bot first; the bot can be
    /// invited again after a while.
    pub churn_per_hour: f32,
}

impl Default for GuildCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            found: true,
            min_level: 5,
            party_size: 5,
            max_guilds: 3,
            accept_player_invites: false,
            churn_per_hour: 0.2,
        }
    }
}

/// `[pvp]`: fights between players. Everything is off until `enabled` is set, and attacking real
/// players is a switch of its own (see [`Config::may_attack_players`]).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PvpCfg {
    /// Master switch of the pvp features (goals `gank` and `warzone`, the defence below).
    pub enabled: bool,
    /// A bot fights back when a player attacks it.
    pub defend: bool,
    /// Bots of hostile factions attack each other.
    pub attack_bots: bool,
    /// Bots attack real players (characters that are not ours); ignored while `bots.gm_bootstrap`
    /// is on, see [`Config::may_attack_players`].
    pub attack_players: bool,
    /// Bots ambush trade-pack carriers (goal `gank`).
    pub gank: bool,
    /// Bots fight in zones at war (goal `warzone`).
    pub warzone: bool,
    /// Character names that are never attacked (game masters, friends).
    pub never_names: Vec<String>,
    /// When not empty: the only real players that may be attacked.
    pub only_names: Vec<String>,
    /// Most bots that attack together.
    pub max_squad: u8,
    /// A bot below this many percent of its hit points breaks off a fight.
    pub retreat_hp_pct: u8,
    /// Most characters one bot kills in one war of a zone group; it leaves the war after that.
    pub war_kill_cap: u8,
}

impl Default for PvpCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            defend: true,
            attack_bots: false,
            attack_players: false,
            gank: false,
            warzone: false,
            never_names: Vec::new(),
            only_names: Vec::new(),
            max_squad: 3,
            retreat_hp_pct: 35,
            war_kill_cap: 10,
        }
    }
}

/// `[events]`: bots at world events such as rifts. Everything is off until `enabled` is set.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EventsCfg {
    /// Master switch of the event features (goal `event`).
    pub enabled: bool,
    /// At most this many bots join one event.
    pub max_bots_per_event: u32,
    /// At most this share (0..=1) of all bots joins events at once.
    pub max_share: f32,
    /// A bot waits a random number of seconds in this range (low, high) before it joins.
    #[serde(deserialize_with = "pair_of_secs")]
    pub join_delay_secs: [u64; 2],
    /// Bots stay out of zones at war.
    pub avoid_war_zones: bool,
}

impl Default for EventsCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            max_bots_per_event: 6,
            max_share: 0.25,
            join_delay_secs: [10, 120],
            avoid_war_zones: true,
        }
    }
}

/// A `[low, high]` pair of seconds. Exactly two numbers: serde would otherwise ignore a third.
fn pair_of_secs<'de, D: serde::Deserializer<'de>>(d: D) -> Result<[u64; 2], D::Error> {
    let v = Vec::<u64>::deserialize(d)?;
    <[u64; 2]>::try_from(v)
        .map_err(|v| serde::de::Error::invalid_length(v.len(), &"a [low, high] pair"))
}

/// `[party]`: bots and the parties of real players. Everything is off by default.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PartyCfg {
    /// Bots join the parties of real players when invited (goal `companion`).
    pub join_player_parties: bool,
    /// Bots obey party chat commands of the player who leads the party.
    pub chat_commands: bool,
    /// A bot in a player's party follows it into instance dungeons.
    pub follow_into_dungeons: bool,
    /// A bot declines the invitation of a player whose level (when it is known) differs from its
    /// own by more than this.
    pub max_level_gap: u8,
    /// This process has at most this many bots in the parties of real players at once; more
    /// invitations are declined.
    pub max_companions: u32,
    /// At most this many of our bots join one player's party (a party holds five).
    pub max_per_player: u8,
}

impl Default for PartyCfg {
    fn default() -> Self {
        Self {
            join_player_parties: false,
            chat_commands: false,
            follow_into_dungeons: false,
            max_level_gap: 10,
            max_companions: 8,
            max_per_player: 4,
        }
    }
}

/// `[population]`: the population director of the web UI, which starts and stops bots to follow
/// the players. Off by default; with `dry_run` it only says what it would do. The policy keys
/// (`max_online` ... `backoff_open_issues`) are the start values of the policy the operator edits in
/// the web UI; `max_spawn_per_min` and `min_action_gap_secs` are hard limits the UI cannot lift.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PopulationCfg {
    /// Master switch of the director.
    pub enabled: bool,
    /// The director only logs what it would do.
    pub dry_run: bool,
    /// It starts at most this many bots per minute (0 = none).
    pub max_spawn_per_min: u32,
    /// Seconds between two of its actions (at least 1).
    pub min_action_gap_secs: u64,
    /// Bots online, all together, that it will not exceed when it starts more.
    pub max_online: u32,
    /// Each side keeps at least this many bots.
    pub min_per_side: u32,
    /// A side aims for this many characters (players plus bots) per 100 players on the other side.
    pub mirror_pct: u32,
    /// No side gets more bots than this from the director.
    pub per_side_cap: u32,
    /// The most bots one action starts or stops.
    pub step: u32,
    /// Bots above the target a side keeps before the director stops some.
    pub hysteresis: u32,
    /// The director may stop the bots it started.
    pub despawn: bool,
    /// Stops starting bots above this much process memory in megabytes (0 = ignore).
    pub backoff_rss_mb: u32,
    /// ... above this many packets per second from the server (0 = ignore).
    pub backoff_packets_in: u32,
    /// ... after this many failed or short sessions within five minutes (0 = ignore).
    pub backoff_login_failures: u32,
    /// ... with this many open server issues (0 = ignore).
    pub backoff_open_issues: u32,
    /// Where the policy's numbers, wishes and goal weights are kept between runs; empty = in memory
    /// only. `enabled` and `dry_run` are not restored from it: they start as configured.
    pub policy_file: Option<PathBuf>,
    /// The game server's web API (`http://host:port`); empty = not used. Adds the exact number of
    /// players online, the server's status and the auction house's lots to the web UI.
    pub server_api: String,
    /// The name of the environment variable that holds the API's bearer key.
    pub server_api_key_env: String,
    /// A file whose first line is the API's bearer key (used when the variable is not set).
    pub server_api_key_file: Option<PathBuf>,
    /// Seconds between two polls of the server API (at least 5).
    pub server_poll_secs: u64,
}

impl Default for PopulationCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            dry_run: true,
            max_spawn_per_min: 10,
            min_action_gap_secs: 60,
            max_online: 50,
            min_per_side: 0,
            mirror_pct: 100,
            per_side_cap: 20,
            step: 3,
            hysteresis: 2,
            despawn: true,
            backoff_rss_mb: 3072,
            backoff_packets_in: 30_000,
            backoff_login_failures: 8,
            backoff_open_issues: 50,
            policy_file: None,
            server_api: String::new(),
            server_api_key_env: String::new(),
            server_api_key_file: None,
            server_poll_secs: 30,
        }
    }
}

impl PopulationCfg {
    /// The policy the web UI starts with.
    pub fn policy(&self) -> pb_web::Policy {
        pb_web::Policy {
            active: self.enabled,
            dry_run: self.dry_run,
            max_online: self.max_online,
            min_per_side: self.min_per_side,
            mirror_pct: self.mirror_pct,
            per_side_cap: self.per_side_cap,
            step: self.step,
            hysteresis: self.hysteresis,
            despawn: self.despawn,
            backoff_rss_mb: self.backoff_rss_mb,
            backoff_packets_in: self.backoff_packets_in,
            backoff_login_failures: self.backoff_login_failures,
            backoff_open_issues: self.backoff_open_issues,
            ..pb_web::Policy::default()
        }
    }
}

/// Whether `url` is an `http://` or `https://` address with a host and no credentials in it.
fn plain_http_url(url: &str) -> bool {
    let Some(rest) = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
    else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    !authority.is_empty()
        && !authority.contains('@')
        && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// `[farm]`: bots that plant and harvest crops (goal `farm`). Off by default.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FarmCfg {
    /// Master switch of the farming features.
    pub enabled: bool,
    /// One bot tends at most this many plots.
    pub max_plots: u32,
    /// The game server's `World.GrowthRate` (crops grow this many times faster). It is not on the
    /// wire; set it when the server does.
    pub growth_rate: f32,
    /// Bots plant on the public farms (the farm sub zones, needs `farm_areas.json` in the baked
    /// data). Off: only on the land of their own houses.
    pub public_farms: bool,
    /// Bots plant on the land of their own houses.
    pub own_land: bool,
    /// A bot plants the seeds a harvest gives back on the spot it harvested.
    pub replant: bool,
    /// The farthest a bot walks to a farm, a vendor or a plot (metres); trips longer than
    /// `bots.long_travel_teleport_m` use the GM `/move` when `bots.gm_bootstrap` is on.
    pub max_trip_m: f32,
    /// Bots with a shady streak (`Profile::outlaw`) also plant on unprotected land out in the
    /// wild (an "illegal" farm: no labor-free protection, anybody may take the crop).
    pub wild: bool,
    /// One bot keeps at most this many wild plots (part of `max_plots`).
    pub wild_plots: u32,
    /// The `outlaw` trait (0..=1) from which a bot plants wild farms of its own accord (a farm goal
    /// started with the `wild` argument always does).
    pub wild_outlaw: f32,
    /// Outlaw bots harvest ripe wild crops that are not theirs (crops of the other bots of this
    /// process only, unless `steal_from_players`). The server lets anybody harvest a crop out in
    /// the wild; within a day of its planting the thief leaves footprints (crime evidence).
    pub steal: bool,
    /// With `steal`: the crops of real players are fair game too. Never while
    /// `bots.gm_bootstrap` is on.
    pub steal_from_players: bool,
}

impl Default for FarmCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            max_plots: 5,
            growth_rate: 1.0,
            public_farms: true,
            own_land: true,
            replant: true,
            max_trip_m: 4_000.0,
            wild: false,
            wild_plots: 3,
            wild_outlaw: 0.6,
            steal: false,
            steal_from_players: false,
        }
    }
}

/// `[gear]`: bots put on the best gear they have for their level, socket gems and tier up as they
/// progress (goal `gear`, see [`crate::bot::gear`]).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GearCfg {
    /// Master switch of the gear features: the equip duty, the goal, the quest reward pick.
    pub enabled: bool,
    /// The goal buys better pieces from merchants.
    pub buy: bool,
    /// The goal mints the pieces and gems of the bot's tier with GM commands (needs
    /// `bots.gm_bootstrap`).
    pub mint: bool,
    /// The goal sockets lunagems into free sockets.
    pub socket: bool,
    /// The goal regrades worn pieces with scrolls (needs `bots.gm_bootstrap`; a regrade can fail).
    pub regrade: bool,
    /// The most a purchase may cost, in percent of the bot's money (1..=100).
    pub max_spend_pct: u8,
    /// Seconds between two looks of the equip duty at the bag (1..=3600).
    pub check_secs: u32,
    /// The farthest a bot goes to a merchant, in metres.
    pub max_trip_m: f32,
}

impl Default for GearCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            buy: true,
            mint: true,
            socket: true,
            regrade: false,
            max_spend_pct: 30,
            check_secs: 20,
            max_trip_m: 3_000.0,
        }
    }
}

/// `[personality]`: every bot has a [`Profile`](crate::roster::Profile) (how chatty, crude and
/// funny it is, whether it explores, does shady things, cares about gear, and when and how much it
/// plays). Off: every bot behaves like the neutral profile.
///
/// The chat keys: how coarse any bot may be (`profanity`, a hard cap on every line, canned or
/// model-written), whether the LLM writes some of the unasked lines, and the process-wide rates of
/// the ambient lines and the channel lines (the bots' own chattiness only decides who of them
/// speaks within these).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PersonalityCfg {
    /// Master switch of the personalities. Off: every bot plays the neutral profile, and its chat
    /// stays clean.
    pub enabled: bool,
    /// The coarsest language any bot may use in chat: `off`, `mild` (damn, hell, crap, wtf, ffs)
    /// or `strong` (harsher exclamations too). Slurs, hate, sexual words, insults and threats are
    /// refused at every level.
    pub profanity: pb_llm::persona::Profanity,
    /// With `[llm] enabled`: the model may write some of the ambient lines too (at most two a
    /// minute for all bots together; the canned line is the fallback).
    pub llm_chatter: bool,
    /// All bots together say at most this many ambient lines per minute (remarks and jokes in
    /// local chat while a real player is near; 0 = none).
    pub ambient_per_min: u32,
    /// All bots together say at most this many lines per hour on the shout, trade and
    /// group-finding channels (0 = none).
    pub channel_lines_per_hour: u32,
}

impl Default for PersonalityCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            profanity: pb_llm::persona::Profanity::Mild,
            llm_chatter: true,
            ambient_per_min: 2,
            channel_lines_per_hour: 4,
        }
    }
}

impl PersonalityCfg {
    /// Checks the chat rates (a typo must not open the floodgates).
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.ambient_per_min <= 60,
            "personality.ambient_per_min {} is above the limit of 60",
            self.ambient_per_min
        );
        ensure!(
            self.channel_lines_per_hour <= 120,
            "personality.channel_lines_per_hour {} is above the limit of 120",
            self.channel_lines_per_hour
        );
        Ok(())
    }
}

/// `[presence]`: bots log in and out on schedules of their own (when in the day they play, how
/// long a session lasts, breaks), so the population rises and falls like a real server's. Off by
/// default: every bot stays online. See [`crate::presence`].
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PresenceCfg {
    /// Master switch of the schedules. On: the supervisor starts nobody at launch and the
    /// schedules log the bots in and out (`bots.count` only sizes the roster).
    pub enabled: bool,
    /// Bots online at least, all together (the schedules bring sessions forward to keep it).
    pub min_online: u32,
    /// Bots online at most, all together (0 = no limit but the roster).
    pub max_online: u32,
    /// The schedules log in at most this many bots per minute (through the login ramp).
    pub max_starts_per_min: u32,
    /// ... and log out at most this many per minute.
    pub max_stops_per_min: u32,
    /// Chance (0..=1) that a bot says a short goodbye before it logs out (only with
    /// `[social] enabled`).
    pub goodbye_chance: f32,
    /// The schedules' clock is UTC plus this many hours; unset = the machine's local time.
    pub utc_offset_hours: Option<f32>,
}

impl Default for PresenceCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            min_online: 0,
            max_online: 0,
            max_starts_per_min: 20,
            max_stops_per_min: 3,
            goodbye_chance: 0.1,
            utc_offset_hours: None,
        }
    }
}

impl PresenceCfg {
    /// Checks the values.
    pub fn validate(&self) -> anyhow::Result<()> {
        for (key, v) in [
            ("min_online", self.min_online),
            ("max_online", self.max_online),
        ] {
            ensure!(
                v <= MAX_BOTS,
                "presence.{key} {v} is above the limit of {MAX_BOTS}"
            );
        }
        ensure!(
            self.max_online == 0 || self.min_online <= self.max_online,
            "presence.min_online must not be above presence.max_online"
        );
        for (key, v) in [
            ("max_starts_per_min", self.max_starts_per_min),
            ("max_stops_per_min", self.max_stops_per_min),
        ] {
            ensure!(
                (1..=1000).contains(&v),
                "presence.{key} must be in 1..=1000"
            );
        }
        ensure!(
            self.goodbye_chance.is_finite() && (0.0..=1.0).contains(&self.goodbye_chance),
            "presence.goodbye_chance must be in 0..=1"
        );
        if let Some(h) = self.utc_offset_hours {
            ensure!(
                h.is_finite() && (-14.0..=14.0).contains(&h),
                "presence.utc_offset_hours must be in -14..=14"
            );
        }
        Ok(())
    }
}

/// `[explore]`: bots go and see places they have not been (goal `explore`); what they saw is
/// kept in the journal.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExploreCfg {
    /// Master switch of exploring.
    pub enabled: bool,
    /// The farthest an exploring trip goes (metres, straight line; portals and the GM `/move`
    /// work as for any trip).
    pub max_trip_m: f32,
    /// A destination with monsters this many levels above the bot is left out (also along the
    /// way, the quest goal's danger checks apply too).
    pub level_gap: u8,
    /// Minutes a bot of average curiosity waits between two trips (an explorer at heart half as
    /// long, a homebody twice as long).
    pub trip_gap_mins: u32,
}

impl Default for ExploreCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            max_trip_m: 6_000.0,
            level_gap: 5,
            trip_gap_mins: 45,
        }
    }
}

impl ExploreCfg {
    /// Checks the values.
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.max_trip_m.is_finite() && (100.0..=100_000.0).contains(&self.max_trip_m),
            "explore.max_trip_m {} must be between 100 and 100000",
            self.max_trip_m
        );
        ensure!(
            (1..=60).contains(&self.level_gap),
            "explore.level_gap must be in 1..=60"
        );
        ensure!(
            self.trip_gap_mins <= 1440,
            "explore.trip_gap_mins must be at most 1440"
        );
        Ok(())
    }
}

/// `[social]`: how the bots talk. Replies come from the LLM when `[llm]` is on and from a table of
/// canned lines otherwise.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SocialCfg {
    /// Answer whispers and local chat of players. Off: the bots never say anything on their own.
    pub enabled: bool,
    /// A local line from a real player within this many metres may get an answer (a line that
    /// names the bot, and every whisper, always may).
    pub reply_radius_m: f32,
    /// Chance (0..=1) that a bot answers a local line that does not name it.
    pub reply_chance: f32,
    /// All bots together answer at most this many lines per minute (0 = none).
    pub replies_per_min: u32,
    /// Bots greet each other: "gg" after a duel, thanks after a trade, a hello to a bot nearby.
    pub bot_chatter: bool,
    /// Chatter only while a real player is within 150 m, so nobody talks to an empty street.
    pub chatter_needs_player: bool,
    /// All bots together say at most this many chatter lines per minute (0 = none).
    pub chatter_per_min: u32,
    /// One bot says a chatter line at most this often, in seconds.
    pub chatter_gap_secs: u64,
    /// Also answer the whispers and local lines of our own bots. For testing only: two bots can
    /// answer each other (the limits end it quickly).
    pub reply_to_bots: bool,
}

impl Default for SocialCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            reply_radius_m: 30.0,
            reply_chance: 0.4,
            replies_per_min: 20,
            bot_chatter: true,
            chatter_needs_player: true,
            chatter_per_min: 4,
            chatter_gap_secs: 60,
            reply_to_bots: false,
        }
    }
}

/// What happens with an issue that is worth telling the team about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QueueMode {
    /// A background task appends reportable issues to `queue_file`, within the rate limits.
    #[default]
    Auto,
    /// Only the web/API "queue" action appends an entry.
    Manual,
    /// Never touch the queue file (issues are still collected, kept and shown in the web UI).
    Off,
}

/// `[issues]`: server issue reports, see [`crate::issues`].
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IssuesCfg {
    /// Collect issue reports at all.
    pub enabled: bool,
    /// Where the book of issues (`issues.json`) lives; default `<directory of bots.roster>/issues`.
    pub dir: Option<PathBuf>,
    /// Where the evidence folders go; default `<dir>/evidence`.
    pub evidence_dir: Option<PathBuf>,
    /// The operator's issue queue (a text file the team works from); entries are appended to it.
    pub queue_file: Option<PathBuf>,
    /// What to do with reportable issues, see [`QueueMode`].
    pub queue_mode: QueueMode,
    /// Label of the time zone in a queue entry's header (`EST`); default `UTC±HH:MM`.
    pub time_label: Option<String>,
    /// The game server's log file, tailed to correlate issues with server warnings and errors.
    pub server_log: Option<PathBuf>,
    /// Occurrences after which an issue is reportable.
    pub min_occurrences: u32,
    /// Distinct bots after which an issue is reportable.
    pub min_bots: u32,
    /// At most this many queue entries per hour, all kinds together.
    pub max_queue_per_hour: u32,
    /// At most this many new queue entries per issue kind per 24 hours.
    pub max_queue_per_kind_per_day: u32,
}

impl Default for IssuesCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            dir: None,
            evidence_dir: None,
            queue_file: None,
            queue_mode: QueueMode::Auto,
            time_label: None,
            server_log: None,
            min_occurrences: 3,
            min_bots: 2,
            max_queue_per_hour: 6,
            max_queue_per_kind_per_day: 2,
        }
    }
}

impl IssuesCfg {
    /// The directory of the issue book: `issues.dir`, else `issues` next to the roster file.
    pub fn resolved_dir(&self, roster: &Path) -> PathBuf {
        match &self.dir {
            Some(d) => d.clone(),
            None => roster
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .join("issues"),
        }
    }

    /// The directory of the evidence folders: `issues.evidence_dir`, else `<dir>/evidence`.
    pub fn resolved_evidence_dir(&self, roster: &Path) -> PathBuf {
        match &self.evidence_dir {
            Some(d) => d.clone(),
            None => self.resolved_dir(roster).join("evidence"),
        }
    }

    /// `time_label` without surrounding blanks; `None` when unset or blank.
    pub fn label(&self) -> Option<&str> {
        self.time_label
            .as_deref()
            .map(str::trim)
            .filter(|l| !l.is_empty())
    }

    /// Checks the values that would otherwise fail late and obscurely.
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.min_occurrences >= 1,
            "issues.min_occurrences must be at least 1"
        );
        ensure!(self.min_bots >= 1, "issues.min_bots must be at least 1");
        for (name, p) in [
            ("dir", &self.dir),
            ("evidence_dir", &self.evidence_dir),
            ("queue_file", &self.queue_file),
            ("server_log", &self.server_log),
        ] {
            if let Some(p) = p {
                ensure!(
                    !p.as_os_str().is_empty(),
                    "issues.{name} is empty (leave it out instead)"
                );
            }
        }
        if let Some(l) = self.label() {
            ensure!(
                l.chars().count() <= 16 && !l.chars().any(|c| c.is_control() || c == ']'),
                "issues.time_label must be at most 16 characters without ']' or control characters"
            );
        }
        Ok(())
    }
}

impl Config {
    /// Reads, parses and validates `path`.
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        let text = std::fs::read_to_string(path).with_context(|| {
            format!(
                "reading {} (create one with `playerbots example-config > {}`)",
                path.display(),
                path.display()
            )
        })?;
        Config::parse(&text).with_context(|| format!("in {}", path.display()))
    }

    /// Parses and validates a config from TOML text.
    pub fn parse(text: &str) -> anyhow::Result<Config> {
        let cfg: Config = toml::from_str(text).context("parsing the config")?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// The commented example config (`playerbots.example.toml`).
    pub fn example() -> &'static str {
        include_str!("../../../playerbots.example.toml")
    }

    /// The password the operator chose: `server.password`, else `$PLAYERBOTS_PASSWORD`; `None` when
    /// neither is set (see [`crate::secrets`] for what happens then).
    pub fn explicit_password(&self) -> Option<String> {
        if !self.server.password.is_empty() {
            return Some(self.server.password.clone());
        }
        match std::env::var(PASSWORD_ENV) {
            Ok(p) if !p.is_empty() => Some(p),
            _ => None,
        }
    }

    /// Whether bots may sell goods to real players (characters that are not one of ours):
    /// `market.sell_to_players` with `market.enabled`, and never while `bots.gm_bootstrap` is on,
    /// because what a GM-bootstrapped bot holds may be minted goods and gold that no player should
    /// be able to collect (the same rule as `behavior.trade_with_players`).
    pub fn may_sell_to_players(&self) -> bool {
        self.market.enabled && self.market.sell_to_players && !self.bots.gm_bootstrap
    }

    /// Whether bots may attack real players (characters that are not one of ours):
    /// `pvp.attack_players` with `pvp.enabled`, and never while `bots.gm_bootstrap` is on, because
    /// a GM-bootstrapped bot may hold minted gear and levels that a player cannot match (the same
    /// rule as [`Config::may_sell_to_players`]).
    pub fn may_attack_players(&self) -> bool {
        self.pvp.enabled && self.pvp.attack_players && !self.bots.gm_bootstrap
    }

    /// Checks the values that would otherwise fail late and obscurely.
    pub fn validate(&self) -> anyhow::Result<()> {
        let s = &self.server;
        ensure!(!s.login.trim().is_empty(), "server.login is empty");
        ensure!(
            valid_account_prefix(&s.account_prefix),
            "server.account_prefix {:?} must be 1..=24 characters of letters, digits and _ . - @",
            s.account_prefix
        );
        ensure!(
            !self.data.world.trim().is_empty(),
            "data.world must not be empty"
        );
        let b = &self.bots;
        ensure!(
            b.count <= MAX_BOTS,
            "bots.count {} is above the limit of {MAX_BOTS}",
            b.count
        );
        ensure!(b.tick_ms >= 50, "bots.tick_ms must be at least 50");
        ensure!(
            b.idle_tick_ms >= b.tick_ms,
            "bots.idle_tick_ms must not be below bots.tick_ms"
        );
        ensure!(
            (50..=b.tick_ms).contains(&b.move_update_ms),
            "bots.move_update_ms must be between 50 and bots.tick_ms"
        );
        ensure!(
            b.move_speed.is_finite() && b.move_speed > 0.0 && b.move_speed <= 25.0,
            "bots.move_speed must be in (0, 25] m/s"
        );
        ensure!(
            b.walk_speed.is_finite() && b.walk_speed > 0.0 && b.walk_speed <= b.move_speed,
            "bots.walk_speed must be in (0, move_speed]"
        );
        ensure!(
            b.long_travel_teleport_m.is_finite() && b.long_travel_teleport_m >= 0.0,
            "bots.long_travel_teleport_m must be 0 or positive"
        );
        ensure!(
            b.reconnect_min_secs >= 1 && b.reconnect_min_secs <= b.reconnect_max_secs,
            "bots.reconnect_min_secs must be in 1..=reconnect_max_secs"
        );
        ensure!(!b.races.is_empty(), "bots.races is empty");
        for r in &b.races {
            if race_id(r).is_none() {
                bail!(
                    "bots.races: unknown race {r:?} (known: nuian, fairy, dwarf, elf, hariharan, ferre, returned, warborn, or an id 1..=8)"
                );
            }
        }
        let w = &self.behavior;
        for (name, v) in [
            ("quest", w.quest),
            ("grind", w.grind),
            ("duel", w.duel),
            ("trade", w.trade),
            ("trade_run", w.trade_run),
            ("housing", w.housing),
            ("rest", w.rest),
            ("wander", w.wander),
        ] {
            ensure!(
                v.is_finite() && v >= 0.0,
                "behavior.{name} must be a number >= 0"
            );
        }
        ensure!(
            w.persona_spread.is_finite() && (0.0..=1.0).contains(&w.persona_spread),
            "behavior.persona_spread must be in 0..=1"
        );
        ensure!(
            w.jump_rate.is_finite() && w.jump_rate >= 0.0,
            "behavior.jump_rate must be a number >= 0"
        );
        let m = &self.mounts;
        ensure!(
            m.summon_over_m.is_finite() && m.summon_over_m >= 0.0,
            "mounts.summon_over_m must be a number >= 0"
        );
        ensure!(
            m.dismount_under_m.is_finite()
                && m.dismount_under_m >= 0.0
                && m.dismount_under_m <= m.summon_over_m,
            "mounts.dismount_under_m must be in 0..=mounts.summon_over_m"
        );
        ensure!(
            m.speed.is_finite() && m.speed > 0.0 && m.speed <= 25.0,
            "mounts.speed must be in (0, 25] m/s"
        );
        ensure!(
            m.idle_despawn_secs <= 86_400,
            "mounts.idle_despawn_secs must be at most 86400"
        );
        ensure!(
            m.glide_drop_m.is_finite() && m.glide_drop_m > 0.0,
            "mounts.glide_drop_m must be a number > 0"
        );
        ensure!(
            m.glide_lookahead_m.is_finite() && (4.0..=200.0).contains(&m.glide_lookahead_m),
            "mounts.glide_lookahead_m must be between 4 and 200 m"
        );
        ensure!(
            m.glide_speed.is_finite() && m.glide_speed > 0.0 && m.glide_speed <= 25.0,
            "mounts.glide_speed must be in (0, 25] m/s"
        );
        ensure!(
            m.glide_descent.is_finite() && m.glide_descent > 0.0 && m.glide_descent <= 25.0,
            "mounts.glide_descent must be in (0, 25] m/s"
        );
        ensure!(
            self.dungeons.relax_after_secs <= crate::bot::coordinator::MAX_RELAX_SECS,
            "dungeons.relax_after_secs {} is above {}: the coordinator drops a party wish after \
             {} s, so a longer wait would never form the smaller party",
            self.dungeons.relax_after_secs,
            crate::bot::coordinator::MAX_RELAX_SECS,
            crate::bot::coordinator::WISH_TTL.as_secs()
        );
        ensure!(
            self.market.ad_interval_secs >= 60,
            "market.ad_interval_secs must be at least 60"
        );
        let g = &self.guild;
        ensure!(g.min_level >= 1, "guild.min_level must be at least 1");
        ensure!(
            (1..=50).contains(&g.party_size),
            "guild.party_size must be in 1..=50"
        );
        ensure!(
            g.max_guilds <= MAX_BOTS,
            "guild.max_guilds {} is above the limit of {MAX_BOTS}",
            g.max_guilds
        );
        ensure!(
            g.churn_per_hour.is_finite() && (0.0..=60.0).contains(&g.churn_per_hour),
            "guild.churn_per_hour must be a number in 0..=60"
        );
        let pt = &self.party;
        ensure!(
            pt.max_level_gap >= 1,
            "party.max_level_gap must be at least 1"
        );
        ensure!(
            (1..=4).contains(&pt.max_per_player),
            "party.max_per_player must be in 1..=4"
        );
        ensure!(
            pt.max_companions <= MAX_BOTS,
            "party.max_companions {} is above the limit of {MAX_BOTS}",
            pt.max_companions
        );
        let pv = &self.pvp;
        ensure!(
            (1..=50).contains(&pv.max_squad),
            "pvp.max_squad must be in 1..=50"
        );
        ensure!(
            pv.retreat_hp_pct <= 100,
            "pvp.retreat_hp_pct must be in 0..=100"
        );
        ensure!(
            (1..=100).contains(&pv.war_kill_cap),
            "pvp.war_kill_cap must be in 1..=100"
        );
        for (key, names) in [
            ("never_names", &pv.never_names),
            ("only_names", &pv.only_names),
        ] {
            ensure!(
                names.iter().all(|n| !n.trim().is_empty()),
                "pvp.{key} must not hold an empty name"
            );
        }
        let ev = &self.events;
        ensure!(
            ev.max_bots_per_event <= MAX_BOTS,
            "events.max_bots_per_event {} is above the limit of {MAX_BOTS}",
            ev.max_bots_per_event
        );
        ensure!(
            ev.max_share.is_finite() && (0.0..=1.0).contains(&ev.max_share),
            "events.max_share must be in 0..=1"
        );
        ensure!(
            ev.join_delay_secs[0] <= ev.join_delay_secs[1],
            "events.join_delay_secs must be [low, high] with low <= high"
        );
        let po = &self.population;
        ensure!(
            po.max_spawn_per_min <= MAX_BOTS,
            "population.max_spawn_per_min {} is above the limit of {MAX_BOTS}",
            po.max_spawn_per_min
        );
        ensure!(
            po.min_action_gap_secs >= 1,
            "population.min_action_gap_secs must be at least 1"
        );
        for (key, v) in [
            ("max_online", po.max_online),
            ("min_per_side", po.min_per_side),
            ("per_side_cap", po.per_side_cap),
        ] {
            ensure!(
                v <= MAX_BOTS,
                "population.{key} {v} is above the limit of {MAX_BOTS}"
            );
        }
        ensure!(
            po.mirror_pct <= 1000,
            "population.mirror_pct must be at most 1000"
        );
        ensure!(
            (1..=1000).contains(&po.step),
            "population.step must be in 1..=1000"
        );
        ensure!(
            po.hysteresis <= 1000,
            "population.hysteresis must be at most 1000"
        );
        ensure!(
            po.server_poll_secs >= 5,
            "population.server_poll_secs must be at least 5"
        );
        ensure!(
            po.server_api.is_empty() || plain_http_url(&po.server_api),
            "population.server_api must be an http:// or https:// address without a login in it \
             (the key goes in server_api_key_env or server_api_key_file)"
        );
        ensure!(
            po.server_api_key_env.is_empty()
                || (po
                    .server_api_key_env
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
                    && !po
                        .server_api_key_env
                        .starts_with(|c: char| c.is_ascii_digit())),
            "population.server_api_key_env must be the NAME of an environment variable"
        );
        ensure!(
            self.farm.max_plots <= 1000,
            "farm.max_plots {} is above the limit of 1000",
            self.farm.max_plots
        );
        ensure!(
            self.farm.growth_rate.is_finite() && (0.01..=10_000.0).contains(&self.farm.growth_rate),
            "farm.growth_rate {} must be between 0.01 and 10000",
            self.farm.growth_rate
        );
        ensure!(
            self.farm.max_trip_m.is_finite() && (50.0..=100_000.0).contains(&self.farm.max_trip_m),
            "farm.max_trip_m {} must be between 50 and 100000",
            self.farm.max_trip_m
        );
        ensure!(
            self.farm.wild_plots as usize <= crate::journal::MAX_WILD_PLOTS,
            "farm.wild_plots {} is above the limit of {}",
            self.farm.wild_plots,
            crate::journal::MAX_WILD_PLOTS
        );
        ensure!(
            self.farm.wild_outlaw.is_finite() && (0.0..=1.0).contains(&self.farm.wild_outlaw),
            "farm.wild_outlaw must be in 0..=1"
        );
        self.presence.validate()?;
        self.explore.validate()?;
        let g = &self.gear;
        ensure!(
            (1..=100).contains(&g.max_spend_pct),
            "gear.max_spend_pct {} must be between 1 and 100",
            g.max_spend_pct
        );
        ensure!(
            (1..=3_600).contains(&g.check_secs),
            "gear.check_secs {} must be between 1 and 3600",
            g.check_secs
        );
        ensure!(
            g.max_trip_m.is_finite() && (50.0..=100_000.0).contains(&g.max_trip_m),
            "gear.max_trip_m {} must be between 50 and 100000",
            g.max_trip_m
        );
        let c = &self.social;
        ensure!(
            c.reply_radius_m.is_finite() && c.reply_radius_m >= 0.0,
            "social.reply_radius_m must be a number >= 0"
        );
        ensure!(
            c.reply_chance.is_finite() && (0.0..=1.0).contains(&c.reply_chance),
            "social.reply_chance must be in 0..=1"
        );
        self.personality.validate()?;
        ensure!(
            self.llm.director_interval_secs >= 30,
            "llm.director_interval_secs must be at least 30"
        );
        self.issues.validate()?;
        Ok(())
    }
}

/// Whether `prefix` makes valid login names (`^[\p{L}\p{Nd}_.\-@]{1,32}$` once an id of up to
/// eight digits is appended).
pub fn valid_account_prefix(prefix: &str) -> bool {
    let n = prefix.chars().count();
    (1..=24).contains(&n)
        && prefix
            .chars()
            .all(|c| c.is_alphabetic() || c.is_numeric() || matches!(c, '_' | '.' | '-' | '@'))
}

/// Race id of a race name (`nuian` ... `warborn`, case-insensitive) or a numeric id 1..=8.
pub fn race_id(name: &str) -> Option<u8> {
    let n = name.trim().to_ascii_lowercase();
    if let Ok(id) = n.parse::<u8>() {
        return (1..=8).contains(&id).then_some(id);
    }
    Some(match n.as_str() {
        "nuian" => 1,
        "fairy" => 2,
        "dwarf" => 3,
        "elf" => 4,
        "hariharan" => 5,
        "ferre" => 6,
        "returned" => 7,
        "warborn" => 8,
        _ => return None,
    })
}

/// Display name of a race id.
pub fn race_name(id: u8) -> &'static str {
    match id {
        1 => "Nuian",
        2 => "Fairy",
        3 => "Dwarf",
        4 => "Elf",
        5 => "Hariharan",
        6 => "Ferre",
        7 => "Returned",
        8 => "Warborn",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_example_config_parses_and_matches_the_defaults() {
        let cfg = Config::parse(Config::example()).expect("example config");
        let def = Config::default();
        assert_eq!(cfg.server.login, def.server.login);
        assert_eq!(cfg.server.account_prefix, "pb");
        assert_eq!(cfg.bots.tick_ms, def.bots.tick_ms);
        assert_eq!(cfg.bots.idle_tick_ms, def.bots.idle_tick_ms);
        assert_eq!(cfg.bots.move_update_ms, def.bots.move_update_ms);
        assert_eq!(cfg.bots.races, def.bots.races);
        assert!(!cfg.bots.gm_bootstrap);
        assert_eq!(cfg.web.bind.to_string(), "127.0.0.1:8790");
        assert!(!cfg.llm.enabled);
        // Paths for the dev machine are commented out in the example.
        assert!(cfg.data.baked.is_none());
    }

    #[test]
    fn an_empty_file_is_a_valid_config() {
        let cfg = Config::parse("").unwrap();
        assert_eq!(cfg.bots.count, 10);
        assert_eq!(cfg.data.world, "main_world");
        assert_eq!(cfg.bots.race_ids(), vec![1, 4, 5, 6]);
    }

    #[test]
    fn partial_tables_keep_the_other_defaults() {
        let cfg = Config::parse("[bots]\ncount = 3\n[behavior]\ngrind = 2.5\n").unwrap();
        assert_eq!(cfg.bots.count, 3);
        assert_eq!(cfg.bots.ramp_ms, 750);
        assert_eq!(cfg.behavior.grind, 2.5);
        assert_eq!(cfg.behavior.quest, 1.0);
    }

    #[test]
    fn unknown_keys_and_bad_values_are_rejected() {
        assert!(Config::parse("[bots]\nconut = 3\n").is_err());
        assert!(Config::parse("[nope]\nx = 1\n").is_err());
        assert!(Config::parse("[bots]\ntick_ms = 10\n").is_err());
        assert!(Config::parse("[bots]\ntick_ms = 500\nidle_tick_ms = 100\n").is_err());
        assert!(Config::parse("[bots]\nmove_update_ms = 20\n").is_err());
        assert!(Config::parse("[bots]\nmove_update_ms = 300\n").is_err());
        assert!(Config::parse("[bots]\nmove_update_ms = 250\n").is_ok());
        assert!(Config::parse("[bots]\nraces = [\"gnome\"]\n").is_err());
        assert!(Config::parse("[bots]\nraces = []\n").is_err());
        assert!(Config::parse("[bots]\nmove_speed = 0\n").is_err());
        assert!(Config::parse("[bots]\nwalk_speed = 9.0\n").is_err());
        assert!(Config::parse("[bots]\nreconnect_min_secs = 130\n").is_err());
        assert!(Config::parse("[server]\naccount_prefix = \"a b\"\n").is_err());
        assert!(Config::parse("[server]\naccount_prefix = \"\"\n").is_err());
        assert!(Config::parse("[behavior]\ngrind = -1\n").is_err());
        assert!(Config::parse("[behavior]\npersona_spread = 2\n").is_err());
        assert!(Config::parse("[bots]\ncount = 999999\n").is_err());
        assert!(Config::parse("[social]\nreply_chance = 1.5\n").is_err());
        assert!(Config::parse("[social]\nreply_radius_m = -1\n").is_err());
        assert!(Config::parse("[social]\nchater = true\n").is_err());
        assert!(Config::parse("[llm]\ndirector_interval_secs = 5\n").is_err());
    }

    #[test]
    fn the_social_and_director_keys_have_quiet_defaults() {
        let cfg = Config::parse("").unwrap();
        assert!(cfg.social.enabled && cfg.social.bot_chatter);
        assert!(cfg.social.chatter_needs_player && !cfg.social.reply_to_bots);
        assert_eq!(cfg.social.chatter_per_min, 4);
        assert_eq!(cfg.social.chatter_gap_secs, 60);
        assert!(!cfg.llm.director, "the director is opt-in");
        assert_eq!(cfg.llm.director_interval_secs, 600);
        let cfg = Config::parse(
            "[llm]\nenabled = true\ndirector = true\ndirector_interval_secs = 90\n[social]\nbot_chatter = false\n",
        )
        .unwrap();
        assert!(cfg.llm.director && !cfg.social.bot_chatter);
        assert_eq!(cfg.llm.director_interval_secs, 90);
    }

    #[test]
    fn issue_reports_are_on_by_default_without_a_queue_file() {
        let cfg = Config::parse("").unwrap();
        let i = &cfg.issues;
        assert!(i.enabled);
        assert_eq!(i.queue_mode, QueueMode::Auto);
        assert!(i.queue_file.is_none() && i.server_log.is_none() && i.time_label.is_none());
        assert!(i.dir.is_none() && i.evidence_dir.is_none());
        assert_eq!((i.min_occurrences, i.min_bots), (3, 2));
        assert_eq!((i.max_queue_per_hour, i.max_queue_per_kind_per_day), (6, 2));
        // the example file documents exactly the defaults
        let ex = Config::parse(Config::example()).unwrap().issues;
        assert_eq!(ex.enabled, i.enabled);
        assert_eq!(ex.queue_mode, i.queue_mode);
        assert_eq!((ex.min_occurrences, ex.min_bots), (3, 2));
        assert_eq!(
            (ex.max_queue_per_hour, ex.max_queue_per_kind_per_day),
            (6, 2)
        );
        assert!(ex.queue_file.is_none() && ex.server_log.is_none() && ex.dir.is_none());
    }

    #[test]
    fn issue_report_paths_default_next_to_the_roster() {
        let cfg = Config::parse("").unwrap();
        let roster = Path::new("data/roster.json");
        assert_eq!(
            cfg.issues.resolved_dir(roster),
            PathBuf::from("data").join("issues")
        );
        assert_eq!(
            cfg.issues.resolved_evidence_dir(roster),
            PathBuf::from("data").join("issues").join("evidence")
        );
        let cfg = Config::parse(
            "[issues]\ndir = \"state\"\nevidence_dir = \"ev\"\nqueue_file = \"q.txt\"\nqueue_mode = \"manual\"\ntime_label = \" EST \"\nserver_log = \"s.log\"\nmin_occurrences = 5\nmin_bots = 3\nmax_queue_per_hour = 1\nmax_queue_per_kind_per_day = 1\n",
        )
        .unwrap();
        assert_eq!(cfg.issues.resolved_dir(roster), PathBuf::from("state"));
        assert_eq!(
            cfg.issues.resolved_evidence_dir(roster),
            PathBuf::from("ev")
        );
        assert_eq!(cfg.issues.queue_mode, QueueMode::Manual);
        assert_eq!(cfg.issues.label(), Some("EST"));
        assert_eq!(cfg.issues.queue_file.as_deref(), Some(Path::new("q.txt")));
        assert_eq!(cfg.issues.min_occurrences, 5);
    }

    #[test]
    fn bad_issue_settings_are_rejected() {
        for bad in [
            "[issues]\nqueue_mode = \"sometimes\"\n",
            "[issues]\nmin_occurrences = 0\n",
            "[issues]\nmin_bots = 0\n",
            "[issues]\nqueue_file = \"\"\n",
            "[issues]\nserver_log = \"\"\n",
            "[issues]\ntime_label = \"this label is far too long\"\n",
            "[issues]\ntime_label = \"E]T\"\n",
            "[issues]\nqueu_file = \"q.txt\"\n",
            "[issues]\nmax_queue_per_hour = -1\n",
        ] {
            assert!(Config::parse(bad).is_err(), "{bad}");
        }
        for ok in [
            "[issues]\nqueue_mode = \"off\"\n",
            "[issues]\nenabled = false\n",
            "[issues]\nmax_queue_per_hour = 0\n",
            "[issues]\ntime_label = \"\"\n",
        ] {
            assert!(Config::parse(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn race_names_and_ids() {
        assert_eq!(race_id("Nuian"), Some(1));
        assert_eq!(race_id(" HARIHARAN "), Some(5));
        assert_eq!(race_id("4"), Some(4));
        assert_eq!(race_id("9"), None);
        assert_eq!(race_id("0"), None);
        assert_eq!(race_name(6), "Ferre");
    }

    #[test]
    fn account_prefixes() {
        assert!(valid_account_prefix("pb"));
        assert!(valid_account_prefix("bot_.-@é1"));
        assert!(!valid_account_prefix(""));
        assert!(!valid_account_prefix("p b"));
        assert!(!valid_account_prefix("<x>"));
        assert!(!valid_account_prefix(&"a".repeat(25)));
    }

    #[test]
    fn the_password_is_never_printed() {
        let mut cfg = Config::default();
        cfg.server.password = "hunter2".into();
        let shown = format!("{cfg:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
        assert_eq!(cfg.explicit_password().as_deref(), Some("hunter2"));
    }

    #[test]
    fn trade_with_players_is_off_unless_asked_for() {
        assert!(!Config::default().behavior.trade_with_players);
        let cfg = Config::parse(
            "[behavior]
trade_with_players = true
",
        )
        .unwrap();
        assert!(cfg.behavior.trade_with_players);
        assert!(
            !Config::parse(Config::example())
                .unwrap()
                .behavior
                .trade_with_players
        );
    }

    #[test]
    fn the_dungeon_level_raise_is_off_by_default_and_documented() {
        assert!(!Config::default().behavior.dungeon_level_up);
        assert!(
            !Config::parse(Config::example())
                .unwrap()
                .behavior
                .dungeon_level_up
        );
        assert!(Config::example().contains("dungeon_level_up = false"));
        let cfg = Config::parse("[behavior]\ndungeon_level_up = true\n").unwrap();
        assert!(cfg.behavior.dungeons && cfg.behavior.dungeon_level_up);
        assert!(Config::parse("[behavior]\ndungeon_level_up = 1\n").is_err());
    }

    #[test]
    fn the_dungeon_keys_default_to_no_entry_limit_and_a_long_wait() {
        let d = Config::default().dungeons;
        assert!(!d.entry_limit, "the 1.2 server has no entry limit");
        assert_eq!(d.relax_after_secs, crate::bot::party::RELAX_AFTER_SECS);
        assert_eq!(d.relax_after_secs, 600);
        // the example file documents both keys with the defaults
        let ex = Config::parse(Config::example()).unwrap().dungeons;
        assert!(!ex.entry_limit);
        assert_eq!(ex.relax_after_secs, 600);
        assert_documented(
            "dungeons",
            &["entry_limit = false", "relax_after_secs = 600"],
        );
        // each key can be set on its own
        let cfg = Config::parse(
            "[dungeons]
entry_limit = true
relax_after_secs = 45
",
        )
        .unwrap();
        assert!(cfg.dungeons.entry_limit);
        assert_eq!(cfg.dungeons.relax_after_secs, 45);
        // the wish lifetime and the sweep period cap the wait below the gather window
        let cap = crate::bot::coordinator::MAX_RELAX_SECS;
        assert_eq!(cap, 600);
        assert!(Config::parse(&format!("[dungeons]\nrelax_after_secs = {cap}\n")).is_ok());
        assert!(Config::parse(&format!("[dungeons]\nrelax_after_secs = {}\n", cap + 1)).is_err());
        assert!(Config::parse("[dungeons]\nentry_limit = 1\n").is_err());
        assert!(Config::parse("[dungeons]\nentry_limits = true\n").is_err());
    }

    #[test]
    fn the_mounts_are_on_and_documented() {
        let m = Config::default().mounts;
        assert!(m.enabled && m.mounts && m.gliders);
        assert_eq!(m.summon_over_m, 250.0);
        assert_eq!(m.dismount_under_m, 40.0);
        assert_documented(
            "mounts",
            &[
                "enabled = true",
                "mounts = true",
                "gliders = true",
                "summon_over_m = 250.0",
                "dismount_under_m = 40.0",
                "speed = 10.0",
                "item = 0",
                "glider_item = 0",
                "idle_despawn_secs = 120",
                "glide_drop_m = 8.0",
                "glide_lookahead_m = 30.0",
                "glide_speed = 9.0",
                "glide_descent = 3.0",
            ],
        );
        let ex = Config::parse(Config::example()).unwrap().mounts;
        assert!(ex.enabled && ex.mounts && ex.gliders);
        assert_eq!(ex.speed, 10.0);
        for bad in [
            "[mounts]
dismount_under_m = 300.0
",
            "[mounts]
speed = 0.0
",
            "[mounts]
glide_drop_m = 0.0
",
            "[mounts]
glide_speed = 30.0
",
            // the ledge search samples the ground every tick: a huge look-ahead would stall a bot
            "[mounts]
glide_lookahead_m = 1000000.0
",
            "[mounts]
glide_lookahead_m = 3.0
",
        ] {
            assert!(Config::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_travel_features_are_on_unless_switched_off() {
        let b = Config::default().behavior;
        assert!(b.portals && b.dungeons && b.sailing && b.navmesh);
        // the example file documents all four and keeps the defaults
        let ex = Config::parse(Config::example()).unwrap().behavior;
        assert!(ex.portals && ex.dungeons && ex.sailing && ex.navmesh);
        for key in ["portals", "dungeons", "sailing", "navmesh"] {
            assert!(
                Config::example().contains(&format!(
                    "
{key} = true"
                )),
                "{key}"
            );
        }
        // each one switches off on its own
        let cfg = Config::parse(
            "[behavior]
portals = false
",
        )
        .unwrap();
        assert!(!cfg.behavior.portals && cfg.behavior.dungeons && cfg.behavior.sailing);
        let cfg = Config::parse(
            "[behavior]
dungeons = false
",
        )
        .unwrap();
        assert!(cfg.behavior.portals && !cfg.behavior.dungeons && cfg.behavior.sailing);
        let cfg = Config::parse(
            "[behavior]
sailing = false
",
        )
        .unwrap();
        assert!(cfg.behavior.portals && cfg.behavior.dungeons && !cfg.behavior.sailing);
        let cfg = Config::parse(
            "[behavior]
navmesh = false
",
        )
        .unwrap();
        assert!(!cfg.behavior.navmesh && cfg.behavior.portals && cfg.behavior.sailing);
        // they are switches, not weights, and a typo is not silently ignored
        for bad in [
            "[behavior]
portals = 0.5
",
            "[behavior]
dungeons = \"no\"
",
            "[behavior]
sailing = 1
",
            "[behavior]
sail = true
",
        ] {
            assert!(Config::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_market_keys_have_quiet_defaults() {
        let m = Config::default().market;
        assert!(m.enabled && m.auction && m.chat_ads);
        assert!(!m.sell_to_players, "selling to players is opt-in");
        assert_eq!(m.meet_at, MeetAt::Auto);
        assert_eq!((m.max_ads_per_hour, m.ad_interval_secs), (12, 900));
        // the example file documents every key with its default
        let ex = Config::parse(Config::example()).unwrap().market;
        assert!(ex.enabled && ex.auction && ex.chat_ads && !ex.sell_to_players);
        assert_eq!(ex.meet_at, MeetAt::Auto);
        assert_eq!((ex.max_ads_per_hour, ex.ad_interval_secs), (12, 900));
        let section = Config::example()
            .split(
                "
[market]
",
            )
            .nth(1)
            .and_then(|s| {
                s.split(
                    "
[",
                )
                .next()
            })
            .expect("a [market] section");
        for key in [
            "enabled = true",
            "auction = true",
            "chat_ads = true",
            "sell_to_players = false",
            "meet_at = \"auto\"",
            "max_ads_per_hour = 12",
            "ad_interval_secs = 900",
        ] {
            assert!(
                section.contains(&format!(
                    "
{key}
"
                )),
                "{key}"
            );
        }
        // each key can be set on its own
        let cfg = Config::parse(
            "[market]
enabled = false
auction = false
chat_ads = false
sell_to_players = true
meet_at = \"mirage\"
max_ads_per_hour = 0
ad_interval_secs = 60
",
        )
        .unwrap();
        let m = cfg.market;
        assert!(!m.enabled && !m.auction && !m.chat_ads && m.sell_to_players);
        assert_eq!(m.meet_at, MeetAt::Mirage);
        assert_eq!((m.max_ads_per_hour, m.ad_interval_secs), (0, 60));
        for (text, want) in [("auto", MeetAt::Auto), ("town", MeetAt::Town)] {
            let cfg = Config::parse(&format!(
                "[market]
meet_at = \"{text}\"
"
            ))
            .unwrap();
            assert_eq!(cfg.market.meet_at, want);
        }
        // a typo, a bad place and bad numbers are refused
        for bad in [
            "[market]
auctions = true
",
            "[market]
meet_at = \"moon\"
",
            "[market]
meet_at = true
",
            "[market]
enabled = 1
",
            "[market]
max_ads_per_hour = -1
",
            "[market]
ad_interval_secs = 59
",
            "[market]
ad_interval_secs = 0
",
            "[market]
sell_to_players = \"yes\"
",
        ] {
            assert!(Config::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn players_only_buy_from_bots_that_hold_nothing_minted() {
        let mut cfg = Config::default();
        assert!(!cfg.may_sell_to_players(), "off by default");
        cfg.market.sell_to_players = true;
        assert!(cfg.may_sell_to_players());
        // the market master switch wins
        cfg.market.enabled = false;
        assert!(!cfg.may_sell_to_players());
        cfg.market.enabled = true;
        // what a GM-bootstrapped bot holds may be minted: nothing of it is sold to a player
        cfg.bots.gm_bootstrap = true;
        assert!(!cfg.may_sell_to_players());
        // (and the old switch does not open the market)
        cfg.market.sell_to_players = false;
        cfg.behavior.trade_with_players = true;
        cfg.bots.gm_bootstrap = false;
        assert!(!cfg.may_sell_to_players());
    }

    #[test]
    fn the_lifelike_keys_are_on_and_validated() {
        let b = Config::default().behavior;
        assert!(b.lifelike && b.emotes);
        assert_eq!(b.jump_rate, 1.0);
        let ex = Config::parse(Config::example()).unwrap().behavior;
        assert!(ex.lifelike && ex.emotes);
        assert_eq!(ex.jump_rate, 1.0);
        for key in ["lifelike = true", "jump_rate = 1.0", "emotes = true"] {
            assert!(
                Config::example().contains(&format!(
                    "
{key}
"
                )),
                "{key}"
            );
        }
        let cfg = Config::parse(
            "[behavior]
lifelike = false
jump_rate = 0
emotes = false
",
        )
        .unwrap();
        assert!(!cfg.behavior.lifelike && !cfg.behavior.emotes);
        assert_eq!(cfg.behavior.jump_rate, 0.0);
        assert_eq!(
            Config::parse(
                "[behavior]
jump_rate = 2.5
"
            )
            .unwrap()
            .behavior
            .jump_rate,
            2.5
        );
        for bad in [
            "[behavior]
jump_rate = -0.5
",
            "[behavior]
jump_rate = \"often\"
",
            "[behavior]
lifelike = 1
",
            "[behavior]
emote = true
",
        ] {
            assert!(Config::parse(bad).is_err(), "{bad}");
        }
    }

    /// The text of `[section]` in the example config.
    fn example_section(name: &str) -> &'static str {
        let head = format!("\n[{name}]\n");
        Config::example()
            .split(head.as_str())
            .nth(1)
            .and_then(|s| s.split("\n[").next())
            .unwrap_or_else(|| panic!("a [{name}] section"))
    }

    /// Every `key = value` line must be in the section of the example, so the file documents each
    /// key with its default.
    fn assert_documented(section: &str, lines: &[&str]) {
        let text = example_section(section);
        for line in lines {
            assert!(
                text.contains(&format!("\n{line}\n")),
                "[{section}] {line} is not in the example"
            );
        }
    }

    #[test]
    fn the_wave_one_sections_are_all_off_by_default() {
        let c = Config::default();
        assert!(!c.guild.enabled && !c.pvp.enabled && !c.events.enabled);
        assert!(!c.population.enabled && !c.farm.enabled);
        assert!(!c.party.join_player_parties && !c.party.chat_commands);
        assert!(!c.party.follow_into_dungeons);
        // the example file documents every key with its default and parses to the same values
        let ex = Config::parse(Config::example()).unwrap();
        assert!(!ex.guild.enabled && !ex.pvp.enabled && !ex.events.enabled);
        assert!(!ex.population.enabled && !ex.farm.enabled);
        assert!(!ex.party.join_player_parties && !ex.party.chat_commands);
        assert!(!ex.party.follow_into_dungeons);
        assert!(!c.may_attack_players() && !ex.may_attack_players());
    }

    #[test]
    fn the_guild_keys_have_quiet_defaults() {
        let g = Config::default().guild;
        assert!(!g.enabled && g.found && !g.accept_player_invites);
        assert_eq!((g.min_level, g.party_size, g.max_guilds), (5, 5, 3));
        let ex = Config::parse(Config::example()).unwrap().guild;
        assert!(!ex.enabled && ex.found && !ex.accept_player_invites);
        assert_eq!((ex.min_level, ex.party_size, ex.max_guilds), (5, 5, 3));
        assert_documented(
            "guild",
            &[
                "enabled = false",
                "found = true",
                "min_level = 5",
                "party_size = 5",
                "max_guilds = 3",
                "accept_player_invites = false",
                "churn_per_hour = 0.2",
            ],
        );
        assert_eq!((g.churn_per_hour, ex.churn_per_hour), (0.2, 0.2));
        let cfg = Config::parse(
            "[guild]\nenabled = true\nfound = false\nmin_level = 20\nparty_size = 2\nmax_guilds = 0\naccept_player_invites = true\nchurn_per_hour = 6\n",
        )
        .unwrap();
        let g = cfg.guild;
        assert!(g.enabled && !g.found && g.accept_player_invites);
        assert_eq!((g.min_level, g.party_size, g.max_guilds), (20, 2, 0));
        assert_eq!(g.churn_per_hour, 6.0);
        for bad in [
            "[guild]\nfounds = true\n",
            "[guild]\nenabled = 1\n",
            "[guild]\nmin_level = 0\n",
            "[guild]\nmin_level = 256\n",
            "[guild]\nparty_size = 0\n",
            "[guild]\nparty_size = 51\n",
            "[guild]\nmax_guilds = -1\n",
            "[guild]\nmax_guilds = 5001\n",
            "[guild]\nchurn_per_hour = -1\n",
            "[guild]\nchurn_per_hour = 61\n",
            "[guild]\nchurn_per_hour = \"often\"\n",
        ] {
            assert!(Config::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_pvp_keys_have_quiet_defaults() {
        let p = Config::default().pvp;
        assert!(!p.enabled && p.defend && !p.attack_bots && !p.attack_players);
        assert!(!p.gank && !p.warzone);
        assert!(p.never_names.is_empty() && p.only_names.is_empty());
        assert_eq!((p.max_squad, p.retreat_hp_pct, p.war_kill_cap), (3, 35, 10));
        let ex = Config::parse(Config::example()).unwrap().pvp;
        assert!(!ex.enabled && ex.defend && !ex.attack_bots && !ex.attack_players);
        assert!(!ex.gank && !ex.warzone);
        assert!(ex.never_names.is_empty() && ex.only_names.is_empty());
        assert_eq!(
            (ex.max_squad, ex.retreat_hp_pct, ex.war_kill_cap),
            (3, 35, 10)
        );
        assert_documented(
            "pvp",
            &[
                "enabled = false",
                "defend = true",
                "attack_bots = false",
                "attack_players = false",
                "gank = false",
                "warzone = false",
                "never_names = []",
                "only_names = []",
                "max_squad = 3",
                "retreat_hp_pct = 35",
                "war_kill_cap = 10",
            ],
        );
        let cfg = Config::parse(
            "[pvp]\nenabled = true\ndefend = false\nattack_bots = true\nattack_players = true\ngank = true\nwarzone = true\nnever_names = [\"Admin\", \"Gm Bob\"]\nonly_names = [\"Rival\"]\nmax_squad = 5\nretreat_hp_pct = 0\nwar_kill_cap = 4\n",
        )
        .unwrap();
        let p = cfg.pvp;
        assert!(p.enabled && !p.defend && p.attack_bots && p.attack_players);
        assert!(p.gank && p.warzone);
        assert_eq!(p.never_names, ["Admin", "Gm Bob"]);
        assert_eq!(p.only_names, ["Rival"]);
        assert_eq!((p.max_squad, p.retreat_hp_pct, p.war_kill_cap), (5, 0, 4));
        for bad in [
            "[pvp]\ngangk = true\n",
            "[pvp]\nattack_players = \"yes\"\n",
            "[pvp]\nnever_names = \"Admin\"\n",
            "[pvp]\nnever_names = [\"\"]\n",
            "[pvp]\nonly_names = [\"  \"]\n",
            "[pvp]\nmax_squad = 0\n",
            "[pvp]\nmax_squad = 51\n",
            "[pvp]\nretreat_hp_pct = 101\n",
            "[pvp]\nretreat_hp_pct = -1\n",
            "[pvp]\nwar_kill_cap = 0\n",
            "[pvp]\nwar_kill_cap = 101\n",
        ] {
            assert!(Config::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn players_are_attacked_only_when_every_switch_says_so() {
        let mut cfg = Config::default();
        assert!(!cfg.may_attack_players(), "off by default");
        cfg.pvp.attack_players = true;
        assert!(!cfg.may_attack_players(), "the master switch is off");
        cfg.pvp.enabled = true;
        assert!(cfg.may_attack_players());
        // attacking bots, ganking and warzones do not open the players
        cfg.pvp.attack_players = false;
        cfg.pvp.attack_bots = true;
        cfg.pvp.gank = true;
        cfg.pvp.warzone = true;
        assert!(!cfg.may_attack_players());
        cfg.pvp.attack_players = true;
        // what a GM-bootstrapped bot holds may be minted: it never attacks a player
        cfg.bots.gm_bootstrap = true;
        assert!(!cfg.may_attack_players());
        cfg.bots.gm_bootstrap = false;
        assert!(cfg.may_attack_players());
    }

    #[test]
    fn the_events_keys_have_quiet_defaults() {
        let e = Config::default().events;
        assert!(!e.enabled && e.avoid_war_zones);
        assert_eq!(e.max_bots_per_event, 6);
        assert_eq!(e.max_share, 0.25);
        assert_eq!(e.join_delay_secs, [10, 120]);
        let ex = Config::parse(Config::example()).unwrap().events;
        assert!(!ex.enabled && ex.avoid_war_zones);
        assert_eq!((ex.max_bots_per_event, ex.max_share), (6, 0.25));
        assert_eq!(ex.join_delay_secs, [10, 120]);
        assert_documented(
            "events",
            &[
                "enabled = false",
                "max_bots_per_event = 6",
                "max_share = 0.25",
                "join_delay_secs = [10, 120]",
                "avoid_war_zones = true",
            ],
        );
        let cfg = Config::parse(
            "[events]\nenabled = true\nmax_bots_per_event = 0\nmax_share = 1\njoin_delay_secs = [0, 0]\navoid_war_zones = false\n",
        )
        .unwrap();
        let e = cfg.events;
        assert!(e.enabled && !e.avoid_war_zones);
        assert_eq!((e.max_bots_per_event, e.max_share), (0, 1.0));
        assert_eq!(e.join_delay_secs, [0, 0]);
        for bad in [
            "[events]\nenable = true\n",
            "[events]\nmax_share = 1.5\n",
            "[events]\nmax_share = -0.1\n",
            "[events]\nmax_share = \"half\"\n",
            "[events]\nmax_bots_per_event = 5001\n",
            "[events]\njoin_delay_secs = [120, 10]\n",
            "[events]\njoin_delay_secs = [10]\n",
            "[events]\njoin_delay_secs = [1, 2, 3]\n",
            "[events]\njoin_delay_secs = [-1, 5]\n",
        ] {
            assert!(Config::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_party_keys_are_all_off() {
        let p = Config::default().party;
        assert!(!p.join_player_parties && !p.chat_commands && !p.follow_into_dungeons);
        assert_eq!(
            (p.max_level_gap, p.max_companions, p.max_per_player),
            (10, 8, 4)
        );
        assert_documented(
            "party",
            &[
                "join_player_parties = false",
                "chat_commands = false",
                "follow_into_dungeons = false",
                "max_level_gap = 10",
                "max_companions = 8",
                "max_per_player = 4",
            ],
        );
        let cfg = Config::parse(
            "[party]\njoin_player_parties = true\nchat_commands = true\nfollow_into_dungeons = true\nmax_level_gap = 5\nmax_companions = 2\nmax_per_player = 3\n",
        )
        .unwrap();
        let p = cfg.party;
        assert!(p.join_player_parties && p.chat_commands && p.follow_into_dungeons);
        assert_eq!(
            (p.max_level_gap, p.max_companions, p.max_per_player),
            (5, 2, 3)
        );
        for bad in [
            "[party]\njoin_parties = true\n",
            "[party]\nchat_commands = 1\n",
            "[party]\nmax_level_gap = 0\n",
            "[party]\nmax_per_player = 0\n",
            "[party]\nmax_per_player = 5\n",
        ] {
            assert!(Config::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_population_keys_are_off_and_a_dry_run_by_default() {
        let p = Config::default().population;
        assert!(!p.enabled && p.dry_run);
        assert_eq!((p.max_spawn_per_min, p.min_action_gap_secs), (10, 60));
        let ex = Config::parse(Config::example()).unwrap().population;
        assert!(!ex.enabled && ex.dry_run);
        assert_eq!((ex.max_spawn_per_min, ex.min_action_gap_secs), (10, 60));
        assert_documented(
            "population",
            &[
                "enabled = false",
                "dry_run = true",
                "max_spawn_per_min = 10",
                "min_action_gap_secs = 60",
                "max_online = 50",
                "min_per_side = 0",
                "mirror_pct = 100",
                "per_side_cap = 20",
                "step = 3",
                "hysteresis = 2",
                "despawn = true",
                "backoff_rss_mb = 3072",
                "backoff_packets_in = 30000",
                "backoff_login_failures = 8",
                "backoff_open_issues = 50",
                "server_api = \"\"",
                "server_api_key_env = \"\"",
                "server_poll_secs = 30",
            ],
        );
        let cfg = Config::parse(
            "[population]\nenabled = true\ndry_run = false\nmax_spawn_per_min = 0\nmin_action_gap_secs = 1\n",
        )
        .unwrap();
        let p = cfg.population;
        assert!(p.enabled && !p.dry_run);
        assert_eq!((p.max_spawn_per_min, p.min_action_gap_secs), (0, 1));
        for bad in [
            "[population]\ndryrun = true\n",
            "[population]\nmin_action_gap_secs = 0\n",
            "[population]\nmax_spawn_per_min = 5001\n",
            "[population]\nmax_spawn_per_min = -1\n",
            "[population]\nenabled = \"on\"\n",
            "[population]\nmax_online = 5001\n",
            "[population]\nper_side_cap = 5001\n",
            "[population]\nmirror_pct = 1001\n",
            "[population]\nstep = 0\n",
            "[population]\nserver_poll_secs = 4\n",
            "[population]\nserver_api = \"ftp://x\"\n",
            "[population]\nserver_api = \"http://user:secret@127.0.0.1:1280\"\n",
            "[population]\nserver_api = \"http://\"\n",
            "[population]\nserver_api_key_env = \"sk-abc def\"\n",
            "[population]\nserver_api_key_env = \"1KEY\"\n",
        ] {
            assert!(Config::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_population_policy_starts_from_the_keys() {
        let cfg = Config::parse(
            "[population]\nenabled = true\ndry_run = false\nmax_online = 80\nmirror_pct = 150\n\
             step = 5\nserver_api = \"http://127.0.0.1:1280\"\nserver_api_key_env = \"AAEMU_API_KEY\"\n",
        )
        .unwrap();
        let p = cfg.population.policy();
        assert!(p.active && !p.dry_run);
        assert_eq!((p.max_online, p.mirror_pct, p.step), (80, 150, 5));
        assert!(cfg.population.server_api_key_file.is_none());
        let off = Config::default().population.policy();
        assert!(!off.active && off.dry_run);
        assert_eq!(off.hysteresis, 2);
    }

    #[test]
    fn the_farm_keys_are_off_by_default() {
        let f = Config::default().farm;
        assert!(!f.enabled);
        assert_eq!(f.max_plots, 5);
        let ex = Config::parse(Config::example()).unwrap().farm;
        assert!(!ex.enabled);
        assert_eq!(ex.max_plots, 5);
        assert_eq!((ex.growth_rate, ex.max_trip_m), (1.0, 4_000.0));
        assert!(ex.public_farms && ex.own_land && ex.replant);
        assert_documented(
            "farm",
            &[
                "enabled = false",
                "max_plots = 5",
                "growth_rate = 1.0",
                "public_farms = true",
                "own_land = true",
                "replant = true",
                "max_trip_m = 4000.0",
            ],
        );
        let cfg = Config::parse("[farm]\nenabled = true\nmax_plots = 0\n").unwrap();
        assert!(cfg.farm.enabled);
        assert_eq!(cfg.farm.max_plots, 0);
        for bad in [
            "[farm]\nplots = 3\n",
            "[farm]\nmax_plots = 1001\n",
            "[farm]\nmax_plots = -1\n",
            "[farm]\nenabled = 1\n",
            "[farm]\ngrowth_rate = 0.0\n",
            "[farm]\ngrowth_rate = nan\n",
            "[farm]\nmax_trip_m = 10.0\n",
            "[farm]\nwild_plots = 33\n",
            "[farm]\nwild_outlaw = 1.5\n",
            "[farm]\nsteal = 1\n",
        ] {
            assert!(Config::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_wild_farm_keys_are_off_by_default() {
        let f = Config::default().farm;
        assert!(!f.wild && !f.steal && !f.steal_from_players);
        assert_eq!((f.wild_plots, f.wild_outlaw), (3, 0.6));
        let ex = Config::parse(Config::example()).unwrap().farm;
        assert!(!ex.wild && !ex.steal && !ex.steal_from_players);
        assert_eq!((ex.wild_plots, ex.wild_outlaw), (3, 0.6));
        assert_documented(
            "farm",
            &[
                "wild = false",
                "wild_plots = 3",
                "wild_outlaw = 0.6",
                "steal = false",
                "steal_from_players = false",
            ],
        );
        let cfg =
            Config::parse("[farm]\nwild = true\nwild_plots = 0\nwild_outlaw = 0.0\nsteal = true\n")
                .unwrap();
        assert!(cfg.farm.wild && cfg.farm.steal);
        assert_eq!(cfg.farm.wild_plots, 0);
    }

    #[test]
    fn the_presence_keys_are_off_and_documented() {
        let p = Config::default().presence;
        assert!(!p.enabled);
        assert_eq!((p.min_online, p.max_online), (0, 0));
        assert_eq!((p.max_starts_per_min, p.max_stops_per_min), (20, 3));
        assert_eq!(p.goodbye_chance, 0.1);
        assert!(p.utc_offset_hours.is_none());
        let ex = Config::parse(Config::example()).unwrap().presence;
        assert!(!ex.enabled);
        assert_eq!((ex.min_online, ex.max_online), (0, 0));
        assert_eq!((ex.max_starts_per_min, ex.max_stops_per_min), (20, 3));
        assert_eq!(ex.goodbye_chance, 0.1);
        assert!(
            ex.utc_offset_hours.is_none(),
            "commented out in the example"
        );
        assert_documented(
            "presence",
            &[
                "enabled = false",
                "min_online = 0",
                "max_online = 0",
                "max_starts_per_min = 20",
                "max_stops_per_min = 3",
                "goodbye_chance = 0.1",
                "# utc_offset_hours = 1.0",
            ],
        );
        let cfg = Config::parse(
            "[presence]\nenabled = true\nmin_online = 5\nmax_online = 40\nutc_offset_hours = -5.5\n",
        )
        .unwrap();
        let p = cfg.presence;
        assert!(p.enabled);
        assert_eq!((p.min_online, p.max_online), (5, 40));
        assert_eq!(p.utc_offset_hours, Some(-5.5));
        for bad in [
            "[presence]\nenable = true\n",
            "[presence]\nmin_online = 10\nmax_online = 5\n",
            "[presence]\nmax_online = 5001\n",
            "[presence]\nmax_stops_per_min = 0\n",
            "[presence]\nmax_starts_per_min = 1001\n",
            "[presence]\ngoodbye_chance = 2.0\n",
            "[presence]\nutc_offset_hours = 15.0\n",
            "[presence]\nutc_offset_hours = nan\n",
        ] {
            assert!(Config::parse(bad).is_err(), "{bad}");
        }
        // no upper bound: any floor goes
        assert!(Config::parse("[presence]\nmin_online = 30\n").is_ok());
    }

    #[test]
    fn the_explore_keys_are_on_and_documented() {
        let e = Config::default().explore;
        assert!(e.enabled);
        assert_eq!(
            (e.max_trip_m, e.level_gap, e.trip_gap_mins),
            (6_000.0, 5, 45)
        );
        let ex = Config::parse(Config::example()).unwrap().explore;
        assert!(ex.enabled);
        assert_eq!(
            (ex.max_trip_m, ex.level_gap, ex.trip_gap_mins),
            (6_000.0, 5, 45)
        );
        assert_documented(
            "explore",
            &[
                "enabled = true",
                "max_trip_m = 6000.0",
                "level_gap = 5",
                "trip_gap_mins = 45",
            ],
        );
        for bad in [
            "[explore]\nmax_trip_m = 10.0\n",
            "[explore]\nlevel_gap = 0\n",
            "[explore]\ntrip_gap_mins = 2000\n",
            "[explore]\nzones = 3\n",
        ] {
            assert!(Config::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_gear_keys_have_safe_defaults() {
        let g = Config::default().gear;
        assert!(g.enabled && g.buy && g.mint && g.socket);
        assert!(!g.regrade, "regrading is opt-in");
        assert_eq!(
            (g.max_spend_pct, g.check_secs, g.max_trip_m),
            (30, 20, 3_000.0)
        );
        let ex = Config::parse(Config::example()).unwrap().gear;
        assert!(ex.enabled && ex.buy && ex.mint && ex.socket && !ex.regrade);
        assert_eq!(
            (ex.max_spend_pct, ex.check_secs, ex.max_trip_m),
            (30, 20, 3_000.0)
        );
        assert_documented(
            "gear",
            &[
                "enabled = true",
                "buy = true",
                "mint = true",
                "socket = true",
                "regrade = false",
                "max_spend_pct = 30",
                "check_secs = 20",
                "max_trip_m = 3000.0",
            ],
        );
        let cfg = Config::parse("[gear]\nregrade = true\nmax_spend_pct = 100\n").unwrap();
        assert!(cfg.gear.regrade);
        assert_eq!(cfg.gear.max_spend_pct, 100);
        for bad in [
            "[gear]\nspend = 3\n",
            "[gear]\nmax_spend_pct = 0\n",
            "[gear]\nmax_spend_pct = 101\n",
            "[gear]\nmax_spend_pct = -1\n",
            "[gear]\ncheck_secs = 0\n",
            "[gear]\ncheck_secs = 3601\n",
            "[gear]\nmax_trip_m = 10.0\n",
            "[gear]\nmax_trip_m = nan\n",
            "[gear]\nregrade = 1\n",
        ] {
            assert!(Config::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_personality_keys_default_to_mild_and_low_rates() {
        use pb_llm::persona::Profanity;
        let p = Config::default().personality;
        assert!(p.enabled && p.llm_chatter);
        assert_eq!(p.profanity, Profanity::Mild);
        assert_eq!((p.ambient_per_min, p.channel_lines_per_hour), (2, 4));
        let ex = Config::parse(Config::example()).unwrap().personality;
        assert!(ex.enabled && ex.llm_chatter);
        assert_eq!(ex.profanity, Profanity::Mild);
        assert_eq!((ex.ambient_per_min, ex.channel_lines_per_hour), (2, 4));
        assert_documented(
            "personality",
            &[
                "enabled = true",
                "profanity = \"mild\"",
                "llm_chatter = true",
                "ambient_per_min = 2",
                "channel_lines_per_hour = 4",
            ],
        );
        for (text, want) in [
            ("off", Profanity::Off),
            ("mild", Profanity::Mild),
            ("strong", Profanity::Strong),
        ] {
            let cfg = Config::parse(&format!("[personality]\nprofanity = \"{text}\"\n")).unwrap();
            assert_eq!(cfg.personality.profanity, want);
        }
        let cfg = Config::parse("[personality]\nambient_per_min = 0\nchannel_lines_per_hour = 0\n")
            .unwrap();
        assert_eq!(cfg.personality.ambient_per_min, 0);
        for bad in [
            "[personality]\nprofanity = \"extreme\"\n",
            "[personality]\nprofanity = true\n",
            "[personality]\nprofanity = \"Mild\"\n",
            "[personality]\nambient_per_min = 61\n",
            "[personality]\nchannel_lines_per_hour = 121\n",
            "[personality]\nambient_per_min = -1\n",
            "[personality]\njokes = true\n",
        ] {
            assert!(Config::parse(bad).is_err(), "{bad}");
        }
    }
}
