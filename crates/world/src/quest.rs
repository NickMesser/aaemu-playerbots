//! Quests compiled for bots from `quest_contexts` / `quest_components` / `quest_acts` and the
//! `quest_act_*` detail tables.
//!
//! The reference is the server's own loader (AAEmu Rust `systems/quests/data.rs`,
//! GPL-3.0-or-later): tutorial quests (category 45) are not quests, components of a quest are
//! ordered by `(component_kind_id, id)`, acts by `(component, id)`, and acts that point at a
//! component the database does not hold are ignored by the server (about a quarter of
//! `quest_acts` in the 1.2 data), so they are ignored here too.
//!
//! Component kinds: 2 Start, 3 Supply, 4 Progress, 5 Fail, 6 Ready, 7 Drop, 8 Reward.

use anyhow::Result;
use rusqlite::Connection;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::geom::IdMap;
use crate::sql::{each_row, RowExt};
use crate::text::{TextTable, Texts};

/// `quest_categories.id` of the tutorial "quests" the server does not treat as quests.
pub const CATEGORY_TUTORIAL: u32 = 45;

const KIND_START: u8 = 2;
const KIND_SUPPLY: u8 = 3;
const KIND_PROGRESS: u8 = 4;
const KIND_READY: u8 = 6;

/// How a quest begins.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum QuestStart {
    /// Talk to this NPC template.
    Npc(u32),
    /// Use this doodad template.
    Doodad(u32),
    /// Obtain / use this item template.
    Item(u32),
    /// Enter this sphere (`spheres.id`); the position comes from [`Quest::start_component`] via
    /// `World::quest_sphere`.
    Sphere(u32),
    /// Reach this level (starts by itself).
    Level(u8),
    /// Starts by itself after this quest is completed.
    AfterQuest(u32),
    /// Some other trigger (kill an NPC, emote, skill, buff, equip ...) or none.
    Other,
}

/// How a quest is handed in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuestReport {
    /// Talk to this NPC template.
    Npc(u32),
    /// Use this doodad template.
    Doodad(u32),
    /// The quest completes by itself when its objectives are done.
    Auto,
    /// Completed from the quest journal.
    Journal,
    /// The data does not say.
    Unknown,
}

/// What one objective asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObjKind {
    /// Talk to an NPC (`CSQuestTalkMade`).
    Talk { npc: u32 },
    /// Kill `count` of any of these NPC templates.
    Hunt { npcs: Box<[u32]>, count: u32 },
    /// Hold `count` of `item`. `droppers`: NPC templates whose loot packs can drop it and that
    /// spawn in this world, best drop rate first (may be empty: the item comes from elsewhere,
    /// see `World::gather_doodads`).
    Gather {
        item: u32,
        count: u32,
        droppers: Box<[u32]>,
    },
    /// Use `item` `count` times.
    UseItem { item: u32, count: u32 },
    /// Interact with doodad template `doodad` `count` times.
    Interact { doodad: u32, count: u32 },
    /// Enter sphere `sphere` (`spheres.id`; the position is `World::quest_sphere(quest,
    /// objective.component)`).
    Sphere { sphere: u32 },
    /// Craft `craft` `count` times.
    Craft { craft: u32, count: u32 },
    /// Reach level `level`.
    Level { level: u8 },
    /// Anything not compiled; the text is the act class (e.g. `QuestActObjZoneKill`).
    Other(Box<str>),
}

impl ObjKind {
    /// Short stable label of the variant.
    pub fn label(&self) -> &'static str {
        match self {
            ObjKind::Talk { .. } => "Talk",
            ObjKind::Hunt { .. } => "Hunt",
            ObjKind::Gather { .. } => "Gather",
            ObjKind::UseItem { .. } => "UseItem",
            ObjKind::Interact { .. } => "Interact",
            ObjKind::Sphere { .. } => "Sphere",
            ObjKind::Craft { .. } => "Craft",
            ObjKind::Level { .. } => "Level",
            ObjKind::Other(_) => "Other",
        }
    }
}

/// One objective of a progress component.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Objective {
    /// `quest_components.id` it belongs to.
    pub component: u32,
    /// `quest_acts.id` (sent in `CSQuestTalkMade`).
    pub act: u32,
    /// What to do.
    pub kind: ObjKind,
}

/// One progress component: all its objectives must be met.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuestStep {
    /// `quest_components.id`.
    pub component: u32,
    /// Objectives in act order.
    pub objectives: Box<[Objective]>,
}

/// A compiled quest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Quest {
    /// `quest_contexts.id`.
    pub id: u32,
    /// English name (Korean if there is no translation).
    pub name: Box<str>,
    /// Recommended level (0 if unset).
    pub level: u8,
    /// `quest_contexts.category_id`.
    pub category: u32,
    /// `quest_contexts.detail_id` (1 normal, 2 main, 3 saga, 6 task, 7 daily, 8 livelihood, ...).
    pub detail: u8,
    /// `quest_contexts.zone_id`: a `zones.id`, NOT a zone key (see `World::zone_key_for_id`).
    pub zone_id: u32,
    /// Chapter within the story line (0 = none).
    pub chapter_idx: u16,
    /// Position within the chapter (0 = none).
    pub quest_idx: u16,
    /// Can be done again.
    pub repeatable: bool,
    /// Part of a story line (`chapter_idx != 0 || quest_idx != 0`).
    pub story: bool,
    /// The primary way to start it (first of [`Quest::starts`]).
    pub start: QuestStart,
    /// Every way to start it, NPC first, then doodad, item, sphere, level.
    pub starts: Box<[QuestStart]>,
    /// The start component (position of a sphere start: `World::quest_sphere(id, start_component)`).
    pub start_component: u32,
    /// Quests the data lists as prerequisites (`QuestActConAcceptComponent`); the server does not
    /// enforce them when an NPC starts the quest.
    pub after: Box<[u32]>,
    /// NPC templates whose death starts the quest (`QuestActConAcceptNpcKill`).
    pub kill_start: Box<[u32]>,
    /// Items the quest hands over when it starts (`QuestActSupplyItem` in Supply components),
    /// `(item, count)`. A gather objective for such an item needs no gathering.
    pub supplies: Box<[(u32, u32)]>,
    /// Progress components that have objectives, in component order.
    pub progress: Box<[QuestStep]>,
    /// How to hand it in.
    pub report: QuestReport,
    /// Number of selectable rewards (`QuestActSupplySelectiveItem` acts); the reward index sent
    /// when completing is 1-based.
    pub selective_rewards: u8,
}

/// Aggregate numbers about the compiled quests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QuestStats {
    /// Compiled quests.
    pub quests: usize,
    /// Quests with at least one progress objective.
    pub with_objectives: usize,
    /// Quests whose start is [`QuestStart::Other`].
    pub start_other: usize,
    /// Quests with [`QuestReport::Unknown`].
    pub report_unknown: usize,
    /// Objectives per [`ObjKind::label`].
    pub objectives: Vec<(&'static str, usize)>,
    /// `Other` objectives per act class, most frequent first.
    pub other_classes: Vec<(Box<str>, usize)>,
    /// Gather objectives without any spawned NPC dropper.
    pub gather_without_droppers: usize,
    /// Gather objectives for an item the quest itself supplies.
    pub gather_supplied: usize,
}

/// All quests plus the start indexes.
#[derive(Debug, Default)]
pub struct Quests {
    by_id: IdMap<Quest>,
    from_npc: FxHashMap<u32, Box<[u32]>>,
    from_doodad: FxHashMap<u32, Box<[u32]>>,
}

// ---------------------------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Class {
    AcceptNpc,
    AcceptDoodad,
    AcceptItem,
    AcceptItemGain,
    AcceptSphere,
    AcceptLevelUp,
    AcceptComponent,
    AcceptNpcKill,
    ReportNpc,
    ReportDoodad,
    ReportJournal,
    AutoComplete,
    SelectiveItem,
    SupplyItem,
    ObjTalk,
    ObjMonsterHunt,
    ObjMonsterGroupHunt,
    ObjItemGather,
    ObjItemUse,
    ObjInteraction,
    ObjSphere,
    ObjCraft,
    ObjLevel,
    ObjTalkNpcGroup,
    /// An objective act this crate does not compile.
    OtherObj,
    /// Not an objective and not needed (supply, check, etc.).
    Ignored,
}

fn classify(ty: &str) -> Class {
    match ty {
        "QuestActConAcceptNpc" => Class::AcceptNpc,
        "QuestActConAcceptDoodad" => Class::AcceptDoodad,
        "QuestActConAcceptItem" => Class::AcceptItem,
        "QuestActConAcceptItemGain" => Class::AcceptItemGain,
        "QuestActConAcceptSphere" => Class::AcceptSphere,
        "QuestActConAcceptLevelUp" => Class::AcceptLevelUp,
        "QuestActConAcceptComponent" => Class::AcceptComponent,
        "QuestActConAcceptNpcKill" => Class::AcceptNpcKill,
        "QuestActConReportNpc" => Class::ReportNpc,
        "QuestActConReportDoodad" => Class::ReportDoodad,
        "QuestActConReportJournal" => Class::ReportJournal,
        "QuestActConAutoComplete" => Class::AutoComplete,
        "QuestActSupplySelectiveItem" => Class::SelectiveItem,
        "QuestActSupplyItem" => Class::SupplyItem,
        "QuestActObjTalk" => Class::ObjTalk,
        "QuestActObjMonsterHunt" => Class::ObjMonsterHunt,
        "QuestActObjMonsterGroupHunt" => Class::ObjMonsterGroupHunt,
        "QuestActObjItemGather" => Class::ObjItemGather,
        "QuestActObjItemUse" => Class::ObjItemUse,
        "QuestActObjInteraction" => Class::ObjInteraction,
        "QuestActObjSphere" => Class::ObjSphere,
        "QuestActObjCraft" => Class::ObjCraft,
        "QuestActObjLevel" => Class::ObjLevel,
        "QuestActObjTalkNpcGroup" => Class::ObjTalkNpcGroup,
        t if t.starts_with("QuestActObj") => Class::OtherObj,
        _ => Class::Ignored,
    }
}

impl Class {
    /// The `quest_acts.act_detail_type` of a compiled objective class.
    fn objective_name(self) -> &'static str {
        match self {
            Class::ObjTalk => "QuestActObjTalk",
            Class::ObjMonsterHunt => "QuestActObjMonsterHunt",
            Class::ObjMonsterGroupHunt => "QuestActObjMonsterGroupHunt",
            Class::ObjItemGather => "QuestActObjItemGather",
            Class::ObjItemUse => "QuestActObjItemUse",
            Class::ObjInteraction => "QuestActObjInteraction",
            Class::ObjSphere => "QuestActObjSphere",
            Class::ObjCraft => "QuestActObjCraft",
            Class::ObjLevel => "QuestActObjLevel",
            Class::ObjTalkNpcGroup => "QuestActObjTalkNpcGroup",
            _ => "QuestActObj",
        }
    }
}

struct ActRow {
    id: u32,
    detail: u32,
    class: Class,
    /// The full class name, kept only for [`Class::OtherObj`].
    name: Option<Box<str>>,
}

struct CompRow {
    id: u32,
    kind: u8,
    acts: Vec<ActRow>,
}

struct RawQuest {
    id: u32,
    name: Box<str>,
    category: u32,
    repeatable: bool,
    level: u8,
    chapter_idx: u16,
    quest_idx: u16,
    zone_id: u32,
    detail: u8,
    comps: Vec<CompRow>,
}

/// Detail rows of the act tables, keyed by the detail id (`quest_acts.act_detail_id`).
#[derive(Default)]
struct Details {
    accept_npc: FxHashMap<u32, u32>,
    accept_doodad: FxHashMap<u32, u32>,
    accept_item: FxHashMap<u32, u32>,
    accept_item_gain: FxHashMap<u32, u32>,
    accept_sphere: FxHashMap<u32, u32>,
    accept_level: FxHashMap<u32, u32>,
    accept_component: FxHashMap<u32, u32>,
    accept_npc_kill: FxHashMap<u32, u32>,
    report_npc: FxHashMap<u32, u32>,
    report_doodad: FxHashMap<u32, u32>,
    supply_item: FxHashMap<u32, (u32, u32)>,
    talk: FxHashMap<u32, u32>,
    hunt: FxHashMap<u32, (u32, u32)>,
    group_hunt: FxHashMap<u32, (u32, u32)>,
    gather: FxHashMap<u32, (u32, u32)>,
    item_use: FxHashMap<u32, (u32, u32)>,
    interaction: FxHashMap<u32, (u32, u32)>,
    sphere: FxHashMap<u32, u32>,
    craft: FxHashMap<u32, (u32, u32)>,
    level: FxHashMap<u32, u32>,
    talk_group: FxHashMap<u32, u32>,
    monster_group: FxHashMap<u32, Vec<u32>>,
    npc_group: FxHashMap<u32, Vec<u32>>,
}

fn map1(conn: &Connection, sql: &str) -> Result<FxHashMap<u32, u32>> {
    let mut m = FxHashMap::default();
    each_row(conn, sql, |r| {
        m.entry(r.u32(0)).or_insert(r.u32(1));
    })?;
    Ok(m)
}

fn map2(conn: &Connection, sql: &str) -> Result<FxHashMap<u32, (u32, u32)>> {
    let mut m = FxHashMap::default();
    each_row(conn, sql, |r| {
        m.entry(r.u32(0)).or_insert((r.u32(1), r.u32(2)));
    })?;
    Ok(m)
}

fn groups(conn: &Connection, sql: &str) -> Result<FxHashMap<u32, Vec<u32>>> {
    let mut m: FxHashMap<u32, Vec<u32>> = FxHashMap::default();
    each_row(conn, sql, |r| m.entry(r.u32(0)).or_default().push(r.u32(1)))?;
    for v in m.values_mut() {
        v.sort_unstable();
        v.dedup();
    }
    Ok(m)
}

impl Details {
    fn load(conn: &Connection) -> Result<Details> {
        Ok(Details {
            accept_npc: map1(conn, "SELECT id, npc_id FROM quest_act_con_accept_npcs")?,
            accept_doodad: map1(
                conn,
                "SELECT id, doodad_id FROM quest_act_con_accept_doodads",
            )?,
            accept_item: map1(conn, "SELECT id, item_id FROM quest_act_con_accept_items")?,
            accept_item_gain: map1(
                conn,
                "SELECT id, item_id FROM quest_act_con_accept_item_gains",
            )?,
            accept_sphere: map1(
                conn,
                "SELECT id, sphere_id FROM quest_act_con_accept_spheres",
            )?,
            accept_level: map1(conn, "SELECT id, level FROM quest_act_con_accept_level_ups")?,
            accept_component: map1(
                conn,
                "SELECT id, quest_context_id FROM quest_act_con_accept_components",
            )?,
            accept_npc_kill: map1(
                conn,
                "SELECT id, npc_id FROM quest_act_con_accept_npc_kills",
            )?,
            report_npc: map1(conn, "SELECT id, npc_id FROM quest_act_con_report_npcs")?,
            report_doodad: map1(
                conn,
                "SELECT id, doodad_id FROM quest_act_con_report_doodads",
            )?,
            supply_item: map2(
                conn,
                "SELECT id, item_id, count FROM quest_act_supply_items",
            )?,
            talk: map1(conn, "SELECT id, npc_id FROM quest_act_obj_talks")?,
            hunt: map2(
                conn,
                "SELECT id, npc_id, count FROM quest_act_obj_monster_hunts",
            )?,
            group_hunt: map2(
                conn,
                "SELECT id, quest_monster_group_id, count FROM quest_act_obj_monster_group_hunts",
            )?,
            gather: map2(
                conn,
                "SELECT id, item_id, count FROM quest_act_obj_item_gathers",
            )?,
            item_use: map2(
                conn,
                "SELECT id, item_id, count FROM quest_act_obj_item_uses",
            )?,
            interaction: map2(
                conn,
                "SELECT id, doodad_id, count FROM quest_act_obj_interactions",
            )?,
            sphere: map1(conn, "SELECT id, sphere_id FROM quest_act_obj_spheres")?,
            craft: map2(conn, "SELECT id, craft_id, count FROM quest_act_obj_crafts")?,
            level: map1(conn, "SELECT id, level FROM quest_act_obj_levels")?,
            talk_group: map1(
                conn,
                "SELECT id, npc_group_id FROM quest_act_obj_talk_npc_groups",
            )?,
            monster_group: groups(
                conn,
                "SELECT quest_monster_group_id, npc_id FROM quest_monster_npcs",
            )?,
            npc_group: groups(conn, "SELECT npc_group_id, npc_id FROM npc_group_members")?,
        })
    }
}

/// NPC templates that can drop each wanted item and that `has_spawn`, best drop rate first.
pub(crate) fn load_droppers(
    conn: &Connection,
    items: &FxHashSet<u32>,
    has_spawn: &dyn Fn(u32) -> bool,
) -> Result<FxHashMap<u32, Box<[u32]>>> {
    let mut packs: FxHashMap<u32, Vec<(u32, i64)>> = FxHashMap::default();
    each_row(
        conn,
        "SELECT loot_pack_id, item_id, drop_rate, always_drop FROM loots",
        |r| {
            let item = r.u32(1);
            if !items.contains(&item) {
                return;
            }
            let rate = if r.flag(3) { i64::MAX } else { r.int(2) };
            if rate > 0 {
                packs.entry(r.u32(0)).or_default().push((item, rate));
            }
        },
    )?;
    let mut best: FxHashMap<u32, FxHashMap<u32, i64>> = FxHashMap::default();
    each_row(
        conn,
        "SELECT npc_id, loot_pack_id FROM loot_pack_dropping_npcs",
        |r| {
            let npc = r.u32(0);
            let Some(list) = packs.get(&r.u32(1)) else {
                return;
            };
            if !has_spawn(npc) {
                return;
            }
            for &(item, rate) in list {
                let e = best.entry(item).or_default().entry(npc).or_insert(0);
                *e = (*e).max(rate);
            }
        },
    )?;
    Ok(best
        .into_iter()
        .map(|(item, npcs)| {
            let mut v: Vec<(u32, i64)> = npcs.into_iter().collect();
            v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            (item, v.into_iter().map(|(n, _)| n).collect())
        })
        .collect())
}

fn start_rank(s: &QuestStart) -> u8 {
    match s {
        QuestStart::Npc(_) => 0,
        QuestStart::Doodad(_) => 1,
        QuestStart::Item(_) => 2,
        QuestStart::Sphere(_) => 3,
        QuestStart::Level(_) => 4,
        QuestStart::AfterQuest(_) => 5,
        QuestStart::Other => 6,
    }
}

fn sorted_unique(mut v: Vec<u32>) -> Box<[u32]> {
    v.sort_unstable();
    v.dedup();
    v.into_boxed_slice()
}

struct Compiler<'a> {
    details: &'a Details,
    droppers: &'a FxHashMap<u32, Box<[u32]>>,
    has_spawn: &'a dyn Fn(u32) -> bool,
}

impl Compiler<'_> {
    fn objective(&self, comp: u32, act: &ActRow) -> Objective {
        let d = self.details;
        // A compilable class whose detail row is missing (or unusable) stays visible as Other.
        let other = |act: &ActRow| {
            ObjKind::Other(
                act.name
                    .clone()
                    .unwrap_or_else(|| act.class.objective_name().into()),
            )
        };
        let kind = match act.class {
            Class::ObjTalk => d.talk.get(&act.detail).map(|&npc| ObjKind::Talk { npc }),
            Class::ObjMonsterHunt => d.hunt.get(&act.detail).map(|&(npc, count)| ObjKind::Hunt {
                npcs: Box::new([npc]),
                count,
            }),
            Class::ObjMonsterGroupHunt => {
                d.group_hunt.get(&act.detail).and_then(|&(group, count)| {
                    let npcs = d.monster_group.get(&group)?;
                    (!npcs.is_empty()).then(|| ObjKind::Hunt {
                        npcs: npcs.clone().into_boxed_slice(),
                        count,
                    })
                })
            }
            Class::ObjItemGather => {
                d.gather
                    .get(&act.detail)
                    .map(|&(item, count)| ObjKind::Gather {
                        item,
                        count,
                        droppers: self.droppers.get(&item).cloned().unwrap_or_default(),
                    })
            }
            Class::ObjItemUse => d
                .item_use
                .get(&act.detail)
                .map(|&(item, count)| ObjKind::UseItem { item, count }),
            Class::ObjInteraction => d
                .interaction
                .get(&act.detail)
                .filter(|(doodad, _)| *doodad != 0)
                .map(|&(doodad, count)| ObjKind::Interact { doodad, count }),
            Class::ObjSphere => d
                .sphere
                .get(&act.detail)
                .map(|&sphere| ObjKind::Sphere { sphere }),
            Class::ObjCraft => d
                .craft
                .get(&act.detail)
                .map(|&(craft, count)| ObjKind::Craft { craft, count }),
            Class::ObjLevel => d.level.get(&act.detail).map(|&l| ObjKind::Level {
                level: l.min(255) as u8,
            }),
            Class::ObjTalkNpcGroup => d.talk_group.get(&act.detail).and_then(|g| {
                // Talking to any member counts; pick one that actually spawns.
                let members = d.npc_group.get(g)?;
                let npc = members
                    .iter()
                    .copied()
                    .find(|n| (self.has_spawn)(*n))
                    .or_else(|| members.first().copied())?;
                Some(ObjKind::Talk { npc })
            }),
            _ => None,
        }
        .unwrap_or_else(|| other(act));
        Objective {
            component: comp,
            act: act.id,
            kind,
        }
    }

    fn compile(&self, raw: RawQuest) -> Quest {
        let d = self.details;
        let mut starts: Vec<QuestStart> = Vec::new();
        let mut after: Vec<u32> = Vec::new();
        let mut kill_start: Vec<u32> = Vec::new();
        let mut start_component = 0;
        let mut report = None;
        let mut auto_complete = false;
        let mut selective = 0usize;
        let mut supplies: Vec<(u32, u32)> = Vec::new();
        let mut progress = Vec::new();

        for comp in &raw.comps {
            if comp.kind == KIND_START && start_component == 0 {
                start_component = comp.id;
            }
            let mut objectives = Vec::new();
            for act in &comp.acts {
                match (comp.kind, act.class) {
                    (KIND_START, Class::AcceptNpc) => {
                        starts.extend(d.accept_npc.get(&act.detail).map(|&n| QuestStart::Npc(n)))
                    }
                    (KIND_START, Class::AcceptDoodad) => starts.extend(
                        d.accept_doodad
                            .get(&act.detail)
                            .map(|&n| QuestStart::Doodad(n)),
                    ),
                    (KIND_START, Class::AcceptItem) => {
                        starts.extend(d.accept_item.get(&act.detail).map(|&n| QuestStart::Item(n)))
                    }
                    (KIND_START, Class::AcceptItemGain) => starts.extend(
                        d.accept_item_gain
                            .get(&act.detail)
                            .map(|&n| QuestStart::Item(n)),
                    ),
                    (KIND_START, Class::AcceptSphere) => starts.extend(
                        d.accept_sphere
                            .get(&act.detail)
                            .map(|&n| QuestStart::Sphere(n)),
                    ),
                    (KIND_START, Class::AcceptLevelUp) => starts.extend(
                        d.accept_level
                            .get(&act.detail)
                            .map(|&l| QuestStart::Level(l.min(255) as u8)),
                    ),
                    (KIND_START, Class::AcceptComponent) => {
                        after.extend(d.accept_component.get(&act.detail))
                    }
                    (KIND_START, Class::AcceptNpcKill) => {
                        kill_start.extend(d.accept_npc_kill.get(&act.detail))
                    }
                    (KIND_READY, Class::ReportNpc) if report.is_none() => {
                        report = d.report_npc.get(&act.detail).map(|&n| QuestReport::Npc(n))
                    }
                    (KIND_READY, Class::ReportDoodad) if report.is_none() => {
                        report = d
                            .report_doodad
                            .get(&act.detail)
                            .map(|&n| QuestReport::Doodad(n))
                    }
                    (KIND_READY, Class::ReportJournal) if report.is_none() => {
                        report = Some(QuestReport::Journal)
                    }
                    (KIND_SUPPLY, Class::SupplyItem) => {
                        supplies.extend(d.supply_item.get(&act.detail).copied())
                    }
                    (KIND_PROGRESS, c) if is_objective(c) => {
                        objectives.push(self.objective(comp.id, act))
                    }
                    _ => {}
                }
                match act.class {
                    Class::AutoComplete => auto_complete = true,
                    Class::SelectiveItem => selective += 1,
                    _ => {}
                }
            }
            if !objectives.is_empty() {
                progress.push(QuestStep {
                    component: comp.id,
                    objectives: objectives.into_boxed_slice(),
                });
            }
        }

        let mut seen = FxHashSet::default();
        starts.retain(|s| seen.insert(*s));
        starts.sort_by_key(start_rank); // stable: act order within a rank
        let after = sorted_unique(after);
        if starts.is_empty() {
            if let Some(&q) = after.first() {
                starts.push(QuestStart::AfterQuest(q));
            }
        }
        let report = report.unwrap_or(if auto_complete {
            QuestReport::Auto
        } else {
            QuestReport::Unknown
        });

        Quest {
            id: raw.id,
            name: raw.name,
            level: raw.level,
            category: raw.category,
            detail: raw.detail,
            zone_id: raw.zone_id,
            chapter_idx: raw.chapter_idx,
            quest_idx: raw.quest_idx,
            repeatable: raw.repeatable,
            story: raw.chapter_idx != 0 || raw.quest_idx != 0,
            start: starts.first().copied().unwrap_or(QuestStart::Other),
            starts: starts.into_boxed_slice(),
            start_component,
            after,
            kill_start: sorted_unique(kill_start),
            supplies: supplies.into_boxed_slice(),
            progress: progress.into_boxed_slice(),
            report,
            selective_rewards: selective.min(255) as u8,
        }
    }
}

fn is_objective(c: Class) -> bool {
    matches!(
        c,
        Class::ObjTalk
            | Class::ObjMonsterHunt
            | Class::ObjMonsterGroupHunt
            | Class::ObjItemGather
            | Class::ObjItemUse
            | Class::ObjInteraction
            | Class::ObjSphere
            | Class::ObjCraft
            | Class::ObjLevel
            | Class::ObjTalkNpcGroup
            | Class::OtherObj
    )
}

impl Quests {
    /// Compiles every non-tutorial quest. `has_npc_spawn(template)` filters gather droppers to
    /// NPCs that spawn in the loaded world.
    pub fn load(
        conn: &Connection,
        texts: &Texts,
        has_npc_spawn: &dyn Fn(u32) -> bool,
    ) -> Result<Quests> {
        // Contexts.
        let mut quests: Vec<RawQuest> = Vec::new();
        let mut slot_of: FxHashMap<u32, usize> = FxHashMap::default();
        each_row(
            conn,
            "SELECT id, name, category_id, repeatable, level, chapter_idx, quest_idx, zone_id, \
             detail_id FROM quest_contexts ORDER BY id",
            |r| {
                let id = r.u32(0);
                let category = r.u32(2);
                if category == CATEGORY_TUTORIAL || slot_of.contains_key(&id) {
                    return;
                }
                slot_of.insert(id, quests.len());
                quests.push(RawQuest {
                    id,
                    name: texts.name_or(TextTable::Quest, id, r.text(1)),
                    category,
                    repeatable: r.flag(3),
                    level: r.u8(4),
                    chapter_idx: r.u16(5),
                    quest_idx: r.u16(6),
                    zone_id: r.u32(7),
                    detail: r.u8(8),
                    comps: Vec::new(),
                });
            },
        )?;

        // Components, in the server's order.
        let mut comp_slot: FxHashMap<u32, (usize, usize)> = FxHashMap::default();
        each_row(
            conn,
            "SELECT id, quest_context_id, component_kind_id FROM quest_components \
             ORDER BY quest_context_id, component_kind_id, id",
            |r| {
                let Some(&slot) = slot_of.get(&r.u32(1)) else {
                    return;
                };
                let id = r.u32(0);
                if comp_slot.contains_key(&id) {
                    return;
                }
                comp_slot.insert(id, (slot, quests[slot].comps.len()));
                quests[slot].comps.push(CompRow {
                    id,
                    kind: r.u8(2),
                    acts: Vec::new(),
                });
            },
        )?;

        // Acts of components that exist.
        let mut seen_acts: FxHashSet<u32> = FxHashSet::default();
        each_row(
            conn,
            "SELECT id, quest_component_id, act_detail_id, act_detail_type FROM quest_acts \
             ORDER BY quest_component_id, id",
            |r| {
                let Some(&(slot, idx)) = comp_slot.get(&r.u32(1)) else {
                    return;
                };
                let id = r.u32(0);
                if !seen_acts.insert(id) {
                    return;
                }
                let ty = r.text(3);
                let class = classify(ty);
                if class == Class::Ignored {
                    return;
                }
                quests[slot].comps[idx].acts.push(ActRow {
                    id,
                    detail: r.u32(2),
                    class,
                    name: (class == Class::OtherObj).then(|| ty.into()),
                });
            },
        )?;

        let details = Details::load(conn)?;
        let gather_items: FxHashSet<u32> = details.gather.values().map(|g| g.0).collect();
        let droppers = load_droppers(conn, &gather_items, has_npc_spawn)?;

        let compiler = Compiler {
            details: &details,
            droppers: &droppers,
            has_spawn: has_npc_spawn,
        };
        let compiled: Vec<(u32, Quest)> = quests
            .into_iter()
            .map(|raw| (raw.id, compiler.compile(raw)))
            .collect();
        Ok(Quests::from_quests(compiled))
    }

    fn from_quests(compiled: Vec<(u32, Quest)>) -> Quests {
        let by_id = IdMap::build(compiled);
        let mut from_npc: FxHashMap<u32, Vec<u32>> = FxHashMap::default();
        let mut from_doodad: FxHashMap<u32, Vec<u32>> = FxHashMap::default();
        for q in by_id.values() {
            for s in q.starts.iter() {
                match s {
                    QuestStart::Npc(n) => from_npc.entry(*n).or_default().push(q.id),
                    QuestStart::Doodad(d) => from_doodad.entry(*d).or_default().push(q.id),
                    _ => {}
                }
            }
        }
        let freeze = |m: FxHashMap<u32, Vec<u32>>| -> FxHashMap<u32, Box<[u32]>> {
            m.into_iter()
                .map(|(k, mut v)| {
                    v.sort_unstable();
                    v.dedup();
                    (k, v.into_boxed_slice())
                })
                .collect()
        };
        Quests {
            by_id,
            from_npc: freeze(from_npc),
            from_doodad: freeze(from_doodad),
        }
    }

    /// One quest.
    pub fn get(&self, id: u32) -> Option<&Quest> {
        self.by_id.get(id)
    }

    /// All quests in id order.
    pub fn iter(&self) -> impl Iterator<Item = &Quest> {
        self.by_id.values().iter()
    }

    /// Number of quests.
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Whether there are no quests.
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Quests an NPC template starts, by id.
    pub fn from_npc(&self, npc: u32) -> &[u32] {
        self.from_npc.get(&npc).map_or(&[], |v| &v[..])
    }

    /// Quests a doodad template starts, by id.
    pub fn from_doodad(&self, doodad: u32) -> &[u32] {
        self.from_doodad.get(&doodad).map_or(&[], |v| &v[..])
    }

    /// NPC templates that start at least one quest.
    pub fn npc_starters(&self) -> impl Iterator<Item = u32> + '_ {
        self.from_npc.keys().copied()
    }

    /// Items named by gather objectives.
    pub fn gather_items(&self) -> FxHashSet<u32> {
        let mut items = FxHashSet::default();
        for q in self.iter() {
            for step in q.progress.iter() {
                for o in step.objectives.iter() {
                    if let ObjKind::Gather { item, .. } = o.kind {
                        items.insert(item);
                    }
                }
            }
        }
        items
    }

    /// Counts for logging and tests.
    pub fn stats(&self) -> QuestStats {
        const LABELS: [&str; 9] = [
            "Talk", "Hunt", "Gather", "UseItem", "Interact", "Sphere", "Craft", "Level", "Other",
        ];
        let mut counts = [0usize; 9];
        let mut other: FxHashMap<Box<str>, usize> = FxHashMap::default();
        let mut s = QuestStats {
            quests: self.len(),
            ..QuestStats::default()
        };
        for q in self.iter() {
            if !q.progress.is_empty() {
                s.with_objectives += 1;
            }
            if q.start == QuestStart::Other {
                s.start_other += 1;
            }
            if q.report == QuestReport::Unknown {
                s.report_unknown += 1;
            }
            for step in q.progress.iter() {
                for o in step.objectives.iter() {
                    let label = o.kind.label();
                    if let Some(i) = LABELS.iter().position(|l| *l == label) {
                        counts[i] += 1;
                    }
                    match &o.kind {
                        ObjKind::Other(c) => *other.entry(c.clone()).or_default() += 1,
                        ObjKind::Gather { item, droppers, .. } => {
                            if droppers.is_empty() {
                                s.gather_without_droppers += 1;
                            }
                            if q.supplies.iter().any(|(i, _)| i == item) {
                                s.gather_supplied += 1;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        s.objectives = LABELS.iter().copied().zip(counts).collect();
        let mut other: Vec<(Box<str>, usize)> = other.into_iter().collect();
        other.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        s.other_classes = other;
        s
    }

    /// Approximate heap bytes held.
    pub fn bytes(&self) -> usize {
        let mut n = self.by_id.shallow_bytes();
        for q in self.iter() {
            n += q.name.len()
                + q.starts.len() * std::mem::size_of::<QuestStart>()
                + (q.after.len() + q.kill_start.len()) * 4
                + q.supplies.len() * 8
                + q.progress.len() * std::mem::size_of::<QuestStep>();
            for step in q.progress.iter() {
                n += step.objectives.len() * std::mem::size_of::<Objective>();
                for o in step.objectives.iter() {
                    n += match &o.kind {
                        ObjKind::Hunt { npcs, .. } => npcs.len() * 4,
                        ObjKind::Gather { droppers, .. } => droppers.len() * 4,
                        ObjKind::Other(c) => c.len(),
                        _ => 0,
                    };
                }
            }
        }
        let idx = |m: &FxHashMap<u32, Box<[u32]>>| {
            m.values().map(|v| v.len() * 4 + 16 + 8).sum::<usize>()
        };
        n + idx(&self.from_npc) + idx(&self.from_doodad)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A miniature compact database with the quest shapes the real data has.
    fn fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE quest_contexts (id INT, name TEXT, category_id INT, repeatable NUM, level INT,
              chapter_idx INT, quest_idx INT, zone_id INT, detail_id INT);
            CREATE TABLE quest_components (id INT, quest_context_id INT, component_kind_id INT);
            CREATE TABLE quest_acts (id INT, quest_component_id INT, act_detail_id INT, act_detail_type TEXT);
            CREATE TABLE quest_act_con_accept_npcs (id INT, npc_id INT);
            CREATE TABLE quest_act_con_accept_doodads (id INT, doodad_id INT);
            CREATE TABLE quest_act_con_accept_items (id INT, item_id INT);
            CREATE TABLE quest_act_con_accept_item_gains (id INT, item_id INT);
            CREATE TABLE quest_act_con_accept_spheres (id INT, sphere_id INT);
            CREATE TABLE quest_act_con_accept_level_ups (id INT, level INT);
            CREATE TABLE quest_act_con_accept_components (id INT, quest_context_id INT);
            CREATE TABLE quest_act_con_accept_npc_kills (id INT, npc_id INT);
            CREATE TABLE quest_act_con_report_npcs (id INT, npc_id INT);
            CREATE TABLE quest_act_con_report_doodads (id INT, doodad_id INT);
            CREATE TABLE quest_act_supply_items (id INT, item_id INT, count INT);
            CREATE TABLE quest_act_obj_talks (id INT, npc_id INT);
            CREATE TABLE quest_act_obj_monster_hunts (id INT, npc_id INT, count INT);
            CREATE TABLE quest_act_obj_monster_group_hunts (id INT, quest_monster_group_id INT, count INT);
            CREATE TABLE quest_act_obj_item_gathers (id INT, item_id INT, count INT);
            CREATE TABLE quest_act_obj_item_uses (id INT, item_id INT, count INT);
            CREATE TABLE quest_act_obj_interactions (id INT, doodad_id INT, count INT);
            CREATE TABLE quest_act_obj_spheres (id INT, sphere_id INT);
            CREATE TABLE quest_act_obj_crafts (id INT, craft_id INT, count INT);
            CREATE TABLE quest_act_obj_levels (id INT, level INT);
            CREATE TABLE quest_act_obj_talk_npc_groups (id INT, npc_group_id INT);
            CREATE TABLE quest_monster_npcs (id INT, quest_monster_group_id INT, npc_id INT);
            CREATE TABLE npc_group_members (id INT, npc_group_id INT, npc_id INT);
            CREATE TABLE loots (loot_pack_id INT, item_id INT, drop_rate INT, always_drop TEXT);
            CREATE TABLE loot_pack_dropping_npcs (id INT, npc_id INT, loot_pack_id INT, default_pack TEXT);

            INSERT INTO quest_contexts VALUES
              (330, 'A', 4, 'f', 1, NULL, NULL, 125, 1),
              (250, 'B', 4, 'f', 1, 0, 0, 125, 1),
              (251, 'C', 4, 't', 2, 1, 3, 125, 2),
              (900, 'Tutorial', 45, 'f', 1, 0, 0, 1, 1),
              (901, 'Chain', 4, 'f', 5, 0, 0, 125, 1),
              (902, 'Kill start', 32, 'f', 11, 0, 0, 125, 1),
              (903, 'Mixed', 4, 'f', 7, 0, 0, 125, 7);

            -- 330: NPC start, no progress component (orphan act), NPC report
            INSERT INTO quest_components VALUES (1520, 330, 2), (1521, 330, 6), (1522, 330, 8), (1519, 330, 3);
            INSERT INTO quest_act_supply_items VALUES (5, 777, 2);
            INSERT INTO quest_acts VALUES (9870, 1519, 5, 'QuestActSupplyItem'),
              (9874, 1520, 1250, 'QuestActConAcceptNpc'),
              (5060, 3543, 260, 'QuestActObjItemGather'),
              (2438, 1521, 329, 'QuestActConReportNpc');
            INSERT INTO quest_act_con_accept_npcs VALUES (1250, 3597), (77, 3512), (78, 3600);
            INSERT INTO quest_act_con_report_npcs VALUES (329, 3511), (104, 3512);

            -- 250: doodad start, kill 3 of npc 3492, auto complete in the reward
            INSERT INTO quest_components VALUES (271, 250, 2), (7069, 250, 4), (737, 250, 8);
            INSERT INTO quest_acts VALUES (26340, 271, 143, 'QuestActConAcceptDoodad'),
              (10467, 7069, 404, 'QuestActObjMonsterHunt'),
              (26341, 737, 782, 'QuestActConAutoComplete'),
              (26342, 737, 76, 'QuestActSupplyExp');
            INSERT INTO quest_act_con_accept_doodads VALUES (143, 5047);
            INSERT INTO quest_act_obj_monster_hunts VALUES (404, 3492, 3);

            -- 251: npc start, gather 3 of item 4058 (dropped by npc 3475 and 3476), report; story
            INSERT INTO quest_components VALUES (383, 251, 2), (633, 251, 4), (638, 251, 6), (639, 251, 8);
            INSERT INTO quest_acts VALUES (333, 383, 77, 'QuestActConAcceptNpc'),
              (10473, 633, 616, 'QuestActObjItemGather'),
              (457, 638, 104, 'QuestActConReportNpc'),
              (36911, 639, 1, 'QuestActSupplySelectiveItem'),
              (36912, 639, 2, 'QuestActSupplySelectiveItem');
            INSERT INTO quest_act_obj_item_gathers VALUES (616, 4058, 3), (260, 4058, 3);
            INSERT INTO loots VALUES (4530, 4058, 10000000, 'f'), (1616, 4058, 0, 'f'), (7000, 4058, 5000, 'f');
            INSERT INTO loot_pack_dropping_npcs VALUES (2093, 3475, 4530, 't'), (5163, 3476, 7000, 'f'),
              (5164, 3477, 1616, 'f'), (5165, 3478, 4530, 't');

            -- 901: starts after 330 (and is the only start), tutorial 900 has acts that must vanish
            INSERT INTO quest_components VALUES (9010, 901, 2), (9011, 901, 4);
            INSERT INTO quest_acts VALUES (90100, 9010, 1, 'QuestActConAcceptComponent'),
              (90101, 9011, 1, 'QuestActObjTalk'),
              (90102, 9011, 2, 'QuestActObjTalkNpcGroup');
            INSERT INTO quest_act_con_accept_components VALUES (1, 330);
            INSERT INTO quest_act_obj_talks VALUES (1, 3511);
            INSERT INTO quest_act_obj_talk_npc_groups VALUES (2, 55);
            INSERT INTO npc_group_members VALUES (1, 55, 3601), (2, 55, 3602);
            INSERT INTO quest_components VALUES (9001, 900, 2);
            INSERT INTO quest_acts VALUES (90001, 9001, 9, 'QuestActConAcceptNpc');
            INSERT INTO quest_act_con_accept_npcs VALUES (9, 1);

            -- 902: started by killing npc 3475
            INSERT INTO quest_components VALUES (9020, 902, 2), (9021, 902, 4), (9022, 902, 6);
            INSERT INTO quest_acts VALUES (90200, 9020, 1, 'QuestActConAcceptNpcKill'),
              (90201, 9021, 1, 'QuestActObjMonsterGroupHunt'),
              (90202, 9021, 1, 'QuestActObjZoneKill'),
              (90203, 9022, 1, 'QuestActConReportJournal');
            INSERT INTO quest_act_con_accept_npc_kills VALUES (1, 3475);
            INSERT INTO quest_act_obj_monster_group_hunts VALUES (1, 77, 5);
            INSERT INTO quest_monster_npcs VALUES (1, 77, 3480), (2, 77, 3481), (3, 77, 3480);

            -- 903: several objective kinds in two progress components
            INSERT INTO quest_components VALUES (9030, 903, 2), (9031, 903, 4), (9032, 903, 4), (9033, 903, 6);
            INSERT INTO quest_acts VALUES (90300, 9030, 5, 'QuestActConAcceptSphere'),
              (90301, 9030, 78, 'QuestActConAcceptNpc'),
              (90310, 9031, 1, 'QuestActObjItemUse'),
              (90311, 9031, 1, 'QuestActObjInteraction'),
              (90312, 9031, 2, 'QuestActObjInteraction'),
              (90320, 9032, 1, 'QuestActObjSphere'),
              (90321, 9032, 1, 'QuestActObjCraft'),
              (90322, 9032, 1, 'QuestActObjLevel'),
              (90330, 9033, 1, 'QuestActConReportDoodad');
            INSERT INTO quest_act_con_accept_spheres VALUES (5, 888);
            INSERT INTO quest_act_obj_item_uses VALUES (1, 111, 2);
            INSERT INTO quest_act_obj_interactions VALUES (1, 222, 4), (2, 0, 1);
            INSERT INTO quest_act_obj_spheres VALUES (1, 333);
            INSERT INTO quest_act_obj_crafts VALUES (1, 6226, 2);
            INSERT INTO quest_act_obj_levels VALUES (1, 10);
            INSERT INTO quest_act_con_report_doodads VALUES (1, 444);
            ",
        )
        .unwrap();
        conn
    }

    fn load(conn: &Connection) -> Quests {
        let mut texts = Texts::empty();
        texts.insert(TextTable::Quest, 330, "A Friendly Reminder");
        // NPCs 3475, 3476, 3477 spawn; 3478 does not.
        Quests::load(conn, &texts, &|n| (3475..=3477).contains(&n) || n == 3602).unwrap()
    }

    #[test]
    fn tutorial_quests_are_not_quests() {
        let q = load(&fixture());
        assert!(q.get(900).is_none());
        assert_eq!(q.len(), 6);
        assert!(q.from_npc(1).is_empty());
    }

    #[test]
    fn quest_without_progress_component_ignores_orphan_acts() {
        let q = load(&fixture());
        let a = q.get(330).unwrap();
        assert_eq!(&*a.name, "A Friendly Reminder");
        assert_eq!(a.start, QuestStart::Npc(3597));
        assert_eq!(a.start_component, 1520);
        assert!(a.progress.is_empty());
        assert_eq!(&a.supplies[..], &[(777, 2)]);
        assert_eq!(a.report, QuestReport::Npc(3511));
        assert!(!a.story && !a.repeatable);
        assert_eq!(a.zone_id, 125);
    }

    #[test]
    fn doodad_start_hunt_and_auto_complete() {
        let q = load(&fixture());
        let b = q.get(250).unwrap();
        assert_eq!(b.start, QuestStart::Doodad(5047));
        assert_eq!(q.from_doodad(5047), &[250]);
        assert_eq!(b.report, QuestReport::Auto);
        assert_eq!(b.progress.len(), 1);
        let o = &b.progress[0].objectives[0];
        assert_eq!((o.component, o.act), (7069, 10467));
        assert_eq!(
            o.kind,
            ObjKind::Hunt {
                npcs: Box::new([3492]),
                count: 3
            }
        );
    }

    #[test]
    fn gather_droppers_are_spawned_npcs_best_rate_first() {
        let q = load(&fixture());
        let c = q.get(251).unwrap();
        assert!(c.repeatable && c.story);
        assert_eq!((c.chapter_idx, c.quest_idx), (1, 3));
        assert_eq!(c.selective_rewards, 2);
        assert_eq!(c.start, QuestStart::Npc(3512));
        assert_eq!(c.report, QuestReport::Npc(3512));
        match &c.progress[0].objectives[0].kind {
            // 3475: 100%, 3476: 0.05%; 3477's pack has a 0% entry; 3478 does not spawn.
            ObjKind::Gather {
                item,
                count,
                droppers,
            } => {
                assert_eq!((*item, *count), (4058, 3));
                assert_eq!(&droppers[..], &[3475, 3476]);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(q.from_npc(3512), &[251]);
        assert_eq!(q.from_npc(3597), &[330]);
        assert!(q.gather_items().contains(&4058));
    }

    #[test]
    fn start_only_after_another_quest() {
        let q = load(&fixture());
        let c = q.get(901).unwrap();
        assert_eq!(c.start, QuestStart::AfterQuest(330));
        assert_eq!(&c.after[..], &[330]);
        // Talk objective + talk-group objective resolved to a spawned member.
        let objs = &c.progress[0].objectives;
        assert_eq!(objs[0].kind, ObjKind::Talk { npc: 3511 });
        assert_eq!(objs[1].kind, ObjKind::Talk { npc: 3602 });
        assert_eq!(c.report, QuestReport::Unknown);
    }

    #[test]
    fn kill_started_group_hunt_and_other_objectives() {
        let q = load(&fixture());
        let k = q.get(902).unwrap();
        assert_eq!(k.start, QuestStart::Other);
        assert_eq!(&k.kill_start[..], &[3475]);
        assert_eq!(k.report, QuestReport::Journal);
        let objs = &k.progress[0].objectives;
        assert_eq!(
            objs[0].kind,
            ObjKind::Hunt {
                npcs: Box::new([3480, 3481]),
                count: 5
            }
        );
        assert_eq!(objs[1].kind, ObjKind::Other("QuestActObjZoneKill".into()));
    }

    #[test]
    fn mixed_objective_kinds_and_start_priority() {
        let q = load(&fixture());
        let m = q.get(903).unwrap();
        // NPC beats sphere even though the sphere act comes first.
        assert_eq!(
            &m.starts[..],
            &[QuestStart::Npc(3600), QuestStart::Sphere(888)]
        );
        assert_eq!(m.start, QuestStart::Npc(3600));
        assert_eq!(m.report, QuestReport::Doodad(444));
        assert_eq!(m.detail, 7);
        assert_eq!(m.progress.len(), 2);
        let kinds: Vec<_> = m
            .progress
            .iter()
            .flat_map(|s| s.objectives.iter().map(|o| o.kind.clone()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                ObjKind::UseItem {
                    item: 111,
                    count: 2
                },
                ObjKind::Interact {
                    doodad: 222,
                    count: 4
                },
                ObjKind::Other("QuestActObjInteraction".into()),
                ObjKind::Sphere { sphere: 333 },
                ObjKind::Craft {
                    craft: 6226,
                    count: 2
                },
                ObjKind::Level { level: 10 },
            ]
        );
    }

    #[test]
    fn stats_count_kinds_and_other_classes() {
        let q = load(&fixture());
        let s = q.stats();
        assert_eq!(s.quests, 6);
        let count = |label: &str| s.objectives.iter().find(|o| o.0 == label).unwrap().1;
        assert_eq!(count("Talk"), 2);
        assert_eq!(count("Hunt"), 2);
        assert_eq!(count("Gather"), 1);
        assert_eq!(count("Other"), 2);
        assert_eq!(s.gather_without_droppers, 0);
        assert!(s
            .other_classes
            .iter()
            .any(|c| &*c.0 == "QuestActObjZoneKill" && c.1 == 1));
        assert_eq!(s.report_unknown, 1);
    }
}
