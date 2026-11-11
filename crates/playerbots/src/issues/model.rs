//! The issue model: what a detector reports and what the runtime attaches to it.
//!
//! A detector builds an [`IssueReport`] with the builder methods (kind, a stable `key`, a title and
//! its judgement) and hands it to [`crate::hub::Hub::report_issue`] (a goal: `Ctx::report_issue`,
//! the bot loop: `Bot::file_issue`), which attach the runtime context of the bot ([`BotRef`], the
//! time, the packet [`TrailEntry`]s and the bot's log) before the report is deduplicated by its
//! [fingerprint](IssueReport::fingerprint).
//!
//! Every text field is bounded and single-line after [`IssueReport::normalize`]; IPv4 addresses
//! (other than 127.x) and credential-looking `key=value` pairs are removed from all of them.

use std::borrow::Cow;
use std::fmt::{self, Display};

use serde::{Deserialize, Serialize};

use super::redact::{one_line, scrub};

/// Longest title, in characters.
pub const MAX_TITLE_CHARS: usize = 160;
/// Longest dedup key, in characters.
pub const MAX_KEY_CHARS: usize = 160;
/// Longest `expected` / `actual` text, in characters.
pub const MAX_TEXT_CHARS: usize = 400;
/// Longest area hint, in characters.
pub const MAX_AREA_CHARS: usize = 160;
/// Most facts of a report.
pub const MAX_FACTS: usize = 12;
/// Longest fact name, in characters.
pub const MAX_FACT_NAME_CHARS: usize = 40;
/// Longest fact value, in characters.
pub const MAX_FACT_VALUE_CHARS: usize = 160;
/// Most trail entries of a report (the newest are kept).
pub const MAX_TRAIL: usize = 48;
/// Most bytes of the triggering packet that are kept.
pub const MAX_PACKET_HEAD: usize = 96;
/// Most bot log lines of a report (the newest are kept).
pub const MAX_BOT_LOG: usize = 12;
/// Most server log lines of a report.
pub const MAX_SERVER_LOG: usize = 30;
/// Longest log line, in characters.
pub const MAX_LOG_LINE_CHARS: usize = 300;

/// What went wrong, as a category. `snake_case` in JSON.
///
/// Two reports are the same issue when kind and [`IssueReport::key`] are equal.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[serde(rename_all = "snake_case")]
pub enum IssueKind {
    /// The connection ended although the bot did not ask for it (and was not kicked).
    UnexpectedDisconnect,
    /// The server kicked the bot (`SCKickedPacket`-style notice).
    Kicked,
    /// The login or lobby refused a bot that should be able to log in.
    LoginFailure,
    /// The server stopped answering for a long time (no packet at all, or no ping answer).
    ServerStall,
    /// A packet the bot could not parse, or whose fields are impossible.
    MalformedPacket,
    /// A request got no reaction although the server always reacts to it.
    NoResponse,
    /// A quest objective was done but the server did not credit it.
    QuestCreditMissing,
    /// The server refused to start or continue a quest the bot may do.
    QuestRefused,
    /// The server refused the turn-in of a finished quest.
    QuestTurnInRefused,
    /// An NPC the world data says is here is not (the spawn did not come).
    NpcMissing,
    /// A doodad the world data says is here is not.
    DoodadMissing,
    /// An NPC or doodad is in view but no walkable way gets the bot within the server's reach
    /// (a house storey or a cave floor the navigation mesh does not connect).
    TargetUnreachable,
    /// A monster that should take damage is immune or never loses health.
    TargetImmune,
    /// The bot's character died but the server never said so.
    DeathWithoutPacket,
    /// The bot logged in dead (or with zero health) and never recovered.
    LoggedInDead,
    /// A skill the bot may use was refused, and the reason is not one the bot caused.
    SkillRefusedUnexpectedly,
    /// A player trade ended with other goods or money than were put up.
    TradeMismatch,
    /// A sale or a quest reward did not pay what the data says.
    PaymentMissing,
    /// Placing or building a house was refused although the bot met its requirements.
    HouseRefused,
    /// A craft was refused although the bot met its requirements.
    CraftRefused,
    /// The server logged a warning or an error while (and about) a bot's action.
    ServerLogDuringAction,
    /// Anything else a detector wants to say.
    Other,
}

impl IssueKind {
    /// Every kind, in declaration order.
    pub const ALL: [IssueKind; 22] = [
        IssueKind::UnexpectedDisconnect,
        IssueKind::Kicked,
        IssueKind::LoginFailure,
        IssueKind::ServerStall,
        IssueKind::MalformedPacket,
        IssueKind::NoResponse,
        IssueKind::QuestCreditMissing,
        IssueKind::QuestRefused,
        IssueKind::QuestTurnInRefused,
        IssueKind::NpcMissing,
        IssueKind::DoodadMissing,
        IssueKind::TargetUnreachable,
        IssueKind::TargetImmune,
        IssueKind::DeathWithoutPacket,
        IssueKind::LoggedInDead,
        IssueKind::SkillRefusedUnexpectedly,
        IssueKind::TradeMismatch,
        IssueKind::PaymentMissing,
        IssueKind::HouseRefused,
        IssueKind::CraftRefused,
        IssueKind::ServerLogDuringAction,
        IssueKind::Other,
    ];

    /// The `snake_case` name used in JSON, in fingerprints and in evidence folder names.
    pub fn as_str(self) -> &'static str {
        match self {
            IssueKind::UnexpectedDisconnect => "unexpected_disconnect",
            IssueKind::Kicked => "kicked",
            IssueKind::LoginFailure => "login_failure",
            IssueKind::ServerStall => "server_stall",
            IssueKind::MalformedPacket => "malformed_packet",
            IssueKind::NoResponse => "no_response",
            IssueKind::QuestCreditMissing => "quest_credit_missing",
            IssueKind::QuestRefused => "quest_refused",
            IssueKind::QuestTurnInRefused => "quest_turn_in_refused",
            IssueKind::NpcMissing => "npc_missing",
            IssueKind::DoodadMissing => "doodad_missing",
            IssueKind::TargetUnreachable => "target_unreachable",
            IssueKind::TargetImmune => "target_immune",
            IssueKind::DeathWithoutPacket => "death_without_packet",
            IssueKind::LoggedInDead => "logged_in_dead",
            IssueKind::SkillRefusedUnexpectedly => "skill_refused_unexpectedly",
            IssueKind::TradeMismatch => "trade_mismatch",
            IssueKind::PaymentMissing => "payment_missing",
            IssueKind::HouseRefused => "house_refused",
            IssueKind::CraftRefused => "craft_refused",
            IssueKind::ServerLogDuringAction => "server_log_during_action",
            IssueKind::Other => "other",
        }
    }

    /// Whether a single high-severity occurrence is enough to make the issue reportable (these
    /// are the symptoms that are almost never a bot-side misunderstanding).
    pub fn always_report(self) -> bool {
        matches!(
            self,
            IssueKind::DeathWithoutPacket
                | IssueKind::MalformedPacket
                | IssueKind::Kicked
                | IssueKind::ServerLogDuringAction
        )
    }
}

impl Display for IssueKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Who most likely caused the problem.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub enum Suspect {
    /// The server code misbehaves.
    Server,
    /// The server's data (compact.sqlite3, spawn files) is wrong or incomplete.
    Data,
    /// The bot misunderstood something: stays local, never goes to the queue by itself.
    Bot,
    /// The detector cannot tell.
    #[default]
    Unknown,
}

impl Suspect {
    /// The `snake_case` name.
    pub fn as_str(self) -> &'static str {
        match self {
            Suspect::Server => "server",
            Suspect::Data => "data",
            Suspect::Bot => "bot",
            Suspect::Unknown => "unknown",
        }
    }

    /// Whether the team should hear about issues with this suspect.
    pub fn is_reportable(self) -> bool {
        !matches!(self, Suspect::Bot)
    }
}

/// How bad the symptom is.
#[derive(
    Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// A nuisance.
    Low,
    /// Something does not work as it should.
    #[default]
    Medium,
    /// The bot is stuck, disconnected or the server is visibly broken.
    High,
}

impl Severity {
    /// The `snake_case` name.
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Low => "low",
            Severity::Medium => "medium",
            Severity::High => "high",
        }
    }
}

/// Direction of a trail entry.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Dir {
    /// Client to server: a packet the bot sent.
    #[serde(rename = "c2s")]
    C2S,
    /// Server to client: a packet the bot received.
    #[serde(rename = "s2c")]
    S2C,
}

impl Dir {
    /// `C2S` / `S2C`.
    pub fn as_str(self) -> &'static str {
        match self {
            Dir::C2S => "C2S",
            Dir::S2C => "S2C",
        }
    }
}

/// One packet of the trail that led to an issue (the newest last).
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct TrailEntry {
    /// Milliseconds between this packet and the moment of the report.
    pub t_ms_before: u32,
    /// Sent or received.
    pub dir: Dir,
    /// Packet level (1 = game, 2 = proxy).
    pub level: u8,
    /// The opcode.
    pub opcode: u16,
    /// The packet's name (`aa_proto::game::sc_name` / `cs_name`).
    pub name: Cow<'static, str>,
}

impl TrailEntry {
    /// An entry with a static packet name (what `sc_name` / `cs_name` return).
    pub fn new(t_ms_before: u32, dir: Dir, level: u8, opcode: u16, name: &'static str) -> Self {
        TrailEntry {
            t_ms_before,
            dir,
            level,
            opcode,
            name: Cow::Borrowed(name),
        }
    }

    /// `-1234 ms  S2C L1 0x0123 SCFooPacket`, the line of `trail.txt`.
    pub fn render(&self) -> String {
        format!(
            "-{:>6} ms  {} L{} 0x{:04x} {}",
            self.t_ms_before,
            self.dir.as_str(),
            self.level,
            self.opcode,
            self.name
        )
    }
}

/// One slot of the [`TrailRing`]: 8 bytes, no name.
#[derive(Debug, Clone, Copy, Default)]
struct RingEntry {
    /// The bot clock in milliseconds, wrapped to 32 bit (49 days).
    t_ms: u32,
    opcode: u16,
    /// Bit 0: `1` = received (S2C); bits 1..: the packet level.
    flags: u8,
}

/// A bounded ring of the last packets of a bot, for [`IssueReport::trail`]. A bot keeps one
/// (`push` for each packet it sends or receives, with the bot clock in milliseconds) and takes a
/// [`TrailRing::snapshot`] when it files a report.
///
/// Compact on purpose (every bot has one): a slot is 8 bytes (time, opcode, direction and level),
/// the packet *names* are looked up in the opcode tables only when a snapshot is taken, and the
/// 48 slots are allocated on the first `push`, so a bot that never connected costs a null pointer.
#[derive(Debug, Default, Clone)]
pub struct TrailRing {
    slots: Option<Box<[RingEntry; MAX_TRAIL]>>,
    /// Index of the oldest entry.
    head: u8,
    len: u8,
}

impl TrailRing {
    /// Packets kept.
    pub const CAPACITY: usize = MAX_TRAIL;

    /// An empty ring.
    pub fn new() -> TrailRing {
        TrailRing::default()
    }

    /// Records a packet seen at `now_ms` (the bot clock).
    pub fn push(&mut self, now_ms: u64, dir: Dir, level: u8, opcode: u16) {
        let slots = self
            .slots
            .get_or_insert_with(|| Box::new([RingEntry::default(); MAX_TRAIL]));
        let entry = RingEntry {
            t_ms: now_ms as u32,
            opcode,
            flags: level << 1 | u8::from(dir == Dir::S2C),
        };
        let cap = Self::CAPACITY;
        if usize::from(self.len) < cap {
            slots[(usize::from(self.head) + usize::from(self.len)) % cap] = entry;
            self.len += 1;
        } else {
            slots[usize::from(self.head)] = entry;
            self.head = ((usize::from(self.head) + 1) % cap) as u8;
        }
    }

    /// The packets, oldest first, with their age at `now_ms` and their names.
    pub fn snapshot(&self, now_ms: u64) -> Vec<TrailEntry> {
        let Some(slots) = self.slots.as_deref() else {
            return Vec::new();
        };
        let now = now_ms as u32;
        (0..usize::from(self.len))
            .map(|i| {
                let e = slots[(usize::from(self.head) + i) % Self::CAPACITY];
                let (dir, level) = (
                    if e.flags & 1 == 1 { Dir::S2C } else { Dir::C2S },
                    e.flags >> 1,
                );
                let name = match dir {
                    Dir::S2C => aa_proto::game::sc_name(level, e.opcode),
                    Dir::C2S => aa_proto::game::cs_name(level, e.opcode),
                };
                TrailEntry::new(now.wrapping_sub(e.t_ms), dir, level, e.opcode, name)
            })
            .collect()
    }

    /// Number of packets held.
    pub fn len(&self) -> usize {
        usize::from(self.len)
    }

    /// Whether the ring is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Forgets everything (a new connection).
    pub fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }
}

/// Who the reporting bot is and what it was doing. Built by the bot side
/// ([`crate::issues::bot_context`]).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
pub struct BotRef {
    /// Bot id.
    pub id: u32,
    /// Character name.
    pub name: String,
    /// Character id (0 when not known).
    pub char_id: u32,
    /// Character level.
    pub level: u8,
    /// Race name.
    pub race: String,
    /// Class (archetype) name.
    pub class: String,
    /// World position.
    pub pos: [f32; 3],
    /// Zone name, empty when unknown.
    pub zone: String,
    /// Short goal name (`quest`, `grind`, ...).
    pub goal: String,
    /// The one-line description of what the bot was doing.
    pub detail: String,
}

impl BotRef {
    /// `bot <name> (level L <race> <class>) doing <goal> (<detail>) at (x, y, z) in <zone>`, the
    /// parts that are unknown left out.
    pub fn describe(&self) -> String {
        let mut s = format!("bot {}", self.name);
        let mut who = Vec::new();
        if self.level > 0 {
            who.push(format!("level {}", self.level));
        }
        for p in [&self.race, &self.class] {
            if !p.is_empty() {
                who.push(p.clone());
            }
        }
        if !who.is_empty() {
            s.push_str(&format!(" ({})", who.join(" ")));
        }
        match (self.goal.is_empty(), self.detail.is_empty()) {
            (false, false) => s.push_str(&format!(" doing {} ({})", self.goal, self.detail)),
            (false, true) => s.push_str(&format!(" doing {}", self.goal)),
            (true, false) => s.push_str(&format!(" doing {}", self.detail)),
            (true, true) => {}
        }
        if self.pos != [0.0; 3] {
            s.push_str(&format!(
                " at ({:.0}, {:.0}, {:.0})",
                self.pos[0], self.pos[1], self.pos[2]
            ));
        }
        if !self.zone.is_empty() {
            s.push_str(&format!(" in {}", self.zone));
        }
        s
    }
}

fn default_severity() -> Severity {
    Severity::Medium
}

/// One observation of a problem, built by a detector and completed by the bot side.
///
/// ```
/// use playerbots::issues::{IssueKind, IssueReport, Severity, Suspect};
///
/// let report = IssueReport::new(IssueKind::QuestCreditMissing, "quest=251 npc=3475", "Quest 251 gives no credit for the kill")
///     .expected("kill credit for 3475 after the monster died")
///     .actual("the quest log did not change within 5 s")
///     .suspect(Suspect::Server)
///     .severity(Severity::Medium)
///     .area("rust/crates/aaemu-game/src/systems/quests/")
///     .fact("quest", 251)
///     .fact("npc", 3475);
/// assert!(report.fingerprint().starts_with("pb:"));
/// ```
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct IssueReport {
    /// What went wrong.
    pub kind: IssueKind,
    /// Stable dedup key within the kind (`quest=251 npc=3475`): the same problem always has the
    /// same key, whichever bot meets it, and nothing that changes from one occurrence to the next
    /// (positions, times, counters) belongs in it.
    pub key: String,
    /// One line that says it all.
    pub title: String,
    /// What the bot expected to happen.
    #[serde(default)]
    pub expected: String,
    /// What happened instead.
    #[serde(default)]
    pub actual: String,
    /// Who is most likely at fault.
    #[serde(default)]
    pub suspect: Suspect,
    /// How bad it is.
    #[serde(default = "default_severity")]
    pub severity: Severity,
    /// Server area hint: a relative path in AAEmu-1.2-Rust (`rust/crates/aaemu-game/src/systems/quests/`).
    /// A hint, not verified.
    #[serde(default)]
    pub area: Option<String>,
    /// A few `(name, value)` details (ids, counts), at most [`MAX_FACTS`].
    #[serde(default)]
    pub facts: Vec<(String, String)>,
    /// The reporting bot (filled by the bot side).
    #[serde(default)]
    pub bot: Option<BotRef>,
    /// Unix time in milliseconds of the observation (filled when 0).
    #[serde(default)]
    pub at_ms: u64,
    /// The packets before the observation, oldest first (at most [`MAX_TRAIL`]).
    #[serde(default)]
    pub trail: Vec<TrailEntry>,
    /// Hex of the first [`MAX_PACKET_HEAD`] bytes of the triggering packet.
    #[serde(default)]
    pub packet_head_hex: Option<String>,
    /// The bot's last log lines (at most [`MAX_BOT_LOG`]).
    #[serde(default)]
    pub bot_log: Vec<String>,
    /// Server log lines around the observation (filled by the issue service).
    #[serde(default)]
    pub server_log: Vec<String>,
}

impl IssueReport {
    /// A report with the three things every detector knows. Suspect is [`Suspect::Unknown`] and
    /// severity [`Severity::Medium`] until set.
    pub fn new(kind: IssueKind, key: impl Into<String>, title: impl Into<String>) -> IssueReport {
        IssueReport {
            kind,
            key: key.into(),
            title: title.into(),
            expected: String::new(),
            actual: String::new(),
            suspect: Suspect::Unknown,
            severity: Severity::Medium,
            area: None,
            facts: Vec::new(),
            bot: None,
            at_ms: 0,
            trail: Vec::new(),
            packet_head_hex: None,
            bot_log: Vec::new(),
            server_log: Vec::new(),
        }
    }

    /// What the bot expected.
    pub fn expected(mut self, text: impl Into<String>) -> Self {
        self.expected = text.into();
        self
    }

    /// What happened instead.
    pub fn actual(mut self, text: impl Into<String>) -> Self {
        self.actual = text.into();
        self
    }

    /// Who is most likely at fault.
    pub fn suspect(mut self, suspect: Suspect) -> Self {
        self.suspect = suspect;
        self
    }

    /// How bad it is.
    pub fn severity(mut self, severity: Severity) -> Self {
        self.severity = severity;
        self
    }

    /// The server area hint (a relative path in AAEmu-1.2-Rust).
    pub fn area(mut self, area: impl Into<String>) -> Self {
        self.area = Some(area.into());
        self
    }

    /// Adds a detail; more than [`MAX_FACTS`] are dropped.
    pub fn fact(mut self, name: impl Into<String>, value: impl Display) -> Self {
        if self.facts.len() < MAX_FACTS {
            self.facts.push((name.into(), value.to_string()));
        }
        self
    }

    /// Sets the observation time (unix milliseconds); the default is "now" at the hub.
    pub fn at(mut self, unix_ms: u64) -> Self {
        self.at_ms = unix_ms;
        self
    }

    /// Sets the reporting bot.
    pub fn bot(mut self, bot: BotRef) -> Self {
        self.bot = Some(bot);
        self
    }

    /// Sets the packet trail (oldest first); only the newest [`MAX_TRAIL`] are kept.
    pub fn trail(mut self, trail: Vec<TrailEntry>) -> Self {
        self.trail = trail;
        self
    }

    /// Keeps the first [`MAX_PACKET_HEAD`] bytes of the triggering packet, as hex.
    pub fn packet_head(mut self, bytes: &[u8]) -> Self {
        self.packet_head_hex = Some(hex(&bytes[..bytes.len().min(MAX_PACKET_HEAD)]));
        self
    }

    /// Sets the bot's last log lines; only the newest [`MAX_BOT_LOG`] are kept.
    pub fn bot_log(mut self, lines: Vec<String>) -> Self {
        self.bot_log = lines;
        self
    }

    /// The fingerprint of this issue (`pb:` and 8 hex digits).
    pub fn fingerprint(&self) -> String {
        fingerprint(self.kind, &self.key)
    }

    /// Bounds every field, makes the texts single-line and removes IPv4 addresses (other than
    /// 127.x) and credential-looking pairs. `now_ms` fills a missing time. Idempotent.
    pub fn normalize(&mut self, now_ms: u64) {
        self.key = normalize_key(&self.key);
        self.title = one_line(&self.title, MAX_TITLE_CHARS);
        self.expected = one_line(&self.expected, MAX_TEXT_CHARS);
        self.actual = one_line(&self.actual, MAX_TEXT_CHARS);
        self.area = self
            .area
            .take()
            .map(|a| one_line(&a, MAX_AREA_CHARS))
            .filter(|a| !a.is_empty());
        self.facts.truncate(MAX_FACTS);
        for (name, value) in &mut self.facts {
            *name = one_line(name, MAX_FACT_NAME_CHARS);
            *value = one_line(value, MAX_FACT_VALUE_CHARS);
        }
        if let Some(b) = &mut self.bot {
            b.name = one_line(&b.name, 40);
            b.race = one_line(&b.race, 24);
            b.class = one_line(&b.class, 40);
            b.zone = one_line(&b.zone, 80);
            b.goal = one_line(&b.goal, 24);
            b.detail = one_line(&b.detail, MAX_LOG_LINE_CHARS);
            for c in &mut b.pos {
                if !c.is_finite() {
                    *c = 0.0;
                }
            }
        }
        if self.trail.len() > MAX_TRAIL {
            let drop = self.trail.len() - MAX_TRAIL;
            self.trail.drain(..drop);
        }
        for t in &mut self.trail {
            if t.name.chars().count() > 80 {
                t.name = Cow::Owned(one_line(&t.name, 80));
            }
        }
        if let Some(h) = &mut self.packet_head_hex {
            h.retain(|c| c.is_ascii_hexdigit());
            h.truncate(MAX_PACKET_HEAD * 2);
        }
        trim_lines(&mut self.bot_log, MAX_BOT_LOG);
        trim_lines(&mut self.server_log, MAX_SERVER_LOG);
        if self.at_ms == 0 {
            self.at_ms = now_ms;
        }
    }
}

/// Keeps the newest `max` lines of `lines`, each single-line, scrubbed and bounded.
fn trim_lines(lines: &mut Vec<String>, max: usize) {
    if lines.len() > max {
        let drop = lines.len() - max;
        lines.drain(..drop);
    }
    for l in lines.iter_mut() {
        *l = one_line(l, MAX_LOG_LINE_CHARS);
    }
}

/// Lower-case hex of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[usize::from(b >> 4)] as char);
        s.push(DIGITS[usize::from(b & 15)] as char);
    }
    s
}

/// Trims a key, collapses its blank runs to one space and bounds it, so equal problems get equal
/// keys however a detector spaced them.
pub fn normalize_key(key: &str) -> String {
    let collapsed: Vec<&str> = key.split_whitespace().collect();
    let joined = scrub(&collapsed.join(" "));
    joined.chars().take(MAX_KEY_CHARS).collect()
}

/// The fingerprint of an issue: `pb:` and 8 hex digits of the FNV-1a hash (64 bit, folded to 32)
/// of the kind's name, a zero byte and the normalized key. Stable across versions and machines;
/// it is the marker in the team's issue queue.
pub fn fingerprint(kind: IssueKind, key: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let key = normalize_key(key);
    for &b in kind
        .as_str()
        .as_bytes()
        .iter()
        .chain(std::iter::once(&0u8))
        .chain(key.as_bytes())
    {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("pb:{:08x}", (h ^ (h >> 32)) as u32)
}

/// `pb:abcd1234` from `pb:abcd1234`, `abcd1234` or `PB:ABCD1234`; `None` if it is none of those.
pub fn parse_fingerprint(text: &str) -> Option<String> {
    let t = text.trim();
    let hexpart = t
        .strip_prefix("pb:")
        .or_else(|| t.strip_prefix("PB:"))
        .unwrap_or(t);
    (hexpart.len() == 8 && hexpart.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| format!("pb:{}", hexpart.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_are_stable_and_distinguish_kind_and_key() {
        // (golden values: a changed hash would orphan every marker in the team's queue)
        assert_eq!(
            fingerprint(IssueKind::QuestCreditMissing, "quest=251 npc=3475"),
            "pb:6e5fe818"
        );
        let a = fingerprint(IssueKind::QuestCreditMissing, "quest=251 npc=3475");
        assert_eq!(
            a,
            fingerprint(IssueKind::QuestCreditMissing, "quest=251 npc=3475")
        );
        assert_ne!(
            a,
            fingerprint(IssueKind::QuestCreditMissing, "quest=251 npc=3476")
        );
        assert_ne!(
            a,
            fingerprint(IssueKind::QuestRefused, "quest=251 npc=3475")
        );
        // blanks around and inside the key do not matter
        assert_eq!(
            a,
            fingerprint(IssueKind::QuestCreditMissing, "  quest=251   npc=3475 ")
        );
        assert!(a.starts_with("pb:") && a.len() == 11);
        assert!(a[3..].bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn fingerprints_parse_in_every_spelling() {
        assert_eq!(
            parse_fingerprint("pb:6f3b2e1c").as_deref(),
            Some("pb:6f3b2e1c")
        );
        assert_eq!(
            parse_fingerprint(" 6F3B2E1C ").as_deref(),
            Some("pb:6f3b2e1c")
        );
        assert_eq!(
            parse_fingerprint("PB:6f3b2e1c").as_deref(),
            Some("pb:6f3b2e1c")
        );
        for bad in [
            "",
            "pb:",
            "pb:1234567",
            "pb:123456789",
            "pb:6f3b2e1g",
            "x:6f3b2e1c",
        ] {
            assert_eq!(parse_fingerprint(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn kind_names_match_serde_and_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for k in IssueKind::ALL {
            assert_eq!(
                serde_json::to_string(&k).unwrap(),
                format!("\"{}\"", k.as_str())
            );
            let back: IssueKind = serde_json::from_str(&format!("\"{}\"", k.as_str())).unwrap();
            assert_eq!(back, k);
            assert!(seen.insert(k.as_str()), "{k} listed twice");
        }
        assert_eq!(seen.len(), 22);
        assert_eq!(serde_json::to_string(&Suspect::Data).unwrap(), "\"data\"");
        assert_eq!(serde_json::to_string(&Severity::High).unwrap(), "\"high\"");
        assert_eq!(serde_json::to_string(&Dir::C2S).unwrap(), "\"c2s\"");
    }

    #[test]
    fn always_report_kinds_are_the_symptoms_that_are_rarely_the_bots_fault() {
        let always: Vec<_> = IssueKind::ALL
            .into_iter()
            .filter(|k| k.always_report())
            .collect();
        assert_eq!(
            always,
            vec![
                IssueKind::Kicked,
                IssueKind::MalformedPacket,
                IssueKind::DeathWithoutPacket,
                IssueKind::ServerLogDuringAction
            ]
        );
        assert!(Suspect::Server.is_reportable() && Suspect::Data.is_reportable());
        assert!(Suspect::Unknown.is_reportable() && !Suspect::Bot.is_reportable());
        assert!(Severity::High > Severity::Medium && Severity::Medium > Severity::Low);
    }

    #[test]
    fn the_builder_fills_the_fields() {
        let r = IssueReport::new(IssueKind::NpcMissing, "npc=77", "NPC 77 is not there")
            .expected("npc 77 at the spawn")
            .actual("nothing in view after 10 s")
            .suspect(Suspect::Data)
            .severity(Severity::Low)
            .area("rust/crates/aaemu-game/src/systems/npcs/")
            .fact("npc", 77)
            .fact("seen", "no")
            .at(1234)
            .packet_head(&[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(r.kind, IssueKind::NpcMissing);
        assert_eq!(r.suspect, Suspect::Data);
        assert_eq!(r.severity, Severity::Low);
        assert_eq!(
            r.facts,
            vec![("npc".into(), "77".into()), ("seen".into(), "no".into())]
        );
        assert_eq!(r.packet_head_hex.as_deref(), Some("deadbeef"));
        assert_eq!(r.at_ms, 1234);
        // the defaults
        let d = IssueReport::new(IssueKind::Other, "k", "t");
        assert_eq!(
            (d.suspect, d.severity),
            (Suspect::Unknown, Severity::Medium)
        );
        // more facts than the cap are dropped
        let mut many = IssueReport::new(IssueKind::Other, "k", "t");
        for i in 0..40 {
            many = many.fact(format!("f{i}"), i);
        }
        assert_eq!(many.facts.len(), MAX_FACTS);
    }

    #[test]
    fn normalizing_bounds_cleans_and_fills_the_time() {
        let mut r = IssueReport::new(
            IssueKind::Other,
            "a\n b\t c",
            format!("title\r\nwith  breaks {}", "x".repeat(500)),
        )
        .actual("connected to 203.0.113.9:1237 and 127.0.0.1:1237 password=hunter2 ok")
        .fact("ip", "10.1.2.3")
        .packet_head(&[7u8; 300])
        .trail(
            (0..100)
                .map(|i| TrailEntry::new(i, Dir::S2C, 1, 1, "SCx"))
                .collect(),
        )
        .bot_log((0..50).map(|i| format!("line {i}")).collect());
        r.server_log = (0..80).map(|i| format!("log {i} 8.8.8.8")).collect();
        r.normalize(5_000);
        assert_eq!(r.key, "a b c");
        assert!(!r.title.contains(['\r', '\n']) && r.title.chars().count() <= MAX_TITLE_CHARS);
        assert!(r.actual.contains("x.x.x.x:1237"), "{}", r.actual);
        assert!(r.actual.contains("127.0.0.1:1237"), "{}", r.actual);
        assert!(!r.actual.contains("hunter2"), "{}", r.actual);
        assert_eq!(r.facts[0].1, "x.x.x.x");
        assert_eq!(
            r.packet_head_hex.as_ref().unwrap().len(),
            MAX_PACKET_HEAD * 2
        );
        assert_eq!(r.trail.len(), MAX_TRAIL);
        assert_eq!(
            r.trail.last().unwrap().t_ms_before,
            99,
            "the newest are kept"
        );
        assert_eq!(r.bot_log.len(), MAX_BOT_LOG);
        assert_eq!(r.bot_log.last().unwrap(), "line 49");
        assert_eq!(r.server_log.len(), MAX_SERVER_LOG);
        assert!(r.server_log[0].contains("x.x.x.x"));
        assert_eq!(r.at_ms, 5_000);
        // idempotent
        let again = {
            let mut c = r.clone();
            c.normalize(9_999);
            c
        };
        assert_eq!(again, r);
    }

    #[test]
    fn a_report_survives_json_and_reads_old_json_without_the_newer_fields() {
        let r = IssueReport::new(IssueKind::Kicked, "reason=3", "Kicked")
            .suspect(Suspect::Server)
            .bot(BotRef {
                id: 4,
                name: "Aria".into(),
                level: 12,
                ..BotRef::default()
            })
            .trail(vec![TrailEntry::new(
                5,
                Dir::C2S,
                1,
                0x2c,
                "CSChangeTargetPacket",
            )]);
        let json = serde_json::to_string(&r).unwrap();
        let back: IssueReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
        let minimal: IssueReport =
            serde_json::from_str(r#"{"kind":"other","key":"k","title":"t"}"#).unwrap();
        assert_eq!(minimal.severity, Severity::Medium);
        assert_eq!(minimal.suspect, Suspect::Unknown);
        assert!(minimal.trail.is_empty() && minimal.bot.is_none());
    }

    #[test]
    fn the_trail_ring_keeps_the_newest_and_reports_ages() {
        let mut ring = TrailRing::new();
        assert!(ring.is_empty());
        assert!(ring.snapshot(5).is_empty());
        for i in 0..100u64 {
            ring.push(1_000 + i * 10, Dir::S2C, 1, i as u16);
        }
        assert_eq!(ring.len(), TrailRing::CAPACITY);
        let snap = ring.snapshot(2_000);
        assert_eq!(snap.len(), TrailRing::CAPACITY);
        assert_eq!(snap.last().unwrap().opcode, 99);
        assert_eq!(snap.last().unwrap().t_ms_before, 2_000 - (1_000 + 990));
        assert_eq!(snap[0].opcode, 100 - TrailRing::CAPACITY as u16);
        assert!(snap
            .windows(2)
            .all(|w| w[0].t_ms_before >= w[1].t_ms_before));
        // a clock that ran backwards never underflows to a huge age that matters: it wraps
        assert!(ring.snapshot(0).last().unwrap().t_ms_before > 1_000_000);
        ring.clear();
        assert!(ring.snapshot(5).is_empty() && ring.is_empty());
        // after a clear the ring fills from the start again
        ring.push(10, Dir::C2S, 1, 7);
        assert_eq!(ring.snapshot(15).len(), 1);
    }

    #[test]
    fn the_trail_ring_is_compact_and_resolves_names_from_the_opcode_tables() {
        use aa_proto::opcodes::{cs, pp, sc};
        // 8 bytes a slot, one pointer when empty
        assert_eq!(std::mem::size_of::<RingEntry>(), 8);
        assert!(std::mem::size_of::<TrailRing>() <= 16);
        let mut ring = TrailRing::new();
        ring.push(100, Dir::C2S, 1, cs::CSStartSkillPacket);
        ring.push(120, Dir::S2C, 1, sc::SCSkillStartedPacket);
        ring.push(130, Dir::C2S, 2, pp::PingPacket);
        ring.push(140, Dir::S2C, 2, pp::PongPacket);
        ring.push(150, Dir::S2C, 1, 0xfff0);
        let snap = ring.snapshot(200);
        let names: Vec<_> = snap.iter().map(|e| (e.dir, e.level, &*e.name)).collect();
        assert_eq!(
            names,
            vec![
                (Dir::C2S, 1, "CSStartSkillPacket"),
                (Dir::S2C, 1, "SCSkillStartedPacket"),
                (Dir::C2S, 2, "PingPacket"),
                (Dir::S2C, 2, "PongPacket"),
                (Dir::S2C, 1, "Unknown"),
            ]
        );
        assert_eq!(snap[0].t_ms_before, 100);
    }

    #[test]
    fn the_trail_line_is_readable() {
        let e = TrailEntry::new(1234, Dir::S2C, 1, 0x123, "SCFooPacket");
        assert_eq!(e.render(), "-  1234 ms  S2C L1 0x0123 SCFooPacket");
    }

    #[test]
    fn a_bot_is_described_with_what_is_known() {
        let full = BotRef {
            id: 3,
            name: "Aria".into(),
            char_id: 9,
            level: 12,
            race: "Elf".into(),
            class: "Archery".into(),
            pos: [15000.4, 14800.6, 120.0],
            zone: "Solzreed".into(),
            goal: "quest".into(),
            detail: "Quest 251 Wild Boars: kill 3".into(),
        };
        assert_eq!(
            full.describe(),
            "bot Aria (level 12 Elf Archery) doing quest (Quest 251 Wild Boars: kill 3) at (15000, 14801, 120) in Solzreed"
        );
        let bare = BotRef {
            name: "Aria".into(),
            ..BotRef::default()
        };
        assert_eq!(bare.describe(), "bot Aria");
    }
}
