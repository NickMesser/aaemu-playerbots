//! Companions: bots in the parties of real players (the party feature, goal `companion`).
//!
//! A real player invites a bot, or whispers "inv" to it; the bot joins when it is idle enough, its
//! level fits and this process has not too many companions already, and says no at once
//! otherwise (the server never expires an invitation and a pending one blocks every later one).
//! Once in the party the bot follows the leader, helps in the fights (the combat controller
//! assists the members' targets for any team), announces its role, obeys a closed set of chat
//! commands and, with `follow_into_dungeons`, goes through a dungeon door behind the leader.
//!
//! Layout, pure pieces first (each is tested without a connection):
//!
//! | File | What it is |
//! | --- | --- |
//! | `policy.rs` | May the bot join, may it invite: [`policy::judge`] over [`policy::Facts`]. |
//! | `command.rs` | The chat commands ([`command::parse`]), dungeon names, the short lines it says. |
//! | `follow.rs` | Where to stand next to the leader and when to walk ([`follow::decide`]). |
//! | `upkeep.rs` | Role, handing the lead back, leaving ([`upkeep::plan`]). |
//! | `enter.rs` | The machine that follows the leader through a dungeon door ([`enter::Machine`]). |
//! | `duty.rs` | The glue the brain calls every tick: invitations, upkeep, replies, chat. |
//!
//! The goal itself is [`crate::bot::goals::companion`]. Per-bot state is [`Mem`], behind the
//! pointer `BotState::party` already has (the size of a bot is limited); the process-wide count of
//! companions is the hub's [`Board`].

pub mod command;
pub mod duty;
pub mod enter;
pub mod escort;
pub mod follow;
pub mod lead;
pub mod policy;
pub mod upkeep;

use std::time::{Duration, Instant};

use parking_lot::Mutex;
use smallvec::SmallVec;

pub use duty::{
    claims, duty, forward_chat, holding, in_player_party, leader_char, mem_of, peek, say_to,
    snapshot_if_changed,
};

/// How the companion behaves when nothing special is going on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Follow the leader.
    #[default]
    Follow,
    /// Stand where you are (`stay`).
    Stay,
}

/// A whisper waiting for its moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    /// Bot clock at which it is sent.
    pub due_ms: u64,
    /// Whom to whisper.
    pub to: Box<str>,
    /// The line.
    pub text: String,
}

/// An invitation the bot is about to answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pending {
    /// The `at_ms` of the invitation (to tell a new one from the one being answered).
    pub invite_ms: u64,
    /// Bot clock at which the answer goes out.
    pub due_ms: u64,
}

/// What a companion remembers (one box per bot, inside `PartyMem`). Lost with the connection.
#[derive(Debug, Default, Clone)]
pub struct Mem {
    /// The invitation being answered.
    pub pending: Option<Pending>,
    /// The bot said yes to this player (character id) at this bot clock and waits for the team.
    pub joining: Option<(u32, u64)>,
    /// The player (character id) whose party the board counts the bot in (0 = none).
    pub registered: u32,
    /// Follow or stay.
    pub mode: Mode,
    /// Walk to this character (a `come`), 0 = nobody.
    pub come: u32,
    /// Attack the leader's target until this bot clock (an `assist`).
    pub assist_until_ms: u64,
    /// A player told the bot to leave.
    pub leave: bool,
    /// The bot clock at which the leave request was sent (0 = not yet).
    pub leave_sent_ms: u64,
    /// The dungeon (zone group) a player asked the bot to walk to.
    pub dungeon: Option<u32>,
    /// The bot told the player it arrived at that dungeon's entrance.
    pub dungeon_told: bool,
    /// Current trade escort offered by the party leader.
    pub escort: Option<escort::Escort>,
    /// Whispers waiting to be sent.
    pub replies: SmallVec<[Reply; 2]>,
    /// Players who asked for an invitation `(name hash, bot clock)`.
    pub asked: SmallVec<[(u64, u64); 3]>,
    /// The player the bot invited `(name, bot clock)`.
    pub inviting: Option<(Box<str>, u64)>,
    /// Players the bot told no `(name hash, bot clock)`, so it does not repeat itself.
    pub declined: SmallVec<[(u64, u64); 3]>,
    /// The team the bot announced its role in.
    pub role_sent: u32,
    /// Attempts to hand the lead over and the bot clock of the last.
    pub hand: (u8, u64),
    /// The team the hand-over was counted for.
    pub hand_team: u32,
    /// Bot clock since which no real player of the team was online (0 = one is).
    pub gone_since_ms: u64,
    /// The bot clock at which the bot left the party on its own accord (a reason was logged).
    pub left_why: Option<upkeep::Leave>,
    /// The team the bot logged its joining for.
    pub logged_team: u32,
    /// The key of the party the web snapshot shows (see `snapshot_if_changed`).
    pub web_key: (u32, u32, u32, u8),
}

impl Mem {
    /// Queues a whisper to `to`, due at `due_ms`; at most two wait.
    pub fn reply(&mut self, due_ms: u64, to: &str, text: String) {
        if self.replies.len() >= 2 {
            self.replies.remove(0);
        }
        self.replies.push(Reply {
            due_ms,
            to: to.into(),
            text,
        });
    }
}

/// One bot's place on the [`Board`].
#[derive(Debug, Clone, Copy)]
struct Slot {
    bot: u32,
    player: u32,
    touched: Instant,
}

/// Why the board refused a place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// The process has as many companions as it may.
    TooMany,
    /// The player has as many of our bots as it may.
    FullParty,
}

/// The companions of this process: who is in whose party. The caps of `[party]` are enforced here
/// (one short mutex), and a place that is not renewed ([`Board::touch`]) for [`Board::TTL`] is
/// dropped, so a bot that lost its connection or crashed does not hold one for ever.
#[derive(Debug, Default)]
pub struct Board {
    slots: Mutex<Vec<Slot>>,
}

impl Board {
    /// A place not renewed for this long is dropped.
    pub const TTL: Duration = Duration::from_secs(45);

    fn prune(slots: &mut Vec<Slot>, now: Instant) {
        slots.retain(|s| now.saturating_duration_since(s.touched) < Self::TTL);
    }

    /// Takes a place for `bot` in the party of `player`; renews it when `bot` has one.
    pub fn reserve(
        &self,
        bot: u32,
        player: u32,
        max_total: usize,
        max_per_player: usize,
        now: Instant,
    ) -> Result<(), Refused> {
        let mut g = self.slots.lock();
        Self::prune(&mut g, now);
        if let Some(s) = g.iter_mut().find(|s| s.bot == bot) {
            s.player = player;
            s.touched = now;
            return Ok(());
        }
        if g.len() >= max_total {
            return Err(Refused::TooMany);
        }
        if g.iter().filter(|s| s.player == player).count() >= max_per_player {
            return Err(Refused::FullParty);
        }
        g.push(Slot {
            bot,
            player,
            touched: now,
        });
        Ok(())
    }

    /// Renews the place of `bot`; false when it has none (it expired).
    pub fn touch(&self, bot: u32, now: Instant) -> bool {
        let mut g = self.slots.lock();
        Self::prune(&mut g, now);
        match g.iter_mut().find(|s| s.bot == bot) {
            Some(s) => {
                s.touched = now;
                true
            }
            None => false,
        }
    }

    /// Gives the place of `bot` back.
    pub fn release(&self, bot: u32) {
        self.slots.lock().retain(|s| s.bot != bot);
    }

    /// Bots in the parties of real players.
    pub fn count(&self, now: Instant) -> usize {
        let mut g = self.slots.lock();
        Self::prune(&mut g, now);
        g.len()
    }

    /// Bots in the party of `player`.
    pub fn of_player(&self, player: u32, now: Instant) -> usize {
        let mut g = self.slots.lock();
        Self::prune(&mut g, now);
        g.iter().filter(|s| s.player == player).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_board_enforces_both_caps_and_lets_a_bot_renew() {
        let b = Board::default();
        let t = Instant::now();
        assert_eq!(b.reserve(1, 100, 3, 2, t), Ok(()));
        assert_eq!(b.reserve(2, 100, 3, 2, t), Ok(()));
        // a third bot in the same player's party
        assert_eq!(b.reserve(3, 100, 3, 2, t), Err(Refused::FullParty));
        assert_eq!(b.reserve(3, 200, 3, 2, t), Ok(()));
        // the process cap
        assert_eq!(b.reserve(4, 300, 3, 2, t), Err(Refused::TooMany));
        assert_eq!(
            (b.count(t), b.of_player(100, t), b.of_player(200, t)),
            (3, 2, 1)
        );
        // a bot that has a place keeps it when it asks again
        assert_eq!(b.reserve(1, 100, 3, 2, t), Ok(()));
        assert_eq!(b.count(t), 3);
        // a place given back is free again
        b.release(1);
        assert_eq!(b.reserve(4, 100, 3, 2, t), Ok(()));
        b.release(99);
        assert_eq!(b.count(t), 3);
    }

    #[test]
    fn a_place_that_is_not_renewed_expires() {
        let b = Board::default();
        let t = Instant::now();
        b.reserve(1, 100, 8, 4, t).unwrap();
        b.reserve(2, 100, 8, 4, t).unwrap();
        let later = t + Board::TTL - Duration::from_secs(1);
        assert!(b.touch(1, later));
        let much_later = t + Board::TTL + Duration::from_secs(1);
        // bot 2 was not renewed, bot 1 was
        assert_eq!(b.count(much_later), 1);
        assert!(b.touch(1, much_later));
        assert!(!b.touch(2, much_later));
    }

    #[test]
    fn at_most_two_whispers_wait() {
        let mut m = Mem::default();
        m.reply(10, "A", "one".into());
        m.reply(20, "B", "two".into());
        m.reply(30, "C", "three".into());
        let to: Vec<&str> = m.replies.iter().map(|r| &*r.to).collect();
        assert_eq!(to, ["B", "C"]);
        assert_eq!(Mode::default(), Mode::Follow);
    }
}
