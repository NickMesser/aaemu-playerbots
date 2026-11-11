//! The tick context: everything a goal or the combat controller may touch during one tick.

use std::sync::Arc;

use aa_proto::cs::{self, Out};
use aa_world::World;
use tokio::time::Instant;

use super::combat::Combat;
use super::coordinator::CoordHandle;
use super::motion::{Motion, MotionEnv, Pace};
use super::state::BotState;
use super::watch::{Finding, Pending, Req};
use crate::archetype::Archetype;
use crate::config::Config;
use crate::hub::{Hub, Logger};
use crate::issues::IssueReport;
use crate::roster::{Profile, Temperament};

/// Who a bot is: fixed for the life of its task (a rename only changes the name).
#[derive(Debug, Clone)]
pub struct Persona {
    /// Bot id.
    pub id: u32,
    /// Character name.
    pub name: Box<str>,
    /// Class build.
    pub archetype: Arc<Archetype>,
    /// Temperament with the configured spread applied.
    pub temper: Temperament,
    /// Personality (chat style, exploring, shady business, gear, play time), see
    /// [`crate::roster::Profile`].
    pub profile: Profile,
}

/// Everything one tick works with. Built by the bot task around its own fields; goals receive
/// it as `&mut Ctx` and must not keep any borrow of it between ticks.
pub struct Ctx<'a> {
    /// The static world, shared by all bots.
    pub world: &'a World,
    /// The configuration.
    pub cfg: &'a Config,
    /// Who the bot is.
    pub persona: &'a Persona,
    /// What the bot knows.
    pub state: &'a mut BotState,
    /// The movement controller.
    pub motion: &'a mut Motion,
    /// The combat controller.
    pub combat: &'a mut Combat,
    /// Packets to send after the tick (the bot task flushes them).
    pub out: &'a mut Vec<Out>,
    /// The pairing service.
    pub coord: &'a CoordHandle,
    /// The hub: map markers, the other bots' character ids.
    pub hub: &'a Hub,
    /// The bot's event log.
    pub log: &'a Logger,
    /// Randomness (per bot, so ticks do not contend).
    pub rng: &'a mut fastrand::Rng,
    /// The tick's wall-clock instant.
    pub now: Instant,
    /// The bot clock in milliseconds (monotonic, starts at the task start); the same value as
    /// `state.now_ms`.
    pub now_ms: u64,
    /// A real player (a character that is not one of our bots) is within 150 m.
    pub real_player_near: bool,
}

impl Ctx<'_> {
    /// The bot's position.
    pub fn pos(&self) -> [f32; 3] {
        self.state.me.pos
    }

    /// Queues a packet.
    pub fn send(&mut self, out: Out) {
        self.out.push(out);
    }

    /// Says a line in local chat. A line the server could take for a GM command (a leading `/`,
    /// also behind NULs, spaces or invisible characters) or that is empty, too long or has control
    /// characters is dropped; GM commands go through [`Ctx::gm`].
    pub fn say(&mut self, text: &str) {
        if let Some(p) = super::goals::social::say_packet(text) {
            self.out.push(p);
        }
    }

    /// Clears the server-side target (`CSChangeTarget(0)`) and tells the combat controller, which
    /// re-announces its target when it fights on.
    pub fn clear_target(&mut self) {
        self.out.push(cs::change_target(0));
        self.combat.target_cleared();
    }

    /// Sends a GM command. Only when `bots.gm_bootstrap` is on; returns whether it was sent.
    ///
    /// The one place a bot's GM lines are made (see [`crate::gm`]): `line` is one of the short
    /// forms the goals write (`/level 36`, `/labor 100`, `/move x y z`, `/item add self ...`,
    /// `/gold add self ...`); it is rewritten so that it names the bot itself and the server-side
    /// target is cleared in front of it in the same flush, so a target left over from a duel can
    /// never be the victim. Anything else is refused. Logged at debug level.
    pub fn gm(&mut self, line: &str) -> bool {
        if !self.cfg.bots.gm_bootstrap {
            return false;
        }
        let wire = match crate::gm::plan(line, &self.persona.name) {
            Ok(w) => w,
            Err(e) => {
                self.log.debug(format!(
                    "gm: refused {line:?} for {}: {e}",
                    self.persona.name
                ));
                return false;
            }
        };
        self.send_gm(wire)
    }

    /// Advances the bot's house `house_obj` by `steps` build steps (`/build`, which works on the
    /// targeted house): the house is targeted in front of the command, in the same flush.
    pub fn gm_build(&mut self, house_obj: u32, steps: u32) -> bool {
        if !self.cfg.bots.gm_bootstrap || house_obj == 0 {
            return false;
        }
        self.out.push(cs::change_target(house_obj));
        self.combat.target_cleared();
        self.send_gm(crate::gm::plan_build(steps))
    }

    fn send_gm(&mut self, wire: crate::gm::GmWire) -> bool {
        if wire.pre == crate::gm::Pre::ClearTarget {
            self.clear_target();
        }
        let Some(out) = cs::gm_command(&wire.line) else {
            return false;
        };
        if wire.line.starts_with("/move") {
            // the server answers a /move with SCTeleportUnit (or a chat line that refuses it)
            self.expect(Req::Teleport, 0);
        }
        self.log
            .debug(format!("gm {}: {}", self.persona.name, wire.line));
        self.out.push(out);
        true
    }

    /// Files a report of a problem with the server (see [`crate::issues`]). The bot's identity,
    /// position, zone, goal and last log lines are attached unless the report has them; the hub
    /// counts it and does the slow work in the background, so this never waits for I/O. Report an
    /// episode once, not on every tick.
    pub fn report_issue(&self, report: IssueReport) {
        let (pos, char_id) = (self.state.me.pos, self.state.me.char_id);
        let report = self.state.watch.attach_trail(report, self.now_ms);
        let report = self
            .log
            .with_slot(|g| crate::issues::attach_context(report, g, char_id, Some(pos)));
        self.hub.report_issue(report);
    }

    /// Leaves a finding of a detector (see [`super::watch`]) for the bot loop to file with the
    /// packet trail. Each episode is kept once per session, so a goal may call this every tick
    /// the symptom lasts.
    pub fn find(&mut self, finding: Finding) {
        self.state.watch.push(finding);
    }

    /// Registers a request the server must answer (see [`super::watch::tracker`]). Requests whose
    /// packets say what they are (quests, loot, skills) are registered by the bot loop itself;
    /// this is for the others.
    pub fn expect(&mut self, req: Req, id: u32) {
        let pos = self.state.me.pos;
        self.state.watch.tracker.expect(Pending {
            req,
            id,
            aux: 0,
            sent_ms: self.now_ms,
            wait_ms: super::watch::tracker::WAIT_MS,
            pos: [pos[0], pos[1]],
        });
    }

    /// Whether GM commands may be used.
    pub fn gm_allowed(&self) -> bool {
        self.cfg.bots.gm_bootstrap
    }

    /// Starts a trip to (x, y), stopping `radius` metres short. A trip of more than 1.2 km goes
    /// through a portal when `behavior.portals` is on and the bot's book has an entry near the
    /// destination (then the motion stays idle while the portal journey runs and the goal waits,
    /// see [`crate::bot::travel`]); long trips otherwise use `/move` when the config allows it.
    pub fn travel(&mut self, x: f32, y: f32, radius: f32, pace: Pace) {
        // a far trip may go by portal (a gate to a book entry near the destination, or a recall)
        if super::travel::far_trip(self, x, y) {
            return;
        }
        let teleport = if self.cfg.bots.gm_bootstrap {
            self.cfg.bots.long_travel_teleport_m
        } else {
            0.0
        };
        let mut env = MotionEnv {
            ground: self.state.place.ground(self.world),
            now_ms: self.now_ms,
            out: &mut *self.out,
            rng: &mut *self.rng,
            send_gap_ms: 0,
            combat: false,
        };
        let jump = self
            .motion
            .travel_to(x, y, radius, pace, teleport, &mut env);
        if let Some([jx, jy, jz]) = jump {
            if self.gm(&format!("/move {jx:.1} {jy:.1} {jz:.1}")) {
                self.motion.begin_teleport(self.now_ms);
            }
        }
    }

    /// Starts a trip to (x, y, z), stopping `radius` metres short in three dimensions: the
    /// navmesh search uses `z` to pick the floor the target stands on (a house storey, a cave
    /// under the terrain). No portal or `/move` is used: this is for the last few hundred metres
    /// to an NPC or doodad in view.
    pub fn travel_to_floor(&mut self, x: f32, y: f32, z: f32, radius: f32, pace: Pace) {
        self.motion.go_to_z(x, y, z, radius, pace, self.now_ms);
    }

    /// Whether `bots.gm_bootstrap` lets this bot use `/move` for a trip of `dist` metres.
    pub fn would_teleport(&self, dist: f32) -> bool {
        self.cfg.bots.gm_bootstrap
            && self.cfg.bots.long_travel_teleport_m > 0.0
            && dist > self.cfg.bots.long_travel_teleport_m
    }
}
