//! Farming: what a bot remembers about its plots, and the pieces the farm goal is made of.
//!
//! * [`FarmMem`] (this file): the plots a bot planted, which of them are ripe, where it must not
//!   plant again and whether the harvest it is bringing in counts as the bot's own. It lives behind
//!   one pointer in [`MarketState`](super::market::MarketState) (`Inner::farm`), so a bot that never
//!   farmed pays eight bytes more and the size budget of [`Bot`](super::Bot) stays where it was.
//! * [`plan`]: which crop to grow (what is wished for, what is scarce, what pays) and what the
//!   plan needs (seeds to buy, a vendor, a place).
//! * [`site`]: where to plant: the land of the bot's houses, the public farms and (for an outlaw
//!   with `[farm] wild`) a small patch out in the wild.
//! * [`board`]: what all bots are growing (one board on the hub), so that thirty bots do not all
//!   plant the one scarce item.
//!
//! The memory follows the server's events through [`FarmMem::observe`] (a crop of the bot appears,
//! changes its phase, vanishes, the list of the public farm crops arrives) and is looked at by the
//! goal ([`goals::farm`](super::goals::farm)) and by the web snapshot.

pub mod board;
pub mod plan;
pub mod site;

#[cfg(test)]
mod tests;

use aa_proto::sc_world::{CommonFarmEntry, WorldEvent};
use aa_world::{Crop, FarmKind, Farming};
use smallvec::SmallVec;

use aa_world::World;

use super::state::{dist2d, BotState};
use crate::hub::Hub;

/// The most plots a bot remembers, whatever `farm.max_plots` says.
pub const MAX_PLOTS: usize = 64;
/// A spot the server refused stays out of the choice this long.
pub const BAD_SPOT_MS: u64 = 10 * 60_000;
/// A seed the server refused to plant stays out of the choice this long.
pub const BAD_SEED_MS: u64 = 30 * 60_000;
/// A plot whose phase is unknown to the bot is looked at again after this long.
pub const RECHECK_MS: u64 = 5 * 60_000;
/// A crop that vanishes while the bot stands this close is gone (harvested, destroyed); further
/// away it only left the bot's view.
pub const VANISH_M: f32 = 12.0;
/// A plot the bot planted this recently is not dropped by an older list of the public farms.
pub const FRESH_MS: u64 = 30_000;
/// Two plots this close are the same plot (crops keep at least `site::spacing_m` of 1.6 m
/// between them, so neighbours are never this close).
pub const SAME_SPOT_M: f32 = 0.75;
/// Land type of a plot on the land of one of the bot's houses; the public farms use their
/// [`FarmKind`] as `u8`.
pub const OWN_LAND: u8 = 0;
/// Land type of a plot out in the open, on nobody's land (a wild, "illegal" farm): it costs the
/// seed's labor, and anybody may harvest it.
pub const WILD_LAND: u8 = 9;
/// Land type of a crop of somebody else the bot is taking (`[farm] steal`): never planted again,
/// never honest, forgotten when the run ends.
pub const STOLEN_LAND: u8 = 10;
/// Plots that left the memory kept for the goal at most (see [`FarmMem::take_gone`]).
pub const MAX_GONE: usize = 4;
/// Crops of others out in the wild kept in view at most (see [`FarmMem::foreign`]).
pub const MAX_FOREIGN: usize = 4;

/// A crop of somebody else, out in the wild, in the bot's view (for `[farm] steal`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Foreign {
    /// Its object id.
    pub obj: u32,
    /// The crop's doodad.
    pub doodad: u32,
    /// The character that planted it.
    pub owner: u32,
    /// Where it stands.
    pub pos: [f32; 3],
    /// Its phase.
    pub phase: u32,
}

/// A plot that left the memory: harvested by the bot, or gone without it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gone {
    /// Where it stood.
    pub pos: [f32; 3],
    /// Its land type.
    pub land: u8,
    /// The bot harvested it (else it vanished: taken by somebody, withered, destroyed).
    pub harvested: bool,
}

/// The crops the memory can tell apart (the world's [`Farming`], or a table in tests).
pub trait CropBook {
    /// The crop that grows from doodad `doodad`.
    fn by_doodad(&self, doodad: u32) -> Option<&Crop>;
}

impl CropBook for Farming {
    fn by_doodad(&self, doodad: u32) -> Option<&Crop> {
        self.crop_of_doodad(doodad)
    }
}

/// What [`FarmMem::observe`] needs to know about the bot and the world.
#[derive(Clone, Copy)]
pub struct Watcher<'a> {
    /// The bot's character id (a crop is the bot's when its owner is).
    pub char_id: u32,
    /// The bot's position.
    pub pos: [f32; 3],
    /// The bot clock in milliseconds.
    pub now_ms: u64,
    /// The crops.
    pub book: &'a dyn CropBook,
    /// The land type at (x, y): [`OWN_LAND`] or a [`FarmKind`] as `u8`.
    pub land: &'a dyn Fn(f32, f32) -> u8,
}

/// One crop of the bot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Plot {
    /// Object id while the crop is in view, else 0.
    pub obj: u32,
    /// The crop's doodad template.
    pub doodad: u32,
    /// The seed it grew from.
    pub seed: u32,
    /// Where it stands.
    pub pos: [f32; 3],
    /// Its current phase (`func_group_id`), 0 when not seen yet.
    pub phase: u32,
    /// It can be harvested.
    pub ripe: bool,
    /// The bot clock at which it is worth going back (0 = whenever).
    pub check_ms: u64,
    /// The bot clock at which it was planted, 0 for a plot found again.
    pub planted_ms: u64,
    /// The seeds were bought (the harvest may be sold).
    pub honest: bool,
    /// [`OWN_LAND`], or the [`FarmKind`] of the public farm.
    pub land: u8,
}

impl Plot {
    /// Whether the plot is worth a visit at `now_ms`.
    pub fn due(&self, now_ms: u64) -> bool {
        self.ripe || (self.check_ms != 0 && now_ms >= self.check_ms)
    }

    /// The farm kind of a public plot.
    pub fn kind(&self) -> Option<FarmKind> {
        FarmKind::from_group(u32::from(self.land))
    }
}

/// What a bot knows about farming. See the module docs.
#[derive(Debug, Default)]
pub struct FarmMem {
    /// The bot's crops.
    pub plots: SmallVec<[Plot; 4]>,
    /// Spots the server refused `(x, y, until bot clock ms)`.
    bad_spots: SmallVec<[(f32, f32, u64); 6]>,
    /// Seeds the server refused `(seed, until bot clock ms)`.
    bad_seeds: SmallVec<[(u32, u64); 3]>,
    /// Deferred livestock actions at a given position and phase.
    pub stock_retry: SmallVec<[([f32; 3], u32, u64); 4]>,
    /// While a harvest is under way: whether its seeds were bought (see the ledger docs).
    pub harvest_honest: Option<bool>,
    /// The list of the public farm crops was asked for in this session.
    pub list_asked: bool,
    /// The bot clock at which the last farm run ended, 0 before the first.
    pub last_end_ms: u64,
    /// Seeds planted in this session.
    pub planted: u32,
    /// Crops harvested in this session.
    pub harvested: u32,
    /// Crops that vanished near the bot without a harvest.
    pub lost: u32,
    /// The doodad of the crop harvested last (the goal logs what the ledger made of its produce).
    pub last_crop: u32,
    /// Crops the bot just harvested, whose removal is expected (not a loss).
    reaped: SmallVec<[u32; 4]>,
    /// Plots that left the memory since the goal last looked, at most [`MAX_GONE`].
    gone: SmallVec<[Gone; 2]>,
    /// Crops of others out in the wild in view, newest last, at most [`MAX_FOREIGN`]; kept only
    /// by a bot that already farms.
    pub foreign: SmallVec<[Foreign; 2]>,
}

impl FarmMem {
    /// The plot with object id `obj`.
    pub fn by_obj(&self, obj: u32) -> Option<&Plot> {
        if obj == 0 {
            return None;
        }
        self.plots.iter().find(|p| p.obj == obj)
    }

    /// The plot standing within [`SAME_SPOT_M`] of (x, y).
    pub fn at(&self, x: f32, y: f32) -> Option<&Plot> {
        self.plots
            .iter()
            .find(|p| dist2d(p.pos, [x, y, 0.0]) < SAME_SPOT_M)
    }

    /// How many plots are on land of type `land`.
    pub fn count_on(&self, land: u8) -> usize {
        self.plots.iter().filter(|p| p.land == land).count()
    }

    /// How many plots are due at `now_ms`.
    pub fn due(&self, now_ms: u64) -> usize {
        self.plots.iter().filter(|p| p.due(now_ms)).count()
    }

    /// Notes a new plot (planted just now). Returns false when the memory is full.
    pub fn plant(&mut self, plot: Plot) -> bool {
        if self.plots.len() >= MAX_PLOTS {
            return false;
        }
        self.plots.push(plot);
        self.planted = self.planted.saturating_add(1);
        true
    }

    /// Forgets the plot at index `i`.
    pub fn drop_plot(&mut self, i: usize) {
        if i < self.plots.len() {
            self.plots.remove(i);
        }
    }

    /// The bot stood at the place of plot `i` and its crop was not there: it is forgotten and
    /// counts as lost.
    pub fn lose_plot(&mut self, i: usize) {
        if i < self.plots.len() {
            let p = self.plots.remove(i);
            self.note_gone(p.pos, p.land, false);
            self.lost = self.lost.saturating_add(1);
        }
    }

    fn note_gone(&mut self, pos: [f32; 3], land: u8, harvested: bool) {
        if self.gone.len() >= MAX_GONE {
            self.gone.remove(0);
        }
        self.gone.push(Gone {
            pos,
            land,
            harvested,
        });
    }

    /// Whether plots left the memory since the last [`FarmMem::take_gone`].
    pub fn has_gone(&self) -> bool {
        !self.gone.is_empty()
    }

    /// The plots that left the memory since the last call (the farm goal writes the wild ones
    /// to the journal).
    pub fn take_gone(&mut self) -> SmallVec<[Gone; 2]> {
        std::mem::take(&mut self.gone)
    }

    /// The bot harvested crop `obj`: its removal from the world is no loss.
    pub fn expect_removal(&mut self, obj: u32) {
        if self.reaped.len() >= 4 {
            self.reaped.remove(0);
        }
        self.reaped.push(obj);
    }

    /// Whether (x, y) is a spot the server refused lately.
    pub fn spot_is_bad(&self, x: f32, y: f32, now_ms: u64) -> bool {
        self.bad_spots
            .iter()
            .any(|&(bx, by, until)| until > now_ms && dist2d([bx, by, 0.0], [x, y, 0.0]) < 1.5)
    }

    /// Remembers that the server refused to plant at (x, y).
    pub fn mark_bad_spot(&mut self, x: f32, y: f32, now_ms: u64) {
        self.bad_spots.retain(|b| b.2 > now_ms);
        if self.bad_spots.len() >= 6 {
            self.bad_spots.remove(0);
        }
        self.bad_spots.push((x, y, now_ms + BAD_SPOT_MS));
    }

    /// Whether the server refused to plant `seed` lately.
    pub fn seed_is_bad(&self, seed: u32, now_ms: u64) -> bool {
        self.bad_seeds
            .iter()
            .any(|&(s, until)| s == seed && until > now_ms)
    }

    /// Remembers that the server refused to plant `seed` (not for a reason of the spot).
    pub fn mark_bad_seed(&mut self, seed: u32, now_ms: u64) {
        self.bad_seeds.retain(|b| b.1 > now_ms);
        if self.bad_seeds.len() >= 3 {
            self.bad_seeds.remove(0);
        }
        self.bad_seeds.push((seed, now_ms + BAD_SEED_MS));
    }

    /// The produce all plots will give, as `(item, expected count)` summed per item, the seeds
    /// themselves left out (they are the next crop, not the product).
    pub fn growing(&self, book: &dyn CropBook) -> SmallVec<[(u32, f32); 4]> {
        let mut out: SmallVec<[(u32, f32); 4]> = SmallVec::new();
        for p in &self.plots {
            let Some(crop) = book.by_doodad(p.doodad) else {
                continue;
            };
            for y in crop.produce.iter().filter(|y| y.item != crop.seed) {
                match out.iter_mut().find(|e| e.0 == y.item) {
                    Some(e) => e.1 += y.average(),
                    None => out.push((y.item, y.average())),
                }
            }
        }
        out
    }

    /// Follows one server event (see the module docs). Only crops the bot owns matter.
    pub fn observe(&mut self, w: &Watcher<'_>, ev: &WorldEvent) {
        match ev {
            WorldEvent::DoodadsCreated(list) => {
                if w.char_id == 0 {
                    return;
                }
                for d in list.iter().filter(|d| d.owner_id != 0) {
                    let Some(crop) = w.book.by_doodad(d.template) else {
                        continue;
                    };
                    let pos = [d.pos.0, d.pos.1, d.pos.2];
                    if d.owner_id == w.char_id {
                        self.on_created(w, crop, d.obj, pos, d.phase);
                    } else if (w.land)(pos[0], pos[1]) == WILD_LAND {
                        self.on_foreign(d.obj, crop.doodad, d.owner_id, pos, d.phase);
                    }
                }
            }
            WorldEvent::DoodadsRemoved(objs) => {
                for &obj in objs.iter() {
                    self.on_removed(w, obj);
                    self.foreign.retain(|f| f.obj != obj);
                }
            }
            WorldEvent::DoodadPhase { obj, phase } => {
                if let Some(f) = self.foreign.iter_mut().find(|f| f.obj == *obj) {
                    f.phase = *phase;
                }
                self.on_phase(w, *obj, *phase)
            }
            WorldEvent::CommonFarmList(list) => {
                for e in list.iter() {
                    self.on_listed(w, e);
                }
                // the list is every crop of the character on the public farms: the ones missing
                // from it are gone (the gardens and the wild plots are not on it)
                let now = w.now_ms;
                self.plots.retain(|p| {
                    p.kind().is_none()
                        || (p.planted_ms != 0 && p.planted_ms + FRESH_MS > now)
                        || list.iter().any(|e| {
                            e.template == p.doodad
                                && dist2d(p.pos, [e.pos.0, e.pos.1, e.pos.2]) < SAME_SPOT_M
                        })
                });
            }
            _ => {}
        }
    }

    fn on_foreign(&mut self, obj: u32, doodad: u32, owner: u32, pos: [f32; 3], phase: u32) {
        self.foreign.retain(|f| f.obj != obj);
        if self.foreign.len() >= MAX_FOREIGN {
            self.foreign.remove(0);
        }
        self.foreign.push(Foreign {
            obj,
            doodad,
            owner,
            pos,
            phase,
        });
    }

    /// Forgets the crops of others the bot took on for a run ([`STOLEN_LAND`]).
    pub fn drop_stolen(&mut self) {
        self.plots.retain(|p| p.land != STOLEN_LAND);
    }

    fn on_created(&mut self, w: &Watcher<'_>, crop: &Crop, obj: u32, pos: [f32; 3], phase: u32) {
        let ripe = crop.is_ripe(phase);
        let i = self
            .plots
            .iter()
            .position(|p| p.obj == obj && obj != 0)
            .or_else(|| {
                self.plots.iter().position(|p| {
                    p.obj == 0 && p.doodad == crop.doodad && dist2d(p.pos, pos) < SAME_SPOT_M
                })
            });
        match i {
            Some(i) => {
                let p = &mut self.plots[i];
                p.obj = obj;
                p.phase = phase;
                p.ripe = ripe;
                if ripe {
                    p.check_ms = 0;
                }
            }
            None if self.plots.len() < MAX_PLOTS => self.plots.push(Plot {
                obj,
                doodad: crop.doodad,
                seed: crop.seed,
                pos,
                phase,
                ripe,
                check_ms: if ripe { 0 } else { w.now_ms + RECHECK_MS },
                planted_ms: 0,
                honest: false,
                land: (w.land)(pos[0], pos[1]),
            }),
            None => {}
        }
    }

    fn on_removed(&mut self, w: &Watcher<'_>, obj: u32) {
        let Some(i) = self.plots.iter().position(|p| p.obj == obj && obj != 0) else {
            return;
        };
        if dist2d(self.plots[i].pos, w.pos) <= VANISH_M {
            let p = self.plots.remove(i);
            let harvested = match self.reaped.iter().position(|&o| o == obj) {
                Some(k) => {
                    self.reaped.remove(k);
                    true
                }
                None => {
                    self.lost = self.lost.saturating_add(1);
                    false
                }
            };
            self.note_gone(p.pos, p.land, harvested);
        } else {
            self.plots[i].obj = 0;
        }
    }

    fn on_phase(&mut self, w: &Watcher<'_>, obj: u32, phase: u32) {
        let Some(p) = self.plots.iter_mut().find(|p| p.obj == obj && obj != 0) else {
            return;
        };
        p.phase = phase;
        if let Some(crop) = w.book.by_doodad(p.doodad) {
            p.ripe = crop.is_ripe(phase);
            if p.ripe {
                p.check_ms = 0;
            }
        }
    }

    fn on_listed(&mut self, w: &Watcher<'_>, e: &CommonFarmEntry) {
        let Some(crop) = w.book.by_doodad(e.template) else {
            return;
        };
        let pos = [e.pos.0, e.pos.1, e.pos.2];
        let ripe = crop.is_ripe(e.phase);
        let check = if ripe {
            0
        } else {
            w.now_ms + u64::from(e.time_left).max(1_000)
        };
        let land = u8::try_from(e.farm_type).unwrap_or(1).max(1);
        let i = self
            .plots
            .iter()
            .position(|p| p.doodad == e.template && dist2d(p.pos, pos) < SAME_SPOT_M);
        match i {
            Some(i) => {
                let p = &mut self.plots[i];
                p.phase = e.phase;
                p.ripe = ripe;
                p.land = land;
                if ripe || p.planted_ms == 0 || p.check_ms == 0 {
                    p.check_ms = check;
                }
            }
            None if self.plots.len() < MAX_PLOTS => self.plots.push(Plot {
                obj: 0,
                doodad: crop.doodad,
                seed: crop.seed,
                pos,
                phase: e.phase,
                ripe,
                check_ms: check,
                planted_ms: 0,
                honest: false,
                land,
            }),
            None => {}
        }
    }
}

/// Feeds a server event to the bot's farm memory (when it concerns a crop of the bot) and returns
/// whether the set of plots changed. The caller does this for every event before the usual
/// filters, because the crops are not kept as doodads in view.
pub fn observe(state: &mut BotState, world: &World, ev: &WorldEvent) -> bool {
    let farm = world.farm();
    if farm.crops().is_empty() {
        return false;
    }
    let char_id = state.me.char_id;
    // (a bot that farms already also keeps an eye on the crops of others out in the wild)
    let farms = state.market.farm().is_some();
    let relevant = match ev {
        WorldEvent::DoodadsCreated(list) => {
            char_id != 0
                && list.iter().any(|d| {
                    (d.owner_id == char_id || (farms && d.owner_id != 0))
                        && farm.is_crop_doodad(d.template)
                })
        }
        WorldEvent::DoodadsRemoved(_) | WorldEvent::DoodadPhase { .. } => state
            .market
            .farm()
            .is_some_and(|f| !f.plots.is_empty() || !f.foreign.is_empty()),
        WorldEvent::CommonFarmList(_) => true,
        _ => false,
    };
    if !relevant {
        return false;
    }
    let (pos, now_ms) = (state.me.pos, state.now_ms);
    let areas = farm.areas();
    let gardens = site::gardens(&state.houses, |t| {
        world
            .housing_design(t)
            .map_or(4.0, crate::bot::goals::housing::garden_radius)
    });
    let housing = |x: f32, y: f32| world.housing_area_at(x, y).is_some();
    let land = |x: f32, y: f32| site::land_of(areas, &gardens, &housing, x, y);
    let watcher = Watcher {
        char_id,
        pos,
        now_ms,
        book: farm,
        land: &land,
    };
    let mem = state.market.farm_mut();
    let before = mem.plots.len();
    mem.observe(&watcher, ev);
    mem.plots.len() != before
}

/// Tells the farm board what the bot's plots will give.
pub fn publish(hub: &Hub, bot: u32, state: &BotState, world: &World) {
    let growing = state
        .market
        .farm()
        .map(|m| m.growing(world.farm()))
        .unwrap_or_default();
    hub.farms.publish(bot, &growing);
}

/// The bot's plots as lines for the web UI (nothing for a bot that never farmed).
pub fn plot_lines(state: &BotState, world: &World, now_ms: u64, out: &mut Vec<pb_web::PlotLine>) {
    let Some(mem) = state.market.farm() else {
        return;
    };
    for p in mem.plots.iter().take(MAX_PLOTS) {
        let crop = world.doodad(p.doodad).map_or("a crop", |d| &*d.name);
        let state = if p.ripe {
            "ripe".to_owned()
        } else if p.obj == 0 {
            "out of view".to_owned()
        } else if p.check_ms > now_ms {
            let left = (p.check_ms - now_ms) / 60_000;
            if left == 0 {
                "growing, less than a minute left".to_owned()
            } else {
                format!("growing, about {left} min left")
            }
        } else {
            "due".to_owned()
        };
        out.push(pb_web::PlotLine {
            x: p.pos[0],
            y: p.pos[1],
            crop: crop.to_owned(),
            state,
            honest: p.honest,
            land: match p.kind() {
                Some(k) => k.name(),
                None if p.land == WILD_LAND => "wild",
                None => "own land",
            }
            .to_owned(),
        });
    }
}

/// The server's refusals of a planting (the `ErrorMessageType` ids the doodad placement sends,
/// `systems/economy/error_message.rs` and `systems/npcs/doodad/placement.rs` of the server).
pub mod refusal {
    /// The land belongs to a house whose owner does not let the character plant there.
    pub const NO_PERM: i16 = 2;
    /// The spot is outside what the doodad's area rules allow.
    pub const AREA: i16 = 276;
    /// Another doodad is too close.
    pub const TOO_CLOSE_DOODAD: i16 = 277;
    /// A unit stands too close.
    pub const TOO_CLOSE_UNIT: i16 = 278;
    /// The spot is too far from the character.
    pub const TOO_FAR: i16 = 154;
    /// The character has as many crops on public farms of this kind as allowed.
    pub const FARM_COUNT: i16 = 493;
    /// The public farm does not take this crop.
    pub const FARM_TYPE: i16 = 494;
    /// Not enough labor points (`LABOR_POWER_NEEDED`; a seed out in the open costs the labor of
    /// its use skill).
    pub const NO_LABOR: i16 = 9;
    /// The seed could not be used.
    pub const BAD_ITEM: i16 = 207;
    /// The zone forbids the item.
    pub const ZONE_BANS_ITEM: i16 = 440;
}
