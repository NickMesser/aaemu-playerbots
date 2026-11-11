//! Parties: the coordinator's orders, what a bot knows of its team, and the planner that forms
//! parties for instance dungeons (owned by the dungeons feature).
//!
//! * A [`PartyOrder`] reaches each member as [`BotCmd::Party`](super::cmd::BotCmd::Party); the
//!   brain queues the goal the [`PartyPurpose`] names (today only
//!   [`GoalKind::Dungeon`](super::cmd::GoalKind)) and hands the order to it, where the goal reads
//!   it through [`Goal::with_party`](super::goals::Goal::with_party). Like a
//!   [`PairOrder`](super::cmd::PairOrder), an order is consumed by the goal it starts: a goal
//!   started any other way gets none.
//! * [`TeamLog`] follows the server's team packets (invitation, joined, member joined / left,
//!   left, dismissed, owner changed) and what the members target, for the dungeon goal and the
//!   combat controller's assist. It lives in [`PartyMem`] next to the dungeon memory (the last
//!   run, the entries the server counts, the dungeons the server refused), one box per bot that
//!   ever met a team.
//! * [`Wish`], [`Site`] and [`plan`] are the coordinator's side: the bots that want a dungeon,
//!   the dungeons it may send them to and the pure choice of who goes where.
//!
//! Everything here is plain data and pure functions; the `CSInviteToTeam` flow, who enters when
//! and what happens inside is the dungeon goal's business.

use aa_proto::sc_activity::{ActivityEvent, TeamMemberBrief};
use aa_world::travel::Dungeon;
use aa_world::World;
use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use super::state::{BotState, Unit, UnitKind};

mod remote;

pub use remote::{LootRule, Remote};

/// The most members of one party (the server's team size for dungeons is 5).
pub const MAX_PARTY: usize = 5;
/// The smallest party that runs a dungeon.
pub const MIN_PARTY: usize = 2;
/// The smallest party the planner fills before it waits for the rest of a five-player dungeon: a
/// tank, a healer and two damage dealers. A party of fewer bots is formed only after the long
/// relax wait, never 120 s into the rally (the live run's two-bot parties).
pub const RALLY_MIN: usize = 4;
/// Seconds the bots of a rally wait for one more asker before a [`RALLY_MIN`] party starts.
pub const RALLY_WINDOW_SECS: u64 = 90;
/// Default seconds a wish waits for a full party before a smaller one is formed
/// (`[dungeons] relax_after_secs`). Long on purpose: the live runs formed two-bot parties after
/// 120 s, before the third and fourth asker had picked the goal.
pub const RELAX_AFTER_SECS: u64 = 600;
/// Bots of one party start at most this far from each other (metres) ...
pub const PARTY_NEAR_M: f32 = 800.0;
/// ... plus this much for every second the longest-waiting member has waited.
pub const PARTY_NEAR_GROWTH_M: f32 = 60.0;
/// Without GM travel a party only goes to a dungeon this near (metres) ...
pub const WALK_NEAR_M: f32 = 2_500.0;
/// ... plus this much for every second of waiting.
pub const WALK_GROWTH_M: f32 = 100.0;
/// A level step weighs this many metres in the choice of the party.
const LEVEL_WEIGHT_M: f32 = 20.0;
/// The server counts at most this many entries per zone group and window.
pub const ENTRY_WINDOW_MS: u64 = 4 * 3600 * 1000;

/// What a party is formed for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartyPurpose {
    /// Clear an instance dungeon.
    Dungeon {
        /// The `indun_zones` zone group of the dungeon (what `aa_world::World::dungeons()`
        /// orders by).
        zone_group: u32,
    },
    /// Sign the charter of a new guild: the leader founds it, the others sign (the guild feature,
    /// [`super::guild`]).
    Guild,
    /// Lie in wait together for trade pack carriers (the gank goal, see
    /// [`pvp::squad`](super::pvp::squad)).
    Ambush {
        /// The leader's post, whole metres (x).
        x: i32,
        /// The leader's post, whole metres (y).
        y: i32,
    },
}

/// The coordinator's order to one member of a party.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartyOrder {
    /// Coordinator-assigned id of this group (unique per process; not the server's team id).
    pub party_id: u32,
    /// Bot id of the leader (one of `members`).
    pub leader: u32,
    /// Bot ids of every member, the leader included.
    pub members: SmallVec<[u32; MAX_PARTY]>,
    /// The role of each member, aligned with `members` (empty for a party formed for something
    /// other than a dungeon).
    pub roles: SmallVec<[PartyRole; MAX_PARTY]>,
    /// What the party is for.
    pub purpose: PartyPurpose,
}

impl PartyOrder {
    /// Whether bot `bot` leads the party.
    pub fn is_leader(&self, bot: u32) -> bool {
        self.leader == bot
    }

    /// Whether bot `bot` is a member.
    pub fn has_member(&self, bot: u32) -> bool {
        self.members.contains(&bot)
    }

    /// The role of bot `bot` (`Dps` when the order names none).
    pub fn role_of(&self, bot: u32) -> PartyRole {
        self.members
            .iter()
            .position(|&m| m == bot)
            .and_then(|i| self.roles.get(i).copied())
            .unwrap_or(PartyRole::Dps)
    }

    /// The zone group of the dungeon the party is for.
    pub fn zone_group(&self) -> u32 {
        match self.purpose {
            PartyPurpose::Dungeon { zone_group } => zone_group,
            // (a charter has no dungeon)
            PartyPurpose::Guild => 0,
            PartyPurpose::Ambush { .. } => 0,
        }
    }
}

// ---- the team, as the bot sees it ----------------------------------------------------------

/// A member of the bot's team.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Member {
    /// Character id.
    pub char_id: u32,
    /// Level when the server last said.
    pub level: u8,
    /// Object id the server gave (0 when offline or not refreshed).
    pub obj_id: u32,
}

/// An invitation to join a team that has not been answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invite {
    /// The team the inviter has (0 when none exists yet).
    pub team_id: u32,
    /// Character id of the inviter.
    pub from_char: u32,
    /// Name of the inviter.
    pub from_name: Box<str>,
    /// Party (`true`) or raid.
    pub is_party: bool,
    /// Bot clock when it arrived.
    pub at_ms: u64,
}

/// How the bot last left a team it was in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Left {
    /// The team.
    pub team_id: u32,
    /// The server kicked the bot.
    pub kicked: bool,
    /// The team was dismissed.
    pub dismissed: bool,
    /// Bot clock.
    pub at_ms: u64,
}

/// What a bot knows about its team, from the team packets. See the module docs.
#[derive(Debug, Default, Clone)]
pub struct TeamLog {
    /// The team the bot is in (0 = none).
    pub team_id: u32,
    /// Character id of the owner (the leader).
    pub owner: u32,
    /// The members, the bot included.
    pub members: SmallVec<[Member; MAX_PARTY]>,
    /// An invitation waiting for an answer.
    pub invite: Option<Invite>,
    /// How the bot last left a team.
    pub left: Option<Left>,
    /// Bot clock of the last `SCJoinedTeam`.
    pub joined_ms: u64,
    /// Our invitations that were declined.
    pub rejected: u8,
    /// Bot clock of the last declined invitation.
    pub rejected_ms: u64,
    /// Membership changes so far (joined, member joined / left).
    pub changes: u32,
    /// What the members target `(member object, target object, bot clock)`.
    pub member_targets: SmallVec<[(u32, u32, u64); MAX_PARTY]>,
    /// Monsters that target a member `(monster, member object, bot clock)`.
    pub attackers: SmallVec<[(u32, u32, u64); 6]>,
    /// What the bot knows of each member: role, online, zone, instance, position, hit points
    /// (see [`Remote`]).
    pub remote: SmallVec<[Remote; MAX_PARTY]>,
    /// The loot rule of the team, once the server told it.
    pub loot: Option<LootRule>,
}

/// A target noted longer ago than this is forgotten.
pub const TARGET_FRESH_MS: u64 = 12_000;

impl TeamLog {
    /// Whether the bot is in a team.
    pub fn in_team(&self) -> bool {
        self.team_id != 0
    }

    /// Whether character `char_id` is in the team.
    pub fn has(&self, char_id: u32) -> bool {
        self.members.iter().any(|m| m.char_id == char_id)
    }

    /// Number of members (the bot included).
    pub fn size(&self) -> usize {
        self.members.len()
    }

    /// Applies one activity event. Returns whether anything about the team changed.
    pub fn apply(&mut self, ev: &ActivityEvent, now_ms: u64) -> bool {
        match ev {
            ActivityEvent::AskToJoinTeam {
                team_id,
                sender_char_id,
                name,
                is_party,
            } => {
                self.invite = Some(Invite {
                    team_id: *team_id,
                    from_char: *sender_char_id,
                    from_name: name.clone(),
                    is_party: *is_party,
                    at_ms: now_ms.max(1),
                });
                true
            }
            ActivityEvent::JoinedTeam {
                team_id,
                owner_id,
                members,
                ..
            } => {
                self.team_id = *team_id;
                self.owner = *owner_id;
                self.members = members.iter().map(member_of).collect();
                self.remote = members.iter().map(Remote::from_brief).collect();
                self.invite = None;
                self.left = None;
                self.joined_ms = now_ms.max(1);
                self.changes += 1;
                self.member_targets.clear();
                self.attackers.clear();
                true
            }
            ActivityEvent::TeamMemberJoined {
                team_id, member, ..
            } if *team_id == self.team_id => {
                let m = member_of(member);
                if let Some(x) = self.members.iter_mut().find(|x| x.char_id == m.char_id) {
                    *x = m;
                } else if self.members.len() < 50 {
                    self.members.push(m);
                }
                self.note_joined(member);
                self.changes += 1;
                true
            }
            ActivityEvent::TeamMemberLeft {
                team_id, char_id, ..
            } if *team_id == self.team_id => {
                self.members.retain(|m| m.char_id != *char_id);
                self.remote.retain(|r| r.char_id != *char_id);
                self.changes += 1;
                true
            }
            ActivityEvent::LeftTeam {
                team_id,
                kicked,
                dismissed,
            } => {
                self.forget_team(*team_id, *kicked, *dismissed, now_ms);
                true
            }
            ActivityEvent::TeamDismissed { team_id } if *team_id == self.team_id => {
                self.forget_team(*team_id, false, true, now_ms);
                true
            }
            ActivityEvent::RejectedTeam { .. } => {
                self.rejected = self.rejected.saturating_add(1);
                self.rejected_ms = now_ms.max(1);
                true
            }
            ActivityEvent::TeamOwnerChanged { team_id, owner_id } if *team_id == self.team_id => {
                self.owner = *owner_id;
                true
            }
            // (where the members are, their role, their object ids: see `remote.rs`)
            ActivityEvent::TeamRemoteMembers { .. }
            | ActivityEvent::TeamMemberDisconnected { .. }
            | ActivityEvent::TeamMemberRoleChanged { .. }
            | ActivityEvent::TeamRefreshMember { .. }
            | ActivityEvent::TeamLootingRuleChanged { .. } => self.apply_vitals(ev, now_ms),
            _ => false,
        }
    }

    fn forget_team(&mut self, team_id: u32, kicked: bool, dismissed: bool, now_ms: u64) {
        self.left = Some(Left {
            team_id,
            kicked,
            dismissed,
            at_ms: now_ms.max(1),
        });
        self.team_id = 0;
        self.owner = 0;
        self.members.clear();
        self.remote.clear();
        self.loot = None;
        self.member_targets.clear();
        self.attackers.clear();
        self.changes += 1;
    }

    /// The last target member `member_obj` announced `(target, bot clock)`, however long ago (the
    /// server tells a change only, so a member that fights for minutes announces nothing).
    pub fn target_of(&self, member_obj: u32) -> Option<(u32, u64)> {
        self.member_targets
            .iter()
            .find(|t| t.0 == member_obj && t.1 != 0)
            .map(|t| (t.1, t.2))
    }

    /// Whether unit `obj` is a member of the team other than the bot.
    fn is_member_unit(&self, obj: u32, units: &FxHashMap<u32, Unit>) -> bool {
        self.members
            .iter()
            .any(|m| m.obj_id != 0 && m.obj_id == obj)
            || units
                .get(&obj)
                .is_some_and(|u| u.kind == UnitKind::Character && self.has(u.id))
    }

    /// `SCTargetChanged`: unit `obj` now targets `target`. Keeps what the members target and
    /// which monsters go after them (for the assist).
    pub fn on_target(
        &mut self,
        obj: u32,
        target: u32,
        me_obj: u32,
        units: &FxHashMap<u32, Unit>,
        now_ms: u64,
    ) {
        if !self.in_team() || obj == me_obj {
            return;
        }
        if self.is_member_unit(obj, units) {
            upsert(&mut self.member_targets, MAX_PARTY, obj, target, now_ms);
            return;
        }
        let npc = units.get(&obj).is_some_and(|u| u.kind == UnitKind::Npc);
        if !npc {
            return;
        }
        if target != 0 && (target == me_obj || self.is_member_unit(target, units)) {
            upsert(&mut self.attackers, 6, obj, target, now_ms);
        } else {
            self.attackers.retain(|a| a.0 != obj);
        }
    }

    /// What the members of the team target right now: `(member object, target)` pairs that are
    /// fresh at `now_ms`.
    pub fn member_targets_at(&self, now_ms: u64) -> impl Iterator<Item = (u32, u32)> + '_ {
        self.member_targets
            .iter()
            .filter(move |t| t.1 != 0 && now_ms.saturating_sub(t.2) <= TARGET_FRESH_MS)
            .map(|t| (t.0, t.1))
    }

    /// The monsters that target a member (or the bot), fresh at `now_ms`.
    pub fn attackers_at(&self, now_ms: u64) -> impl Iterator<Item = (u32, u32)> + '_ {
        self.attackers
            .iter()
            .filter(move |t| now_ms.saturating_sub(t.2) <= TARGET_FRESH_MS)
            .map(|t| (t.0, t.1))
    }
}

fn member_of(m: &TeamMemberBrief) -> Member {
    Member {
        char_id: m.char_id,
        level: m.level,
        obj_id: m.obj_id,
    }
}

fn upsert<const N: usize>(
    list: &mut SmallVec<[(u32, u32, u64); N]>,
    cap: usize,
    key: u32,
    value: u32,
    now_ms: u64,
) {
    match list.iter_mut().find(|e| e.0 == key) {
        Some(e) => {
            e.1 = value;
            e.2 = now_ms;
        }
        None => {
            if list.len() >= cap {
                list.remove(0);
            }
            list.push((key, value, now_ms));
        }
    }
}

// ---- the bot's dungeon memory --------------------------------------------------------------

/// What a bot remembers about teams and dungeons (one box per bot, made on the first team event
/// or dungeon goal). Lost with the connection, like the rest of [`BotState`].
#[derive(Debug, Default, Clone)]
pub struct PartyMem {
    /// The team.
    pub team: TeamLog,
    /// Bot clock at which the last dungeon goal ended (0 = never).
    pub last_run_ms: u64,
    /// Fresh dungeon entries the server counts `(zone group, bot clock)`, oldest first.
    pub entries: SmallVec<[(u32, u64); 6]>,
    /// Dungeons the server refused for a while `(zone group, until bot clock)`; `u64::MAX` is
    /// for good (a required item).
    pub backoff: SmallVec<[(u32, u64); 4]>,
    /// Dungeon runs finished (this connection).
    pub runs: u16,
    /// Dungeon goals that gave up (this connection).
    pub failed_runs: u16,
    /// The companion memory (the party of real players, see [`super::companion`]); made on first
    /// use.
    pub companion: Option<Box<super::companion::Mem>>,
}

impl PartyMem {
    /// Notes an entry into a fresh run of `zone_group` at `now_ms`.
    pub fn note_entry(&mut self, zone_group: u32, now_ms: u64) {
        self.entries
            .retain(|e| now_ms.saturating_sub(e.1) < ENTRY_WINDOW_MS);
        if self.entries.len() >= 6 {
            self.entries.remove(0);
        }
        self.entries.push((zone_group, now_ms));
    }

    /// Entries of `zone_group` the server still counts at `now_ms`.
    pub fn entries_in_window(&self, zone_group: u32, now_ms: u64) -> usize {
        self.entries
            .iter()
            .filter(|e| e.0 == zone_group && now_ms.saturating_sub(e.1) < ENTRY_WINDOW_MS)
            .count()
    }

    /// Leaves `zone_group` alone until `until_ms` (`u64::MAX`: for good).
    pub fn back_off(&mut self, zone_group: u32, until_ms: u64) {
        match self.backoff.iter_mut().find(|b| b.0 == zone_group) {
            Some(b) => b.1 = b.1.max(until_ms),
            None => {
                if self.backoff.len() >= 4 {
                    self.backoff.remove(0);
                }
                self.backoff.push((zone_group, until_ms));
            }
        }
    }

    /// Whether the bot should not try `zone_group` now: the server refused the dungeon lately,
    /// or `enter_count` is set and the entries it allows per window are used up. `None` means the
    /// entry limit is not counted (`[dungeons] entry_limit` off, as for 1.2).
    pub fn blocked(&self, zone_group: u32, enter_count: Option<u32>, now_ms: u64) -> bool {
        let used_up =
            enter_count.is_some_and(|n| self.entries_in_window(zone_group, now_ms) >= n as usize);
        used_up
            || self
                .backoff
                .iter()
                .any(|b| b.0 == zone_group && now_ms < b.1)
    }
}

impl BotState {
    /// The team log, once the bot met a team.
    pub fn team(&self) -> Option<&TeamLog> {
        self.party.as_deref().map(|p| &p.team)
    }

    /// The dungeon memory, created on first use.
    pub fn party_mem(&mut self) -> &mut PartyMem {
        self.party.get_or_insert_with(Default::default)
    }
}

/// Feeds an activity event to the team log (creating it on the first team event).
pub fn observe(mem: &mut Option<Box<PartyMem>>, ev: &ActivityEvent, now_ms: u64) {
    let is_team_event = matches!(
        ev,
        ActivityEvent::AskToJoinTeam { .. }
            | ActivityEvent::JoinedTeam { .. }
            | ActivityEvent::TeamMemberJoined { .. }
            | ActivityEvent::TeamMemberLeft { .. }
            | ActivityEvent::LeftTeam { .. }
            | ActivityEvent::TeamDismissed { .. }
            | ActivityEvent::RejectedTeam { .. }
            | ActivityEvent::TeamOwnerChanged { .. }
            | ActivityEvent::TeamRemoteMembers { .. }
            | ActivityEvent::TeamMemberDisconnected { .. }
            | ActivityEvent::TeamMemberRoleChanged { .. }
            | ActivityEvent::TeamRefreshMember { .. }
            | ActivityEvent::TeamLootingRuleChanged { .. }
    );
    if !is_team_event {
        return;
    }
    mem.get_or_insert_with(Default::default)
        .team
        .apply(ev, now_ms);
}

// ---- dungeons the bots may run -------------------------------------------------------------

/// Whether the bots run dungeon `d`: open to ordinary characters, no item to pay, one zone, a
/// world with a spawn point, and room for a party.
pub fn usable(world: &World, d: &Dungeon) -> bool {
    !d.closed
        && !d.client_driven
        && !d.select_channel
        && d.required_item == 0
        && d.max_players >= MIN_PARTY as u16
        && !d.entrances.is_empty()
        && world
            .dungeon_world(d)
            .is_some_and(|w| w.zone_keys.len() == 1 && w.spawn_for_zone(d.zone_key).is_some())
}

/// The index of the dungeon in `World::dungeons()` whose zone group is `zone_group`.
pub fn dungeon_index(world: &World, zone_group: u32) -> Option<usize> {
    world
        .dungeons()
        .iter()
        .position(|d| d.zone_group == zone_group)
}

/// A dungeon the coordinator may send a party to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Site {
    /// Index in `World::dungeons()` (bit `idx` of [`Wish::skip`]).
    pub idx: u16,
    /// Zone group (what the order names).
    pub zone_group: u32,
    /// Lowest character level.
    pub level_min: u8,
    /// Highest character level.
    pub level_max: u8,
    /// Most members of a party.
    pub max_party: u8,
    /// The first entrance (x, y).
    pub at: (f32, f32),
}

/// The sites of `world`.
pub fn sites(world: &World) -> Vec<Site> {
    world
        .dungeons()
        .iter()
        .enumerate()
        .filter(|(i, d)| *i < 32 && usable(world, d))
        .filter_map(|(i, d)| {
            let p = d.entrances.first()?.spawns.first()?;
            Some(Site {
                idx: i as u16,
                zone_group: d.zone_group,
                level_min: d.level_min,
                level_max: d.level_max,
                max_party: d.max_players.min(MAX_PARTY as u16) as u8,
                at: (p.x, p.y),
            })
        })
        .collect()
}

/// What a bot does in a party (derived from its class build, see [`PartyRole::of`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, PartialOrd, Ord)]
pub enum PartyRole {
    /// Pulls and holds the monsters' attention.
    Tank,
    /// Heals the others and stays back.
    Healer,
    /// Anything else.
    #[default]
    Dps,
}

impl PartyRole {
    /// The role of a class build: a healer build heals, a tank build (or a shield-and-plate build)
    /// tanks, everything else deals damage.
    pub fn of(a: &crate::archetype::Archetype) -> PartyRole {
        use crate::archetype::Role;
        match a.role {
            Role::Healer => PartyRole::Healer,
            Role::Tank => PartyRole::Tank,
            _ if a.uses_shield && a.armor_type == 3 => PartyRole::Tank,
            _ => PartyRole::Dps,
        }
    }

    /// Tanks lead a party (they pull); without a tank the highest level does.
    pub fn lead_rank(self) -> u8 {
        match self {
            PartyRole::Tank => 0,
            PartyRole::Healer | PartyRole::Dps => 1,
        }
    }
}

/// A bot that wants a dungeon.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Wish {
    /// The bot.
    pub bot: u32,
    /// Where it stands (x, y).
    pub at: (f32, f32),
    /// Its level.
    pub level: u8,
    /// Its alliance (the server refuses invitations between hostile factions).
    pub alliance: u32,
    /// What it does in a party.
    pub role: PartyRole,
    /// Dungeons it cannot enter now, a bit per [`Site::idx`].
    pub skip: u32,
    /// The bot may raise its level to a dungeon's minimum (GM).
    pub raise: bool,
    /// The bot can travel far at once (GM `/move`).
    pub jump: bool,
}

impl Wish {
    /// Whether the bot can run `site`: not skipped and its level fits (or can be raised).
    pub fn fits(&self, site: &Site) -> bool {
        if self.skip & (1 << site.idx) != 0 {
            return false;
        }
        if self.level > site.level_max {
            return false;
        }
        self.level >= site.level_min || self.raise
    }
}

/// A party the planner chose.
#[derive(Debug, Clone, PartialEq)]
pub struct Formation {
    /// The dungeon.
    pub site: Site,
    /// The members as indices into the wishes given to [`plan`]; the leader first.
    pub members: SmallVec<[usize; MAX_PARTY]>,
}

fn dist((ax, ay): (f32, f32), (bx, by): (f32, f32)) -> f32 {
    ((ax - bx).powi(2) + (ay - by).powi(2)).sqrt()
}

/// Chooses one party out of `wishes` (each with the seconds it has waited), or `None`.
///
/// Every wish in turn, longest-waiting first, is a seed; for each dungeon its bot can run, the
/// planner takes other bots of the same alliance that can run it too, up to the dungeon's player
/// limit ([`Site::max_party`], never more than [`MAX_PARTY`]) and within a distance that grows
/// with the waiting time. A tank and a healer are preferred before the nearest damage dealers, so
/// the party gets the mix it needs. A full party is formed at once; a party of [`RALLY_MIN`] or
/// more starts after [`RALLY_WINDOW_SECS`] of rallying for one more; a smaller one
/// ([`MIN_PARTY`]) only after the seed waited `relax_after_secs`. Among the dungeons the cheapest
/// wins (walking distance to the entrance plus a level-mismatch penalty); the leader is a tank
/// when there is one, else the member with the highest level (the stable sort keeps the seed
/// first on a tie). Without GM travel a dungeon farther than [`WALK_NEAR_M`] (growing with the
/// waiting time) is out of reach.
pub fn plan(wishes: &[(Wish, u64)], sites: &[Site], relax_after_secs: u64) -> Option<Formation> {
    let mut seeds: SmallVec<[usize; 16]> = (0..wishes.len()).collect();
    seeds.sort_by_key(|&i| std::cmp::Reverse(wishes[i].1));
    for seed in seeds {
        let (sw, waited) = wishes[seed];
        let mut best: Option<(f32, Formation)> = None;
        for site in sites.iter().filter(|s| sw.fits(s)) {
            let want = usize::from(site.max_party).clamp(MIN_PARTY, MAX_PARTY);
            let near = PARTY_NEAR_M + PARTY_NEAR_GROWTH_M * waited as f32;
            let mut mates: SmallVec<[(f32, usize); 16]> = wishes
                .iter()
                .enumerate()
                .filter(|&(j, (w, _))| {
                    j != seed
                        && w.alliance == sw.alliance
                        && w.fits(site)
                        && dist(w.at, sw.at) <= near
                })
                .map(|(j, (w, _))| (dist(w.at, sw.at), j))
                .collect();
            mates.sort_by(|a, b| a.0.total_cmp(&b.0));
            // one tank, one healer, then the nearest of the rest (when the seed already is one,
            // that place is filled by a damage dealer instead)
            let need = want.saturating_sub(1);
            let mut picked: SmallVec<[usize; MAX_PARTY]> = SmallVec::new();
            for role in [PartyRole::Tank, PartyRole::Healer] {
                if picked.len() >= need {
                    break;
                }
                let already = sw.role == role || picked.iter().any(|&j| wishes[j].0.role == role);
                if already {
                    continue;
                }
                if let Some(pos) = mates.iter().position(|&(_, j)| wishes[j].0.role == role) {
                    picked.push(mates.remove(pos).1);
                }
            }
            for &(_, j) in mates.iter() {
                if picked.len() >= need {
                    break;
                }
                picked.push(j);
            }
            let size = picked.len() + 1;
            let youngest = picked
                .iter()
                .map(|&j| wishes[j].1)
                .chain(std::iter::once(waited))
                .min()
                .unwrap_or(waited);
            let full = size >= want;
            let rallied = size >= RALLY_MIN.min(want) && youngest >= RALLY_WINDOW_SECS;
            let relaxed = size >= MIN_PARTY && waited >= relax_after_secs;
            if !(full || rallied || relaxed) {
                continue;
            }
            let mut members: SmallVec<[usize; MAX_PARTY]> = SmallVec::new();
            members.push(seed);
            members.extend(picked);
            // far dungeons need GM travel from everyone
            let walkers = members.iter().filter(|&&m| !wishes[m].0.jump).count();
            let entrance_m: f32 = members
                .iter()
                .map(|&m| dist(wishes[m].0.at, site.at))
                .sum::<f32>()
                / members.len() as f32;
            if walkers > 0 && entrance_m > WALK_NEAR_M + WALK_GROWTH_M * waited as f32 {
                continue;
            }
            let mut mismatch = 0.0;
            for &m in &members {
                let l = wishes[m].0.level;
                mismatch += f32::from(site.level_min.saturating_sub(l));
            }
            let cost = entrance_m + LEVEL_WEIGHT_M * mismatch;
            if best.as_ref().is_none_or(|b| cost < b.0) {
                // the leader: a tank first, then a healer, then the highest level
                members.sort_by_key(|&m| {
                    (
                        wishes[m].0.role.lead_rank(),
                        std::cmp::Reverse(wishes[m].0.level),
                    )
                });
                best = Some((
                    cost,
                    Formation {
                        site: *site,
                        members,
                    },
                ));
            }
        }
        if let Some((_, f)) = best {
            return Some(f);
        }
    }
    None
}

// ---- the board: what the leader tells the members -----------------------------------------

/// How far a party has come, as the leader reports it to the members. Later stages are greater.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    /// Team and travel to the entrance.
    Forming,
    /// Inside, working through the route.
    Clearing,
    /// The clear is over: everyone goes to the exit.
    Leaving,
    /// Everybody is out and the team is gone.
    Done,
}

#[derive(Debug, Clone, Copy)]
struct BoardEntry {
    party: u32,
    stage: Stage,
    /// The leader's target and when it was set.
    target: (u32, std::time::Instant),
}

/// The leader's side of a party, readable by its members (bot clocks differ between bots, so it
/// keeps real instants). A few entries at most; a mutex held for a lookup.
#[derive(Debug, Default)]
pub struct PartyBoard {
    entries: parking_lot::Mutex<Vec<BoardEntry>>,
}

/// Most parties on the board.
const BOARD_MAX: usize = 16;

impl PartyBoard {
    fn with<R>(&self, party: u32, f: impl FnOnce(&mut BoardEntry) -> R) -> R {
        let mut g = self.entries.lock();
        if let Some(e) = g.iter_mut().find(|e| e.party == party) {
            return f(e);
        }
        if g.len() >= BOARD_MAX {
            g.remove(0);
        }
        g.push(BoardEntry {
            party,
            stage: Stage::Forming,
            target: (0, std::time::Instant::now()),
        });
        let last = g.len() - 1;
        f(&mut g[last])
    }

    /// The stage of `party` (`Forming` when it was never reported).
    pub fn stage(&self, party: u32) -> Stage {
        self.entries
            .lock()
            .iter()
            .find(|e| e.party == party)
            .map_or(Stage::Forming, |e| e.stage)
    }

    /// Reports a stage; it never goes back.
    pub fn set_stage(&self, party: u32, stage: Stage) {
        self.with(party, |e| e.stage = e.stage.max(stage));
    }

    /// The leader's current target (0 = none).
    pub fn set_target(&self, party: u32, obj: u32, now: std::time::Instant) {
        self.with(party, |e| e.target = (obj, now));
    }

    /// The leader's target if it was set within `fresh`.
    pub fn target(
        &self,
        party: u32,
        now: std::time::Instant,
        fresh: std::time::Duration,
    ) -> Option<u32> {
        self.entries
            .lock()
            .iter()
            .find(|e| e.party == party)
            .filter(|e| e.target.0 != 0 && now.saturating_duration_since(e.target.1) <= fresh)
            .map(|e| e.target.0)
    }

    /// Removes a finished party.
    pub fn forget(&self, party: u32) {
        self.entries.lock().retain(|e| e.party != party);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aa_proto::sc_activity::{TeamMemberBrief, TeamPerson};

    fn mbr(id: u32, obj: u32) -> TeamMemberBrief {
        TeamMemberBrief {
            char_id: id,
            name: format!("C{id}").into(),
            level: 20,
            role: 0,
            obj_id: obj,
        }
    }

    fn joined(team: u32, owner: u32, ids: &[(u32, u32)]) -> ActivityEvent {
        ActivityEvent::JoinedTeam {
            team_id: team,
            owner_id: owner,
            is_party: true,
            members: ids.iter().map(|&(c, o)| mbr(c, o)).collect(),
        }
    }

    #[test]
    fn leader_and_members() {
        let o = PartyOrder {
            party_id: 7,
            leader: 3,
            members: SmallVec::from_slice(&[3, 4, 9]),
            roles: Default::default(),
            purpose: PartyPurpose::Dungeon { zone_group: 12 },
        };
        assert!(o.is_leader(3) && !o.is_leader(4));
        assert!(o.has_member(9) && !o.has_member(5));
        assert_eq!(o.zone_group(), 12);
        assert!(!o.members.spilled());
        // roles are aligned with the members; a party without them is all damage
        assert_eq!(o.role_of(3), PartyRole::Dps);
        let with_roles = PartyOrder {
            roles: [PartyRole::Tank, PartyRole::Healer, PartyRole::Dps]
                .into_iter()
                .collect(),
            ..o.clone()
        };
        assert_eq!(with_roles.role_of(3), PartyRole::Tank);
        assert_eq!(with_roles.role_of(4), PartyRole::Healer);
        assert_eq!(with_roles.role_of(9), PartyRole::Dps);
        assert_eq!(with_roles.role_of(99), PartyRole::Dps, "not a member");
    }

    #[test]
    fn the_team_log_follows_the_team_packets() {
        let mut t = TeamLog::default();
        assert!(!t.in_team());
        // an invitation is kept until answered
        assert!(t.apply(
            &ActivityEvent::AskToJoinTeam {
                team_id: 0,
                sender_char_id: 5,
                name: "Lead".into(),
                is_party: true,
            },
            100
        ));
        let inv = t.invite.clone().unwrap();
        assert_eq!(
            (inv.from_char, &*inv.from_name, inv.at_ms),
            (5, "Lead", 100)
        );
        // joining clears it and fills the members
        t.apply(&joined(9, 5, &[(5, 50), (6, 60)]), 200);
        assert!(t.in_team() && t.invite.is_none());
        assert_eq!((t.team_id, t.owner, t.size()), (9, 5, 2));
        assert!(t.has(5) && t.has(6) && !t.has(7));
        // a member joins, another one is kicked
        t.apply(
            &ActivityEvent::TeamMemberJoined {
                team_id: 9,
                member: mbr(7, 70),
                party: 0,
            },
            300,
        );
        assert_eq!(t.size(), 3);
        t.apply(
            &ActivityEvent::TeamMemberLeft {
                team_id: 9,
                char_id: 6,
                kicked: true,
            },
            400,
        );
        assert!(!t.has(6) && t.size() == 2);
        // events of another team are not ours
        assert!(!t.apply(
            &ActivityEvent::TeamMemberLeft {
                team_id: 77,
                char_id: 5,
                kicked: false
            },
            410
        ));
        assert!(t.has(5));
        // the owner changes, then the bot is kicked
        t.apply(
            &ActivityEvent::TeamOwnerChanged {
                team_id: 9,
                owner_id: 7,
            },
            420,
        );
        assert_eq!(t.owner, 7);
        t.apply(
            &ActivityEvent::LeftTeam {
                team_id: 9,
                kicked: true,
                dismissed: false,
            },
            500,
        );
        assert!(!t.in_team() && t.members.is_empty());
        let left = t.left.unwrap();
        assert!(left.kicked && !left.dismissed && left.at_ms == 500);
        // a rejection of our invitation is counted
        t.apply(
            &ActivityEvent::RejectedTeam {
                name: "X".into(),
                party: true,
            },
            600,
        );
        assert_eq!((t.rejected, t.rejected_ms), (1, 600));
    }

    fn person(char_id: u32, zone: u32, instance: u32, pos: [f32; 3], obj: u32) -> TeamPerson {
        TeamPerson {
            char_id,
            zone_id: zone,
            instance_id: instance,
            level: 22,
            hp: 600,
            hp_max: 1200,
            mp: 10,
            mp_max: 20,
            pos,
            obj_id: obj,
        }
    }

    #[test]
    fn the_team_log_knows_where_the_members_are() {
        let mut t = TeamLog::default();
        t.apply(&joined(9, 5, &[(5, 50), (6, 60)]), 200);
        // a join packet knows names, roles and levels, nothing about position
        let r = t.remote_of(5).unwrap();
        assert_eq!(
            (&*r.name, r.level, r.online, r.located()),
            ("C5", 20, true, false)
        );
        assert_eq!(r.hp_pct(), None);
        assert_eq!(t.owner_remote().map(|r| r.char_id), Some(5));
        // the owner is somewhere else: a position packet tells zone, instance, hit points
        assert!(t.apply(
            &ActivityEvent::TeamRemoteMembers {
                members: vec![person(5, 301, 7, [10.0, 20.0, 30.0], 51)].into(),
            },
            1_000
        ));
        let r = t.remote_of(5).unwrap();
        assert_eq!(
            (r.zone_id, r.instance_id, r.pos),
            (301, 7, [10.0, 20.0, 30.0])
        );
        assert_eq!((r.hp_pct(), r.seen_ms, r.obj_id), (Some(50), 1_000, 51));
        // the object id reaches the member list too (the assist looks members up by it)
        assert_eq!(
            t.members.iter().find(|m| m.char_id == 5).unwrap().obj_id,
            51
        );
        // a character that is not in the team is ignored
        assert!(!t.apply(
            &ActivityEvent::TeamRemoteMembers {
                members: vec![person(99, 1, 0, [0.0; 3], 9)].into(),
            },
            1_100
        ));
        // a role is chosen, an object id refreshed, the loot rule changed
        t.apply(
            &ActivityEvent::TeamMemberRoleChanged {
                team_id: 9,
                char_id: 6,
                role: 2,
            },
            1_200,
        );
        assert_eq!(t.remote_of(6).unwrap().role, 2);
        t.apply(
            &ActivityEvent::TeamRefreshMember {
                team_id: 9,
                char_id: 6,
                obj_id: 66,
            },
            1_300,
        );
        assert_eq!(t.remote_of(6).unwrap().obj_id, 66);
        assert_eq!(
            t.members.iter().find(|m| m.char_id == 6).unwrap().obj_id,
            66
        );
        t.apply(
            &ActivityEvent::TeamLootingRuleChanged {
                team_id: 9,
                method: 2,
                min_grade: 3,
                master: 5,
                roll_bop: true,
                flags: 1,
            },
            1_400,
        );
        assert_eq!(t.loot.map(|l| (l.method, l.master)), Some((2, 5)));
        // packets of another team change nothing
        assert!(!t.apply(
            &ActivityEvent::TeamMemberRoleChanged {
                team_id: 77,
                char_id: 6,
                role: 3
            },
            1_500
        ));
        assert_eq!(t.remote_of(6).unwrap().role, 2);
        // the owner goes offline: no object id, no longer online, the last place is kept
        t.apply(
            &ActivityEvent::TeamMemberDisconnected {
                team_id: 9,
                char_id: 5,
                person: Box::new(person(5, 301, 7, [11.0, 21.0, 31.0], 0)),
            },
            2_000,
        );
        let r = t.remote_of(5).unwrap();
        assert!(!r.online && r.obj_id == 0);
        assert_eq!(r.pos, [11.0, 21.0, 31.0]);
        assert_eq!(t.members.iter().find(|m| m.char_id == 5).unwrap().obj_id, 0);
        // a member that comes back is online again, with an object id
        t.apply(
            &ActivityEvent::TeamRemoteMembers {
                members: vec![person(5, 301, 7, [12.0, 22.0, 32.0], 52)].into(),
            },
            3_000,
        );
        assert!(t.remote_of(5).unwrap().online);
        // members leave and the team ends: the records go with them
        t.apply(
            &ActivityEvent::TeamMemberLeft {
                team_id: 9,
                char_id: 6,
                kicked: false,
            },
            3_100,
        );
        assert!(t.remote_of(6).is_none());
        t.apply(&ActivityEvent::TeamDismissed { team_id: 9 }, 3_200);
        assert!(t.remote.is_empty() && t.loot.is_none());
        // (nothing is applied while the bot is in no team)
        assert!(!t.apply(
            &ActivityEvent::TeamRemoteMembers {
                members: vec![person(5, 1, 0, [0.0; 3], 1)].into(),
            },
            3_300
        ));
    }

    #[test]
    fn a_member_that_joins_later_gets_a_record_and_keeps_it_across_a_return() {
        let mut t = TeamLog::default();
        t.apply(&joined(9, 5, &[(5, 50)]), 100);
        t.apply(
            &ActivityEvent::TeamMemberJoined {
                team_id: 9,
                member: mbr(7, 70),
                party: 0,
            },
            200,
        );
        assert_eq!(t.remote_of(7).map(|r| r.obj_id), Some(70));
        t.apply(
            &ActivityEvent::TeamRemoteMembers {
                members: vec![person(7, 100, 0, [1.0, 2.0, 3.0], 70)].into(),
            },
            300,
        );
        // the same member joins again (a re-login sends SCTeamMemberJoined): the place is kept
        t.apply(
            &ActivityEvent::TeamMemberJoined {
                team_id: 9,
                member: mbr(7, 71),
                party: 0,
            },
            400,
        );
        let r = t.remote_of(7).unwrap();
        assert_eq!((r.obj_id, r.pos, r.online), (71, [1.0, 2.0, 3.0], true));
        assert_eq!(t.remote.len(), 2);
    }

    #[test]
    fn a_dismissed_team_is_gone() {
        let mut t = TeamLog::default();
        t.apply(&joined(3, 1, &[(1, 10), (2, 20)]), 10);
        t.apply(&ActivityEvent::TeamDismissed { team_id: 3 }, 20);
        assert!(!t.in_team());
        assert!(t.left.unwrap().dismissed);
    }

    fn npc(pos: [f32; 3]) -> Unit {
        Unit {
            kind: UnitKind::Npc,
            id: 1,
            pos,
            hp: 100,
            faction: 0,
            level: 20,
            flags: 0,
            target: 0,
            name: None,
        }
    }

    fn character(id: u32) -> Unit {
        Unit {
            kind: UnitKind::Character,
            id,
            pos: [0.0; 3],
            hp: 100,
            faction: 0,
            level: 20,
            flags: 0,
            target: 0,
            name: Some("x".into()),
        }
    }

    #[test]
    fn member_targets_and_attackers_are_followed() {
        let mut t = TeamLog::default();
        t.apply(&joined(3, 1, &[(1, 10), (2, 20), (3, 30)]), 10);
        let mut units = FxHashMap::default();
        units.insert(10, character(1));
        units.insert(20, character(2));
        units.insert(500, npc([1.0, 1.0, 1.0]));
        units.insert(501, npc([2.0, 2.0, 2.0]));
        // the leader (object 10) targets monster 500
        t.on_target(10, 500, 30, &units, 1_000);
        assert_eq!(t.member_targets_at(1_500).collect::<Vec<_>>(), [(10, 500)]);
        // it is forgotten after a while
        assert_eq!(t.member_targets_at(1_000 + TARGET_FRESH_MS + 1).count(), 0);
        // a monster targets the second member, another one the bot itself (object 30)
        t.on_target(500, 20, 30, &units, 1_100);
        t.on_target(501, 30, 30, &units, 1_200);
        let a: Vec<_> = t.attackers_at(1_300).collect();
        assert_eq!(a, [(500, 20), (501, 30)]);
        // a monster that picks somebody else is no attacker any more
        t.on_target(500, 999, 30, &units, 1_400);
        assert_eq!(t.attackers_at(1_500).collect::<Vec<_>>(), [(501, 30)]);
        // the bot's own target changes and non-members' are not recorded
        t.on_target(30, 500, 30, &units, 1_500);
        assert_eq!(t.member_targets.len(), 1);
        // outside a team nothing is kept
        let mut none = TeamLog::default();
        none.on_target(10, 500, 30, &units, 10);
        assert!(none.member_targets.is_empty());
    }

    #[test]
    fn target_lists_are_bounded() {
        let mut t = TeamLog::default();
        t.apply(&joined(3, 1, &[(1, 10)]), 10);
        let mut units = FxHashMap::default();
        units.insert(10, character(1));
        for i in 0..20u32 {
            units.insert(1000 + i, npc([0.0; 3]));
            t.on_target(1000 + i, 10, 99, &units, 100 + u64::from(i));
        }
        assert_eq!(t.attackers.len(), 6);
        assert_eq!(t.attackers.last().unwrap().0, 1019);
    }

    #[test]
    fn entries_are_counted_per_zone_group_and_window() {
        let mut m = PartyMem::default();
        m.note_entry(50, 1_000);
        m.note_entry(50, 2_000);
        m.note_entry(47, 3_000);
        assert_eq!(m.entries_in_window(50, 4_000), 2);
        assert_eq!(m.entries_in_window(47, 4_000), 1);
        // the window passes
        assert_eq!(m.entries_in_window(50, 1_000 + ENTRY_WINDOW_MS), 1);
        assert_eq!(m.entries_in_window(50, 2_000 + ENTRY_WINDOW_MS), 0);
        // and a long list stays bounded
        for i in 0..20 {
            m.note_entry(1, 5_000 + i);
        }
        assert_eq!(m.entries.len(), 6);
    }

    #[test]
    fn the_entry_limit_and_refusals_block_a_dungeon() {
        let mut m = PartyMem::default();
        assert!(!m.blocked(50, Some(3), 10));
        for i in 0..3 {
            m.note_entry(50, 100 + i);
        }
        assert!(m.blocked(50, Some(3), 1_000), "three entries used up");
        assert!(
            !m.blocked(50, Some(3), 1_000 + ENTRY_WINDOW_MS),
            "the window passed"
        );
        // the server refused a group for ten minutes
        m.back_off(47, 600_000);
        assert!(m.blocked(47, Some(3), 1_000));
        assert!(!m.blocked(47, Some(3), 600_000));
        // for good
        m.back_off(47, u64::MAX);
        assert!(m.blocked(47, Some(3), u64::MAX - 1));
        for g in 0..10 {
            m.back_off(100 + g, 5);
        }
        assert_eq!(m.backoff.len(), 4);
    }

    #[test]
    fn the_entry_limit_is_not_counted_when_it_is_switched_off() {
        let mut m = PartyMem::default();
        for i in 0..6 {
            m.note_entry(50, 100 + i);
        }
        // `None` is `[dungeons] entry_limit = false` (the default): the entries never block
        assert!(!m.blocked(50, None, 1_000));
        assert!(
            m.blocked(50, Some(3), 1_000),
            "counting it blocks as before"
        );
        // a refusal the server sent is still learned with the counting switched off
        m.back_off(47, 600_000);
        assert!(m.blocked(47, None, 1_000));
        assert!(!m.blocked(47, None, 600_000));
    }

    fn site(idx: u16, group: u32, lo: u8, hi: u8, max: u8, at: (f32, f32)) -> Site {
        Site {
            idx,
            zone_group: group,
            level_min: lo,
            level_max: hi,
            max_party: max,
            at,
        }
    }

    fn wish(bot: u32, at: (f32, f32), level: u8) -> Wish {
        Wish {
            bot,
            at,
            level,
            alliance: 148,
            role: PartyRole::Dps,
            skip: 0,
            raise: false,
            jump: false,
        }
    }

    fn wish_as(bot: u32, at: (f32, f32), level: u8, role: PartyRole) -> Wish {
        Wish {
            role,
            ..wish(bot, at, level)
        }
    }

    fn at_secs(w: Wish, secs: u64) -> (Wish, u64) {
        (w, secs)
    }

    fn bots(f: &Formation, wishes: &[(Wish, u64)]) -> Vec<u32> {
        f.members.iter().map(|&i| wishes[i].0.bot).collect()
    }

    #[test]
    fn the_class_builds_give_the_party_roles() {
        let a = crate::archetype::Archetypes::builtin().unwrap();
        assert_eq!(
            PartyRole::of(a.get("Abolisher").unwrap()),
            PartyRole::Tank,
            "the shield-and-plate build tanks"
        );
        assert_eq!(PartyRole::of(a.get("Templar").unwrap()), PartyRole::Healer);
        assert_eq!(PartyRole::of(a.get("Cleric").unwrap()), PartyRole::Healer);
        for name in ["Darkrunner", "Primeval", "Reaper", "Daggerspell"] {
            assert_eq!(
                PartyRole::of(a.get(name).unwrap()),
                PartyRole::Dps,
                "{name}"
            );
        }
    }

    #[test]
    fn a_rally_prefers_a_tank_and_a_healer_and_the_tank_leads() {
        let sites = [site(0, 50, 18, 55, 4, (0.0, 0.0))];
        let w = [
            at_secs(wish(1, (0.0, 0.0), 30), 0),
            at_secs(wish_as(2, (30.0, 0.0), 20, PartyRole::Tank), 0),
            at_secs(wish_as(3, (40.0, 0.0), 19, PartyRole::Healer), 0),
            at_secs(wish(4, (5.0, 0.0), 25), 0),
            at_secs(wish(5, (6.0, 0.0), 24), 0),
        ];
        let f = plan(&w, &sites, RELAX_AFTER_SECS).expect("a party");
        let chosen = bots(&f, &w);
        assert_eq!(f.members.len(), 4, "the dungeon holds four");
        assert!(chosen.contains(&2), "the tank goes: {chosen:?}");
        assert!(chosen.contains(&3), "the healer goes: {chosen:?}");
        assert!(
            chosen.contains(&1) && chosen.contains(&4),
            "the seed and the nearest damage dealer fill the rest: {chosen:?}"
        );
        assert_eq!(w[f.members[0]].0.bot, 2, "the tank leads, not the level 30");
        // a party without a tank stays led by its highest level
        let w = [
            at_secs(wish(1, (0.0, 0.0), 20), 0),
            at_secs(wish_as(2, (5.0, 0.0), 25, PartyRole::Healer), 0),
            at_secs(wish(3, (6.0, 0.0), 30), 0),
        ];
        let f = plan(&w, &[site(0, 50, 18, 55, 3, (0.0, 0.0))], RELAX_AFTER_SECS).expect("a party");
        assert_eq!(w[f.members[0]].0.bot, 3);
    }

    #[test]
    fn four_bots_rally_for_a_fifth_before_they_start() {
        let sites = [site(0, 50, 18, 55, 5, (0.0, 0.0))];
        let w = [
            at_secs(wish(1, (0.0, 0.0), 20), RALLY_WINDOW_SECS),
            at_secs(wish(2, (1.0, 0.0), 21), RALLY_WINDOW_SECS + 10),
            at_secs(wish(3, (2.0, 0.0), 22), RALLY_WINDOW_SECS + 20),
            at_secs(wish(4, (3.0, 0.0), 23), RALLY_WINDOW_SECS - 1),
        ];
        assert!(
            plan(&w, &sites, RELAX_AFTER_SECS).is_none(),
            "the newest asker is still inside the rally window"
        );
        let mut patient = w;
        patient[3].1 = RALLY_WINDOW_SECS;
        let f = plan(&patient, &sites, RELAX_AFTER_SECS).expect("a party of four");
        assert_eq!(f.members.len(), 4);
        assert_eq!(patient[f.members[0]].0.level, 23);
    }

    #[test]
    fn three_bots_of_the_right_level_nearby_make_a_party_of_three() {
        let sites = [site(0, 50, 18, 55, 3, (1000.0, 0.0))];
        let w = [
            at_secs(wish(1, (900.0, 0.0), 20), 0),
            at_secs(wish(2, (950.0, 10.0), 25), 0),
            at_secs(wish(3, (1100.0, 0.0), 19), 0),
            at_secs(wish(4, (1000.0, 50.0), 22), 0),
        ];
        let f = plan(&w, &sites, RELAX_AFTER_SECS).expect("a party");
        assert_eq!(f.site.zone_group, 50);
        assert_eq!(f.members.len(), 3, "the dungeon holds three");
        // the leader is the highest level of the three that were chosen
        let chosen = bots(&f, &w);
        let lead_level = w[f.members[0]].0.level;
        assert!(chosen.iter().all(|&b| {
            let i = w.iter().position(|x| x.0.bot == b).unwrap();
            w[i].0.level <= lead_level
        }));
    }

    #[test]
    fn five_bots_of_the_right_level_nearby_fill_a_five_player_dungeon() {
        let sites = [site(0, 50, 18, 55, 5, (1000.0, 0.0))];
        let w = [
            at_secs(wish(1, (900.0, 0.0), 20), 0),
            at_secs(wish(2, (950.0, 10.0), 25), 0),
            at_secs(wish(3, (1100.0, 0.0), 19), 0),
            at_secs(wish(4, (1000.0, 50.0), 22), 0),
            at_secs(wish(5, (980.0, -20.0), 21), 0),
        ];
        let f = plan(&w, &sites, RELAX_AFTER_SECS).expect("a party");
        assert_eq!(f.site.zone_group, 50);
        assert_eq!(f.members.len(), 5, "the dungeon holds five");
        // the leader is the highest level of the five
        let chosen = bots(&f, &w);
        let lead_level = w[f.members[0]].0.level;
        assert!(chosen.iter().all(|&b| {
            let i = w.iter().position(|x| x.0.bot == b).unwrap();
            w[i].0.level <= lead_level
        }));
        // four bots do not fill it: they wait the relax time, then go four
        let four = &w[..4];
        assert!(plan(four, &sites, RELAX_AFTER_SECS).is_none(), "not yet");
        let mut patient = four.to_vec();
        patient[0].1 = RELAX_AFTER_SECS - 1;
        assert!(plan(&patient, &sites, RELAX_AFTER_SECS).is_none());
        patient[0].1 = RELAX_AFTER_SECS;
        assert_eq!(
            plan(&patient, &sites, RELAX_AFTER_SECS)
                .expect("a party of four after the wait")
                .members
                .len(),
            4
        );
    }

    #[test]
    fn two_bots_wait_for_a_third_and_then_go_alone() {
        let sites = [site(0, 50, 18, 55, 3, (0.0, 0.0))];
        let mut w = vec![
            at_secs(wish(1, (0.0, 0.0), 20), 0),
            at_secs(wish(2, (10.0, 0.0), 21), 0),
        ];
        assert!(plan(&w, &sites, RELAX_AFTER_SECS).is_none(), "not yet");
        w[0].1 = RELAX_AFTER_SECS;
        let f = plan(&w, &sites, RELAX_AFTER_SECS).expect("a party of two after the wait");
        assert_eq!(f.members.len(), 2);
        // a lone bot never makes a party
        assert!(plan(&w[..1], &sites, RELAX_AFTER_SECS).is_none());
    }

    #[test]
    fn levels_alliances_skips_and_distance_decide() {
        let sites = [site(0, 50, 18, 55, 3, (0.0, 0.0))];
        // a level 10 bot cannot go unless it may raise itself
        let low = wish(3, (0.0, 0.0), 10);
        let w = [
            at_secs(wish(1, (0.0, 0.0), 20), 0),
            at_secs(wish(2, (5.0, 0.0), 20), 0),
            at_secs(low, 0),
        ];
        assert!(plan(&w, &sites, RELAX_AFTER_SECS).is_none());
        let mut raise = low;
        raise.raise = true;
        let w2 = [w[0], w[1], at_secs(raise, 0)];
        assert_eq!(
            plan(&w2, &sites, RELAX_AFTER_SECS).unwrap().members.len(),
            3
        );
        // another alliance does not mix
        let mut other = wish(3, (0.0, 0.0), 20);
        other.alliance = 149;
        let w3 = [w[0], w[1], at_secs(other, 0)];
        assert!(plan(&w3, &sites, RELAX_AFTER_SECS).is_none());
        // a bot that cannot enter the dungeon (entry limit) is left out
        let mut skip = wish(3, (0.0, 0.0), 20);
        skip.skip = 1;
        let w4 = [w[0], w[1], at_secs(skip, 0)];
        assert!(plan(&w4, &sites, RELAX_AFTER_SECS).is_none());
        // too high a level
        let w5 = [w[0], w[1], at_secs(wish(3, (0.0, 0.0), 56), 0)];
        assert!(plan(&w5, &sites, RELAX_AFTER_SECS).is_none());
        // bots far apart wait until the limit grows
        let far = wish(3, (PARTY_NEAR_M + 500.0, 0.0), 20);
        let mut w6 = [w[0], w[1], at_secs(far, 0)];
        assert!(plan(&w6, &sites, RELAX_AFTER_SECS).is_none());
        w6[0].1 = 30;
        assert_eq!(
            plan(&w6, &sites, RELAX_AFTER_SECS).unwrap().members.len(),
            3
        );
    }

    #[test]
    fn the_party_goes_to_the_cheapest_dungeon_walkers_only_when_near() {
        let near = site(0, 47, 18, 55, 3, (300.0, 0.0));
        let far = site(1, 50, 18, 55, 3, (9000.0, 0.0));
        let w = [
            at_secs(wish(1, (0.0, 0.0), 20), 0),
            at_secs(wish(2, (10.0, 0.0), 20), 0),
            at_secs(wish(3, (20.0, 0.0), 20), 0),
        ];
        let f = plan(&w, &[far, near], RELAX_AFTER_SECS).unwrap();
        assert_eq!(f.site.zone_group, 47);
        // only a far dungeon: walkers cannot, bots that jump can
        assert!(plan(&w, &[far], RELAX_AFTER_SECS).is_none());
        let mut jumpers = w;
        for x in jumpers.iter_mut() {
            x.0.jump = true;
        }
        assert_eq!(
            plan(&jumpers, &[far], RELAX_AFTER_SECS)
                .unwrap()
                .site
                .zone_group,
            50
        );
        // and the walkers' range grows with the wait
        let mut patient = w;
        patient[0].1 = 70;
        assert_eq!(
            plan(&patient, &[far], RELAX_AFTER_SECS)
                .unwrap()
                .site
                .zone_group,
            50
        );
    }

    #[test]
    fn a_dungeon_for_two_takes_two() {
        let sites = [site(0, 50, 18, 55, 2, (0.0, 0.0))];
        let w = [
            at_secs(wish(1, (0.0, 0.0), 20), 0),
            at_secs(wish(2, (0.0, 0.0), 20), 0),
            at_secs(wish(3, (0.0, 0.0), 20), 0),
        ];
        assert_eq!(plan(&w, &sites, RELAX_AFTER_SECS).unwrap().members.len(), 2);
    }

    #[test]
    fn the_longest_waiting_bot_is_served_first() {
        let sites = [site(0, 50, 18, 55, 3, (0.0, 0.0))];
        // two groups of 3 are possible only on different alliances: the older wish wins
        let mut a = wish(1, (0.0, 0.0), 20);
        a.alliance = 149;
        let mut a2 = wish(2, (0.0, 0.0), 20);
        a2.alliance = 149;
        let mut a3 = wish(3, (0.0, 0.0), 20);
        a3.alliance = 149;
        let w = [
            at_secs(wish(4, (0.0, 0.0), 20), 5),
            at_secs(wish(5, (0.0, 0.0), 20), 5),
            at_secs(wish(6, (0.0, 0.0), 20), 5),
            at_secs(a, 30),
            at_secs(a2, 30),
            at_secs(a3, 30),
        ];
        let f = plan(&w, &sites, RELAX_AFTER_SECS).unwrap();
        let chosen = bots(&f, &w);
        assert!(chosen.iter().all(|b| (1..=3).contains(b)), "{chosen:?}");
    }
}
