//! Dominion snapshots and confirmed siege enrollment from the current game connection.

use std::collections::BTreeMap;

use aa_proto::sc_siege::{self, Event, Siege, Team};

use crate::bot::{ctx::Ctx, state::dist2d};

const MAX_GROUPS: usize = 24;
const MAX_WALK_M: f32 = 2_500.0;
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
pub struct Book(BTreeMap<u16, Site>);

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
            self.0.remove(&group);
            return;
        }
        if !self.0.contains_key(&group) && self.0.len() >= MAX_GROUPS {
            return;
        }
        let site = self.0.entry(group).or_default();
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
        self.0.values().any(|site| site.tower == object)
    }

    pub fn engravable(&self, group: u16) -> bool {
        self.0.get(&group).is_some_and(|site| site.engravable)
    }

    pub fn engraving(&self, group: u16) -> bool {
        self.0.get(&group).is_some_and(|site| site.engraving)
    }

    pub fn note_tower_buff(&mut self, tower: u32, buff: u32) {
        if let Some(site) = self.0.values_mut().find(|site| site.tower == tower) {
            match buff {
                ENGRAVABLE_BUFF => site.engravable = true,
                ENGRAVING_BUFF => site.engraving = true,
                _ => {}
            }
        }
    }

    pub fn enemy(&self, order: Order, character: u32) -> bool {
        self.0.get(&order.group).is_some_and(|site| {
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
    if !(ctx.cfg.guild.enabled && ctx.cfg.pvp.enabled)
        || ctx.cfg.guild.siege_zones.is_empty()
        || ctx.state.place.in_instance()
        || ctx.state.me.dead
        || ctx.state.me.level < 35
        || crate::bot::goals::trade_run::carried_pack_of(ctx).is_some()
    {
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
            let site = guild.sieges.0.get(&z)?;
            let order = order(z, site, guild.log.id, ctx.state.me.char_id)?;
            let dist = dist2d(ctx.pos(), order.position);
            (dist <= MAX_WALK_M || ctx.would_teleport(dist)).then_some((order, dist))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(order, _)| order)
}

pub fn owner(ctx: &Ctx<'_>, group: u16) -> Option<u32> {
    ctx.state
        .guild
        .get()?
        .sieges
        .0
        .get(&group)
        .map(|site| site.owner)
}

pub fn battle_finished(ctx: &Ctx<'_>, group: u16) -> bool {
    ctx.state
        .guild
        .get()
        .and_then(|guild| guild.sieges.0.get(&group))
        .is_some_and(|site| site.siege.period != sc_siege::BATTLE)
}

pub fn context(ctx: &Ctx<'_>) -> Option<String> {
    if ctx.cfg.guild.siege_zones.is_empty() {
        return None;
    }
    let guild = ctx.state.guild.log()?;
    ctx.cfg.guild.siege_zones.get(&guild.id.to_string())?;
    let (won, held) = ctx
        .hub
        .journal
        .read(ctx.persona.id, |j| (j.sieges_won, j.sieges_held));
    let mut summary = format!("Guild siege history includes {won} captures and {held} defenses.");
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
    Some(summary)
}

pub fn patrol_sites<'a>(ctx: &Ctx<'a>) -> &'a [crate::config::TerritoryPatrol] {
    let cfg = &ctx.cfg.guild;
    if !cfg.enabled || cfg.territory_patrols.is_empty() {
        return &[];
    }
    let Some(guild) = ctx.state.guild.get() else {
        return &[];
    };
    let log = &guild.log;
    let character = ctx.state.me.char_id;
    if !log.in_guild()
        || log.is_owner(character)
        || log.member(character).is_none()
        || !ctx.hub.is_bot_char(log.owner)
    {
        return &[];
    }
    let key = log.id.to_string();
    let sites = cfg.territory_patrols.get(&key);
    let holds = cfg.siege_zones.get(&key).is_some_and(|zones| {
        zones.iter().any(|group| {
            guild
                .sieges
                .0
                .get(group)
                .is_some_and(|site| site.owner == log.id)
        })
    });
    if holds {
        sites.map_or(&[], Vec::as_slice)
    } else {
        &[]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        };
        book.observe(&Event::Territory {
            group: 33,
            owner: 44,
            tower: 700,
            position: [5000.0, 6000.0, 120.0],
            siege,
        });
        let attacker = |b: &Book| order(33, &b.0[&33], 43, 7).unwrap();
        assert_eq!(attacker(&book).side, Side::Attack);
        assert!(!attacker(&book).enrolled);
        assert!(book.enemy(attacker(&book), 9));
        assert!(order(33, &book.0[&33], 45, 7).is_none());
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
        assert_eq!(book.0[&33].owner, 44);
        book.observe(&Event::Phase {
            group: 33,
            owner: Some(43),
            siege: Siege::default(),
        });
        assert!(!book.engravable(33));
        assert!(order(33, &book.0[&33], 43, 7).is_none());
        assert_eq!(book.0[&33].owner, 43);
    }
}
