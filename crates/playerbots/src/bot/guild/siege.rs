//! Dominion snapshots and confirmed siege enrollment from the current game connection.

use std::collections::{BTreeMap, VecDeque};

use aa_proto::sc_siege::{self, Event, Siege, Team};
use aa_proto::sc_world::UnitInfo;

use crate::bot::{ctx::Ctx, state::dist2d};

const MAX_GROUPS: usize = 24;
const MAX_WALK_M: f32 = 2_500.0;
const CAMP_MARGIN_M: f32 = 5.0;
pub const ENGRAVABLE_BUFF: u32 = 5131;
pub const ENGRAVING_BUFF: u32 = 5074;

#[derive(Debug, Default)]
struct Site {
    owner: u32,
    tower: u32,
    position: [f32; 3],
    siege: Siege,
    engravable: bool,
    engraving: bool,
}

#[derive(Debug, Default)]
pub struct Book {
    sites: BTreeMap<u16, Site>,
    /// Nearby houses announced before the castle snapshot arrives. Absent with sieges disabled.
    pub(in crate::bot) pending_houses: Option<VecDeque<UnitInfo>>,
}

impl Book {
    pub fn observe(&mut self, event: &Event) {
        let group = match event {
            Event::Territory { group, .. }
            | Event::Phase { group, .. }
            | Event::Declared { group, .. }
            | Event::Member { group, .. }
            | Event::Owner { group, .. }
            | Event::TowerAlert { group, .. }
            | Event::Deleted(group) => *group,
        };
        if matches!(event, Event::Deleted(_)) {
            self.sites.remove(&group);
            return;
        }
        if !self.sites.contains_key(&group) && self.sites.len() >= MAX_GROUPS {
            return;
        }
        let site = self.sites.entry(group).or_default();
        match event {
            Event::Territory {
                owner,
                tower,
                position,
                siege,
                ..
            } => {
                site.owner = *owner;
                site.tower = *tower;
                site.position = *position;
                site.siege = siege.clone();
                if siege.period != sc_siege::BATTLE {
                    site.engravable = false;
                    site.engraving = false;
                }
            }
            Event::Phase { owner, siege, .. } => {
                if let Some(owner) = owner {
                    site.owner = *owner;
                }
                site.siege = siege.clone();
                if siege.period != sc_siege::BATTLE {
                    site.engravable = false;
                    site.engraving = false;
                }
            }
            Event::Declared { attackers, .. } => site.siege.attackers = attackers.clone(),
            Event::Member {
                expedition,
                character,
                added,
                ..
            } => {
                let team = if site.siege.defenders.expedition == *expedition {
                    &mut site.siege.defenders
                } else if site.siege.attackers.expedition == *expedition {
                    &mut site.siege.attackers
                } else {
                    return;
                };
                update_member(team, *character, *added);
            }
            Event::Owner { owner, .. } => site.owner = *owner,
            Event::TowerAlert {
                alert: sc_siege::ENGRAVABLE,
                ..
            } => site.engravable = true,
            Event::TowerAlert {
                alert: sc_siege::ENGRAVING_STARTED,
                ..
            } => site.engraving = true,
            Event::TowerAlert {
                alert: sc_siege::ENGRAVING_STOPPED,
                ..
            } => site.engraving = false,
            Event::TowerAlert {
                alert: sc_siege::ENGRAVING_SUCCEEDED,
                ..
            } => {
                site.engravable = false;
                site.engraving = false;
            }
            Event::TowerAlert { .. } => {}
            Event::Deleted(_) => {}
        }
    }

    pub fn knows_tower(&self, object: u32) -> bool {
        self.sites.values().any(|site| site.tower == object)
    }

    pub fn engravable(&self, group: u16) -> bool {
        self.sites.get(&group).is_some_and(|site| site.engravable)
    }

    pub fn engraving(&self, group: u16) -> bool {
        self.sites.get(&group).is_some_and(|site| site.engraving)
    }

    pub fn note_tower_buff(&mut self, tower: u32, buff: u32) {
        if let Some(site) = self.sites.values_mut().find(|site| site.tower == tower) {
            match buff {
                ENGRAVABLE_BUFF => site.engravable = true,
                ENGRAVING_BUFF => site.engraving = true,
                _ => {}
            }
        }
    }

    pub fn enemy(&self, order: Order, character: u32) -> bool {
        self.sites.get(&order.group).is_some_and(|site| {
            let team = match order.side {
                Side::Attack => &site.siege.defenders,
                Side::Defend => &site.siege.attackers,
            };
            team.members.contains(&character)
        })
    }
}

fn update_member(team: &mut Team, character: u32, added: bool) {
    let mut members = Vec::from(std::mem::take(&mut team.members));
    if added && members.len() < 100 {
        if !members.contains(&character) {
            members.push(character);
        }
    } else if !added {
        members.retain(|&member| member != character);
    }
    team.members = members.into_boxed_slice();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Attack,
    Defend,
}

#[derive(Debug, Clone, Copy)]
pub struct Order {
    pub group: u16,
    pub tower: u32,
    pub position: [f32; 3],
    pub period: u8,
    pub side: Side,
    pub enrolled: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct Declaration {
    pub group: u16,
    pub expedition: u32,
    pub camp: u32,
    pub position: [f32; 2],
}

#[derive(Debug, Clone, Copy)]
pub struct CampBid {
    pub declaration: Declaration,
    pub cycle: i64,
    pub budget: i32,
}

pub const CAMP_START_COPPER: i32 = 5_000_000;

// r208022 compact.sqlite3, siege_zones.declare_item_id keyed by zone_group_id.
fn declaration_camp(group: u16) -> Option<u32> {
    Some(match group {
        33 => 21_134,
        34 => 21_130,
        43 => 21_136,
        44 => 21_135,
        54 => 27_744,
        56 => 27_745,
        _ => return None,
    })
}

fn accepts_declaration(site: &Site, expedition: u32, position: [f32; 2]) -> bool {
    site.siege.period == sc_siege::DECLARATION && camp_site(site, expedition, position)
}

fn camp_site(site: &Site, expedition: u32, position: [f32; 2]) -> bool {
    let [x, y, _] = site.position;
    let distance = (position[0] - x).hypot(position[1] - y);
    site.tower != 0
        && site.owner != 0
        && site.owner != expedition
        && site.siege.attackers.expedition == 0
        && site.siege.radius_declare > 0
        && distance >= f32::from(site.siege.radius_declare) + CAMP_MARGIN_M
}

fn order(group: u16, site: &Site, expedition: u32, character: u32) -> Option<Order> {
    let siege = &site.siege;
    if !matches!(
        siege.period,
        sc_siege::DECLARATION | sc_siege::WARMUP | sc_siege::BATTLE
    ) || siege.attackers.expedition == 0
        || siege.attackers.expedition == siege.defenders.expedition
        || site.tower == 0
        || !site.position.iter().all(|coord| coord.is_finite())
    {
        return None;
    }
    let (team, side) = if expedition == siege.attackers.expedition {
        (&siege.attackers, Side::Attack)
    } else if expedition == siege.defenders.expedition {
        (&siege.defenders, Side::Defend)
    } else {
        return None;
    };
    Some(Order {
        group,
        tower: site.tower,
        position: site.position,
        period: siege.period,
        side,
        enrolled: team.members.contains(&character),
    })
}

pub fn available(ctx: &Ctx<'_>, group: Option<u16>) -> Option<Order> {
    if !may_set_out(ctx) {
        return None;
    }
    let guild = ctx.state.guild.get()?;
    if !ctx.hub.is_bot_char(guild.log.owner) {
        return None;
    }
    let zones = ctx.cfg.guild.siege_zones.get(&guild.log.id.to_string())?;
    zones
        .iter()
        .copied()
        .filter(|&z| group.is_none_or(|want| want == z))
        .filter_map(|z| {
            let site = guild.sieges.sites.get(&z)?;
            let order = order(z, site, guild.log.id, ctx.state.me.char_id)?;
            let dist = dist2d(ctx.pos(), order.position);
            (dist <= MAX_WALK_M || ctx.would_teleport(dist)).then_some((order, dist))
        })
        .min_by(|a, b| {
            b.0.period
                .cmp(&a.0.period)
                .then_with(|| a.1.total_cmp(&b.1))
        })
        .map(|(order, _)| order)
}

fn may_set_out(ctx: &Ctx<'_>) -> bool {
    ctx.cfg.guild.enabled
        && ctx.cfg.pvp.enabled
        && !ctx.cfg.guild.siege_zones.is_empty()
        && !ctx.state.place.in_instance()
        && !ctx.state.me.dead
        && ctx.state.me.level >= 35
        && crate::bot::goals::trade_run::carried_pack_of(ctx).is_none()
}

pub fn declaration(ctx: &Ctx<'_>) -> Option<Declaration> {
    declaration_candidate(ctx, sc_siege::DECLARATION, |camp, _| {
        ctx.state.find_item(camp).is_some()
    })
    .map(|(order, _)| order)
}

pub fn camp_bid(ctx: &Ctx<'_>) -> Option<CampBid> {
    if ctx.cfg.bots.gm_bootstrap || !ctx.cfg.market.enabled || !ctx.cfg.market.auction {
        return None;
    }
    let key = ctx.state.guild.log()?.id.to_string();
    let limit = i64::from(*ctx.cfg.guild.siege_camp_bids.get(&key)?);
    let reserve = ctx
        .cfg
        .guild
        .treasuries
        .get(&key)
        .map_or(0, |terms| i64::from(terms.reserve_copper));
    let budget = i32::try_from(limit.min(ctx.state.me.money.saturating_sub(reserve))).ok()?;
    if budget < CAMP_START_COPPER {
        return None;
    }
    let latest = ctx
        .hub
        .journal
        .read(ctx.persona.id, |journal| journal.siege_camp_bid_cycle);
    let (declaration, cycle) =
        declaration_candidate(ctx, sc_siege::CAMP_AUCTION, |camp, cycle| {
            cycle > latest && ctx.state.find_item(camp).is_none()
        })?;
    Some(CampBid {
        declaration,
        cycle,
        budget,
    })
}

fn declaration_candidate(
    ctx: &Ctx<'_>,
    period: u8,
    wants_item: impl Fn(u32, i64) -> bool,
) -> Option<(Declaration, i64)> {
    if !may_set_out(ctx) || ctx.cfg.guild.siege_declarations.is_empty() {
        return None;
    }
    let guild = ctx.state.guild.get()?;
    let expedition = guild.log.id;
    let character = ctx.state.me.char_id;
    if expedition == 0 || guild.log.owner != character || !ctx.hub.is_bot_char(character) {
        return None;
    }
    if guild.sieges.sites.values().any(|site| {
        let siege = &site.siege;
        (matches!(siege.period, sc_siege::WARMUP | sc_siege::BATTLE)
            || siege.period == sc_siege::DECLARATION && siege.attackers.expedition != 0)
            && (siege.attackers.expedition == expedition
                || siege.defenders.expedition == expedition)
    }) {
        return None;
    }
    let [x, y, _] = ctx.pos();
    ctx.cfg
        .guild
        .siege_declarations
        .get(&expedition.to_string())?
        .iter()
        .filter_map(|place| {
            let group = place.zone_group;
            let camp = declaration_camp(group)?;
            let site = guild.sieges.sites.get(&group)?;
            let [cx, cy] = place.position;
            let distance = (cx - x).hypot(cy - y);
            let cycle = site.siege.started;
            if site.siege.period != period
                || !camp_site(site, expedition, place.position)
                || !wants_item(camp, cycle)
                || period == sc_siege::DECLARATION && distance > MAX_WALK_M
                || period == sc_siege::CAMP_AUCTION && (cycle <= 0 || site.siege.durations[0] <= 0)
            {
                return None;
            }
            ctx.world
                .zone_info_at(cx, cy)
                .filter(|zone| zone.group == group)?;
            ctx.world
                .ground_z(cx, cy)
                .filter(|&z| z > crate::bot::motion::SEA_LEVEL)?;
            Some((
                Declaration {
                    group,
                    expedition,
                    camp,
                    position: place.position,
                },
                cycle,
                distance,
            ))
        })
        .min_by(|a, b| a.2.total_cmp(&b.2))
        .map(|(order, cycle, _)| (order, cycle))
}

pub fn declared_by(ctx: &Ctx<'_>, order: Declaration) -> bool {
    ctx.state.guild.get().is_some_and(|guild| {
        guild.log.id == order.expedition
            && guild
                .sieges
                .sites
                .get(&order.group)
                .is_some_and(|site| site.siege.attackers.expedition == order.expedition)
    })
}

pub fn declaration_open(ctx: &Ctx<'_>, order: Declaration) -> bool {
    may_set_out(ctx)
        && ctx.state.guild.get().is_some_and(|guild| {
            guild.log.id == order.expedition
                && guild.log.owner == ctx.state.me.char_id
                && guild
                    .sieges
                    .sites
                    .get(&order.group)
                    .is_some_and(|site| accepts_declaration(site, order.expedition, order.position))
        })
}

pub fn owner(ctx: &Ctx<'_>, group: u16) -> Option<u32> {
    ctx.state
        .guild
        .get()?
        .sieges
        .sites
        .get(&group)
        .map(|site| site.owner)
}

pub fn battle_finished(ctx: &Ctx<'_>, group: u16) -> bool {
    ctx.state
        .guild
        .get()
        .and_then(|guild| guild.sieges.sites.get(&group))
        .is_some_and(|site| site.siege.period != sc_siege::BATTLE)
}

pub fn context(ctx: &Ctx<'_>) -> Option<String> {
    if ctx.cfg.guild.siege_zones.is_empty() {
        return None;
    }
    let guild = ctx.state.guild.log()?;
    ctx.cfg.guild.siege_zones.get(&guild.id.to_string())?;
    let (declared, won, held) = ctx.hub.journal.read(ctx.persona.id, |j| {
        (j.sieges_declared, j.sieges_won, j.sieges_held)
    });
    let mut summary = format!("My guild siege history includes {declared} declarations, {won} captures and {held} defenses.");
    if let Some(order) = declaration(ctx) {
        summary.push_str(&format!(
            " I have the camp for the open declaration window in territory {}.",
            order.group
        ));
    }
    if let Some(bid) = camp_bid(ctx) {
        summary.push_str(&format!(
            " The camp auction for territory {} is open and I can bid up to {} copper.",
            bid.declaration.group, bid.budget
        ));
    }
    if let Some(order) = available(ctx, None) {
        let side = if order.side == Side::Attack {
            "attack"
        } else {
            "defend"
        };
        let status = if order.enrolled {
            "enrolled"
        } else {
            "eligible to register"
        };
        summary.push_str(&format!(
            " I am {status} to {side} territory {}.",
            order.group
        ));
    }
    let sites = patrol_sites(ctx).len();
    if sites > 0 {
        summary.push_str(&format!(
            " The guild holds a castle and has {sites} resource or trade patrol sites."
        ));
    }
    if !rift_patrols(ctx).is_empty() {
        summary.push_str(" The guild can patrol enabled rifts during an active war.");
    }
    Some(summary)
}

pub fn patrol_sites<'a>(ctx: &Ctx<'a>) -> &'a [crate::config::TerritoryPatrol] {
    let cfg = &ctx.cfg.guild;
    if cfg.territory_patrols.is_empty() {
        return &[];
    }
    holding_guild(ctx)
        .and_then(|key| cfg.territory_patrols.get(&key))
        .map_or(&[], Vec::as_slice)
}

pub fn rift_patrols<'a>(ctx: &Ctx<'a>) -> &'a [u32] {
    let cfg = &ctx.cfg.guild;
    if cfg.rift_patrols.is_empty() {
        return &[];
    }
    holding_guild(ctx)
        .and_then(|key| cfg.rift_patrols.get(&key))
        .map_or(&[], Vec::as_slice)
}

fn holding_guild(ctx: &Ctx<'_>) -> Option<String> {
    let cfg = &ctx.cfg.guild;
    let guild = ctx.state.guild.get()?;
    let log = &guild.log;
    let character = ctx.state.me.char_id;
    if !cfg.enabled
        || !log.in_guild()
        || log.is_owner(character)
        || log.member(character).is_none()
        || !ctx.hub.is_bot_char(log.owner)
    {
        return None;
    }
    let key = log.id.to_string();
    let holds = cfg.siege_zones.get(&key).is_some_and(|zones| {
        zones.iter().any(|group| {
            guild
                .sieges
                .sites
                .get(group)
                .is_some_and(|site| site.owner == log.id)
        })
    });
    holds.then_some(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_camp_can_only_approach_the_matching_open_enemy_territory() {
        let mut site = Site {
            owner: 44,
            tower: 7,
            position: [2000.0, 3000.0, 100.0],
            siege: Siege {
                period: sc_siege::DECLARATION,
                radius_declare: 250,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(declaration_camp(44), Some(21_135));
        assert_eq!(declaration_camp(70), None);
        assert!(accepts_declaration(&site, 43, [2255.0, 3000.0]));
        assert!(!accepts_declaration(&site, 43, [2254.0, 3000.0]));
        assert!(!accepts_declaration(&site, 44, [2255.0, 3000.0]));
        site.siege.attackers.expedition = 45;
        assert!(!accepts_declaration(&site, 43, [2255.0, 3000.0]));
        site.siege.attackers.expedition = 0;
        site.siege.period = sc_siege::WARMUP;
        assert!(!accepts_declaration(&site, 43, [2255.0, 3000.0]));
    }

    #[test]
    fn a_castle_roster_is_scoped_to_the_server_assigned_side() {
        let mut book = Book::default();
        let siege = Siege {
            period: sc_siege::BATTLE,
            defenders: Team {
                expedition: 44,
                members: [9].into(),
            },
            attackers: Team {
                expedition: 43,
                members: Box::default(),
            },
            ..Default::default()
        };
        book.observe(&Event::Territory {
            group: 33,
            owner: 44,
            tower: 700,
            position: [5000.0, 6000.0, 120.0],
            siege,
        });
        let attacker = |b: &Book| order(33, &b.sites[&33], 43, 7).unwrap();
        assert_eq!(attacker(&book).side, Side::Attack);
        assert!(!attacker(&book).enrolled);
        assert!(book.enemy(attacker(&book), 9));
        assert!(order(33, &book.sites[&33], 45, 7).is_none());
        book.observe(&Event::Member {
            group: 33,
            expedition: 43,
            character: 7,
            added: true,
        });
        assert!(attacker(&book).enrolled);
        book.observe(&Event::TowerAlert {
            group: 33,
            alert: sc_siege::ENGRAVABLE,
        });
        assert!(book.engravable(33));
        assert_eq!(book.sites[&33].owner, 44);
        book.observe(&Event::Phase {
            group: 33,
            owner: Some(43),
            siege: Siege::default(),
        });
        assert!(!book.engravable(33));
        assert!(order(33, &book.sites[&33], 43, 7).is_none());
        assert_eq!(book.sites[&33].owner, 43);
    }
}
