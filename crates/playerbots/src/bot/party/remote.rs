//! What a bot knows of the other members of its team beyond who they are: role, whether they are
//! online, where they are (zone, instance, position) and how they fare (hit points, mana).
//!
//! The server tells it in three ways (all parsed by `aa_proto::sc_activity`):
//!
//! * the join packets (`SCJoinedTeam`, `SCTeamMemberJoined`): name, level, role, object id;
//! * `SCTeamRemoteMembersEx` (0xe4): the full state of a member, sent to the others whenever it
//!   moves and, for members out of each other's render distance, on change and every 5 s;
//! * `SCTeamMemberDisconnected` (0xdc), `SCTeamMemberRoleChanged` (0xde) and
//!   `SCRefreshTeamMember` (0xe3): a member went offline, chose a role, got an object id.
//!
//! The companion goal (see [`crate::bot::goals::companion`]) follows a real player with this: the
//! position when the player is out of view, and the zone and instance to notice that the player
//! entered a dungeon. A [`Remote`] is kept for every member of the team, the bot itself included.

use aa_proto::sc_activity::{ActivityEvent, TeamMemberBrief, TeamPerson};

use super::TeamLog;

/// The loot rule of the team (`SCTeamLootingRuleChanged`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LootRule {
    /// Loot method.
    pub method: u8,
    /// Minimum grade that is rolled for.
    pub min_grade: u8,
    /// Character id of the loot master.
    pub master: u32,
    /// Bind-on-pickup items are rolled for.
    pub roll_bop: bool,
}

impl LootRule {
    /// The member who assigns loot when the team uses Master Looter.
    pub fn assigning_member(self) -> Option<u32> {
        const MASTER_LOOTER: u8 = 2;
        (self.method == MASTER_LOOTER && self.master != 0).then_some(self.master)
    }
}

/// What the bot knows of one team member. See the module docs.
#[derive(Debug, Clone, PartialEq)]
pub struct Remote {
    /// Character id.
    pub char_id: u32,
    /// Character name.
    pub name: Box<str>,
    /// Team role (0 undecided, 1 tank, 2 healer, 3 attacker).
    pub role: u8,
    /// The member is online (a join packet counts it online until the server says otherwise).
    pub online: bool,
    /// Level.
    pub level: u8,
    /// Object id (0 when offline or not told yet).
    pub obj_id: u32,
    /// Zone key of the member's position (0 until a position packet arrived).
    pub zone_id: u32,
    /// Instance id of the member's world (0 = the main world, also while unknown).
    pub instance_id: u32,
    /// Hit points (0 until a position packet arrived).
    pub hp: i32,
    /// Maximum hit points.
    pub hp_max: i32,
    /// Mana.
    pub mp: i32,
    /// Maximum mana.
    pub mp_max: i32,
    /// Position in the member's own world; meaningful when `seen_ms` is not 0.
    pub pos: [f32; 3],
    /// Bot clock of the last position packet (0 = none yet).
    pub seen_ms: u64,
}

impl Remote {
    /// The record a join packet makes.
    pub fn from_brief(m: &TeamMemberBrief) -> Remote {
        Remote {
            char_id: m.char_id,
            name: m.name.clone(),
            role: m.role,
            online: true,
            level: m.level,
            obj_id: m.obj_id,
            zone_id: 0,
            instance_id: 0,
            hp: 0,
            hp_max: 0,
            mp: 0,
            mp_max: 0,
            pos: [0.0; 3],
            seen_ms: 0,
        }
    }

    /// Hit points in percent of the maximum; `None` before a position packet told them.
    pub fn hp_pct(&self) -> Option<u8> {
        (self.seen_ms != 0 && self.hp_max > 0)
            .then(|| (i64::from(self.hp.max(0)) * 100 / i64::from(self.hp_max)).clamp(0, 100) as u8)
    }

    /// Whether a position packet told where the member is.
    pub fn located(&self) -> bool {
        self.seen_ms != 0
    }

    /// Takes the state a position packet carries.
    fn set_person(&mut self, p: &TeamPerson, now_ms: u64) {
        self.level = p.level;
        self.zone_id = p.zone_id;
        self.instance_id = p.instance_id;
        self.hp = p.hp;
        self.hp_max = p.hp_max;
        self.mp = p.mp;
        self.mp_max = p.mp_max;
        self.pos = p.pos;
        self.obj_id = p.obj_id;
        self.online = p.obj_id != 0;
        self.seen_ms = now_ms.max(1);
    }
}

impl TeamLog {
    /// What the bot knows of member `char_id`.
    pub fn remote_of(&self, char_id: u32) -> Option<&Remote> {
        self.remote.iter().find(|r| r.char_id == char_id)
    }

    /// What the bot knows of the owner of the team.
    pub fn owner_remote(&self) -> Option<&Remote> {
        self.remote_of(self.owner)
    }

    /// Adds or replaces the record a join packet makes.
    pub(super) fn note_joined(&mut self, m: &TeamMemberBrief) {
        let fresh = Remote::from_brief(m);
        if let Some(r) = self.remote.iter_mut().find(|r| r.char_id == m.char_id) {
            // (a member that comes back keeps what was learned about its position)
            r.name = fresh.name;
            r.role = fresh.role;
            r.level = fresh.level;
            r.obj_id = fresh.obj_id;
            r.online = true;
        } else if self.remote.len() < 50 {
            self.remote.push(fresh);
        }
    }

    /// Applies the packets that tell where the members are and what they are. Returns whether
    /// anything changed (events of another team and members the bot does not know are ignored).
    pub(super) fn apply_vitals(&mut self, ev: &ActivityEvent, now_ms: u64) -> bool {
        if !self.in_team() {
            return false;
        }
        match ev {
            ActivityEvent::TeamRemoteMembers { members } => {
                let mut any = false;
                for p in members.iter() {
                    any |= self.set_person(p, now_ms);
                }
                any
            }
            ActivityEvent::TeamMemberDisconnected {
                team_id, person, ..
            } if *team_id == self.team_id => {
                let changed = self.set_person(person, now_ms);
                if let Some(r) = self.remote.iter_mut().find(|r| r.char_id == person.char_id) {
                    r.online = false;
                    r.obj_id = 0;
                }
                if let Some(m) = self
                    .members
                    .iter_mut()
                    .find(|m| m.char_id == person.char_id)
                {
                    m.obj_id = 0;
                }
                changed || self.has(person.char_id)
            }
            ActivityEvent::TeamMemberRoleChanged {
                team_id,
                char_id,
                role,
            } if *team_id == self.team_id => {
                match self.remote.iter_mut().find(|r| r.char_id == *char_id) {
                    Some(r) => {
                        r.role = *role;
                        true
                    }
                    None => false,
                }
            }
            ActivityEvent::TeamRefreshMember {
                team_id,
                char_id,
                obj_id,
            } if *team_id == self.team_id => {
                if let Some(m) = self.members.iter_mut().find(|m| m.char_id == *char_id) {
                    m.obj_id = *obj_id;
                }
                match self.remote.iter_mut().find(|r| r.char_id == *char_id) {
                    Some(r) => {
                        r.obj_id = *obj_id;
                        r.online = *obj_id != 0;
                        true
                    }
                    None => false,
                }
            }
            ActivityEvent::TeamLootingRuleChanged {
                team_id,
                method,
                min_grade,
                master,
                roll_bop,
                ..
            } if *team_id == self.team_id => {
                self.loot = Some(LootRule {
                    method: *method,
                    min_grade: *min_grade,
                    master: *master,
                    roll_bop: *roll_bop,
                });
                true
            }
            _ => false,
        }
    }

    /// Notes a position packet of one member; false for a character that is not in the team.
    fn set_person(&mut self, p: &TeamPerson, now_ms: u64) -> bool {
        let Some(r) = self.remote.iter_mut().find(|r| r.char_id == p.char_id) else {
            return false;
        };
        r.set_person(p, now_ms);
        if let Some(m) = self.members.iter_mut().find(|m| m.char_id == p.char_id) {
            m.level = p.level;
            m.obj_id = p.obj_id;
        }
        true
    }
}
