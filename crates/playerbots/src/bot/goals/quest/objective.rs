//! The objectives of an active quest: which one is next and how a bot can do it.
//!
//! The server keeps one counter per objective act in `QuestRow::objectives`, numbered over all
//! progress components of the quest in component order (`objective_index` in the server's
//! `update_quest_component_acts`), which is exactly the order of [`Quest::progress`] flattened.
//! A counter reaching [`need`] meets the objective; a quest whose objectives are all met turns
//! `READY` by itself.
//!
//! [`plan`] decides how one objective is done ([`Plan`]) or why it cannot be ([`Failure`]); it
//! only asks the [`Facts`] it is given, so it is tested with a fake world.

use aa_proto::sc_activity::QuestRow;
use aa_world::{ObjKind, Objective, Quest, QuestReport, Vec3, World};

use super::memory::Failure;

/// Objectives this far above the bot are not attempted (the bot would die).
pub const MAX_LEVEL_GAP: u8 = 6;
/// `npcs.npc_grade_id` from which a kill target is a boss or elite; the bots have no boss logic.
pub const BOSS_GRADE: u8 = 5;
/// A boss/elite this many levels above the bot is not attempted (it would die).
pub const BOSS_LEVEL_SLACK: u8 = 1;
/// Most objective counters a quest row carries.
pub const MAX_SLOTS: usize = 5;

/// What the planner needs to know about the static world.
pub trait Facts {
    /// Whether the NPC template spawns anywhere.
    fn npc_spawns(&self, template: u32) -> bool;
    /// Whether the doodad template spawns anywhere.
    fn doodad_spawns(&self, template: u32) -> bool;
    /// The level of an NPC template.
    fn npc_level(&self, template: u32) -> Option<u8>;
    /// `npcs.npc_grade_id` of an NPC template (1 normal, higher grades elites and bosses).
    fn npc_grade(&self, template: u32) -> u8;
    /// Whether a spawn of the template can be walked to from the place the bot checks at.
    fn npc_reachable(&self, template: u32) -> bool;
    /// Doodad templates (that spawn) which yield `item` when used.
    fn gather_doodads(&self, item: u32) -> &[u32];
    /// Centre and radius of the sign sphere of a quest component.
    fn sphere(&self, quest: u32, component: u32) -> Option<(f32, f32, f32)>;
    /// The skill that uses a doodad, 0 if unknown.
    fn doodad_skill(&self, doodad: u32) -> u32;
    /// The skill an item casts, 0 if unknown.
    fn item_skill(&self, item: u32) -> u32;
}

/// The real [`Facts`]: the static world.
#[derive(Clone, Copy)]
pub struct Env<'a> {
    /// The static world.
    pub world: &'a World,
    /// Where the bot stands when it judges a quest; `None` skips the navigation check (the
    /// atlas is built without a bot position).
    pub from: Option<Vec3>,
    /// Whether the bot walks on the navigation mesh; without it every spawn is reachable.
    pub navmesh: bool,
}

impl Facts for Env<'_> {
    fn npc_spawns(&self, template: u32) -> bool {
        self.world.npc_spawns_anywhere(template)
    }

    fn doodad_spawns(&self, template: u32) -> bool {
        self.world.doodad_spawns_anywhere(template)
    }

    fn npc_level(&self, template: u32) -> Option<u8> {
        self.world.npc(template).map(|t| t.level)
    }

    fn npc_grade(&self, template: u32) -> u8 {
        self.world.npc(template).map_or(1, |t| t.grade)
    }

    fn npc_reachable(&self, template: u32) -> bool {
        self.from
            .is_none_or(|from| !self.navmesh || self.world.npc_reachable(template, from))
    }

    fn gather_doodads(&self, item: u32) -> &[u32] {
        self.world.gather_doodads(item)
    }

    fn sphere(&self, quest: u32, component: u32) -> Option<(f32, f32, f32)> {
        self.world
            .quest_sphere(quest, component)
            .map(|s| (s.x, s.y, s.radius))
    }

    fn doodad_skill(&self, doodad: u32) -> u32 {
        self.world.doodad_use_skill(doodad)
    }

    fn item_skill(&self, item: u32) -> u32 {
        self.world.item_use_skill(item)
    }
}

/// How one objective is done.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Plan<'w> {
    /// Talk to this NPC template (`CSQuestTalkMade`).
    Talk {
        /// NPC template.
        npc: u32,
    },
    /// Kill monsters of these templates.
    Kill {
        /// NPC templates that count.
        templates: &'w [u32],
    },
    /// Kill monsters of these templates and loot `item` from them.
    Drops {
        /// NPC templates that drop the item, best drop rate first.
        templates: &'w [u32],
        /// The wanted item.
        item: u32,
    },
    /// Use one of these doodad templates to get `item`.
    Harvest {
        /// Doodad templates that yield the item.
        doodads: &'w [u32],
        /// The wanted item.
        item: u32,
    },
    /// Use this doodad template with its skill.
    Interact {
        /// Doodad template.
        doodad: u32,
        /// Skill that uses it.
        skill: u32,
    },
    /// Use this item from the bag with its skill.
    UseItem {
        /// Item template.
        item: u32,
        /// Skill the item casts.
        skill: u32,
    },
    /// The quest hands the items over when it starts; wait for the server's counter.
    Supplied,
    /// Stand inside this sphere (x, y, radius).
    Sphere {
        /// Centre x.
        x: f32,
        /// Centre y.
        y: f32,
        /// Radius.
        radius: f32,
    },
}

/// The counter value that meets an objective.
pub fn need(kind: &ObjKind) -> i32 {
    let n = match kind {
        ObjKind::Hunt { count, .. }
        | ObjKind::Gather { count, .. }
        | ObjKind::UseItem { count, .. }
        | ObjKind::Interact { count, .. }
        | ObjKind::Craft { count, .. } => *count,
        ObjKind::Talk { .. }
        | ObjKind::Sphere { .. }
        | ObjKind::Level { .. }
        | ObjKind::Other(_) => 1,
    };
    i32::try_from(n.max(1)).unwrap_or(i32::MAX)
}

/// The objective that comes next: its position in the quest, the objective and its counters.
#[derive(Debug, Clone, Copy)]
pub struct Slot<'w> {
    /// Counter index (position over all progress objectives).
    pub idx: usize,
    /// The objective.
    pub obj: &'w Objective,
    /// Counter value that meets it.
    pub need: i32,
    /// Counter value now.
    pub have: i32,
}

/// The counter of objective `idx` in a quest row.
pub fn have(row: &QuestRow, idx: usize) -> i32 {
    row.objectives.get(idx).copied().unwrap_or(0)
}

/// The first objective of `quest` whose counter has not reached its target and whose bit is not
/// set in `skip` (bit `i` = counter index `i`).
pub fn next_unmet<'w>(quest: &'w Quest, row: &QuestRow, skip: u8) -> Option<Slot<'w>> {
    unmet(quest, row).find(|s| s.idx >= MAX_SLOTS || skip & (1 << s.idx) == 0)
}

/// All objectives that are not met yet, in order.
pub fn unmet<'w>(quest: &'w Quest, row: &QuestRow) -> impl Iterator<Item = Slot<'w>> {
    let row = *row;
    quest
        .progress
        .iter()
        .flat_map(|step| step.objectives.iter())
        .enumerate()
        .filter_map(move |(idx, obj)| {
            let need = need(&obj.kind);
            let have = have(&row, idx);
            (have < need).then_some(Slot {
                idx,
                obj,
                need,
                have,
            })
        })
}

/// Decides how `obj` of `quest` is done by a bot of `own_level`.
pub fn plan<'w>(
    facts: &'w dyn Facts,
    quest: &Quest,
    obj: &'w Objective,
    own_level: u8,
) -> Result<Plan<'w>, Failure> {
    match &obj.kind {
        ObjKind::Talk { npc } => {
            if facts.npc_spawns(*npc) {
                Ok(Plan::Talk { npc: *npc })
            } else {
                Err(Failure::Unsupported("talk target does not spawn"))
            }
        }
        ObjKind::Hunt { npcs, .. } => {
            kill_plan(facts, npcs, own_level).map(|templates| Plan::Kill { templates })
        }
        ObjKind::Gather {
            item,
            count,
            droppers,
        } => {
            if quest
                .supplies
                .iter()
                .any(|&(i, c)| i == *item && c >= *count)
            {
                return Ok(Plan::Supplied);
            }
            if !droppers.is_empty() {
                return kill_plan(facts, droppers, own_level).map(|templates| Plan::Drops {
                    templates,
                    item: *item,
                });
            }
            let doodads = facts.gather_doodads(*item);
            if doodads.iter().any(|d| facts.doodad_skill(*d) != 0) {
                Ok(Plan::Harvest {
                    doodads,
                    item: *item,
                })
            } else if doodads.is_empty() {
                Err(Failure::Unsupported("item source unknown"))
            } else {
                Err(Failure::Unsupported("gather doodad skill unknown"))
            }
        }
        ObjKind::UseItem { item, .. } => match facts.item_skill(*item) {
            0 => Err(Failure::Unsupported("item skill unknown")),
            skill => Ok(Plan::UseItem { item: *item, skill }),
        },
        ObjKind::Interact { doodad, .. } => {
            if !facts.doodad_spawns(*doodad) {
                return Err(Failure::Unsupported("doodad does not spawn"));
            }
            match facts.doodad_skill(*doodad) {
                0 => Err(Failure::Unsupported("doodad skill unknown")),
                skill => Ok(Plan::Interact {
                    doodad: *doodad,
                    skill,
                }),
            }
        }
        ObjKind::Sphere { .. } => match facts.sphere(quest.id, obj.component) {
            Some((x, y, radius)) => Ok(Plan::Sphere { x, y, radius }),
            None => Err(Failure::Unsupported("sphere position unknown")),
        },
        ObjKind::Craft { .. } => Err(Failure::Unsupported("craft objective")),
        ObjKind::Level { .. } => Err(Failure::Unsupported("level objective")),
        ObjKind::Other(_) => Err(Failure::Unsupported("other objective")),
    }
}

fn kill_plan<'w>(
    facts: &dyn Facts,
    templates: &'w [u32],
    own_level: u8,
) -> Result<&'w [u32], Failure> {
    let mut lowest: Option<u8> = None;
    let (mut spawned, mut reachable, mut bosses) = (0u32, 0u32, 0u32);
    for t in templates {
        if !facts.npc_spawns(*t) {
            continue;
        }
        spawned += 1;
        let level = facts.npc_level(*t).unwrap_or(0);
        // A boss or elite the bot cannot fight at its level: skip it, like the combat rules do.
        if facts.npc_grade(*t) >= BOSS_GRADE && level > own_level.saturating_add(BOSS_LEVEL_SLACK) {
            bosses += 1;
            continue;
        }
        if !facts.npc_reachable(*t) {
            continue;
        }
        reachable += 1;
        lowest = Some(lowest.map_or(level, |l| l.min(level)));
    }
    match lowest {
        None if bosses > 0 => Err(Failure::TooStrong),
        None if spawned > 0 && reachable == 0 => Err(Failure::Unreachable),
        None if reachable > 0 => Err(Failure::TooStrong),
        None => Err(Failure::Unsupported("monsters do not spawn")),
        Some(l) if l > own_level.saturating_add(MAX_LEVEL_GAP) => Err(Failure::TooStrong),
        Some(_) => Ok(templates),
    }
}

/// Whether the whole of `quest` can be done: every objective has a plan and the turn-in target
/// exists. `own_level` only matters for [`Failure::TooStrong`].
pub fn supported(facts: &dyn Facts, quest: &Quest, own_level: u8) -> Result<(), Failure> {
    for step in quest.progress.iter() {
        for obj in step.objectives.iter() {
            plan(facts, quest, obj, own_level)?;
        }
    }
    report_supported(facts, quest)
}

/// Whether the quest can be turned in.
pub fn report_supported(facts: &dyn Facts, quest: &Quest) -> Result<(), Failure> {
    match quest.report {
        QuestReport::Npc(t) if !facts.npc_spawns(t) => {
            Err(Failure::Unsupported("turn-in NPC does not spawn"))
        }
        QuestReport::Doodad(t) if !facts.doodad_spawns(t) => {
            Err(Failure::Unsupported("turn-in doodad does not spawn"))
        }
        QuestReport::Unknown => Err(Failure::Unsupported("no turn-in target")),
        _ => Ok(()),
    }
}

/// The 1-based reward index to select when the quest offers a choice, 0 otherwise.
pub fn reward_choice(quest: &Quest) -> i32 {
    i32::from(quest.selective_rewards > 0)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use aa_world::{QuestStart, QuestStep};
    use rustc_hash::{FxHashMap, FxHashSet};

    /// A hand-made world.
    #[derive(Default)]
    pub struct Fake {
        pub npcs: FxHashMap<u32, u8>,
        pub grades: FxHashMap<u32, u8>,
        pub unreachable: FxHashSet<u32>,
        pub doodads: FxHashSet<u32>,
        pub gather: FxHashMap<u32, Vec<u32>>,
        pub spheres: FxHashMap<(u32, u32), (f32, f32, f32)>,
        pub doodad_skills: FxHashMap<u32, u32>,
        pub item_skills: FxHashMap<u32, u32>,
    }

    impl Facts for Fake {
        fn npc_spawns(&self, t: u32) -> bool {
            self.npcs.contains_key(&t)
        }
        fn doodad_spawns(&self, t: u32) -> bool {
            self.doodads.contains(&t)
        }
        fn npc_level(&self, t: u32) -> Option<u8> {
            self.npcs.get(&t).copied()
        }
        fn npc_grade(&self, t: u32) -> u8 {
            self.grades.get(&t).copied().unwrap_or(1)
        }
        fn npc_reachable(&self, t: u32) -> bool {
            !self.unreachable.contains(&t)
        }
        fn gather_doodads(&self, item: u32) -> &[u32] {
            self.gather.get(&item).map_or(&[], |v| &v[..])
        }
        fn sphere(&self, q: u32, c: u32) -> Option<(f32, f32, f32)> {
            self.spheres.get(&(q, c)).copied()
        }
        fn doodad_skill(&self, d: u32) -> u32 {
            self.doodad_skills.get(&d).copied().unwrap_or(0)
        }
        fn item_skill(&self, i: u32) -> u32 {
            self.item_skills.get(&i).copied().unwrap_or(0)
        }
    }

    pub fn obj(component: u32, act: u32, kind: ObjKind) -> Objective {
        Objective {
            component,
            act,
            kind,
        }
    }

    /// A quest with one progress step per objective group given.
    pub fn quest(id: u32, steps: Vec<Vec<Objective>>) -> Quest {
        Quest {
            id,
            name: format!("Quest {id}").into(),
            level: 1,
            category: 1,
            detail: 1,
            zone_id: 0,
            chapter_idx: 0,
            quest_idx: 0,
            repeatable: false,
            story: false,
            start: QuestStart::Npc(1),
            starts: Box::new([QuestStart::Npc(1)]),
            start_component: 0,
            after: Box::new([]),
            kill_start: Box::new([]),
            supplies: Box::new([]),
            progress: steps
                .into_iter()
                .enumerate()
                .map(|(i, objectives)| QuestStep {
                    component: 100 + i as u32,
                    objectives: objectives.into_boxed_slice(),
                })
                .collect(),
            report: QuestReport::Npc(2),
            selective_rewards: 0,
        }
    }

    pub fn row(quest: u32, status: u8, objectives: [i32; 5]) -> QuestRow {
        QuestRow {
            id: 1,
            template: quest,
            status,
            objectives,
            left_time_ms: -1,
            component: 0,
            acceptor_type: 0,
            acceptor_id: 0,
        }
    }

    fn hunt(npcs: &[u32], count: u32) -> ObjKind {
        ObjKind::Hunt {
            npcs: npcs.into(),
            count,
        }
    }

    #[test]
    fn counters_follow_the_flattened_objectives() {
        let q = quest(
            1,
            vec![
                vec![
                    obj(10, 1, hunt(&[5], 3)),
                    obj(10, 2, ObjKind::Talk { npc: 9 }),
                ],
                vec![obj(11, 3, hunt(&[6], 2))],
            ],
        );
        let r = row(1, 1, [3, 0, 0, 0, 0]);
        let s = next_unmet(&q, &r, 0).unwrap();
        assert_eq!((s.idx, s.need, s.have), (1, 1, 0));
        assert!(matches!(s.obj.kind, ObjKind::Talk { npc: 9 }));
        let r = row(1, 1, [3, 1, 1, 0, 0]);
        let s = next_unmet(&q, &r, 0).unwrap();
        assert_eq!((s.idx, s.need, s.have), (2, 2, 1));
        let r = row(1, 1, [3, 1, 2, 0, 0]);
        assert!(next_unmet(&q, &r, 0).is_none());
        assert_eq!(unmet(&q, &row(1, 1, [0; 5])).count(), 3);
    }

    #[test]
    fn skipped_objectives_are_passed_over() {
        let q = quest(
            1,
            vec![vec![
                obj(10, 1, hunt(&[5], 3)),
                obj(10, 2, ObjKind::Talk { npc: 9 }),
            ]],
        );
        let r = row(1, 1, [0; 5]);
        assert_eq!(next_unmet(&q, &r, 0b01).unwrap().idx, 1);
        assert_eq!(next_unmet(&q, &r, 0b10).unwrap().idx, 0);
        assert!(next_unmet(&q, &r, 0b11).is_none());
    }

    #[test]
    fn objective_needs() {
        assert_eq!(need(&ObjKind::Talk { npc: 1 }), 1);
        assert_eq!(need(&hunt(&[1], 3)), 3);
        assert_eq!(need(&hunt(&[1], 0)), 1);
        assert_eq!(need(&ObjKind::Level { level: 9 }), 1);
        assert_eq!(need(&ObjKind::Other("QuestActObjZoneKill".into())), 1);
    }

    #[test]
    fn talk_and_hunt_need_spawns() {
        let mut f = Fake::default();
        let q = quest(1, vec![]);
        let talk = obj(1, 1, ObjKind::Talk { npc: 9 });
        assert_eq!(
            plan(&f, &q, &talk, 1),
            Err(Failure::Unsupported("talk target does not spawn"))
        );
        f.npcs.insert(9, 1);
        assert_eq!(plan(&f, &q, &talk, 1), Ok(Plan::Talk { npc: 9 }));
        let kill = obj(1, 2, hunt(&[5, 6], 3));
        assert_eq!(
            plan(&f, &q, &kill, 1),
            Err(Failure::Unsupported("monsters do not spawn"))
        );
        f.npcs.insert(6, 2);
        assert!(
            matches!(plan(&f, &q, &kill, 1), Ok(Plan::Kill { templates }) if templates == [5, 6])
        );
    }

    #[test]
    fn monsters_far_above_the_bot_are_too_strong() {
        let mut f = Fake::default();
        f.npcs.insert(5, 20);
        let q = quest(1, vec![]);
        let kill = obj(1, 2, hunt(&[5], 3));
        assert_eq!(plan(&f, &q, &kill, 1), Err(Failure::TooStrong));
        assert_eq!(plan(&f, &q, &kill, 14), Ok(Plan::Kill { templates: &[5] }));
        assert_eq!(plan(&f, &q, &kill, 13), Err(Failure::TooStrong));
    }

    #[test]
    fn a_boss_far_above_the_bot_is_skipped() {
        let mut f = Fake::default();
        f.npcs.insert(4227, 10);
        f.grades.insert(4227, 7);
        let q = quest(1, vec![]);
        let kill = obj(1, 2, hunt(&[4227], 1));
        // Quest 2328: a grade-7 boss two levels above a level-8 bot.
        assert_eq!(plan(&f, &q, &kill, 8), Err(Failure::TooStrong));
        // The same boss is not "far above" a level-20 bot.
        assert!(matches!(
            plan(&f, &q, &kill, 20),
            Ok(Plan::Kill { templates }) if templates == [4227]
        ));
    }

    #[test]
    fn monsters_off_the_navigation_mesh_are_unreachable() {
        let mut f = Fake::default();
        f.npcs.insert(5, 3);
        f.unreachable.insert(5);
        let q = quest(1, vec![]);
        let kill = obj(1, 2, hunt(&[5], 1));
        assert_eq!(plan(&f, &q, &kill, 5), Err(Failure::Unreachable));
        f.unreachable.clear();
        assert!(matches!(plan(&f, &q, &kill, 5), Ok(Plan::Kill { .. })));
    }

    #[test]
    fn gather_prefers_droppers_then_doodads() {
        let mut f = Fake::default();
        let q = quest(1, vec![]);
        let with_dropper = obj(
            1,
            1,
            ObjKind::Gather {
                item: 4058,
                count: 3,
                droppers: Box::new([3475]),
            },
        );
        f.npcs.insert(3475, 2);
        assert_eq!(
            plan(&f, &q, &with_dropper, 1),
            Ok(Plan::Drops {
                templates: &[3475],
                item: 4058
            })
        );
        let no_source = obj(
            1,
            2,
            ObjKind::Gather {
                item: 77,
                count: 1,
                droppers: Box::new([]),
            },
        );
        assert_eq!(
            plan(&f, &q, &no_source, 1),
            Err(Failure::Unsupported("item source unknown"))
        );
        f.gather.insert(77, vec![1598]);
        assert_eq!(
            plan(&f, &q, &no_source, 1),
            Err(Failure::Unsupported("gather doodad skill unknown"))
        );
        f.doodad_skills.insert(1598, 5);
        assert_eq!(
            plan(&f, &q, &no_source, 1),
            Ok(Plan::Harvest {
                doodads: &[1598],
                item: 77
            })
        );
    }

    #[test]
    fn items_the_quest_supplies_need_no_gathering() {
        let f = Fake::default();
        let mut q = quest(1, vec![]);
        q.supplies = Box::new([(4058, 3)]);
        let gather = |count| {
            obj(
                1,
                1,
                ObjKind::Gather {
                    item: 4058,
                    count,
                    droppers: Box::new([]),
                },
            )
        };
        assert_eq!(plan(&f, &q, &gather(3), 1), Ok(Plan::Supplied));
        assert_eq!(plan(&f, &q, &gather(2), 1), Ok(Plan::Supplied));
        assert_eq!(
            plan(&f, &q, &gather(4), 1),
            Err(Failure::Unsupported("item source unknown"))
        );
    }

    #[test]
    fn items_doodads_and_spheres_need_their_data() {
        let mut f = Fake::default();
        let q = quest(7, vec![]);
        let item = obj(
            1,
            1,
            ObjKind::UseItem {
                item: 7738,
                count: 1,
            },
        );
        assert_eq!(
            plan(&f, &q, &item, 1),
            Err(Failure::Unsupported("item skill unknown"))
        );
        f.item_skills.insert(7738, 11596);
        assert_eq!(
            plan(&f, &q, &item, 1),
            Ok(Plan::UseItem {
                item: 7738,
                skill: 11596
            })
        );
        let dood = obj(
            2,
            2,
            ObjKind::Interact {
                doodad: 1451,
                count: 1,
            },
        );
        assert_eq!(
            plan(&f, &q, &dood, 1),
            Err(Failure::Unsupported("doodad does not spawn"))
        );
        f.doodads.insert(1451);
        assert_eq!(
            plan(&f, &q, &dood, 1),
            Err(Failure::Unsupported("doodad skill unknown"))
        );
        f.doodad_skills.insert(1451, 14907);
        assert_eq!(
            plan(&f, &q, &dood, 1),
            Ok(Plan::Interact {
                doodad: 1451,
                skill: 14907
            })
        );
        let sph = obj(3, 3, ObjKind::Sphere { sphere: 9 });
        assert_eq!(
            plan(&f, &q, &sph, 1),
            Err(Failure::Unsupported("sphere position unknown"))
        );
        f.spheres.insert((7, 3), (10.0, 20.0, 5.0));
        assert_eq!(
            plan(&f, &q, &sph, 1),
            Ok(Plan::Sphere {
                x: 10.0,
                y: 20.0,
                radius: 5.0
            })
        );
    }

    #[test]
    fn craft_level_and_other_objectives_are_unsupported() {
        let f = Fake::default();
        let q = quest(1, vec![]);
        for kind in [
            ObjKind::Craft { craft: 1, count: 1 },
            ObjKind::Level { level: 5 },
            ObjKind::Other("QuestActObjExpressFire".into()),
        ] {
            let o = obj(1, 1, kind);
            assert!(matches!(plan(&f, &q, &o, 50), Err(Failure::Unsupported(_))));
        }
    }

    #[test]
    fn a_quest_is_supported_when_all_its_parts_are() {
        let mut f = Fake::default();
        f.npcs.insert(2, 1);
        f.npcs.insert(9, 1);
        let mut q = quest(1, vec![vec![obj(1, 1, ObjKind::Talk { npc: 9 })]]);
        assert_eq!(supported(&f, &q, 1), Ok(()));
        q.report = QuestReport::Npc(77);
        assert_eq!(
            supported(&f, &q, 1),
            Err(Failure::Unsupported("turn-in NPC does not spawn"))
        );
        q.report = QuestReport::Unknown;
        assert_eq!(
            supported(&f, &q, 1),
            Err(Failure::Unsupported("no turn-in target"))
        );
        q.report = QuestReport::Auto;
        assert_eq!(supported(&f, &q, 1), Ok(()));
        q.progress = Box::new([QuestStep {
            component: 1,
            objectives: Box::new([obj(1, 1, ObjKind::Level { level: 9 })]),
        }]);
        assert!(supported(&f, &q, 1).is_err());
    }

    /// How many of the real quests a bot can do, by the reason it cannot. Run with
    /// `AA_COMPACT`, `AA_SERVER_DATA` (and `AA_BAKED`) set and `--ignored --nocapture`.
    #[test]
    #[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
    fn coverage_of_the_real_quests() {
        use aa_world::WorldPaths;
        use std::collections::BTreeMap;
        use std::path::PathBuf;
        let (Some(compact), Some(server_data)) = (
            std::env::var_os("AA_COMPACT"),
            std::env::var_os("AA_SERVER_DATA"),
        ) else {
            eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
            return;
        };
        let compact = PathBuf::from(compact);
        let world = World::load(&WorldPaths {
            compact,
            server_data: PathBuf::from(server_data),
            baked: std::env::var_os("AA_BAKED").map(PathBuf::from),
            world: "main_world".into(),
        })
        .unwrap();
        let facts = Env {
            world: &world,
            from: None,
            navmesh: false,
        };
        let mut by_band: BTreeMap<u8, (u32, u32)> = BTreeMap::new();
        let mut reasons: BTreeMap<String, u32> = BTreeMap::new();
        for q in world.quests() {
            if q.level > 30
                || !q
                    .starts
                    .iter()
                    .any(|s| matches!(s, QuestStart::Npc(_) | QuestStart::Doodad(_)))
            {
                continue;
            }
            let band = q.level / 5 * 5;
            let e = by_band.entry(band).or_default();
            e.0 += 1;
            match supported(&facts, q, 50) {
                Ok(()) => e.1 += 1,
                Err(why) => *reasons.entry(why.to_string()).or_default() += 1,
            }
        }
        for (band, (all, ok)) in &by_band {
            println!("levels {band}-{}: {ok}/{all} quests supported", band + 4);
        }
        let mut r: Vec<_> = reasons.into_iter().collect();
        r.sort_by_key(|x| std::cmp::Reverse(x.1));
        for (why, n) in r.iter().take(12) {
            println!("{n:5} {why}");
        }
    }

    /// The audit's accept-time failures on the real data: bosses and spawns off the mesh.
    #[test]
    #[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
    fn real_boss_and_off_mesh_quests_are_refused() {
        use aa_world::WorldPaths;
        use std::path::PathBuf;
        let (Some(compact), Some(server_data)) = (
            std::env::var_os("AA_COMPACT"),
            std::env::var_os("AA_SERVER_DATA"),
        ) else {
            eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
            return;
        };
        let world = World::load(&WorldPaths {
            compact: PathBuf::from(compact),
            server_data: PathBuf::from(server_data),
            baked: std::env::var_os("AA_BAKED").map(PathBuf::from),
            world: "main_world".into(),
        })
        .unwrap();
        let first_giver = |q: u32| {
            let quest = world.quest(q).expect("quest in the data");
            quest.starts.iter().find_map(|s| match s {
                QuestStart::Npc(t) => world.npc_spawns(*t).first().copied(),
                QuestStart::Doodad(t) => world.doodad_spawns(*t).first().copied(),
                _ => None,
            })
        };
        // 2328 "Into the Deep": a grade-7 boss in Aubre Cradle.
        let q = world.quest(2328).unwrap();
        let facts = Env {
            world: &world,
            from: Some(first_giver(2328).unwrap()),
            navmesh: false,
        };
        assert_eq!(supported(&facts, q, q.level), Err(Failure::TooStrong));
        // 1098 / 1593: the audit called their spawns unreachable; the shipped navigation mesh
        // routes to them from the giver, so they are not skipped on this data (the reachability
        // check refuses a quest only when the mesh says so). The unit test above proves the
        // rule; a live check of the two quests is still pending.
        if world.nav().is_some() {
            for quest in [1098u32, 1593] {
                let from = first_giver(quest).expect("a giver");
                let target = world
                    .quest(quest)
                    .unwrap()
                    .progress
                    .iter()
                    .flat_map(|s| s.objectives.iter())
                    .find_map(|o| match &o.kind {
                        ObjKind::Hunt { npcs, .. } => npcs.first().copied(),
                        _ => None,
                    })
                    .expect("a hunt target");
                assert!(
                    world.npc_reachable(target, from),
                    "quest {quest}: the shipped mesh says the target is reachable"
                );
            }
        } else {
            eprintln!("AA_BAKED not set: skipping the navigation check");
        }
    }

    #[test]
    fn rewards_are_one_based() {
        let mut q = quest(1, vec![]);
        assert_eq!(reward_choice(&q), 0);
        q.selective_rewards = 3;
        assert_eq!(reward_choice(&q), 1);
    }
}
