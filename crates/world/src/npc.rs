//! NPC templates (`npcs`): name, level, faction and the service roles an NPC offers.

use anyhow::Result;
use rusqlite::Connection;
use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use crate::geom::IdMap;
use crate::sql::{each_row, RowExt};
use crate::text::{TextTable, Texts};

/// Service roles of an NPC as a bit set.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct NpcRoles(pub u16);

impl NpcRoles {
    /// Sells goods for gold.
    pub const MERCHANT: NpcRoles = NpcRoles(1 << 0);
    /// Buys trade packs ("specialty" goods) for coins.
    pub const SPECIALTY: NpcRoles = NpcRoles(1 << 1);
    /// Teaches skills.
    pub const SKILL_TRAINER: NpcRoles = NpcRoles(1 << 2);
    /// Bank vault.
    pub const BANKER: NpcRoles = NpcRoles(1 << 3);
    /// Priest (resurrection / blessings).
    pub const PRIEST: NpcRoles = NpcRoles(1 << 4);
    /// Teleporter.
    pub const TELEPORTER: NpcRoles = NpcRoles(1 << 5);
    /// Trader (barter merchant).
    pub const TRADER: NpcRoles = NpcRoles(1 << 6);
    /// Blacksmith.
    pub const BLACKSMITH: NpcRoles = NpcRoles(1 << 7);
    /// Auction house.
    pub const AUCTIONEER: NpcRoles = NpcRoles(1 << 8);
    /// Stabler (mount/pet storage).
    pub const STABLER: NpcRoles = NpcRoles(1 << 9);
    /// Repairs equipment.
    pub const REPAIRMAN: NpcRoles = NpcRoles(1 << 10);
    /// Changes active abilities.
    pub const ABILITY_CHANGER: NpcRoles = NpcRoles(1 << 11);
    /// Expedition (guild) services.
    pub const EXPEDITION: NpcRoles = NpcRoles(1 << 12);

    /// The empty set.
    pub const NONE: NpcRoles = NpcRoles(0);

    /// Whether every role of `other` is in `self`.
    pub const fn has(self, other: NpcRoles) -> bool {
        self.0 & other.0 == other.0
    }

    /// Union.
    pub const fn with(self, other: NpcRoles) -> NpcRoles {
        NpcRoles(self.0 | other.0)
    }

    /// Whether no role is set.
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }
}

impl std::fmt::Debug for NpcRoles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const NAMES: [(NpcRoles, &str); 13] = [
            (NpcRoles::MERCHANT, "merchant"),
            (NpcRoles::SPECIALTY, "specialty"),
            (NpcRoles::SKILL_TRAINER, "skill_trainer"),
            (NpcRoles::BANKER, "banker"),
            (NpcRoles::PRIEST, "priest"),
            (NpcRoles::TELEPORTER, "teleporter"),
            (NpcRoles::TRADER, "trader"),
            (NpcRoles::BLACKSMITH, "blacksmith"),
            (NpcRoles::AUCTIONEER, "auctioneer"),
            (NpcRoles::STABLER, "stabler"),
            (NpcRoles::REPAIRMAN, "repairman"),
            (NpcRoles::ABILITY_CHANGER, "ability_changer"),
            (NpcRoles::EXPEDITION, "expedition"),
        ];
        f.write_str("NpcRoles[")?;
        let mut first = true;
        for (role, name) in NAMES {
            if self.has(role) {
                if !first {
                    f.write_str("|")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        f.write_str("]")
    }
}

/// One NPC template.
#[derive(Clone, Debug, PartialEq)]
pub struct NpcTemplate {
    /// `npcs.id`.
    pub id: u32,
    /// English name (Korean if there is no translation).
    pub name: Box<str>,
    /// Level.
    pub level: u8,
    /// `npcs.faction_id`; feed it to `World::relation`.
    pub faction_id: u32,
    /// Raw `npc_kind_id` (in the data 1 is humanoid, 2 animal, 3+ other creature kinds).
    pub kind: u8,
    /// Raw `npc_grade_id` (1 is a normal NPC, higher grades are elites and bosses).
    pub grade: u8,
    /// Attacks players that come near (`npcs.aggression`).
    pub aggressive: bool,
    /// Service roles.
    pub roles: NpcRoles,
    /// Starts at least one quest (set after the quests are compiled).
    pub is_quest_giver: bool,
}

impl NpcTemplate {
    /// Sells goods.
    pub fn is_merchant(&self) -> bool {
        self.roles.has(NpcRoles::MERCHANT)
    }

    /// Buys trade packs.
    pub fn is_specialty(&self) -> bool {
        self.roles.has(NpcRoles::SPECIALTY)
    }

    /// Teaches skills.
    pub fn is_skill_trainer(&self) -> bool {
        self.roles.has(NpcRoles::SKILL_TRAINER)
    }

    /// Bank.
    pub fn is_banker(&self) -> bool {
        self.roles.has(NpcRoles::BANKER)
    }
}

/// Loads the templates.
pub fn load(conn: &Connection, texts: &Texts) -> Result<IdMap<NpcTemplate>> {
    let mut rows = Vec::new();
    each_row(
        conn,
        "SELECT id, name, level, faction_id, npc_kind_id, npc_grade_id, aggression, merchant, \
         specialty, skill_trainer, banker, priest, teleporter, trader, blacksmith, auctioneer, \
         stabler, repairman, ability_changer, expedition FROM npcs",
        |r| {
            let id = r.u32(0);
            let mut roles = NpcRoles::NONE;
            for (col, role) in [
                (7, NpcRoles::MERCHANT),
                (8, NpcRoles::SPECIALTY),
                (9, NpcRoles::SKILL_TRAINER),
                (10, NpcRoles::BANKER),
                (11, NpcRoles::PRIEST),
                (12, NpcRoles::TELEPORTER),
                (13, NpcRoles::TRADER),
                (14, NpcRoles::BLACKSMITH),
                (15, NpcRoles::AUCTIONEER),
                (16, NpcRoles::STABLER),
                (17, NpcRoles::REPAIRMAN),
                (18, NpcRoles::ABILITY_CHANGER),
                (19, NpcRoles::EXPEDITION),
            ] {
                if r.flag(col) {
                    roles = roles.with(role);
                }
            }
            rows.push((
                id,
                NpcTemplate {
                    id,
                    name: texts.name_or(TextTable::Npc, id, r.text(1)),
                    level: r.u8(2),
                    faction_id: r.u32(3),
                    kind: r.u8(4),
                    grade: r.u8(5),
                    aggressive: r.flag(6),
                    roles,
                    is_quest_giver: false,
                },
            ));
        },
    )?;
    Ok(IdMap::build(rows))
}

/// Heap bytes of the template map.
pub fn bytes(map: &IdMap<NpcTemplate>) -> usize {
    map.shallow_bytes() + map.values().iter().map(|n| n.name.len()).sum::<usize>()
}

/// Which NPCs join a fight together (`npc_aggro_links`): NPCs sharing an `aggro_link_id` aggro
/// as one pack, so a bot that pulls one gets all of them. Only the linked templates are kept
/// (572 of 13,382 in the 1.2 data).
#[derive(Debug, Default)]
pub struct AggroLinks {
    by_npc: FxHashMap<u32, SmallVec<[u32; 2]>>,
}

impl AggroLinks {
    /// Reads `npc_aggro_links`; a database without the table has no linked monsters.
    pub fn load(conn: &Connection) -> Result<AggroLinks> {
        let has: i64 = conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'npc_aggro_links'",
            [],
            |r| r.get(0),
        )?;
        if has == 0 {
            return Ok(AggroLinks::default());
        }
        let mut by_npc: FxHashMap<u32, SmallVec<[u32; 2]>> = FxHashMap::default();
        each_row(
            conn,
            "SELECT npc_id, aggro_link_id FROM npc_aggro_links ORDER BY id",
            |r| {
                let npc = r.u32(0);
                let link = r.u32(1);
                if npc != 0 && link != 0 {
                    by_npc.entry(npc).or_default().push(link);
                }
            },
        )?;
        Ok(AggroLinks { by_npc })
    }

    /// Whether pulling NPC `a` brings NPC `b` along: they share an `aggro_link_id`.
    pub fn linked(&self, a: u32, b: u32) -> bool {
        if a == b {
            return true;
        }
        let (Some(la), Some(lb)) = (self.by_npc.get(&a), self.by_npc.get(&b)) else {
            return false;
        };
        la.iter().any(|l| lb.contains(l))
    }

    /// Heap bytes held.
    pub fn bytes(&self) -> usize {
        self.by_npc.capacity() * (std::mem::size_of::<(u32, SmallVec<[u32; 2]>)>() + 8)
            + self.by_npc.values().map(|v| v.len() * 4).sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_roles_and_names() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE npcs (id INT, name TEXT, level INT, faction_id INT, npc_kind_id INT,
               npc_grade_id INT, aggression NUM, merchant NUM, specialty NUM, skill_trainer NUM,
               banker NUM, priest NUM, teleporter NUM, trader NUM, blacksmith NUM, auctioneer NUM,
               stabler NUM, repairman NUM, ability_changer NUM, expedition NUM);
             INSERT INTO npcs VALUES (14766, '프란체스코', 25, 1, 1, 1, 'f', 'f','t','f','f','f','f','f','f','f','f','f','f','f');
             INSERT INTO npcs VALUES (3475, '멧돼지', 2, 115, 2, 1, 't', 'f','f','f','f','f','f','f','f','f','f','f','f','f');",
        )
        .unwrap();
        let mut texts = Texts::empty();
        texts.insert(TextTable::Npc, 14766, "Francesco");
        let npcs = load(&conn, &texts).unwrap();
        let f = npcs.get(14766).unwrap();
        assert_eq!(&*f.name, "Francesco");
        assert!(f.is_specialty() && !f.is_merchant());
        assert_eq!((f.level, f.faction_id), (25, 1));
        let boar = npcs.get(3475).unwrap();
        assert_eq!(&*boar.name, "멧돼지");
        assert!(boar.aggressive && boar.roles.is_none());
        assert_eq!(format!("{:?}", f.roles), "NpcRoles[specialty]");
    }

    #[test]
    fn aggro_links_follow_shared_link_ids() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE npc_aggro_links (id INT, npc_id INT, aggro_link_id INT);
             INSERT INTO npc_aggro_links VALUES (1, 10, 7), (2, 11, 7), (3, 12, 8), (4, 10, 9), (5, 13, 9);",
        )
        .unwrap();
        let links = AggroLinks::load(&conn).unwrap();
        assert!(links.linked(10, 11), "same link 7");
        assert!(!links.linked(11, 12), "different links");
        assert!(links.linked(10, 10), "a monster always comes with itself");
        assert!(links.linked(10, 13), "10 also carries link 9");
        assert!(!links.linked(12, 13), "unlinked templates");
    }
}
