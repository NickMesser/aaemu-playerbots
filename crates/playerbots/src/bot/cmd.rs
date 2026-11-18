//! Commands a bot task receives on its command channel (from the web UI through the hub, and
//! from the coordinator).

use super::party::PartyOrder;
use crate::issues::LogLevel;

/// The goals a bot can pursue, as the web UI names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GoalKind {
    /// Walk around.
    Wander,
    /// Stand still and recover.
    Rest,
    /// Kill monsters.
    Grind,
    /// Do quests.
    Quest,
    /// Duel another bot.
    Duel,
    /// Trade with another bot.
    Trade,
    /// Craft, carry and sell trade packs.
    TradeRun,
    /// Build and use a house.
    Housing,
    /// Enter an instance dungeon with a party (owned by the dungeons feature).
    Dungeon,
    /// Summon a boat and sail (owned by the sailing feature).
    Sail,
    /// List goods at the auction house (owned by the auction feature).
    Auction,
    /// Advertise goods in trade chat and sell them (owned by the market feature).
    Market,
    /// Found, join and run an expedition (owned by the guild feature).
    Guild,
    /// Ambush trade-pack carriers (owned by the pvp feature).
    Gank,
    /// Fight in zones at war (owned by the pvp feature).
    Warzone,
    /// Join a world event such as a rift (owned by the events feature).
    Event,
    /// Follow a real player's party, e.g. into a dungeon (owned by the party feature).
    Companion,
    /// Plant and harvest crops (owned by the farming feature).
    Farm,
    /// See new places (owned by the presence and exploring feature).
    Explore,
    /// Get, equip, socket and upgrade gear for the bot's level (owned by the gear feature).
    Gear,
    /// Carry out a guild production order.
    Work,
    /// Catch and sell a sport fish.
    Fish,
}

/// A set of goals, one bit per [`GoalKind::index`] (the brain's `disabled`, `blocked` and gate
/// masks).
pub type GoalMask = u32;

// (a goal past the mask's width would shift out of it silently)
const _: () = assert!(GoalKind::COUNT <= GoalMask::BITS as usize);

impl GoalKind {
    /// Every goal.
    pub const ALL: [GoalKind; 22] = [
        GoalKind::Wander,
        GoalKind::Rest,
        GoalKind::Grind,
        GoalKind::Quest,
        GoalKind::Duel,
        GoalKind::Trade,
        GoalKind::TradeRun,
        GoalKind::Housing,
        GoalKind::Dungeon,
        GoalKind::Sail,
        GoalKind::Auction,
        GoalKind::Market,
        GoalKind::Guild,
        GoalKind::Gank,
        GoalKind::Warzone,
        GoalKind::Event,
        GoalKind::Companion,
        GoalKind::Farm,
        GoalKind::Explore,
        GoalKind::Gear,
        GoalKind::Work,
        GoalKind::Fish,
    ];

    /// Number of goals.
    pub const COUNT: usize = Self::ALL.len();

    /// Dense index for per-goal arrays.
    pub fn index(self) -> usize {
        self as usize
    }

    /// The short name the web UI shows (`pb_web::Command::GOALS` spelling).
    pub fn name(self) -> &'static str {
        match self {
            GoalKind::Wander => "wander",
            GoalKind::Rest => "rest",
            GoalKind::Grind => "grind",
            GoalKind::Quest => "quest",
            GoalKind::Duel => "duel",
            GoalKind::Trade => "trade",
            GoalKind::TradeRun => "trade_run",
            GoalKind::Housing => "housing",
            GoalKind::Dungeon => "dungeon",
            GoalKind::Sail => "sail",
            GoalKind::Auction => "auction",
            GoalKind::Market => "market",
            GoalKind::Guild => "guild",
            GoalKind::Gank => "gank",
            GoalKind::Warzone => "warzone",
            GoalKind::Event => "event",
            GoalKind::Companion => "companion",
            GoalKind::Farm => "farm",
            GoalKind::Explore => "explore",
            GoalKind::Gear => "gear",
            GoalKind::Work => "work",
            GoalKind::Fish => "fish",
        }
    }

    /// The goal called `name` (a [`pb_web::Command::GOALS`] entry other than `auto`, or `wander`).
    pub fn parse(name: &str) -> Option<GoalKind> {
        GoalKind::ALL.into_iter().find(|g| g.name() == name)
    }
}

/// What a goal is started with when somebody other than the bot's own scoring picks it (the LLM
/// director, the web UI's `act` command, the agent API). Every field is optional and a goal reads
/// only the ones it understands; a goal started by the brain's own choice gets none. Boxed in
/// [`BotCmd::GoalWith`] and in the goals that keep it, so it costs nothing while unused.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GoalArgs {
    /// A point to go to or work at (x, y): the wander goal walks there, the explore goal heads
    /// for it.
    pub dest: Option<[f32; 2]>,
    /// A zone (zone key, `World::zone_key_at`) to head for: the explore goal, the quest and
    /// grind goals where they support it.
    pub zone: Option<u16>,
    /// An item template the goal is about: the seed of a crop, a piece of gear, goods to sell.
    pub item: Option<u32>,
    /// A character the goal is about: a duel partner, a player to follow, a buyer.
    pub target: Option<Box<str>>,
    /// The farm goal plants on unprotected land (a wild, "illegal" farm) instead of a garden or a
    /// public farm.
    pub wild: bool,
    /// Who asked and why, for the bot's log ("director: needs gold", "agent: ...").
    pub why: Option<Box<str>>,
}

/// What the operator chose with `Command::Goal`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalChoice {
    /// The bot chooses (utility scoring).
    Auto,
    /// Keep doing this goal.
    Forced(GoalKind),
}

impl GoalChoice {
    /// Parses a `Command::Goal` value (`auto` or a goal name).
    pub fn parse(name: &str) -> Option<GoalChoice> {
        if name == "auto" {
            Some(GoalChoice::Auto)
        } else {
            GoalKind::parse(name).map(GoalChoice::Forced)
        }
    }
}

/// What a coordinator-paired bot is asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairKind {
    /// A duel.
    Duel,
    /// A player trade.
    Trade,
}

/// Which side of a pairing a bot is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairRole {
    /// Sends the challenge / trade request.
    Initiator,
    /// Waits for it and accepts.
    Responder,
}

/// The coordinator's order to both bots of a pair.
#[derive(Debug, Clone, PartialEq)]
pub struct PairOrder {
    /// What the pair does.
    pub kind: PairKind,
    /// This bot's side.
    pub role: PairRole,
    /// Bot id of the partner.
    pub partner_bot: u32,
    /// Character id of the partner (0 when not known yet).
    pub partner_char: u32,
    /// Character name of the partner.
    pub partner_name: Box<str>,
    /// Where the two meet (x, y).
    pub meet_at: (f32, f32),
}

/// A message to one bot task.
#[derive(Debug, Clone, PartialEq)]
pub enum BotCmd {
    /// `Command::Goal`.
    Goal(GoalChoice),
    /// Start `kind` once, with `args` (the LLM director, the web UI's `act`, the agent API). It
    /// replaces the running goal at the next tick; afterwards the bot chooses again as before
    /// (an operator's forced goal comes back). Boxed so that the command stays small.
    GoalWith {
        /// The goal.
        kind: GoalKind,
        /// Its arguments.
        args: Box<GoalArgs>,
    },
    /// `Command::Pause`: stop acting (keep the connection alive).
    Pause,
    /// `Command::Resume`.
    Resume,
    /// `Command::Say`: a line of local chat.
    Say(String),
    /// `Command::Whisper`: a private line to the named character.
    Whisper {
        /// Character name.
        to: String,
        /// The line.
        text: String,
    },
    /// `Command::Profile`: change the bot's personality (and its roster entry). Boxed so that the
    /// command stays small.
    Profile(Box<crate::roster::ProfilePatch>),
    /// `Command::DuelWith`: duel the bot with this id.
    DuelWith(u32),
    /// `Command::TradeWith`: trade with the bot with this id.
    TradeWith(u32),
    /// The coordinator found a partner.
    Pair(PairOrder),
    /// The coordinator formed a party and this bot is a member (owned by the dungeons feature;
    /// the brain queues [`GoalKind::Dungeon`] and hands the order to the goal). Boxed: an order is
    /// 48 bytes and would make every command (and every command channel slot) 8 bytes larger.
    Party(Box<PartyOrder>),
    /// A path search finished (owned by the motion feature): the answer to the request with
    /// this `ticket`, delivered to [`Motion::on_path_ready`](super::motion::Motion::on_path_ready).
    PathReady {
        /// The ticket the request was made with.
        ticket: u32,
        /// What the nav search found.
        result: Box<aa_world::nav::PathResult>,
    },
    /// A `WARN` or `ERROR` line of the game server's log mentions this bot's character (sent by
    /// the hub when `issues.server_log` is set). The bot files a
    /// [`crate::issues::IssueKind::ServerLogDuringAction`] report with its own context.
    ServerLogLine {
        /// The level of the line (`Warn` or `Error`).
        level: LogLevel,
        /// The line, without colour codes.
        line: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_names_round_trip_and_match_the_web_ui() {
        for g in GoalKind::ALL {
            assert_eq!(GoalKind::parse(g.name()), Some(g));
            assert_eq!(GoalKind::ALL[g.index()], g);
        }
        // every goal the web UI can force (except "auto") is a goal here
        for name in pb_web::Command::GOALS {
            match GoalChoice::parse(name) {
                Some(GoalChoice::Auto) => assert_eq!(name, "auto"),
                Some(GoalChoice::Forced(g)) => assert_eq!(g.name(), name),
                None => panic!("the web UI offers {name:?}, which the bots do not know"),
            }
        }
        assert_eq!(GoalChoice::parse("fly"), None);
    }

    #[test]
    fn auction_and_market_are_goals_the_web_ui_can_force() {
        for (name, kind) in [("auction", GoalKind::Auction), ("market", GoalKind::Market)] {
            assert_eq!(GoalKind::parse(name), Some(kind));
            assert_eq!(kind.name(), name);
            assert_eq!(GoalChoice::parse(name), Some(GoalChoice::Forced(kind)));
            assert!(pb_web::Command::GOALS.contains(&name));
        }
        assert_eq!(GoalKind::ALL.len(), GoalKind::COUNT);
    }

    #[test]
    fn the_wave_one_goals_are_goals_the_web_ui_can_force() {
        for (name, kind) in [
            ("guild", GoalKind::Guild),
            ("gank", GoalKind::Gank),
            ("warzone", GoalKind::Warzone),
            ("event", GoalKind::Event),
            ("companion", GoalKind::Companion),
            ("farm", GoalKind::Farm),
            ("explore", GoalKind::Explore),
            ("gear", GoalKind::Gear),
        ] {
            assert_eq!(GoalKind::parse(name), Some(kind));
            assert_eq!(kind.name(), name);
            assert_eq!(GoalChoice::parse(name), Some(GoalChoice::Forced(kind)));
            assert!(pb_web::Command::GOALS.contains(&name));
        }
        // the goal names are distinct and every goal has a bit in the mask
        let mut mask: GoalMask = 0;
        for g in GoalKind::ALL {
            let bit: GoalMask = 1 << g.index();
            assert_eq!(mask & bit, 0, "{g:?} shares a bit");
            mask |= bit;
        }
        assert_eq!(mask.count_ones() as usize, GoalKind::COUNT);
    }

    #[test]
    fn dungeon_and_sail_are_goals_the_web_ui_can_force() {
        assert_eq!(GoalKind::parse("dungeon"), Some(GoalKind::Dungeon));
        assert_eq!(GoalKind::parse("sail"), Some(GoalKind::Sail));
        assert_eq!(GoalKind::Dungeon.name(), "dungeon");
        assert_eq!(GoalKind::Sail.name(), "sail");
        assert_eq!(
            GoalChoice::parse("sail"),
            Some(GoalChoice::Forced(GoalKind::Sail))
        );
        // and the other way round: every goal here is on the web UI's list
        for g in GoalKind::ALL {
            assert!(
                pb_web::Command::GOALS.contains(&g.name()),
                "the web UI does not offer {g:?}"
            );
        }
    }
}
