//! Short bot-only parties for open-world exploration, grinding and quests.

use aa_proto::cs;
use aa_proto::sc_activity::quest_status;

use super::work::{self, Work};
use super::{FailReason, GoalStatus, Grind, Quest, Wander};
use crate::bot::combat::CombatStatus;
use crate::bot::coordinator::{Dispatch, FieldOffer};
use crate::bot::ctx::Ctx;
use crate::bot::party::{FieldActivity, PartyOrder, PartyPurpose};
use crate::bot::pvp::walk::{Walk, Walker};
use crate::bot::state::{dist2d, UnitKind};

const PARTY_QUEST_REACH_M: f32 = 40.0;

pub fn ready(ctx: &Ctx<'_>) -> bool {
    ctx.cfg.behavior.field_parties
        && ctx.state.me.hp_pct() >= 70
        && !ctx.state.place.in_instance()
        && !ctx.state.place.loading()
        && !ctx.state.team().is_some_and(|t| t.in_team())
}

pub fn cooperation(ctx: &Ctx<'_>) -> f32 {
    (ctx.persona.profile.chatty + 1.0 - ctx.persona.temper.homebody) * 0.5
}

#[derive(Debug)]
enum Outing {
    Explore(Wander),
    Forage(Box<Work>),
    Grind(Grind),
    Quest(Quest),
}

#[derive(Debug, Default)]
pub struct Field {
    order: Option<PartyOrder>,
    deadline: u64,
    next_invite: u64,
    next_relationship_check: u64,
    forage_allowed: bool,
    gathered: u8,
    invite_index: usize,
    accepted: bool,
    team_id: u32,
    walker: Walker,
    outing: Option<Outing>,
    started: bool,
    published_quest: Option<u32>,
    quest_interaction: Option<Quest>,
    last_quest_accept: u32,
    last_quest_report: u32,
    next_quest_offer_ms: u64,
}

impl Field {
    pub fn new(order: Option<Box<PartyOrder>>) -> Box<Self> {
        Box::new(Self {
            order: order.map(|o| *o),
            ..Self::default()
        })
    }

    pub fn permit_forage(&mut self, allowed: bool) {
        self.forage_allowed = allowed;
    }

    pub fn start(&mut self, ctx: &mut Ctx<'_>) {
        self.deadline = ctx.now_ms + 35_000;
        self.invite_index = 1;
        if self.order.is_none() && ready(ctx) {
            let [x, y, _] = ctx.pos();
            ctx.coord.request_field_party(FieldOffer {
                bot: ctx.persona.id,
                at: [x, y],
                mother: crate::bot::guild::duty::mother_of(ctx),
                level: ctx.state.me.level,
                role: crate::bot::companion::duty::role_of(ctx.persona.archetype.role),
                lead: (ctx.persona.profile.dedication + ctx.persona.profile.chatty) * 0.5,
                explore: ctx.persona.profile.explorer,
                quest: if ctx.cfg.behavior.quest > 0.0 {
                    ctx.persona.temper.quester
                } else {
                    0.0
                },
                forage: if self.forage_allowed && work::forage_ready(ctx) {
                    0.65 + 0.35 * ctx.persona.temper.merchant
                } else {
                    0.0
                },
                cooperation: cooperation(ctx),
                outlaw: ctx.persona.profile.outlaw,
            });
        }
    }

    pub fn tick(&mut self, ctx: &mut Ctx<'_>, combat: CombatStatus) -> GoalStatus {
        if !ctx.cfg.behavior.field_parties
            || ctx.now_ms >= self.deadline
            || ctx.state.place.in_instance()
        {
            return if self.team_id != 0 {
                GoalStatus::Done
            } else {
                GoalStatus::Failed(FailReason::Timeout)
            };
        }
        let Some(order) = self.order.as_ref() else {
            return GoalStatus::Running;
        };
        let (leader_char, _) = Dispatch::who(ctx.hub, order.leader);
        let leader_bot = order.leader;
        let party_id = order.party_id;
        let shares_quests = order.purpose == PartyPurpose::Field(FieldActivity::Quest);
        if leader_char == 0 {
            return GoalStatus::Failed(FailReason::NoTarget);
        }
        let own = order.is_leader(ctx.persona.id);
        let team = ctx.state.team().filter(|team| team.in_team());
        if team.is_some_and(|t| t.owner != leader_char) {
            return GoalStatus::Failed(FailReason::Other("already in another team"));
        }
        let size = team.map_or(0, |t| t.size());
        if self.team_id != 0 {
            if team.is_none_or(|t| t.team_id != self.team_id) || ctx.state.me.hp_pct() < 35 {
                return GoalStatus::Done;
            }
        } else if let Some(team) = team.filter(|t| t.size() >= 2) {
            self.team_id = team.team_id;
        }
        if own && self.invite_index < order.members.len() {
            if ctx.now_ms >= self.next_invite {
                let (_, name) = Dispatch::who(ctx.hub, order.members[self.invite_index]);
                if !name.is_empty() {
                    ctx.send(cs::invite_to_team(self.team_id, true, &name));
                }
                self.next_invite = ctx.now_ms + 7_000;
                self.invite_index += 1;
            }
            return GoalStatus::Running;
        }
        if self.team_id == 0 {
            if !own && !self.accepted {
                let invite = ctx.state.team().and_then(|t| t.invite.clone());
                if let Some(i) = invite.filter(|i| i.is_party && i.from_char == leader_char) {
                    ctx.send(cs::reply_to_join_team(
                        i.team_id,
                        i.is_party,
                        i.from_char,
                        false,
                        &i.from_name,
                        false,
                    ));
                    self.accepted = true;
                }
            }
            return GoalStatus::Running;
        }
        if size < 2 {
            return GoalStatus::Done;
        }
        if ctx.now_ms >= self.next_relationship_check {
            self.next_relationship_check = ctx.now_ms + 3_000;
            if team.is_some_and(|team| {
                ctx.hub.journal.read(ctx.persona.id, |j| {
                    team.members.iter().any(|mate| {
                        j.relationships
                            .get(mate.char_id)
                            .is_some_and(|r| r.trust <= -40)
                    })
                })
            }) {
                ctx.log
                    .say("field party: leaving after a breakdown in trust");
                return GoalStatus::Done;
            }
        }
        if !self.started {
            let PartyPurpose::Field(activity) = order.purpose else {
                return GoalStatus::Failed(FailReason::NoTarget);
            };
            if activity == FieldActivity::Forage
                && own
                && !(self.forage_allowed && work::forage_ready(ctx))
            {
                return GoalStatus::Failed(FailReason::Other("foraging order unavailable"));
            }
            self.begin_outing(ctx, activity);
        }
        match &mut self.outing {
            Some(Outing::Explore(wander)) => wander.tick(ctx),
            Some(Outing::Forage(work)) => {
                let status = work.tick(ctx, combat);
                if status == GoalStatus::Done {
                    self.gathered += 1;
                    if self.gathered < 3 && work::forage_ready(ctx) {
                        **work = Work::default();
                        work.start(ctx);
                        return GoalStatus::Running;
                    }
                }
                status
            }
            Some(Outing::Grind(grind)) => grind.tick(ctx, combat),
            Some(Outing::Quest(quest)) => {
                let status = quest.tick(ctx);
                let focus = quest.party_template();
                if self.published_quest != focus {
                    self.published_quest = focus;
                    ctx.log.with_slot(|slot| {
                        slot.field_quest = focus.map(|quest_id| (party_id, quest_id));
                    });
                }
                status
            }
            None => {
                if ctx.combat.busy() {
                    return GoalStatus::Running;
                }
                let position = ctx
                    .state
                    .units
                    .values()
                    .find(|u| u.kind == UnitKind::Character && u.id == leader_char)
                    .map(|u| u.pos)
                    .or_else(|| {
                        let leader = ctx.state.team()?.remote_of(leader_char)?;
                        (leader.online
                            && leader.located()
                            && leader.instance_id == 0
                            && ctx.now_ms.saturating_sub(leader.seen_ms) < 15_000)
                            .then_some(leader.pos)
                    });
                if let Some(pos) = position {
                    let distance = dist2d(ctx.pos(), pos);
                    if shares_quests && distance <= PARTY_QUEST_REACH_M {
                        if self.follow_party_quest(ctx, party_id, leader_bot) {
                            return GoalStatus::Running;
                        }
                    } else if let Some(mut quest) = self.quest_interaction.take() {
                        quest.stop(ctx);
                    }
                    if distance < 6.0 {
                        if ctx.motion.is_moving() {
                            ctx.motion.stop();
                            self.walker.reset();
                        }
                        return GoalStatus::Running;
                    }
                    if self.walker.step(ctx, [pos[0], pos[1]], 4.0) == Walk::Stuck {
                        return GoalStatus::Failed(FailReason::Stuck);
                    }
                } else if let Some(mut quest) = self.quest_interaction.take() {
                    quest.stop(ctx);
                }
                GoalStatus::Running
            }
        }
    }

    fn follow_party_quest(&mut self, ctx: &mut Ctx<'_>, party_id: u32, leader_bot: u32) -> bool {
        if let Some(quest) = self.quest_interaction.as_mut() {
            if quest.tick(ctx) == GoalStatus::Running && quest.interacting() {
                return true;
            }
            quest.stop(ctx);
            self.quest_interaction = None;
        }
        if ctx.now_ms < self.next_quest_offer_ms {
            return false;
        }
        self.next_quest_offer_ms = ctx.now_ms + 3_000;
        let Some((_, quest_id)) = ctx
            .hub
            .slot(leader_bot)
            .and_then(|slot| slot.with(|slot| slot.field_quest))
            .filter(|&(order, _)| order == party_id)
        else {
            return false;
        };
        let row = ctx.state.quests.row(quest_id);
        let reporting = row.is_some_and(|row| row.status == quest_status::READY);
        if !reporting && row.is_some() {
            return false;
        }
        let last = if reporting {
            &mut self.last_quest_report
        } else {
            &mut self.last_quest_accept
        };
        if *last == quest_id {
            return false;
        }
        let Some(quest) = Quest::party_interaction(ctx, quest_id, reporting) else {
            return false;
        };
        *last = quest_id;
        self.quest_interaction = Some(quest);
        self.walker.reset();
        true
    }

    fn begin_outing(&mut self, ctx: &mut Ctx<'_>, activity: FieldActivity) {
        self.started = true;
        self.deadline = ctx.now_ms + 180_000 + (240_000.0 * cooperation(ctx)) as u64;
        if let Some(team) = ctx.state.team() {
            ctx.hub.journal.update(ctx.persona.id, |j| {
                for mate in team
                    .members
                    .iter()
                    .filter(|m| m.char_id != ctx.state.me.char_id)
                {
                    j.relationships.update(mate.char_id, |r| {
                        r.joined_party(crate::hub::unix_ms() / 1000)
                    });
                }
            });
        }
        let role = crate::bot::companion::duty::role_of(ctx.persona.archetype.role);
        ctx.send(cs::set_team_member_role(
            self.team_id,
            ctx.state.me.char_id,
            role,
        ));
        ctx.log.say("field party: joined the open-world team");
        if self
            .order
            .as_ref()
            .is_some_and(|o| o.is_leader(ctx.persona.id))
        {
            let (outing, message) = match activity {
                FieldActivity::Explore => {
                    let mut wander = Wander::default();
                    wander.start(ctx);
                    (Outing::Explore(wander), "Let's explore a bit. Stay close.")
                }
                FieldActivity::Forage => {
                    let mut gather = Box::<Work>::default();
                    gather.start(ctx);
                    (
                        Outing::Forage(gather),
                        "Let's forage together. Keep an eye on the trail.",
                    )
                }
                FieldActivity::Grind => {
                    let mut grind = Grind::default();
                    grind.start(ctx);
                    (Outing::Grind(grind), "Let's clear out monsters together.")
                }
                FieldActivity::Quest => {
                    let mut quest = Quest::default();
                    quest.start(ctx);
                    (Outing::Quest(quest), "Let's work on quests together.")
                }
            };
            self.outing = Some(outing);
            if let Some(message) = super::social::wire_text(message) {
                ctx.send(cs::chat(cs::chat_type::PARTY, "", message));
            }
        }
    }

    pub fn stop(&mut self, ctx: &mut Ctx<'_>) {
        ctx.coord.cancel(ctx.persona.id);
        if self.published_quest.is_some() {
            ctx.log.with_slot(|slot| slot.field_quest = None);
        }
        if let Some(mut quest) = self.quest_interaction.take() {
            quest.stop(ctx);
        }
        match &mut self.outing {
            Some(Outing::Explore(wander)) => wander.stop(ctx),
            Some(Outing::Forage(work)) => work.stop(ctx),
            Some(Outing::Grind(grind)) => grind.stop(ctx),
            Some(Outing::Quest(quest)) => quest.stop(ctx),
            None => ctx.motion.stop(),
        }
        let me = ctx.state.me.char_id;
        let Some(team) = ctx
            .state
            .team()
            .filter(|t| self.team_id != 0 && t.team_id == self.team_id)
        else {
            return;
        };
        let action =
            if team.owner == me && team.members.iter().all(|m| ctx.hub.is_bot_char(m.char_id)) {
                cs::RISKY_DISMISS
            } else {
                cs::RISKY_LEAVE
            };
        ctx.send(cs::ask_risky_team_action(self.team_id, me, action));
    }

    pub fn describe(&self, out: &mut String) {
        out.push_str(match &self.outing {
            Some(Outing::Explore(_)) => "Exploring with a bot party",
            Some(Outing::Forage(_)) => "Leading a foraging party",
            Some(Outing::Grind(_)) => "Leading a grinding party",
            Some(Outing::Quest(_)) => "Leading a questing party",
            None if self.quest_interaction.is_some() => "Working on the party's quest",
            None if self.team_id != 0 => "Following the field party",
            None => "Looking for a nearby field party",
        });
    }
}
