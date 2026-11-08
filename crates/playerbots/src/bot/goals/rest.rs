//! Rest: stand still until hit points and mana have recovered.
//!
//! The server regenerates a standing character; the goal ends when both are nearly full (after at
//! least ten seconds) or after at most 90 seconds.
//!
//! A rest that cut a trade run short while the bot carried a pack gives the run back when it ends
//! ([`Rest::resumes_run`], used by [`super::Goal::tick`]); the new run walks the pack to the buyer.

use crate::bot::ctx::Ctx;
use crate::bot::manner;

use super::trade_run::carried_pack_of;
use super::{jitter_ms, GoalStatus};

/// Shortest rest in milliseconds.
const MIN_MS: u64 = 10_000;
/// Longest rest in milliseconds.
const MAX_MS: (u64, u64) = (45_000, 90_000);
/// Longest rest of a bot that goes back to a trade run afterwards: it walks on with the pack only
/// once it has recovered (or after this long).
const MAX_AFTER_RUN_MS: u64 = 180_000;
/// Recovered enough: hit points and mana in percent.
const HP_OK: u8 = 95;
const MP_OK: u8 = 80;

/// Standing still. See the module docs.
#[derive(Debug, Default, Clone)]
pub struct Rest {
    min_end_ms: u64,
    end_ms: u64,
    /// The rest replaced a trade run that carried a pack.
    after_run: bool,
}

impl Rest {
    /// Called once when the brain picks the goal.
    pub fn start(&mut self, ctx: &mut Ctx<'_>) {
        ctx.motion.stop();
        manner::rest_begin(ctx);
        self.min_end_ms = ctx.now_ms + MIN_MS;
        self.end_ms = ctx.now_ms + jitter_ms(ctx.rng, MAX_MS.0, MAX_MS.1);
        self.after_run = ctx.state.econ.take_interrupted_run(ctx.now_ms);
        if self.after_run {
            self.end_ms = ctx.now_ms + MAX_AFTER_RUN_MS;
        }
    }

    /// Whether the finished rest goes back to the trade run it interrupted: the bot still
    /// carries the pack, which only a sale (or the buyer's refusal) gets rid of.
    pub fn resumes_run(&self, ctx: &Ctx<'_>) -> bool {
        self.after_run && carried_pack_of(ctx).is_some()
    }

    /// One tick.
    pub fn tick(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let now = ctx.now_ms;
        let me = &ctx.state.me;
        let recovered = me.hp_pct() >= HP_OK && me.mp_pct() >= MP_OK;
        if now >= self.end_ms || (now >= self.min_end_ms && recovered) {
            GoalStatus::Done
        } else {
            // (a bot that rests in front of a player looks around and sits down)
            manner::rest_tick(ctx);
            GoalStatus::Running
        }
    }

    /// Called when the goal ends.
    pub fn stop(&mut self, ctx: &mut Ctx<'_>) {
        manner::rest_end(ctx);
    }
}
