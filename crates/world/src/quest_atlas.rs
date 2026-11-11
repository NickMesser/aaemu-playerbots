//! Where the quests are: one compact table of every quest a bot can walk to and start, grouped by
//! zone, and the story line of each playable race.
//!
//! Bots find quests by looking at the givers around them (1.5 km). When nothing is near they need
//! to know where to go, and for that this table is the map: a point per giver spawn of every quest
//! that has an NPC or doodad giver, is not repeatable, not a seasonal event and that the caller
//! says a bot can do at all (objectives and turn-in are supported). Each [`AtlasPoint`] is 24
//! bytes, a quest has at most [`MAX_SPOTS`] of them, so the whole table of the 1.2 data is a few
//! dozen KiB. The zones ([`AtlasZone`]) are the client's zone keys (the sub-areas of a region:
//! Solzreed Peninsula is three of them); a zone's `hub` is the centroid of its givers and `bands`
//! counts its quests per level band.
//!
//! The table is static: which of the quests a given bot may do (level, quests done, race gates,
//! back-off) is judged by the bot at selection time, point by point.
//!
//! The story chain of a race is the story category (the quests with a chapter and a position) that
//! is open to that race and has the most quests; the quests ordered by chapter and position are
//! what a bot of that race walks through to level 28.

use rustc_hash::FxHashMap;

use crate::geom::Vec3;
use crate::quest::{Quest, QuestStart};
use crate::reqs::Who;
use crate::world::World;

/// `quest_contexts.category_id` of the seasonal event quests (Hallowtide and the like): their
/// objects only exist while the event runs.
pub const CATEGORY_EVENT: u32 = 55;
/// Giver spawns kept per quest (the first ones in the spawn files).
pub const MAX_SPOTS: usize = 3;
/// Level bands of [`AtlasZone::bands`]: levels 1 to 5, 6 to 10, and so on; the last band is open.
pub const BANDS: usize = 12;
/// A story category needs this many quests to count as a race's story chain.
const MIN_CHAIN: usize = 20;

/// The level band of `level` (0-based).
pub fn band(level: u8) -> usize {
    ((usize::from(level.max(1)) - 1) / 5).min(BANDS - 1)
}

/// One giver spawn of one quest.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AtlasPoint {
    /// Where the giver spawns.
    pub x: f32,
    /// Where the giver spawns.
    pub y: f32,
    /// The quest it starts.
    pub quest: u32,
    /// The giver's template (NPC or doodad, see [`AtlasPoint::doodad`]).
    pub giver: u32,
    /// The zone key at the spawn.
    pub zone: u16,
    /// The quest's level.
    pub level: u8,
    /// The giver is a doodad.
    pub doodad: bool,
    /// The quest belongs to a story line.
    pub story: bool,
}

/// The quests of one zone key.
#[derive(Debug, Clone, PartialEq)]
pub struct AtlasZone {
    /// The client's zone key.
    pub zone: u16,
    /// The centroid of the givers of the zone.
    pub hub: [f32; 2],
    /// Distinct quests.
    pub quests: u16,
    /// Distinct quests per level band, see [`band`].
    pub bands: [u16; BANDS],
    first: u32,
    len: u32,
}

/// The story line of one race.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chain {
    /// `char_race_id`.
    pub race: u8,
    /// `quest_contexts.category_id` of the story line.
    pub category: u32,
    /// Quest ids by chapter, position and id.
    pub quests: Box<[u32]>,
}

/// A quest as the builder takes it.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// Quest id.
    pub quest: u32,
    /// Quest level.
    pub level: u8,
    /// Part of a story line.
    pub story: bool,
    /// Giver spawns: `(x, y, zone key, giver template, giver is a doodad)`.
    pub spots: Vec<(f32, f32, u16, u32, bool)>,
}

/// The table. See the module docs.
#[derive(Debug, Default)]
pub struct QuestAtlas {
    /// By zone key, then quest id.
    points: Box<[AtlasPoint]>,
    zones: Box<[AtlasZone]>,
    /// `(quest id, index into points)`, sorted.
    by_quest: Box<[(u32, u32)]>,
    chains: Box<[Chain]>,
}

impl QuestAtlas {
    /// Builds the table from `entries` and the race `chains`.
    pub fn from_entries(entries: Vec<Entry>, chains: Vec<Chain>) -> QuestAtlas {
        let mut points: Vec<AtlasPoint> = Vec::new();
        for e in &entries {
            for &(x, y, zone, giver, doodad) in e.spots.iter().take(MAX_SPOTS) {
                points.push(AtlasPoint {
                    x,
                    y,
                    quest: e.quest,
                    giver,
                    zone,
                    level: e.level,
                    doodad,
                    story: e.story,
                });
            }
        }
        points.sort_by(|a, b| {
            (a.zone, a.quest, a.giver, a.doodad)
                .cmp(&(b.zone, b.quest, b.giver, b.doodad))
                .then(a.x.total_cmp(&b.x))
                .then(a.y.total_cmp(&b.y))
        });
        points.dedup_by(|b, a| {
            a.zone == b.zone
                && a.quest == b.quest
                && a.giver == b.giver
                && a.doodad == b.doodad
                && a.x == b.x
                && a.y == b.y
        });
        let mut zones: Vec<AtlasZone> = Vec::new();
        let mut i = 0;
        while i < points.len() {
            let zone = points[i].zone;
            let mut j = i;
            while j < points.len() && points[j].zone == zone {
                j += 1;
            }
            let slice = &points[i..j];
            let n = slice.len() as f32;
            let hub = [
                slice.iter().map(|p| p.x).sum::<f32>() / n,
                slice.iter().map(|p| p.y).sum::<f32>() / n,
            ];
            let mut bands = [0u16; BANDS];
            let mut quests = 0u16;
            let mut last = 0u32;
            for p in slice {
                if p.quest != last {
                    last = p.quest;
                    quests = quests.saturating_add(1);
                    let b = band(p.level);
                    bands[b] = bands[b].saturating_add(1);
                }
            }
            zones.push(AtlasZone {
                zone,
                hub,
                quests,
                bands,
                first: i as u32,
                len: (j - i) as u32,
            });
            i = j;
        }
        let mut by_quest: Vec<(u32, u32)> = points
            .iter()
            .enumerate()
            .map(|(i, p)| (p.quest, i as u32))
            .collect();
        by_quest.sort_unstable();
        QuestAtlas {
            points: points.into_boxed_slice(),
            zones: zones.into_boxed_slice(),
            by_quest: by_quest.into_boxed_slice(),
            chains: chains.into_boxed_slice(),
        }
    }

    /// Every point, grouped by zone.
    pub fn points(&self) -> &[AtlasPoint] {
        &self.points
    }

    /// The zones with at least one quest, by zone key.
    pub fn zones(&self) -> &[AtlasZone] {
        &self.zones
    }

    /// The points of `zone`.
    pub fn zone_points(&self, zone: &AtlasZone) -> &[AtlasPoint] {
        &self.points[zone.first as usize..(zone.first + zone.len) as usize]
    }

    /// The zone with key `key`.
    pub fn zone(&self, key: u16) -> Option<&AtlasZone> {
        self.zones
            .binary_search_by_key(&key, |z| z.zone)
            .ok()
            .map(|i| &self.zones[i])
    }

    /// The giver spawns of `quest`.
    pub fn spots(&self, quest: u32) -> impl Iterator<Item = &AtlasPoint> + '_ {
        let start = self.by_quest.partition_point(|e| e.0 < quest);
        self.by_quest[start..]
            .iter()
            .take_while(move |e| e.0 == quest)
            .map(|e| &self.points[e.1 as usize])
    }

    /// The story line of `race`, if it has one.
    pub fn chain(&self, race: u8) -> Option<&Chain> {
        self.chains.iter().find(|c| c.race == race)
    }

    /// Every race story line.
    pub fn chains(&self) -> &[Chain] {
        &self.chains
    }

    /// Number of giver spawns in the table.
    pub fn len(&self) -> usize {
        self.points.len()
    }

    /// Whether the table is empty.
    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// Approximate heap bytes held.
    pub fn bytes(&self) -> usize {
        self.points.len() * std::mem::size_of::<AtlasPoint>()
            + self.zones.len() * std::mem::size_of::<AtlasZone>()
            + self.by_quest.len() * std::mem::size_of::<(u32, u32)>()
            + self
                .chains
                .iter()
                .map(|c| std::mem::size_of::<Chain>() + c.quests.len() * 4)
                .sum::<usize>()
    }

    /// Reads the table out of the world: `supported(quest)` says whether a bot can do the quest at
    /// all (the world does not know what bots can do).
    pub fn build(world: &World, supported: &dyn Fn(&Quest) -> bool) -> QuestAtlas {
        let mut entries: Vec<Entry> = Vec::new();
        for q in world.quests() {
            if q.repeatable || q.category == CATEGORY_EVENT || !supported(q) {
                continue;
            }
            let mut spots: Vec<(f32, f32, u16, u32, bool)> = Vec::new();
            for s in q.starts.iter() {
                let (template, doodad) = match s {
                    QuestStart::Npc(t) => (*t, false),
                    QuestStart::Doodad(t) => (*t, true),
                    _ => continue,
                };
                let spawns = if doodad {
                    world.doodad_spawns(template)
                } else {
                    world.npc_spawns(template)
                };
                for p in spawns.iter().take(MAX_SPOTS) {
                    let zone = zone_of(world, q, *p);
                    // A closed zone (Aubre Cradle, Airain Rock, ...) is no destination for a
                    // bot: no ordinary player may be there.
                    if world.zone_closed(zone) {
                        continue;
                    }
                    spots.push((p.x, p.y, zone, template, doodad));
                }
            }
            if !spots.is_empty() {
                entries.push(Entry {
                    quest: q.id,
                    level: q.level,
                    story: q.story,
                    spots,
                });
            }
        }
        QuestAtlas::from_entries(entries, race_chains(world))
    }
}

/// The zone key at a giver spawn: the baked zone grid, else the quest's own zone, else 0.
fn zone_of(world: &World, quest: &Quest, p: Vec3) -> u16 {
    world
        .zone_key_at(p.x, p.y)
        .or_else(|| world.zone_key_for_id(quest.zone_id))
        .unwrap_or(0)
}

/// The story line of every race that has one.
fn race_chains(world: &World) -> Vec<Chain> {
    let mut story: Vec<&Quest> = world.quests().filter(|q| q.story).collect();
    story.sort_by_key(|q| (q.category, q.chapter_idx, q.quest_idx, q.id));
    let mut chains = Vec::new();
    for race in 1..=8u8 {
        let Some(faction) = world.race_faction(race) else {
            continue;
        };
        let mother = world.mother_faction(faction);
        let all = |_: u32| true;
        let open = |q: &Quest| {
            [1u8, 2].iter().any(|&gender| {
                world.start_gates().open(
                    q.id,
                    &Who {
                        level: q.level.max(1),
                        race,
                        gender,
                        mother_faction: mother,
                        done: &all,
                        in_journal: &all,
                        owns: &all,
                    },
                )
            })
        };
        let mut by_category: FxHashMap<u32, Vec<u32>> = FxHashMap::default();
        for q in story.iter().filter(|q| open(q)) {
            by_category.entry(q.category).or_default().push(q.id);
        }
        // the longest line wins; on a tie the lower category (a deterministic choice)
        let best = by_category
            .into_iter()
            .filter(|(_, v)| v.len() >= MIN_CHAIN)
            .max_by(|a, b| a.1.len().cmp(&b.1.len()).then(b.0.cmp(&a.0)));
        if let Some((category, quests)) = best {
            chains.push(Chain {
                race,
                category,
                quests: quests.into_boxed_slice(),
            });
        }
    }
    chains
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(quest: u32, level: u8, story: bool, spots: &[(f32, f32, u16)]) -> Entry {
        Entry {
            quest,
            level,
            story,
            spots: spots.iter().map(|&(x, y, z)| (x, y, z, 7, false)).collect(),
        }
    }

    #[test]
    fn bands_are_five_levels_wide() {
        assert_eq!(band(0), 0);
        assert_eq!(band(1), 0);
        assert_eq!(band(5), 0);
        assert_eq!(band(6), 1);
        assert_eq!(band(10), 1);
        assert_eq!(band(28), 5);
        assert_eq!(band(255), BANDS - 1);
    }

    #[test]
    fn points_group_by_zone_and_count_quests_once() {
        let atlas = QuestAtlas::from_entries(
            vec![
                entry(10, 3, false, &[(100.0, 0.0, 2), (300.0, 0.0, 2)]),
                entry(11, 8, true, &[(200.0, 0.0, 2)]),
                entry(12, 8, false, &[(5000.0, 5000.0, 1)]),
            ],
            vec![],
        );
        assert_eq!(atlas.len(), 4);
        let keys: Vec<u16> = atlas.zones().iter().map(|z| z.zone).collect();
        assert_eq!(keys, vec![1, 2]);
        let z2 = atlas.zone(2).unwrap();
        assert_eq!(z2.quests, 2);
        assert_eq!(z2.bands[0], 1);
        assert_eq!(z2.bands[1], 1);
        assert_eq!(z2.hub, [200.0, 0.0]);
        assert_eq!(atlas.zone_points(z2).len(), 3);
        assert!(atlas.zone(9).is_none());
        let z1 = atlas.zone(1).unwrap();
        assert_eq!(z1.hub, [5000.0, 5000.0]);
        assert_eq!(z1.bands[1], 1);
    }

    #[test]
    fn spots_of_a_quest_are_found_by_id() {
        let atlas = QuestAtlas::from_entries(
            vec![
                entry(10, 3, false, &[(100.0, 0.0, 2), (300.0, 0.0, 3)]),
                entry(11, 8, true, &[(200.0, 0.0, 2)]),
            ],
            vec![],
        );
        let xs: Vec<f32> = atlas.spots(10).map(|p| p.x).collect();
        assert_eq!(xs.len(), 2);
        assert!(xs.contains(&100.0) && xs.contains(&300.0));
        assert_eq!(atlas.spots(11).count(), 1);
        assert_eq!(atlas.spots(12).count(), 0);
        assert!(atlas.spots(11).all(|p| p.story));
    }

    #[test]
    fn a_quest_keeps_a_few_spots_and_duplicates_collapse() {
        let spots: Vec<(f32, f32, u16)> = (0..10).map(|i| (i as f32, 0.0, 1)).collect();
        let atlas = QuestAtlas::from_entries(vec![entry(10, 3, false, &spots)], vec![]);
        assert_eq!(atlas.len(), MAX_SPOTS);
        let twice = QuestAtlas::from_entries(
            vec![
                entry(10, 3, false, &[(1.0, 1.0, 1)]),
                entry(10, 3, false, &[(1.0, 1.0, 1)]),
            ],
            vec![],
        );
        assert_eq!(twice.len(), 1);
        assert_eq!(twice.zone(1).unwrap().quests, 1);
    }

    #[test]
    fn chains_are_looked_up_by_race() {
        let atlas = QuestAtlas::from_entries(
            vec![],
            vec![Chain {
                race: 4,
                category: 8,
                quests: Box::new([5, 6, 7]),
            }],
        );
        assert_eq!(atlas.chain(4).unwrap().category, 8);
        assert!(atlas.chain(1).is_none());
        assert_eq!(atlas.chains().len(), 1);
        assert!(atlas.bytes() > 0);
        assert!(atlas.is_empty());
    }
}
