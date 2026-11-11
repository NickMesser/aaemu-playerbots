//! Rest: stand still until hit points and mana have recovered.
//!
//! The server regenerates a standing character; the goal ends when both are nearly full (after at
//! least ten seconds) or after at most 90 seconds. After a resurrection the rest is the longer
//! `[survival] post_rez_rest_ms` one and waits for `post_rez_hp_pct`: the death journal shows 236
//! of 1,357 revival-to-death intervals under 180 s, many of them a fight started while the bot was
//! still weak. A rest that starts next to hostile monsters first walks `rest_safe_spot_m` away
//! from them (82 deaths happened while resting right after leaving an event hurt), and a heal the
//! rotation has is cast when the bot is below `heal_hp_pct`.
//!
//! A rest that cut a trade run short while the bot carried a pack gives the run back when it ends
//! ([`Rest::resumes_run`], used by [`super::Goal::tick`]); the new run walks the pack to the buyer.

use aa_proto::cs;

use crate::bot::combat::attackable;
use crate::bot::ctx::Ctx;
use crate::bot::manner;
use crate::bot::motion::Pace;

use super::trade_run::carried_pack_of;
use super::{jitter_ms, walkable, GoalStatus};

/// Shortest rest in milliseconds.
const MIN_MS: u64 = 10_000;
/// Longest rest in milliseconds.
const MAX_MS: (u64, u64) = (45_000, 90_000);
/// Recovered enough: mana in percent (the hit point line is `[survival] rest_hp_pct`).
const MP_OK: u8 = 80;
/// Attempts to walk away from monsters before the rest gives up on a safe spot.
const MAX_SAFE_TRIES: u8 = 3;

/// Standing still. See the module docs.
#[derive(Debug, Default, Clone)]
pub struct Rest {
    min_end_ms: u64,
    end_ms: u64,
    /// The rest replaced a trade run that carried a pack.
    after_run: bool,
    /// This rest followed a resurrection at once.
    post_rez: bool,
    /// The hit point percentage that counts as recovered.
    hp_ok: u8,
    /// Walking away from monsters before settling down.
    moving_away: bool,
    safe_tries: u8,
    next_heal_ms: u64,
}

impl Rest {
    /// Called once when the brain picks the goal.
    pub fn start(&mut self, ctx: &mut Ctx<'_>) {
        ctx.motion.stop();
        manner::rest_begin(ctx);
        let now = ctx.now_ms;
        self.post_rez = crate::bot::survival::after_resurrection(now, ctx.state.last_resurrect_ms);
        self.hp_ok = crate::bot::survival::rest_hp_target(&ctx.cfg.survival, self.post_rez);
        self.min_end_ms = now + MIN_MS;
        let ordinary = now + jitter_ms(ctx.rng, MAX_MS.0, MAX_MS.1);
        self.end_ms =
            crate::bot::survival::rest_deadline(now, ordinary, &ctx.cfg.survival, self.post_rez);
        self.after_run = ctx.state.econ.take_interrupted_run(now);
        self.moving_away = false;
        self.safe_tries = 0;
        self.next_heal_ms = 0;
        self.move_away_from_monsters(ctx);
    }

    /// Whether the finished rest goes back to the trade run it interrupted: the bot still
    /// carries the pack, which only a sale (or the buyer's refusal) gets rid of.
    pub fn resumes_run(&self, ctx: &Ctx<'_>) -> bool {
        self.after_run && carried_pack_of(ctx).is_some()
    }

    /// One tick.
    pub fn tick(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let now = ctx.now_ms;
        if self.moving_away {
            if ctx.motion.is_moving() {
                return GoalStatus::Running;
            }
            self.moving_away = false;
            if self.safe_tries < MAX_SAFE_TRIES {
                self.move_away_from_monsters(ctx);
            }
            return GoalStatus::Running;
        }
        let me = &ctx.state.me;
        let recovered = me.hp_pct() >= self.hp_ok && me.mp_pct() >= MP_OK;
        if now >= self.end_ms || (now >= self.min_end_ms && recovered) {
            return GoalStatus::Done;
        }
        self.try_heal(ctx);
        // (a bot that rests in front of a player looks around and sits down)
        manner::rest_tick(ctx);
        GoalStatus::Running
    }

    /// Called when the goal ends.
    pub fn stop(&mut self, ctx: &mut Ctx<'_>) {
        manner::rest_end(ctx);
    }

    /// Walks `[survival] rest_safe_spot_m` away from the nearest hostile monster when one is
    /// within `rest_hostile_m`; a bot that rests in an event's last wave otherwise stands still
    /// and is killed.
    fn move_away_from_monsters(&mut self, ctx: &mut Ctx<'_>) {
        let cfg = &ctx.cfg.survival;
        if !cfg.enabled || cfg.rest_hostile_m <= 0.0 || cfg.rest_safe_spot_m <= 0.0 {
            return;
        }
        let me = ctx.state.me.pos;
        let faction = ctx.state.me.faction;
        let mut nearest: Option<([f32; 3], f32)> = None;
        for u in ctx.state.units.values() {
            if attackable(ctx.world, faction, u).is_none() {
                continue;
            }
            let d = u.dist2d(me);
            if d <= cfg.rest_hostile_m && nearest.is_none_or(|n| d < n.1) {
                nearest = Some((u.pos, d));
            }
        }
        let Some((threat, _)) = nearest else {
            return;
        };
        let (dx, dy) = (me[0] - threat[0], me[1] - threat[1]);
        let len = (dx * dx + dy * dy).sqrt();
        let (ux, uy) = if len < 0.1 {
            let a = ctx.rng.f32() * std::f32::consts::TAU;
            (a.cos(), a.sin())
        } else {
            (dx / len, dy / len)
        };
        for turn in [0.0f32, 0.5, -0.5, 1.0, -1.0, 1.5, -1.5] {
            let a = turn;
            let (cx, cy) = (ux * a.cos() - uy * a.sin(), ux * a.sin() + uy * a.cos());
            let (x, y) = (
                me[0] + cx * cfg.rest_safe_spot_m,
                me[1] + cy * cfg.rest_safe_spot_m,
            );
            if walkable(ctx.world, x, y) {
                ctx.motion.go_to(x, y, 2.0, Pace::Run, ctx.now_ms);
                self.moving_away = true;
                self.safe_tries = self.safe_tries.saturating_add(1);
                ctx.log.debug(format!(
                    "rest: walking away from a monster to {x:.0}, {y:.0}"
                ));
                return;
            }
        }
    }

    /// Casts a heal the rotation has (Clergy's antithesis) while resting below
    /// `[survival] heal_hp_pct`: the server regenerates a standing character, but a heal makes the
    /// rest shorter. Quiet when the build has no heal.
    fn try_heal(&mut self, ctx: &mut Ctx<'_>) {
        let cfg = &ctx.cfg.survival;
        let now = ctx.now_ms;
        if !cfg.enabled || now < self.next_heal_ms || ctx.state.me.hp_pct() >= cfg.heal_hp_pct {
            return;
        }
        let Some(skill) = ctx
            .persona
            .archetype
            .rotation()
            .and_then(|r| r.heal_skill())
        else {
            return;
        };
        let Some(info) = ctx.world.skill(skill) else {
            return;
        };
        // (a build without learned skills fights with its starter skill; it has no rotation yet)
        let known = !ctx.state.skills.is_empty() && ctx.state.skill_level(skill).is_some();
        let me = &ctx.state.me;
        if !known
            || me.obj == 0
            || !ctx.state.cooldown_ready(skill, now)
            || i64::from(info.mana_cost(me.level)) > i64::from(me.mp)
        {
            return;
        }
        let me_obj = me.obj;
        let hp = me.hp_pct();
        ctx.out.push(cs::start_skill_unit(skill, me_obj, me_obj));
        let ready = now + u64::from(info.casting_time) + u64::from(info.cooldown_time) + 100;
        ctx.state.set_cooldown(skill, ready);
        self.next_heal_ms = now + 1_000;
        ctx.log
            .debug(format!("rest: casting heal {skill} at {hp}% hit points"));
    }
}
