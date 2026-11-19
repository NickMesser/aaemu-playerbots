//! Nearby ship and cannon snapshots while a Thief is sailing an armed ship.

use aa_proto::sc_travel::ShipMotion;
use aa_proto::sc_world::{unit_state_yaw_radians, unit_type, WorldEvent};
pub use aa_world::travel::boat::CANNON_SLAVE as CANNON;
use rustc_hash::FxHashMap;

const MAX_SLAVES: usize = 96;

#[derive(Clone, Copy, Debug)]
pub struct Slave {
    pub owner: u32,
    pub tl: u16,
    pub template: u32,
    pub parent: u32,
    pub origin: [f32; 3],
    pub yaw: f32,
    pub motion: Option<ShipMotion>,
}

#[derive(Debug, Default)]
pub struct Sight {
    pub slaves: FxHashMap<u32, Slave>,
    pub last_hit: Option<(u32, u32)>,
}

impl Sight {
    pub(super) fn observe(&mut self, event: &WorldEvent) {
        match event {
            WorldEvent::UnitState(info) if info.unit_type == unit_type::SLAVE => {
                if info.complete
                    && info.owner_id != 0
                    && info.tl_id != 0
                    && !info.dead
                    && (self.slaves.len() < MAX_SLAVES || self.slaves.contains_key(&info.obj))
                {
                    self.slaves.insert(
                        info.obj,
                        Slave {
                            owner: info.owner_id,
                            tl: info.tl_id,
                            template: info.id,
                            parent: info.slave_parent,
                            origin: [info.pos.0, info.pos.1, info.pos.2],
                            yaw: unit_state_yaw_radians(info.yaw),
                            motion: None,
                        },
                    );
                }
            }
            WorldEvent::ShipMoved(motion) => {
                if let Some(slave) = self.slaves.get_mut(&motion.obj) {
                    slave.motion = Some(**motion);
                }
            }
            WorldEvent::UnitsRemoved { objs } => {
                for obj in objs {
                    self.slaves.remove(obj);
                }
            }
            WorldEvent::SlaveDespawned { obj } | WorldEvent::SlaveRemoved { obj, .. } => {
                self.slaves.remove(obj);
            }
            WorldEvent::UnitDamaged { caster, target }
                if self
                    .slaves
                    .get(caster)
                    .is_some_and(|slave| slave.template == CANNON) =>
            {
                self.last_hit = Some((*caster, *target));
            }
            _ => {}
        }
    }
}
