//! A scripted server and a party of phase machines: whole runs, tick by tick.
//!
//! The machines are fed the facts the goal would build (from a [`TeamLog`] that follows the
//! scripted server's team packets, positions, the instance state) and their intents are carried
//! out by the script: invitations, team packets, casts, loads, the exit. What this checks is the
//! interplay: who invites whom, when the party is complete, who goes in when, what happens when
//! the server refuses, does nothing or kicks.

use aa_proto::sc_activity::{ActivityEvent, TeamMemberBrief};

use super::entry::EntryError;
use super::machine::{Facts, Fail, Intent, Machine, Phase, TeamFacts, Trip};
use crate::bot::party::TeamLog;

const TICK_MS: u64 = 500;
const WALK_MS: u64 = 20_000;
const LATENCY_MS: u64 = 200;
const PROCESSING_MS: u64 = 1_000;
const LOAD_MS: u64 = 3_000;
const EXIT_LOAD_MS: u64 = 2_000;
const KICK_AFTER_LEAVING_MS: u64 = 15_000;

/// What the script does when the entrance skill is cast.
#[derive(Clone, Copy, PartialEq)]
enum Door {
    /// Accepts, processes, loads.
    Works,
    /// Answers with an error message.
    Refuses(EntryError),
    /// Accepts and never reacts.
    Silent,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum End {
    Running,
    Done,
    Failed(Fail),
}

struct Mate {
    rank: u8,
    char_id: u32,
    machine: Machine,
    team: TeamLog,
    online: bool,
    /// How long the walk to the door takes.
    walk_ms: u64,
    /// Walking to the door; arrives at this time.
    walking_until: Option<u64>,
    at_door: bool,
    in_instance: bool,
    /// Loading finishes at this time.
    load_at: Option<(u64, bool)>,
    /// Inside, at the exit.
    at_exit: bool,
    exit_walk_until: Option<u64>,
    /// Moved out at this time (the exit worked, or the server kicks).
    out_at: Option<u64>,
    cast_error: Option<EntryError>,
    entered_at: Option<u64>,
    end: End,
    intents: Vec<Intent>,
}

struct Team {
    id: u32,
    members: Vec<u32>,
}

struct Sim {
    now: u64,
    mates: Vec<Mate>,
    team: Option<Team>,
    /// `(deliver at, mate index, event)`.
    wire: Vec<(u64, usize, ActivityEvent)>,
    door: Door,
    exit_works: bool,
    /// Ticks of clearing the leader needs.
    clear_ms: u64,
    leaving: bool,
    entered_order: Vec<u8>,
    casts: usize,
}

fn brief(c: u32) -> TeamMemberBrief {
    TeamMemberBrief {
        char_id: c,
        name: format!("C{c}").into(),
        level: 20,
        role: 0,
        obj_id: c * 10,
    }
}

impl Sim {
    fn new(n: u8, door: Door) -> Sim {
        let mates = (0..n)
            .map(|rank| Mate {
                rank,
                char_id: 100 + u32::from(rank),
                machine: Machine::new(true, 0),
                team: TeamLog::default(),
                online: true,
                walk_ms: WALK_MS,
                walking_until: None,
                at_door: false,
                in_instance: false,
                load_at: None,
                at_exit: false,
                exit_walk_until: None,
                out_at: None,
                cast_error: None,
                entered_at: None,
                end: End::Running,
                intents: Vec::new(),
            })
            .collect();
        Sim {
            now: 1_000,
            mates,
            team: None,
            wire: Vec::new(),
            door,
            exit_works: true,
            clear_ms: 60_000,
            leaving: false,
            entered_order: Vec::new(),
            casts: 0,
        }
    }

    fn idx(&self, char_id: u32) -> usize {
        self.mates
            .iter()
            .position(|m| m.char_id == char_id)
            .unwrap()
    }

    fn send(&mut self, to: usize, ev: ActivityEvent) {
        self.wire.push((self.now + LATENCY_MS, to, ev));
    }

    fn facts(&self, i: usize) -> Facts {
        let m = &self.mates[i];
        let leader = &self.mates[0];
        let team = m.team.in_team().then_some(&m.team);
        let mut missing = 0u8;
        for o in self.mates.iter().filter(|o| o.rank != m.rank && o.online) {
            if !team.is_some_and(|t| t.has(o.char_id)) {
                missing |= 1 << o.rank;
            }
        }
        let near = |o: &Mate| o.online && o.at_door && !o.in_instance;
        Facts {
            now_ms: self.now,
            leader: m.rank == 0,
            rank: m.rank,
            expected: self.mates.iter().filter(|o| o.online).count() as u8,
            level: 20,
            level_min: 18,
            level_max: 55,
            raise_ok: false,
            deaths: 0,
            dead: false,
            muster_dead: 0,
            wipes: 0,
            in_instance: m.in_instance,
            loading: m.load_at.is_some(),
            world_ok: true,
            team: TeamFacts {
                in_team: team.is_some(),
                led_by_leader: team.is_some_and(|t| t.owner == leader.char_id),
                size: team.map_or(0, |t| t.size() as u8),
                missing,
                invite: m
                    .team
                    .invite
                    .as_ref()
                    .is_some_and(|i| i.from_char == leader.char_id),
            },
            at_door: m.at_door,
            door_dist: if m.at_door { 0.0 } else { 100.0 },
            muster_far: 0.0,
            trip: if m.walking_until.is_some() {
                Trip::Moving
            } else {
                Trip::Idle
            },
            door_in_view: m.at_door,
            // (a member that comes after the leader went in is not kept waiting)
            muster_ready: self.mates.iter().filter(|o| o.online).all(near)
                || (m.rank != 0 && leader.in_instance),
            muster_near: self.mates.iter().filter(|o| near(o)).count() as u8,
            error: m.cast_error,
            cast_refused: false,
            route_done: m.rank == 0 && m.entered_at.is_some_and(|t| self.now - t >= self.clear_ms),
            party_leaving: m.rank != 0 && self.leaving,
            leader_gone: m.rank != 0 && !leader.online,
            exit_dist: if m.at_exit { 0.0 } else { 100.0 },
            exit_in_view: m.at_exit,
        }
    }

    fn deliver(&mut self) {
        let now = self.now;
        let mut due = Vec::new();
        self.wire.retain(|(at, i, ev)| {
            if *at <= now {
                due.push((*i, ev.clone()));
                false
            } else {
                true
            }
        });
        for (i, ev) in due {
            self.mates[i].team.apply(&ev, now);
        }
    }

    fn join(&mut self, target: usize) {
        let tc = self.mates[target].char_id;
        let leader = self.mates[0].char_id;
        match &mut self.team {
            None => {
                self.team = Some(Team {
                    id: 9,
                    members: vec![leader, tc],
                });
                let members: Vec<_> = [leader, tc].into_iter().map(brief).collect();
                for c in [leader, tc] {
                    let i = self.idx(c);
                    self.send(
                        i,
                        ActivityEvent::JoinedTeam {
                            team_id: 9,
                            owner_id: leader,
                            is_party: true,
                            members: members.clone().into_boxed_slice(),
                        },
                    );
                }
            }
            Some(t) => {
                t.members.push(tc);
                let all = t.members.clone();
                let members: Vec<_> = all.iter().copied().map(brief).collect();
                self.send(
                    target,
                    ActivityEvent::JoinedTeam {
                        team_id: 9,
                        owner_id: leader,
                        is_party: true,
                        members: members.into_boxed_slice(),
                    },
                );
                for c in all.into_iter().filter(|c| *c != tc) {
                    let i = self.idx(c);
                    self.send(
                        i,
                        ActivityEvent::TeamMemberJoined {
                            team_id: 9,
                            member: brief(tc),
                            party: 0,
                        },
                    );
                }
            }
        }
    }

    fn leave(&mut self, who: usize) {
        let c = self.mates[who].char_id;
        let Some(t) = &mut self.team else { return };
        if !t.members.contains(&c) {
            return;
        }
        t.members.retain(|m| *m != c);
        let rest = t.members.clone();
        self.send(
            who,
            ActivityEvent::LeftTeam {
                team_id: 9,
                kicked: false,
                dismissed: false,
            },
        );
        if rest.len() <= 1 {
            for r in rest {
                let i = self.idx(r);
                self.send(i, ActivityEvent::TeamDismissed { team_id: 9 });
            }
            self.team = None;
        } else {
            for r in rest {
                let i = self.idx(r);
                self.send(
                    i,
                    ActivityEvent::TeamMemberLeft {
                        team_id: 9,
                        char_id: c,
                        kicked: false,
                    },
                );
            }
        }
    }

    fn dismiss(&mut self) {
        if let Some(t) = self.team.take() {
            for c in t.members {
                let i = self.idx(c);
                self.send(i, ActivityEvent::TeamDismissed { team_id: 9 });
            }
        }
    }

    fn act(&mut self, i: usize, intent: Intent) {
        let now = self.now;
        self.mates[i].intents.push(intent);
        match intent {
            Intent::Invite(rank) => {
                let t = usize::from(rank);
                if self.mates[t].online
                    && !self.mates[t].team.in_team()
                    && self.mates[t].team.invite.is_none()
                {
                    let leader = self.mates[0].char_id;
                    self.send(
                        t,
                        ActivityEvent::AskToJoinTeam {
                            team_id: self.team.as_ref().map_or(0, |t| t.id),
                            sender_char_id: leader,
                            name: "C100".into(),
                            is_party: true,
                        },
                    );
                }
            }
            Intent::Accept => {
                if self.mates[i].team.invite.take().is_some() {
                    self.join(i);
                }
            }
            Intent::LeaveTeam => self.leave(i),
            Intent::DismissTeam => self.dismiss(),
            Intent::GoToDoor => {
                if self.mates[i].walking_until.is_none() && !self.mates[i].at_door {
                    self.mates[i].walking_until = Some(now + self.mates[i].walk_ms);
                }
            }
            Intent::CastEntry => {
                self.casts += 1;
                match self.door {
                    Door::Works => {
                        self.mates[i].load_at = Some((now + PROCESSING_MS + LOAD_MS, false));
                    }
                    Door::Refuses(e) => self.mates[i].cast_error = Some(e),
                    Door::Silent => {}
                }
            }
            Intent::Clear => {}
            Intent::GoToExit => {
                if self.mates[i].exit_walk_until.is_none() && !self.mates[i].at_exit {
                    self.mates[i].exit_walk_until = Some(now + 5_000);
                }
            }
            Intent::CastExit => {
                if self.exit_works && self.mates[i].out_at.is_none() {
                    self.mates[i].out_at = Some(now + EXIT_LOAD_MS);
                }
            }
            Intent::Done => self.mates[i].end = End::Done,
            Intent::Fail(f) => self.mates[i].end = End::Failed(f),
            Intent::Wait | Intent::PostWish | Intent::RaiseLevel | Intent::Stand => {}
        }
    }

    fn tick(&mut self) {
        self.now += TICK_MS;
        let now = self.now;
        self.deliver();
        for i in 0..self.mates.len() {
            let m = &mut self.mates[i];
            if m.walking_until.is_some_and(|t| now >= t) {
                m.walking_until = None;
                m.at_door = true;
            }
            if m.exit_walk_until.is_some_and(|t| now >= t) {
                m.exit_walk_until = None;
                m.at_exit = true;
            }
            if let Some((t, _)) = m.load_at {
                if now >= t {
                    m.load_at = None;
                    if !m.in_instance {
                        m.in_instance = true;
                        m.at_door = false;
                        m.entered_at = Some(now);
                        self.entered_order.push(m.rank);
                    }
                }
            }
            if m.out_at.is_some_and(|t| now >= t) {
                m.out_at = None;
                m.in_instance = false;
                m.at_exit = false;
            }
        }
        // the server moves a bot that left its team inside the dungeon out after a while
        for i in 0..self.mates.len() {
            let m = &self.mates[i];
            if m.in_instance && m.out_at.is_none() {
                if let Some(l) = m.team.left {
                    if now.saturating_sub(l.at_ms) >= KICK_AFTER_LEAVING_MS {
                        self.mates[i].out_at = Some(now);
                    }
                }
            }
        }
        for i in 0..self.mates.len() {
            if self.mates[i].end != End::Running || !self.mates[i].online {
                continue;
            }
            let f = self.facts(i);
            if self.mates[i].cast_error.is_some() && self.mates[i].machine.phase() == Phase::Enter {
                // (the error is news for one step)
            }
            let intent = self.mates[i].machine.step(&f);
            self.mates[i].cast_error = None;
            if i == 0 && self.mates[0].machine.phase() == Phase::Leave {
                self.leaving = true;
            }
            self.act(i, intent);
        }
    }

    fn run(&mut self, max_ms: u64) {
        let end = self.now + max_ms;
        while self.now < end && self.mates.iter().any(|m| m.online && m.end == End::Running) {
            self.tick();
        }
    }

    fn ends(&self) -> Vec<End> {
        self.mates.iter().map(|m| m.end).collect()
    }
}

#[test]
fn a_party_of_three_forms_walks_enters_one_by_one_clears_and_comes_out() {
    let mut s = Sim::new(3, Door::Works);
    s.run(15 * 60_000);
    assert_eq!(s.ends(), vec![End::Done; 3], "{:?}", s.ends());
    assert_eq!(s.entered_order, vec![0, 1, 2], "the leader goes in first");
    // everybody is out and the team is gone
    assert!(s.mates.iter().all(|m| !m.in_instance && !m.team.in_team()));
    assert!(s.team.is_none());
    // the leader invited each of the others exactly once and nobody was invited twice
    let invites: Vec<_> = s.mates[0]
        .intents
        .iter()
        .filter_map(|i| match i {
            Intent::Invite(r) => Some(*r),
            _ => None,
        })
        .collect();
    assert_eq!(invites, vec![1, 2]);
    // one cast each, no retries
    assert_eq!(s.casts, 3);
    // the members left the team before the leader dismissed it
    let leader_dismissed = s.mates[0].intents.contains(&Intent::DismissTeam);
    let members_left = s.mates[1].intents.contains(&Intent::LeaveTeam)
        && s.mates[2].intents.contains(&Intent::LeaveTeam);
    assert!(members_left || leader_dismissed);
}

#[test]
fn a_member_that_never_comes_online_is_left_behind() {
    let mut s = Sim::new(3, Door::Works);
    s.mates[2].online = false;
    s.run(15 * 60_000);
    assert_eq!(s.ends()[0], End::Done);
    assert_eq!(s.ends()[1], End::Done);
    assert_eq!(s.entered_order, vec![0, 1]);
}

#[test]
fn a_refused_entry_fails_every_member_with_the_servers_reason() {
    for e in [EntryError::VisitLimit, EntryError::NoResource] {
        let mut s = Sim::new(3, Door::Refuses(e));
        s.run(15 * 60_000);
        for (i, end) in s.ends().into_iter().enumerate() {
            assert_eq!(end, End::Failed(Fail::Refused(e)), "bot {i}");
        }
        assert!(s.entered_order.is_empty());
    }
}

#[test]
fn a_busy_dungeon_is_tried_again_and_a_silent_one_is_reported() {
    // a dungeon that says "try later" once and then works
    let mut s = Sim::new(2, Door::Refuses(EntryError::TryLater));
    s.run(40_000);
    s.door = Door::Works;
    s.run(15 * 60_000);
    assert_eq!(s.ends(), vec![End::Done; 2], "{:?}", s.ends());
    // a server that accepts the cast and never loads
    let mut s = Sim::new(3, Door::Silent);
    s.run(15 * 60_000);
    for end in s.ends() {
        assert_eq!(end, End::Failed(Fail::NoLoad));
    }
}

#[test]
fn an_exit_that_does_nothing_ends_with_the_server_moving_the_bots_out() {
    let mut s = Sim::new(3, Door::Works);
    s.exit_works = false;
    s.run(30 * 60_000);
    // every bot left the team, was moved out by the server and the run still ended (the leader
    // reports the exit)
    assert!(s.mates.iter().all(|m| !m.in_instance), "{:?}", s.ends());
    for (i, m) in s.mates.iter().enumerate() {
        assert!(m.machine.exit_failed(), "bot {i}");
    }
    assert!(s
        .ends()
        .into_iter()
        .all(|e| matches!(e, End::Done | End::Failed(Fail::ExitStuck))));
}

#[test]
fn the_members_leave_when_the_leader_drops_out() {
    let mut s = Sim::new(3, Door::Works);
    // run until everybody is inside, then the leader's connection goes
    for _ in 0..4_000 {
        s.tick();
        if s.mates.iter().all(|m| m.in_instance) {
            break;
        }
    }
    assert!(s.mates.iter().all(|m| m.in_instance));
    s.mates[0].online = false;
    s.run(15 * 60_000);
    for m in &s.mates[1..] {
        assert!(!m.in_instance, "the members came out");
        assert_ne!(m.end, End::Running);
    }
}

#[test]
fn a_member_that_comes_to_the_door_after_the_leader_went_in_follows_it() {
    let mut s = Sim::new(3, Door::Works);
    // the walk of bot 1 takes longer than the muster is willing to wait
    s.mates[1].walk_ms = 160_000;
    s.run(20 * 60_000);
    assert_eq!(s.ends(), vec![End::Done; 3], "{:?}", s.ends());
    assert_eq!(s.entered_order, vec![0, 2, 1], "the slow one goes in last");
}
