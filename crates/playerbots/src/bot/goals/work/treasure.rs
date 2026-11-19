//! Unlock a visible Pirate Treasure Chest and take the loot with an owned Freedich key.

use aa_proto::cs;

use crate::bot::cmd::GoalKind;
use crate::bot::ctx::Ctx;
use crate::bot::guild::{profession, treasury};
use crate::bot::pvp::walk::{Walk, Walker};
use crate::bot::state::dist3d;

use super::{FailReason, GoalStatus, Profession};

const CHEST: u32 = 3_012;
const KEY: u32 = 17_673;
const UNLOCK: u32 = 14_355;
const CLOSED: u32 = 6_846;
const OPEN: u32 = 6_847;
const LOOTED: u32 = 6_848;

#[derive(Debug, Clone, Copy)]
enum Stage {
    Approach,
    Unlocking { before: i64, keys: i64 },
    Taking { before: i64, keys: i64 },
}

#[derive(Debug)]
pub(super) struct Hunt {
    chest: u32,
    pos: [f32; 3],
    stage: Stage,
    deadline: u64,
}

pub(super) fn find(ctx: &Ctx<'_>) -> Option<Hunt> {
    if !ctx.cfg.guild.treasure_hunts
        || ctx.state.place.in_instance()
        || ctx.state.place.loading()
        || ctx.state.me.hp_pct() < 65
        || !matches!(
            profession::membership(ctx),
            Some((_, Profession::Thief | Profession::BountyHunter, false))
        )
        || ctx.state.item_count(KEY) == 0
    {
        return None;
    }
    let me = ctx.pos();
    let (&chest, doodad) = ctx
        .state
        .doodads
        .iter()
        .filter(|(_, d)| d.template == CHEST && d.phase == CLOSED && dist3d(me, d.pos) <= 90.0)
        .min_by(|(_, a), (_, b)| dist3d(me, a.pos).total_cmp(&dist3d(me, b.pos)))?;
    Some(Hunt {
        chest,
        pos: doodad.pos,
        stage: Stage::Approach,
        deadline: ctx.now_ms + 60_000,
    })
}

impl Hunt {
    pub(super) fn tick(&mut self, ctx: &mut Ctx<'_>, walker: &mut Walker) -> GoalStatus {
        let state = ctx
            .state
            .doodads
            .get(&self.chest)
            .filter(|chest| chest.template == CHEST);
        let phase = state.map(|chest| chest.phase);
        if let Stage::Taking { before, keys } | Stage::Unlocking { before, keys } = self.stage {
            let copper = ctx.state.me.money.saturating_sub(before);
            if (phase.is_none() || phase == Some(LOOTED))
                && (10_000..=20_000).contains(&copper)
                && ctx.state.item_count(KEY) < keys
            {
                ctx.hub.journal.update(ctx.persona.id, |journal| {
                    journal.treasure_chests = journal.treasure_chests.saturating_add(1);
                });
                treasury::note_receipt(ctx, GoalKind::Work, copper as u64);
                ctx.log.say(format!(
                    "treasure: took {copper} copper from a Pirate Treasure Chest"
                ));
                return GoalStatus::Done;
            }
        }
        if ctx.now_ms >= self.deadline {
            return GoalStatus::Failed(FailReason::Timeout);
        }
        match self.stage {
            Stage::Approach => {
                if phase != Some(CLOSED) || ctx.state.item_count(KEY) == 0 {
                    return GoalStatus::Failed(FailReason::NoTarget);
                }
                if dist3d(ctx.pos(), self.pos) > 3.5 {
                    return match walker.step(ctx, [self.pos[0], self.pos[1]], 1.5) {
                        Walk::Stuck => GoalStatus::Failed(FailReason::Stuck),
                        _ => GoalStatus::Running,
                    };
                }
                ctx.motion.stop();
                self.stage = Stage::Unlocking {
                    before: ctx.state.me.money,
                    keys: ctx.state.item_count(KEY),
                };
                self.deadline = ctx.now_ms + 12_000;
                ctx.send(cs::start_skill_doodad(UNLOCK, ctx.state.me.obj, self.chest));
            }
            Stage::Unlocking { before, keys } if phase == Some(OPEN) => {
                self.stage = Stage::Taking { before, keys };
                self.deadline = ctx.now_ms + 10_000;
                ctx.send(cs::loot_open_bag(self.chest, true));
            }
            _ => {}
        }
        GoalStatus::Running
    }
}
