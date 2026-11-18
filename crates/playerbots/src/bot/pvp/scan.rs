//! Looking for fights: the characters in view that the rules allow the bot to attack.
//!
//! The gank goal asks for carriers of trade packs ([`carriers`]), the warzone goal for any
//! hostile character ([`hunted`]); both weigh a fight with [`sides`] before they take it.

use smallvec::SmallVec;

use crate::bot::ctx::Ctx;
use crate::bot::state::{dist2d, UnitKind};

use super::fight::{self, Odds};
use super::rules::{Purpose, Rules, Target};

/// Most candidates looked at.
const MAX_CANDS: usize = 8;
/// Characters of the target's side count when they stand this close to it (metres) ...
const FOE_NEAR_M: f32 = 30.0;
/// ... and those of the bot's side when they stand this close to the bot.
const FRIEND_NEAR_M: f32 = 40.0;

/// A character the bot may attack.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cand {
    /// Object id.
    pub obj: u32,
    /// Distance to the bot.
    pub dist: f32,
    /// Distance to the point the squad lies in wait at.
    pub dist_post: f32,
    /// Level.
    pub level: u8,
    /// Hit points.
    pub hp: i32,
    /// Template of the pack it carries (0 for none).
    pub pack: u32,
}

/// A side of a fight: `(level, hit points)` of its characters.
pub type Side = SmallVec<[(u8, i32); 8]>;

/// The characters within `radius` metres of the bot that the rules allow for `purpose`, nearest
/// to `post` first. Characters the bot gave up on lately are left out.
pub fn candidates(
    ctx: &Ctx<'_>,
    purpose: Purpose,
    post: [f32; 2],
    radius: f32,
) -> SmallVec<[Cand; MAX_CANDS]> {
    let mut out: SmallVec<[Cand; MAX_CANDS]> = SmallVec::new();
    let rules = Rules::new(ctx.world, ctx.cfg, ctx.state.me.faction);
    let me = ctx.state.me.pos;
    let now = ctx.now_ms;
    for (&obj, u) in &ctx.state.units {
        if u.kind != UnitKind::Character {
            continue;
        }
        let dist = u.dist2d(me);
        if dist > radius {
            continue;
        }
        if ctx.state.pvp.as_ref().is_some_and(|p| p.shunned(obj, now))
            || ctx.state.avoids(obj, 0, now)
        {
            continue;
        }
        let pack = ctx.state.pack_of(obj);
        let t = Target::of(u, ctx.hub.is_bot_char(u.id), pack);
        if rules.check(&t, purpose).is_err() {
            continue;
        }
        if out.len() == MAX_CANDS {
            break;
        }
        out.push(Cand {
            obj,
            dist,
            dist_post: dist2d(u.pos, [post[0], post[1], 0.0]),
            level: u.level,
            hp: u.hp,
            pack,
        });
    }
    out
}

/// The carriers of trade packs the bot may ambush (see [`candidates`]).
pub fn carriers(
    ctx: &Ctx<'_>,
    post: [f32; 2],
    radius: f32,
    raid_catch: bool,
) -> SmallVec<[Cand; MAX_CANDS]> {
    let purpose = if raid_catch {
        Purpose::RaidCatch
    } else {
        Purpose::Gank
    };
    candidates(ctx, purpose, post, radius)
}

/// The hostile characters the bot may fight in a war (see [`candidates`]).
pub fn hunted(ctx: &Ctx<'_>, radius: f32) -> SmallVec<[Cand; MAX_CANDS]> {
    let me = ctx.state.me.pos;
    candidates(ctx, Purpose::Hunt, [me[0], me[1]], radius)
}

/// The two sides of a fight against `target`: the friends at the bot's side (characters of the
/// bot's own alliance close to it) and the foes at the target's (characters hostile to the bot
/// close to the target, the target itself left out), each as `(level, hit points)`.
pub fn sides(ctx: &Ctx<'_>, target: u32) -> (Side, Side) {
    let mut friends = SmallVec::new();
    let mut foes = SmallVec::new();
    let rules = Rules::new(ctx.world, ctx.cfg, ctx.state.me.faction);
    let me = ctx.state.me.pos;
    let Some(tpos) = ctx.state.units.get(&target).map(|u| u.pos) else {
        return (friends, foes);
    };
    let mine = ctx.world.mother_faction(ctx.state.me.faction);
    for (&obj, u) in &ctx.state.units {
        if obj == target || u.kind != UnitKind::Character || u.dead() {
            continue;
        }
        if rules.hostile(u.faction) {
            if u.dist2d(tpos) <= FOE_NEAR_M && foes.len() < 8 {
                foes.push((u.level, u.hp));
            }
        } else if mine != 0
            && ctx.world.mother_faction(u.faction) == mine
            && u.dist2d(me) <= FRIEND_NEAR_M
            && friends.len() < 8
        {
            friends.push((u.level, u.hp));
        }
    }
    (friends, foes)
}

/// The name of the character with object id `obj` if it is in view (for the log).
pub fn name_of(ctx: &Ctx<'_>, obj: u32) -> String {
    ctx.state
        .units
        .get(&obj)
        .and_then(|u| u.name.as_deref())
        .map_or_else(|| format!("unit {obj}"), str::to_owned)
}

/// How the fight against `target` looks to the bot.
pub fn weigh(ctx: &Ctx<'_>, target: u32) -> Odds {
    let Some(u) = ctx.state.units.get(&target) else {
        return Odds::Hopeless;
    };
    let me = &ctx.state.me;
    let (friends, foes) = sides(ctx, target);
    fight::odds((me.level, me.hp), &friends, (u.level, u.hp), &foes)
}
