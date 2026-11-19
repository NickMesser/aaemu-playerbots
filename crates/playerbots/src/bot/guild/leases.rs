//! Guild planting space advertised by the current house owner and server permission.

use aa_proto::sc_activity::{ActivityEvent, HouseInfo};

use crate::bot::state::{BotState, HouseBrief};
use crate::config::{FarmLease, GuildCfg};

const GUILD_PERMISSION: u8 = 1;
const MAX_HOUSES: usize = 32;

#[derive(Debug)]
struct Property {
    house: HouseBrief,
    object: u32,
    owner: u32,
    permission: u8,
}

#[derive(Debug, Default)]
pub struct Book(Vec<Property>);

impl Book {
    fn observe(&mut self, event: &ActivityEvent, cfg: &GuildCfg) {
        match event {
            ActivityEvent::NearbyHouse(info) => self.note(info, cfg),
            ActivityEvent::HousePermission { tl, permission } => {
                if let Some(property) = self.0.iter_mut().find(|property| property.house.tl == *tl)
                {
                    property.permission = *permission;
                }
            }
            ActivityEvent::HouseSold { tl } | ActivityEvent::HouseDemolished { tl } => {
                self.0.retain(|property| property.house.tl != *tl);
            }
            _ => {}
        }
    }

    fn note(&mut self, info: &HouseInfo, cfg: &GuildCfg) {
        if info.obj == 0
            || ![info.x, info.y, info.z].iter().all(|p| p.is_finite())
            || !cfg
                .farm_leases
                .values()
                .flatten()
                .any(|lease| lease.house == info.id)
        {
            return;
        }
        let property = Property {
            object: info.obj,
            owner: info.owner,
            permission: info.permission,
            house: HouseBrief {
                tl: info.tl,
                id: info.id,
                template: info.template,
                pos: [info.x, info.y, info.z],
                build_left: info.all_action.saturating_sub(info.cur_action).max(0),
            },
        };
        self.0.retain(|previous| previous.house.id != info.id);
        if self.0.len() < MAX_HOUSES {
            self.0.push(property);
        }
    }

    pub fn forget(&mut self, objects: &[u32]) {
        self.0
            .retain(|property| !objects.contains(&property.object));
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }
}

pub fn observe(state: &mut BotState, cfg: &GuildCfg, event: &ActivityEvent) {
    if !cfg.enabled || cfg.farm_leases.is_empty() {
        return;
    }
    if matches!(event, ActivityEvent::NearbyHouse(_)) {
        state.guild.mem().leases.observe(event, cfg);
    } else if let Some(guild) = state.guild.get_mut() {
        guild.leases.observe(event, cfg);
    }
}

pub fn available<'a>(
    state: &'a BotState,
    cfg: &'a GuildCfg,
) -> impl Iterator<Item = (u32, HouseBrief, FarmLease)> + 'a {
    state.guild.get().into_iter().flat_map(move |guild| {
        let log = &guild.log;
        let configured = cfg
            .farm_leases
            .get(&log.id.to_string())
            .filter(|_| cfg.enabled && log.in_guild() && log.member(state.me.char_id).is_some());
        configured.into_iter().flatten().filter_map(move |&terms| {
            let property = guild
                .leases
                .0
                .iter()
                .find(|property| property.house.id == terms.house)?;
            (property.permission == GUILD_PERMISSION
                && property.owner != state.me.char_id
                && log.member(property.owner).is_some())
            .then_some((log.id, property.house, terms))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::guild::log::Member;

    #[test]
    fn a_lease_requires_current_membership_and_native_guild_permission() {
        let mut cfg = GuildCfg {
            enabled: true,
            ..Default::default()
        };
        cfg.farm_leases.insert(
            "42".into(),
            vec![FarmLease {
                house: 9,
                rent_copper: 20,
            }],
        );
        let mut state = BotState::new();
        state.me.char_id = 2;
        let guild = state.guild.mem();
        guild.log.id = 42;
        guild.log.members = [1, 2]
            .into_iter()
            .map(|char_id| Member {
                char_id,
                name: "Member".into(),
                level: 30,
                role: 1,
                online: true,
                since_ms: 0,
            })
            .collect();
        guild.leases.0.push(Property {
            house: HouseBrief {
                tl: 92,
                id: 9,
                template: 14,
                pos: [100.0, 200.0, 50.0],
                build_left: 0,
            },
            object: 723,
            owner: 1,
            permission: GUILD_PERMISSION,
        });
        assert_eq!(
            available(&state, &cfg)
                .next()
                .map(|(_, _, terms)| terms.rent_copper),
            Some(20)
        );
        observe(
            &mut state,
            &cfg,
            &ActivityEvent::HousePermission {
                tl: 92,
                permission: 0,
            },
        );
        assert!(available(&state, &cfg).next().is_none());
        observe(
            &mut state,
            &cfg,
            &ActivityEvent::HousePermission {
                tl: 92,
                permission: GUILD_PERMISSION,
            },
        );
        state.guild.mem().log.members.remove(0);
        assert!(available(&state, &cfg).next().is_none());
        observe(&mut state, &cfg, &ActivityEvent::HouseSold { tl: 92 });
        assert!(state.guild.mem().leases.0.is_empty());
    }
}
