//! Player versus player: who a bot may attack, how it defends itself, and the goals that go
//! looking for a fight (`gank` and `warzone`, in [`goals`](super::goals)).
//!
//! Everything is off until `[pvp] enabled`; the pieces are:
//!
//! * [`rules`] pure functions: which characters may be attacked, for what purpose. The bots never
//!   attack a friendly character, never use the force attack, never hit a character that the
//!   server protects (its alliance's home zone), and never touch a name in `never_names`.
//! * [`defence`] and [`fight`] self-defence in the combat controller: who counts as an attacker,
//!   when a fight against a character is given up (retreat, the silent refusal of a protected
//!   zone, time) and where a bot runs to.
//! * [`post`] where an ambush waits (a pure planner over the trade routes).
//! * [`squad`] the ambush squads the coordinator forms.
//! * [`walk`] the leg-wise walking helper the two goals share.
//!
//! [`PvpMem`] is what a bot remembers of all this. It is allocated on first use (one pointer in
//! [`BotState`](super::state::BotState)), because the per-bot budget is tight.

pub mod bounties;
pub mod defence;
pub mod fight;
pub mod post;
pub mod rules;
pub mod scan;
pub mod squad;
pub mod walk;

use smallvec::SmallVec;

/// Most carriers (characters whose backpack slot is occupied) remembered.
pub const MAX_PACKS: usize = 24;
/// Most characters shunned at once.
const MAX_SHUNNED: usize = 6;
/// Most wars whose kills are counted.
const MAX_WARS: usize = 3;

/// A character the bot leaves alone for a while (it could not hurt it, or ran from it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shun {
    /// Object id of the character.
    pub obj: u32,
    /// Bot clock (ms) until which the bot leaves it alone.
    pub until_ms: u64,
}

/// Kills a bot made in one war of a zone group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WarKills {
    /// Zone group.
    pub zone_group: u16,
    /// The Unix second the state ended that the kills belong to (a new war has a new end).
    pub end_unix_s: i64,
    /// Kills so far.
    pub kills: u8,
}

/// A run away from an attacker (see [`fight::flee`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Flee {
    /// Where the bot runs to.
    pub dest: [f32; 2],
    /// Bot clock (ms) at which it gives up running.
    pub until_ms: u64,
    /// The attacker's object id (0 when unknown).
    pub from: u32,
}

/// What a bot remembers of its fights with characters. See the module docs.
#[derive(Debug, Default)]
pub struct PvpMem {
    /// `(object id, template)` of the characters in view whose backpack slot is occupied.
    packs: SmallVec<[(u32, u32); 4]>,
    /// Characters the bot leaves alone for a while.
    shunned: SmallVec<[Shun; MAX_SHUNNED]>,
    /// Ambush posts the bot could not use: `(x, y in whole metres, until bot clock ms)`.
    bad_posts: SmallVec<[([i32; 2], u64); 3]>,
    /// The gank goal is not scored before this bot clock (ms).
    pub gank_next_ms: u64,
    /// The warzone goal is not scored before this bot clock (ms).
    pub war_next_ms: u64,
    /// Kills per war (warzone goal), newest last.
    wars: SmallVec<[WarKills; MAX_WARS]>,
    /// The run in progress, if any.
    pub flee: Option<Flee>,
    /// The honor of the bot as the server last said (`SCUnitPvPPointsChangedPacket`).
    pub honor: i32,
    /// The hostile faction kills as the server last said.
    pub hostile_kills: i32,
    /// Who killed the bot last `(killer object id, bot clock ms)`, when a character did.
    pub killed_by: Option<(u32, u64)>,
    /// Characters the bot killed (this process).
    pub kills: u32,
    /// Times the bot died to a character (this process).
    pub deaths: u32,
    /// Fights given up: protected target, retreat, time.
    pub gave_up: u32,
}

impl PvpMem {
    /// Notes what the backpack slot of character `obj` holds (`template` 0 = nothing).
    pub fn set_pack(&mut self, obj: u32, template: u32) {
        match self.packs.iter().position(|p| p.0 == obj) {
            Some(i) if template == 0 => {
                self.packs.swap_remove(i);
            }
            Some(i) => self.packs[i].1 = template,
            None if template != 0 => {
                if self.packs.len() >= MAX_PACKS {
                    self.packs.remove(0);
                }
                self.packs.push((obj, template));
            }
            None => {}
        }
    }

    /// The backpack template of character `obj`; 0 when it carries nothing the bot knows of.
    pub fn pack_of(&self, obj: u32) -> u32 {
        self.packs.iter().find(|p| p.0 == obj).map_or(0, |p| p.1)
    }

    /// Forgets unit `obj` (it left the view).
    pub fn forget(&mut self, obj: u32) {
        self.packs.retain(|p| p.0 != obj);
    }

    /// The view was dropped (a new world): nothing about units in it is valid any more.
    pub fn clear_view(&mut self) {
        self.packs.clear();
        self.shunned.clear();
        self.flee = None;
    }

    /// Leaves character `obj` alone until `until_ms`.
    pub fn shun(&mut self, obj: u32, until_ms: u64) {
        if let Some(s) = self.shunned.iter_mut().find(|s| s.obj == obj) {
            s.until_ms = s.until_ms.max(until_ms);
            return;
        }
        if self.shunned.len() >= MAX_SHUNNED {
            if let Some(i) = self
                .shunned
                .iter()
                .enumerate()
                .min_by_key(|(_, s)| s.until_ms)
                .map(|(i, _)| i)
            {
                self.shunned.remove(i);
            }
        }
        self.shunned.push(Shun { obj, until_ms });
    }

    /// Whether the bot leaves character `obj` alone at `now_ms`.
    pub fn shunned(&self, obj: u32, now_ms: u64) -> bool {
        self.shunned
            .iter()
            .any(|s| s.obj == obj && now_ms < s.until_ms)
    }

    /// Remembers that the ambush post at (x, y) did not work: no post within 150 m of it is used
    /// before `until_ms`.
    pub fn block_post(&mut self, pos: [f32; 2], until_ms: u64) {
        if self.bad_posts.len() >= 3 {
            self.bad_posts.remove(0);
        }
        self.bad_posts
            .push(([pos[0].round() as i32, pos[1].round() as i32], until_ms));
    }

    /// Whether a post close to `pos` is blocked at `now_ms`.
    pub fn post_blocked(&self, pos: [f32; 2], now_ms: u64) -> bool {
        self.bad_posts.iter().any(|(p, until)| {
            now_ms < *until
                && ((p[0] as f32 - pos[0]).powi(2) + (p[1] as f32 - pos[1]).powi(2)).sqrt() < 150.0
        })
    }

    /// A line for the log with what the server last said about the bot's pvp standing, `None`
    /// while it said nothing.
    pub fn standing(&self) -> Option<String> {
        (self.honor != 0 || self.hostile_kills != 0 || self.kills != 0 || self.deaths != 0).then(
            || {
                format!(
                    "pvp: honor {}, hostile faction kills {}; killed {} and died {} times to characters",
                    self.honor, self.hostile_kills, self.kills, self.deaths
                )
            },
        )
    }

    /// Kills made in the war of `zone_group` that ends at `end_unix_s`.
    pub fn war_kills(&self, zone_group: u16, end_unix_s: i64) -> u8 {
        self.wars
            .iter()
            .find(|w| w.zone_group == zone_group && w.end_unix_s == end_unix_s)
            .map_or(0, |w| w.kills)
    }

    /// Counts a kill in the war of `zone_group` that ends at `end_unix_s`.
    pub fn add_war_kill(&mut self, zone_group: u16, end_unix_s: i64) {
        if let Some(w) = self
            .wars
            .iter_mut()
            .find(|w| w.zone_group == zone_group && w.end_unix_s == end_unix_s)
        {
            w.kills = w.kills.saturating_add(1);
            return;
        }
        if self.wars.len() >= MAX_WARS {
            self.wars.remove(0);
        }
        self.wars.push(WarKills {
            zone_group,
            end_unix_s,
            kills: 1,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_are_noted_and_forgotten() {
        let mut m = PvpMem::default();
        assert_eq!(m.pack_of(5), 0);
        m.set_pack(5, 100);
        m.set_pack(6, 200);
        assert_eq!((m.pack_of(5), m.pack_of(6)), (100, 200));
        m.set_pack(5, 300);
        assert_eq!(m.pack_of(5), 300);
        m.set_pack(5, 0);
        assert_eq!(m.pack_of(5), 0);
        m.forget(6);
        assert_eq!(m.pack_of(6), 0);
        // an empty slot of an unknown unit is nothing to note
        m.set_pack(9, 0);
        assert!(m.packs.is_empty());
    }

    #[test]
    fn the_pack_table_is_bounded() {
        let mut m = PvpMem::default();
        for obj in 1..=(MAX_PACKS as u32 + 10) {
            m.set_pack(obj, 1000 + obj);
        }
        assert_eq!(m.packs.len(), MAX_PACKS);
        // the oldest went first
        assert_eq!(m.pack_of(1), 0);
        assert_eq!(
            m.pack_of(MAX_PACKS as u32 + 10),
            1000 + MAX_PACKS as u32 + 10
        );
    }

    #[test]
    fn shunning_expires_and_is_bounded() {
        let mut m = PvpMem::default();
        m.shun(3, 1_000);
        assert!(m.shunned(3, 999) && !m.shunned(3, 1_000));
        m.shun(3, 5_000);
        m.shun(3, 2_000);
        assert!(m.shunned(3, 4_999), "the later end stands");
        for obj in 10..30 {
            m.shun(obj, 100 + u64::from(obj));
        }
        assert!(m.shunned.len() <= MAX_SHUNNED);
    }

    #[test]
    fn kills_are_counted_per_war() {
        let mut m = PvpMem::default();
        assert_eq!(m.war_kills(20, 1_000), 0);
        m.add_war_kill(20, 1_000);
        m.add_war_kill(20, 1_000);
        m.add_war_kill(17, 1_000);
        assert_eq!(m.war_kills(20, 1_000), 2);
        assert_eq!(m.war_kills(17, 1_000), 1);
        // the next war of the same group starts from zero
        assert_eq!(m.war_kills(20, 9_000), 0);
        m.add_war_kill(20, 9_000);
        m.add_war_kill(15, 9_000);
        assert!(m.wars.len() <= MAX_WARS);
        assert_eq!(m.war_kills(20, 9_000), 1);
    }
}
