//! Guilds (the server calls them expeditions): bots found them, recruit, promote, talk in them
//! and leave them, and a real player can ask a bot officer for an invitation.
//!
//! Everything is off until `[guild] enabled`. The pieces:
//!
//! * [`log`]: [`GuildLog`], what a bot knows about its guild (the packets of
//!   [`aa_proto::guild`] feed it through [`GuildState::on_event`]).
//! * [`book`]: [`GuildBook`], the process-wide registry in the hub: a card per bot, the guilds
//!   the bots belong to, the foundings under way (`max_guilds`), the names that are taken, the
//!   invitation and chat pace of each guild.
//! * [`plan`] and [`found`]: the founding. A bot with no guild that has what it takes sends the
//!   coordinator a wish ([`GuildWish`](plan::GuildWish)); `party_size` wishes of one mother
//!   faction become a founding team ([`PartyOrder`](crate::bot::party::PartyOrder) with the
//!   purpose `Guild`); the goal ([`crate::bot::goals::guild`]) runs the [`found::Machine`]:
//!   team, name, creation, dismissal of the team. The server needs no walking: invitations go by
//!   name.
//! * [`life`] and [`duty`]: what a bot in a guild does, whatever goal it runs: answers
//!   invitations (bots of a bot guild; real players only with `accept_player_invites`), recruits,
//!   promotes officers (the owner), hands the guild over before it leaves, and talks.
//! * [`chat`]: the lines, and the whispers of real players that ask for an invitation.
//! * [`names`]: guild names the server's pattern accepts.
//!
//! # Rules the bots keep
//!
//! * A guild is a bot guild when a bot owns it. Bots recruit and promote only in bot guilds.
//! * An owner never leaves a guild leaderless: it hands the guild to another bot of the guild
//!   first (the server leaves the guild without an owner otherwise), and an owner without a
//!   successor stays.
//! * Real players are never expelled and never promoted or demoted by a bot; a bot invites one
//!   only when asked.
//! * Bots invite bots of their own mother faction.
//!
//! The per-bot budget is tight (`size_of::<Bot>() < 3200`): [`GuildState`] is one pointer, and the
//! [`Mem`] behind it is allocated by the first guild packet or the first duty tick with the
//! feature on.

pub mod book;
pub mod chat;
pub mod duty;
pub mod forming;
pub mod found;
pub(crate) mod land;
pub mod leases;
pub mod life;
pub mod log;
pub mod names;
pub mod plan;
pub mod prices;
pub mod profession;
pub mod recovery;
pub mod report;
pub mod siege;
pub mod supply;
pub mod treasury;

use aa_proto::guild::GuildEvent;

pub use self::book::{Card, GuildBook, Membership};
pub use self::duty::{duty, forward_chat, observe, snapshot_if_changed};
use self::life::Timers;
pub use self::log::GuildLog;

/// Everything a bot remembers about guilds: the log and the duty's timers.
#[derive(Debug, Default)]
pub struct Mem {
    /// What the bot knows about its guild.
    pub log: GuildLog,
    /// The duty's clocks and plans.
    pub t: Timers,
    pub treasury: treasury::Treasury,
    pub leases: leases::Book,
    pub sieges: siege::Book,
    pub(crate) gather_backoff: crate::bot::goals::work::GatherBackoff,
}

/// What a bot knows about its guild (one pointer until the first guild packet or duty tick).
#[derive(Debug, Default)]
pub struct GuildState(Option<Box<Mem>>);

impl GuildState {
    /// The memory, if anything made it yet.
    pub fn get(&self) -> Option<&Mem> {
        self.0.as_deref()
    }

    /// The memory, if anything made it yet.
    pub fn get_mut(&mut self) -> Option<&mut Mem> {
        self.0.as_deref_mut()
    }

    /// The memory, made on first use.
    pub fn mem(&mut self) -> &mut Mem {
        self.0.get_or_insert_with(Default::default)
    }

    /// What the bot knows about its guild, once it has heard of one.
    pub fn log(&self) -> Option<&GuildLog> {
        self.0.as_deref().map(|m| &m.log)
    }

    /// Whether the bot is in a guild.
    pub fn in_guild(&self) -> bool {
        self.log().is_some_and(GuildLog::in_guild)
    }

    /// The guild packet `ev` reached the bot, whose character id is `me`.
    pub fn on_event(&mut self, ev: &GuildEvent, me: u32, now_ms: u64) {
        // (the guild list of a bot that is in none, and a status of a guild it is not in, say
        // nothing: no memory is made for them)
        if self.0.is_none()
            && matches!(
                ev,
                GuildEvent::Factions(_)
                    | GuildEvent::Dismissed { .. }
                    | GuildEvent::RoleChanged { .. }
            )
        {
            return;
        }
        self.mem().log.apply(ev, me, now_ms);
    }

    /// The guild the server says the bot is in at world entry (`SCCharacterState`).
    pub fn on_own_guild(&mut self, guild: u32, now_ms: u64) {
        if guild == 0 && self.0.is_none() {
            return;
        }
        self.mem().log.on_own_guild(guild, now_ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aa_proto::guild::{FactionBrief, MemberBrief};

    #[test]
    fn a_bot_that_hears_nothing_of_guilds_costs_a_pointer() {
        assert_eq!(
            std::mem::size_of::<GuildState>(),
            std::mem::size_of::<usize>()
        );
        let mut s = GuildState::default();
        assert!(s.get().is_none() && !s.in_guild() && s.log().is_none());
        // no guild at world entry, a guild list that names none of ours, a stray dismissal
        s.on_own_guild(0, 1);
        s.on_event(&GuildEvent::Factions(Box::new([])), 5, 2);
        s.on_event(
            &GuildEvent::Dismissed {
                guild: 3,
                success: true,
            },
            5,
            3,
        );
        assert!(s.get().is_none());
    }

    #[test]
    fn a_guild_packet_makes_the_memory() {
        let mut s = GuildState::default();
        s.on_own_guild(4, 10);
        assert!(s.in_guild());
        s.on_event(
            &GuildEvent::MemberList {
                total: 1,
                guild: 4,
                members: vec![MemberBrief {
                    guild: 4,
                    char_id: 5,
                    online: true,
                    name: "Me".into(),
                    level: 10,
                    role: 255,
                }]
                .into_boxed_slice(),
            },
            5,
            11,
        );
        let log = s.log().unwrap();
        assert!(log.loaded && log.is_owner(5));
        s.on_event(
            &GuildEvent::Factions(
                vec![FactionBrief {
                    id: 4,
                    mother_id: 1,
                    name: "Iron Pilgrims".into(),
                    owner_id: 5,
                    owner_name: "Me".into(),
                    created: 0,
                }]
                .into_boxed_slice(),
            ),
            5,
            12,
        );
        assert_eq!(&*s.log().unwrap().name, "Iron Pilgrims");
    }
}
