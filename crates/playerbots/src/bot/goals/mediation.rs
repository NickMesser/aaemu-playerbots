//! Player-mediated reconciliation using the two bots' actual relationship memories.

use std::sync::atomic::Ordering;

use aa_proto::cs::chat_type;
use pb_web::Activity;

use super::delivery::{nearby_requester, tell};
use crate::bot::ctx::Ctx;
use crate::bot::state::{dist2d, ChatIn};
use crate::hub::unix_ms;
use crate::journal::{Mediation, MediationReply};

const QUEST_MS: u64 = 20 * 60_000;
const COOLDOWN_MS: u64 = 60 * 60_000;
const MAX_TRIP_M: f32 = 850.0;

pub(super) fn on_whisper(ctx: &mut Ctx<'_>, line: &ChatIn) -> bool {
    if !ctx.cfg.behavior.delivery_contracts
        || line.kind != chat_type::WHISPER
        || ctx.state.place.in_instance()
        || ctx.state.me.dead
    {
        return false;
    }
    let text = line.text.trim();
    let (verb, origin) = text.split_once(' ').unwrap_or((text, ""));
    if !verb.eq_ignore_ascii_case("mediate") {
        return false;
    }
    let Some((requester, who)) = nearby_requester(ctx, line) else {
        return false;
    };
    let origin = origin.trim();
    if origin.is_empty() {
        offer(ctx, requester, who);
    } else {
        resolve(ctx, requester, who, origin);
    }
    true
}

fn offer(ctx: &mut Ctx<'_>, requester: u32, who: &str) {
    let now = unix_ms();
    let id = ctx.persona.id;
    let grudges = ctx.hub.journal.read(id, |j| {
        let cooling = j
            .last_mediation_ms
            .is_some_and(|at| now.saturating_sub(at) < COOLDOWN_MS);
        if cooling || j.mediation.is_some_and(|quest| quest.expires_ms > now) {
            return None;
        }
        Some(
            j.relationships
                .entries
                .iter()
                .filter(|r| r.trust < 0)
                .map(|r| (r.character, r.trust))
                .collect::<Vec<_>>(),
        )
    });
    let Some(grudges) = grudges else {
        tell(ctx, who, "Let me give the matter some time.");
        return;
    };
    let pos = ctx.pos();
    let peer = grudges
        .into_iter()
        .filter_map(|(character, trust)| {
            let peer = ctx.hub.slot(ctx.hub.bot_id_of(character)?)?;
            peer.with(|g| {
                let snap = &g.snap;
                (!matches!(
                    snap.activity,
                    Activity::Offline | Activity::Connecting | Activity::Dead
                ) && !snap.name.is_empty()
                    && dist2d(pos, [snap.x, snap.y, snap.z]) <= MAX_TRIP_M)
                    .then(|| (trust, character, snap.name.clone()))
            })
        })
        .min_by_key(|(trust, character, _)| (*trust, *character));
    let Some((_, recipient, name)) = peer else {
        tell(
            ctx,
            who,
            "I have no disagreement with anyone nearby that needs your help.",
        );
        return;
    };
    ctx.hub.journal.update(id, |j| {
        j.mediation = Some(Mediation {
            requester,
            recipient,
            expires_ms: now + QUEST_MS,
        });
    });
    tell(
        ctx,
        who,
        &format!(
            "Can you speak to {name} about our disagreement? Go to them and whisper mediate {}.",
            ctx.persona.name
        ),
    );
    ctx.log
        .say(format!("mediation: asked {who} to speak to {name}"));
}

fn resolve(ctx: &mut Ctx<'_>, requester: u32, who: &str, origin: &str) {
    let notify = ctx.hub.slots().into_iter().find_map(|slot| {
        let character = slot.char_id.load(Ordering::Relaxed);
        slot.with(|g| {
            (character != 0 && g.snap.name.eq_ignore_ascii_case(origin))
                .then(|| (slot.id, character, g.snap.name.clone()))
        })
    });
    let Some((id, character, name)) = notify else {
        tell(ctx, who, "I don't know who asked you to speak to me.");
        return;
    };
    let tolerance = 20 + (super::field::cooperation(ctx).clamp(0.0, 1.0) * 30.0).round() as i8;
    let result = ctx.hub.journal.finish_mediation(
        (id, character, &name),
        (ctx.persona.id, ctx.state.me.char_id, &ctx.persona.name),
        requester,
        unix_ms(),
        -tolerance,
    );
    match result {
        MediationReply::Accepted => {
            tell(
                ctx,
                who,
                &format!("Thanks for hearing us out. I'll give {name} another chance."),
            );
            ctx.log
                .say(format!("mediation: reconciled with {name} through {who}"));
        }
        MediationReply::Declined => tell(ctx, who, "Thanks for trying. I think we need more time."),
        MediationReply::NoOffer => tell(
            ctx,
            who,
            "I have no active mediation request from you for that bot.",
        ),
    }
}
