// Wire layouts follow the AAEmu-1.2-Rust writers in systems/social/{dominion,packets/misc}.rs
// (GPL-3.0-or-later).
//! Territory ownership, announced sieges and the server's participant roster.

use crate::opcodes::sc;
use crate::pos::convert_long_to_coord;
use crate::stream::{MarshalError, PacketStream};

type Res<T> = Result<T, MarshalError>;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Team {
    pub expedition: u32,
    pub members: Box<[u32]>,
}

pub const CAMP_AUCTION: u8 = 1;
pub const DECLARATION: u8 = 2;
pub const WARMUP: u8 = 3;
pub const BATTLE: u8 = 4;
pub const ENGRAVABLE: u8 = 3;
pub const ENGRAVING_STARTED: u8 = 4;
pub const ENGRAVING_STOPPED: u8 = 5;
pub const ENGRAVING_SUCCEEDED: u8 = 6;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Siege {
    pub period: u8,
    pub radius_declare: i16,
    pub durations: [i32; 5],
    pub started: i64,
    pub defenders: Team,
    pub attackers: Team,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Territory {
        group: u16,
        owner: u32,
        tower: u32,
        position: [f32; 3],
        siege: Siege,
    },
    Phase {
        group: u16,
        owner: Option<u32>,
        siege: Siege,
    },
    Declared {
        group: u16,
        attackers: Team,
    },
    Member {
        group: u16,
        expedition: u32,
        character: u32,
        added: bool,
    },
    Owner {
        group: u16,
        owner: u32,
    },
    TowerAlert {
        group: u16,
        alert: u8,
    },
    Deleted(u16),
}

pub fn parse(level: u8, opcode: u16, body: &[u8]) -> Res<Option<Event>> {
    if level != 1 {
        return Ok(None);
    }
    let read: fn(&mut PacketStream) -> Res<Event> = match opcode {
        sc::SCDominionDataPacket => territory,
        sc::SCSiegeStatePacket => phase,
        sc::SCSiegeDeclaredPacket => |s| {
            Ok(Event::Declared {
                group: s.read_u16()?,
                attackers: team(s)?,
            })
        },
        sc::SCSiegeMemberPacket => |s| {
            Ok(Event::Member {
                group: s.read_u16()?,
                expedition: s.read_u32()?,
                character: s.read_u32()?,
                added: s.read_bool()?,
            })
        },
        sc::SCDominionOwnerChangedPacket => |s| {
            let group = s.read_u16()?;
            let owner = s.read_u32()?;
            s.skip(8 + 1)?; // reign start and bestowed flag
            Ok(Event::Owner { group, owner })
        },
        sc::SCDominionDeletedPacket => |s| Ok(Event::Deleted(s.read_u16()?)),
        sc::SCSiegeAlertPacket => |s| {
            let group = s.read_u16()?;
            let alert = s.read_u8()?;
            if s.read_u32()? != 0 {
                s.read_string()?;
            }
            Ok(Event::TowerAlert { group, alert })
        },
        _ => return Ok(None),
    };
    read(&mut PacketStream::from_bytes(body)).map(Some)
}

fn position(s: &mut PacketStream) -> Res<[f32; 3]> {
    let x = convert_long_to_coord(s.read_i64()?);
    let y = convert_long_to_coord(s.read_i64()?);
    let z = s.read_f32()?;
    Ok([x, y, z])
}

fn team(s: &mut PacketStream) -> Res<Team> {
    let expedition = s.read_u32()?;
    s.skip(3 + 20 + 3)?; // headquarters object and position, and the three roster limits
    let count = usize::from(s.read_u8()?);
    let members = (0..count)
        .map(|_| s.read_u32())
        .collect::<Res<Vec<_>>>()?
        .into_boxed_slice();
    Ok(Team {
        expedition,
        members,
    })
}

fn siege(s: &mut PacketStream) -> Res<Siege> {
    s.skip(10)?; // territory ids and building limits
    let radius_declare = s.read_i16()?;
    s.skip(6)?; // other territory radii
    let mut durations = [0; 5];
    for duration in &mut durations {
        *duration = s.read_i32()?;
    }
    let started = s.read_i64()?;
    s.skip(8 + 4)?; // fixed date and reinforcement delay
    Ok(Siege {
        period: s.read_u8()?,
        radius_declare,
        durations,
        started,
        defenders: team(s)?,
        attackers: team(s)?,
    })
}

fn territory(s: &mut PacketStream) -> Res<Event> {
    let group = s.read_u16()?;
    let owner = s.read_u32()?;
    s.skip(8)?; // house and tax rate
    let position = position(s)?;
    s.skip(5 * 4 + 5 * 8)?; // treasury figures and history dates
    s.skip(2 + 8 + 20)?; // national tax, monument id and monument position
    let tower = s.read_bc()?;
    let siege = siege(s)?;
    s.skip(8 + 2 + 2)?; // non-PvP window and two announcement flags
    Ok(Event::Territory {
        group,
        owner,
        tower,
        position,
        siege,
    })
}

fn phase(s: &mut PacketStream) -> Res<Event> {
    let group = s.read_u16()?;
    let siege = siege(s)?;
    let owner = s.read_u32()?;
    if owner != 0 {
        s.read_i64()?; // reign start is only present on a change of owner
    }
    Ok(Event::Phase {
        group,
        owner: (owner != 0).then_some(owner),
        siege,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn battle_preserves_team_order_and_owner_changes_have_a_reign_date() {
        let mut wire = PacketStream::new();
        wire.write_u16(25);
        for _ in 0..10 {
            wire.write_u8(0);
        }
        wire.write_i16(250);
        for _ in 0..6 {
            wire.write_u8(0);
        }
        for duration in [1_800, 3_600, 1_800, 5_400, 1_800] {
            wire.write_i32(duration);
        }
        wire.write_i64(123_456).write_i64(0).write_i32(0);
        wire.write_u8(BATTLE);
        for (expedition, members) in [(5, &[14, 16][..]), (9, &[20][..])] {
            wire.write_u32(expedition).write_bc(0);
            wire.write_i64(0).write_i64(0).write_f32(0.0);
            wire.write_u8(80).write_u8(100).write_u8(80);
            wire.write_u8(members.len() as u8);
            for &member in members {
                wire.write_u32(member);
            }
        }
        let before_owner = wire.len();
        wire.write_u32(0);
        let event = parse(1, sc::SCSiegeStatePacket, wire.as_bytes()).unwrap();
        assert_eq!(
            event,
            Some(Event::Phase {
                group: 25,
                owner: None,
                siege: Siege {
                    period: BATTLE,
                    radius_declare: 250,
                    durations: [1_800, 3_600, 1_800, 5_400, 1_800],
                    started: 123_456,
                    defenders: Team {
                        expedition: 5,
                        members: [14, 16].into()
                    },
                    attackers: Team {
                        expedition: 9,
                        members: [20].into()
                    },
                },
            }),
        );
        wire.patch(before_owner, &9u32.to_le_bytes());
        assert!(parse(1, sc::SCSiegeStatePacket, wire.as_bytes()).is_err());
        wire.write_i64(12_345);
        assert!(matches!(
            parse(1, sc::SCSiegeStatePacket, wire.as_bytes()).unwrap(),
            Some(Event::Phase { owner: Some(9), .. })
        ));
    }
}
