//! What using an item casts, and where that skill has to be cast.
//!
//! `items.use_skill_id` is the skill a client starts when the player uses the item (quest items,
//! books, fireworks, potions). Some of those skills only work at a certain place: the
//! `unit_reqs` rows owned by the skill name an NPC the target must be, a doodad that has to be
//! within a range or a quest sphere the caster has to stand in ([`SkillNeed`]).
//!
//! Doodad use skills live on [`DoodadTemplate::use_skill`](crate::DoodadTemplate::use_skill).

use anyhow::Result;
use rusqlite::Connection;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::geom::IdMap;
use crate::sql::{each_row, RowExt};

/// `unit_reqs.kind_id` of "the target is this NPC template".
const TARGET_NPC_KIND: i64 = 0x1B;
/// `unit_reqs.kind_id` of "a doodad of this template is within a range" (millimetres).
const DOODAD_RANGE_KIND: i64 = 0x14;
/// `unit_reqs.kind_id` of "the caster stands inside a quest sphere".
const AREA_SPHERE_KIND: i64 = 0x23;
/// `unit_reqs.kind_id` of "the target NPC is in this quest monster group" (`value2 != 0`
/// negates: the target must not be in the group).
const TARGET_NPC_GROUP_KIND: i64 = 0x22;

/// Where a skill has to be cast.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SkillNeed {
    /// At an NPC of this template.
    Npc(u32),
    /// Within this many metres of a doodad of this template.
    Doodad(u32, f32),
    /// Inside a sphere of the quest.
    Sphere,
    /// At an NPC of a quest monster group (`quest_monster_npcs`); `true` negates: the target
    /// must not be in the group (the server's `skill_urk_target_npc_group`).
    NpcGroup(u32, bool),
}

/// Restoration from an item skill that targets the user.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ItemRecovery {
    /// Direct health restoration.
    pub health: bool,
    /// Direct mana restoration.
    pub mana: bool,
    /// Buff that raises health regeneration, or 0.
    pub health_regen: u32,
    /// Buff that raises mana regeneration, or 0.
    pub mana_regen: u32,
}

/// The use skills of items and the placement rules of those skills.
#[derive(Debug, Default)]
pub struct ItemUses {
    /// `item -> items.use_skill_id` (only items that have one).
    skill: IdMap<u32>,
    /// Placement rules of the use skills (only skills some item casts that have rules).
    needs: FxHashMap<u32, Box<[SkillNeed]>>,
    /// `quest_monster_group_id -> NPC templates` (`quest_monster_npcs`), the groups a
    /// [`SkillNeed::NpcGroup`] names.
    groups: FxHashMap<u32, Box<[u32]>>,
    recovery: FxHashMap<u32, ItemRecovery>,
}

impl ItemUses {
    /// Reads `items.use_skill_id` and the placement `unit_reqs` of those skills.
    pub fn load(conn: &Connection) -> Result<ItemUses> {
        let mut rows: Vec<(u32, u32)> = Vec::new();
        each_row(
            conn,
            "SELECT id, use_skill_id FROM items WHERE use_skill_id != 0",
            |r| {
                if r.u32(1) != 0 {
                    rows.push((r.u32(0), r.u32(1)));
                }
            },
        )?;
        let item_skills: FxHashSet<u32> = rows.iter().map(|r| r.1).collect();
        let mut needs: FxHashMap<u32, Vec<SkillNeed>> = FxHashMap::default();
        each_row(
            conn,
            &format!(
                "SELECT owner_id, kind_id, value1, value2 FROM unit_reqs \
                 WHERE owner_type = 'Skill' AND kind_id IN ({TARGET_NPC_KIND}, {DOODAD_RANGE_KIND}, {AREA_SPHERE_KIND}, {TARGET_NPC_GROUP_KIND}) \
                 ORDER BY id"
            ),
            |r| {
                let skill = r.u32(0);
                if !item_skills.contains(&skill) {
                    return;
                }
                let need = match r.int(1) {
                    TARGET_NPC_KIND => SkillNeed::Npc(r.u32(2)),
                    TARGET_NPC_GROUP_KIND => SkillNeed::NpcGroup(r.u32(2), r.u32(3) != 0),
                    DOODAD_RANGE_KIND => SkillNeed::Doodad(r.u32(2), r.u32(3) as f32 / 1000.0),
                    _ => SkillNeed::Sphere,
                };
                needs.entry(skill).or_default().push(need);
            },
        )?;
        // The NPC groups of the target-group rules (the server's `group_npcs` is
        // `quest_monster_npcs`); only the groups an item skill names are kept.
        let wanted: FxHashSet<u32> = needs
            .values()
            .flatten()
            .filter_map(|n| match n {
                SkillNeed::NpcGroup(group, _) => Some(*group),
                _ => None,
            })
            .collect();
        let mut groups: FxHashMap<u32, Vec<u32>> = FxHashMap::default();
        // (an old database without the table has no target groups)
        let _ = each_row(
            conn,
            "SELECT quest_monster_group_id, npc_id FROM quest_monster_npcs ORDER BY id",
            |r| {
                let group = r.u32(0);
                if wanted.contains(&group) {
                    groups.entry(group).or_default().push(r.u32(1));
                }
            },
        );
        for v in groups.values_mut() {
            v.sort_unstable();
            v.dedup();
        }
        let mut recovery = FxHashMap::<u32, ItemRecovery>::default();
        let _ = each_row(
            conn,
            "SELECT DISTINCT s.id, e.actual_type FROM skills s
             JOIN skill_effects se ON se.skill_id = s.id
             JOIN effects e ON e.id = se.effect_id
             WHERE s.target_type_id = 0 AND s.target_selection_id = 1
             AND e.actual_type IN ('HealEffect', 'RestoreManaEffect')",
            |r| {
                let skill = r.u32(0);
                if item_skills.contains(&skill) {
                    let item = recovery.entry(skill).or_default();
                    match r.text(1) {
                        "HealEffect" => item.health = true,
                        "RestoreManaEffect" => item.mana = true,
                        _ => {}
                    }
                }
            },
        );
        let _ = each_row(
            conn,
            "SELECT DISTINCT s.id, b.id, m.unit_attribute_id FROM skills s
             JOIN skill_effects se ON se.skill_id = s.id
             JOIN effects e ON e.id = se.effect_id AND e.actual_type = 'BuffEffect'
             JOIN buff_effects be ON be.id = e.actual_id
             JOIN buffs b ON b.id = be.buff_id
             JOIN unit_modifiers m ON m.owner_type = 'Buff' AND m.owner_id = b.id
             WHERE s.target_type_id = 0 AND s.target_selection_id = 1 AND b.duration > 0
             AND m.unit_attribute_id IN (11, 12, 67, 68) AND m.value > 0
             ORDER BY b.id",
            |r| {
                let skill = r.u32(0);
                if item_skills.contains(&skill) {
                    let item = recovery.entry(skill).or_default();
                    let slot = match r.u32(2) {
                        11 | 67 => &mut item.health_regen,
                        _ => &mut item.mana_regen,
                    };
                    if *slot == 0 {
                        *slot = r.u32(1);
                    }
                }
            },
        );
        Ok(ItemUses {
            skill: IdMap::build(rows),
            needs: needs
                .into_iter()
                .map(|(s, v)| (s, v.into_boxed_slice()))
                .collect(),
            groups: groups
                .into_iter()
                .map(|(g, v)| (g, v.into_boxed_slice()))
                .collect(),
            recovery,
        })
    }

    /// The skill using item `item` casts, 0 when it has none.
    pub fn skill(&self, item: u32) -> u32 {
        self.skill.get(item).copied().unwrap_or(0)
    }

    pub fn recovery(&self, item: u32) -> ItemRecovery {
        self.recovery
            .get(&self.skill(item))
            .copied()
            .unwrap_or_default()
    }

    /// Where the item use skill `skill` must be cast; empty when it can be cast anywhere.
    pub fn needs(&self, skill: u32) -> &[SkillNeed] {
        self.needs.get(&skill).map_or(&[], |v| &v[..])
    }

    /// The NPC templates of quest monster group `group` (empty when the data has none).
    pub fn group_members(&self, group: u32) -> &[u32] {
        self.groups.get(&group).map_or(&[], |v| &v[..])
    }

    /// Number of items with a use skill.
    pub fn len(&self) -> usize {
        self.skill.len()
    }

    /// Whether no item has a use skill.
    pub fn is_empty(&self) -> bool {
        self.skill.is_empty()
    }

    /// Heap bytes held.
    pub fn bytes(&self) -> usize {
        self.skill.shallow_bytes()
            + self.recovery.capacity() * (std::mem::size_of::<(u32, ItemRecovery)>() + 1)
            + self
                .needs
                .values()
                .map(|v| v.len() * std::mem::size_of::<SkillNeed>() + 16 + 8)
                .sum::<usize>()
            + self
                .groups
                .values()
                .map(|v| v.len() * 4 + 16 + 8)
                .sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_skills_and_their_placement_rules_are_read() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE items (id INT, use_skill_id INT);
             CREATE TABLE unit_reqs (id INT, owner_id INT, owner_type TEXT, kind_id INT, value1 INT, value2 INT);
             CREATE TABLE quest_monster_npcs (id INT, quest_monster_group_id INT, npc_id INT);
             INSERT INTO items VALUES (7738, 11596), (4058, NULL), (99, 5), (100, 0);
             INSERT INTO unit_reqs VALUES
               (1, 11596, 'Skill', 27, 3681, 0),
               (2, 11596, 'Skill', 32, 97, 0),
               (3, 5, 'Skill', 27, 99, 0),
               (4, 11596, 'QuestComponent', 27, 7, 0),
               (5, 11596, 'Skill', 20, 1430, 30000),
               (6, 11596, 'Skill', 35, 12, 0),
               (7, 777, 'Skill', 27, 1, 0),
               (8, 11596, 'Skill', 34, 154, 0),
               (9, 11596, 'Skill', 34, 200, 1);
             INSERT INTO quest_monster_npcs VALUES
               (1, 154, 3463), (2, 154, 3470), (3, 200, 7976);",
        )
        .unwrap();
        let u = ItemUses::load(&conn).unwrap();
        assert_eq!(u.len(), 2);
        assert_eq!(u.skill(7738), 11596);
        assert_eq!(u.skill(4058), 0, "NULL means no skill");
        assert_eq!(u.skill(100), 0);
        assert_eq!(u.skill(1), 0, "unknown items have none");
        // where an item skill must be cast; other kinds and owners are ignored
        assert_eq!(
            u.needs(11596),
            &[
                SkillNeed::Npc(3681),
                SkillNeed::Doodad(1430, 30.0),
                SkillNeed::Sphere,
                SkillNeed::NpcGroup(154, false),
                SkillNeed::NpcGroup(200, true),
            ]
        );
        assert_eq!(u.group_members(154), &[3463, 3470]);
        assert_eq!(u.group_members(200), &[7976]);
        assert!(u.group_members(999).is_empty());
        assert_eq!(u.needs(5), &[SkillNeed::Npc(99)]);
        assert!(u.needs(777).is_empty(), "no item casts skill 777");
        assert!(u.needs(1).is_empty());
        assert!(u.bytes() > 0);
    }

    #[test]
    fn item_recovery_reads_direct_effects_and_self_regeneration() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE items (id INT, use_skill_id INT);
             CREATE TABLE unit_reqs (id INT, owner_id INT, owner_type TEXT, kind_id INT, value1 INT, value2 INT);
             CREATE TABLE skills (id INT, target_type_id INT, target_selection_id INT);
             CREATE TABLE skill_effects (skill_id INT, effect_id INT);
             CREATE TABLE effects (id INT, actual_type TEXT, actual_id INT);
             CREATE TABLE buff_effects (id INT, buff_id INT);
             CREATE TABLE buffs (id INT, duration INT);
             CREATE TABLE unit_modifiers (owner_type TEXT, owner_id INT, unit_attribute_id INT, value INT);
             INSERT INTO items VALUES (28554, 22773), (8518, 11718), (1, 100), (2, 101), (3, 102);
             INSERT INTO skills VALUES (22773, 0, 1), (11718, 0, 1), (100, 5, 2), (101, 0, 1), (102, 0, 1);
             INSERT INTO skill_effects VALUES (22773, 1), (11718, 2), (100, 1), (101, 3), (102, 4);
             INSERT INTO effects VALUES (1, 'BuffEffect', 60), (2, 'HealEffect', 0),
               (3, 'BuffEffect', 61), (4, 'BuffEffect', 62);
             INSERT INTO buff_effects VALUES (60, 6303), (61, 6304), (62, 6305);
             INSERT INTO buffs VALUES (6303, 10000), (6304, 10000), (6305, 0);
             INSERT INTO unit_modifiers VALUES ('Buff', 6303, 67, 15), ('Buff', 6303, 68, 15),
               ('Buff', 6304, 11, -15), ('Buff', 6304, 8, 15), ('Item', 6304, 12, 15),
               ('Buff', 6305, 11, 15);",
        )
        .unwrap();
        let uses = ItemUses::load(&conn).unwrap();
        assert_eq!(
            uses.recovery(28554),
            ItemRecovery {
                health_regen: 6303,
                mana_regen: 6303,
                ..ItemRecovery::default()
            }
        );
        assert_eq!(
            uses.recovery(8518),
            ItemRecovery {
                health: true,
                ..ItemRecovery::default()
            }
        );
        for item in [1, 2, 3] {
            assert_eq!(uses.recovery(item), ItemRecovery::default());
        }
    }
}
