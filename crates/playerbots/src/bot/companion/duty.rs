//! The glue the brain calls on every tick whatever the bot does: it answers the invitations of
//! real players, keeps a companion's membership in order (role, lead, leaving), sends the queued
//! whispers and reads the chat commands. The decisions are in [`super::policy`],
//! [`super::upkeep`] and [`super::command`]; this file turns them into packets and state.
//!
//! * **Invitations** (`party.join_player_parties`). A bot answers the invitation of a character
//!   that is not one of ours within seconds: a no at once (busy, in a team, level gap, caps), a yes
//!   after a human pause. Invitations of our own bots belong to the dungeon and guild goals and are
//!   left alone. [`claims`] tells the dungeon goal's stray rule to keep its hands off a
//!   player's invitation while this feature is on.
//! * **Upkeep.** A bot in the party of a real player counts on the hub's board, announces its role
//!   once, hands the lead back to the first real player when it owns the team, and leaves when
//!   asked, when no real player is left, when they stay offline, or when the levels drifted apart.
//! * **Chat** (`party.chat_commands`). [`forward_chat`] reads the whispers and the local and party
//!   lines before the social duty files them: the commands of the closed set from a member of the
//!   bot's team, and the whispers "inv" from anyone. The social duty does not answer a whisper this
//!   feature answered.

use smallvec::SmallVec;

use aa_proto::cs::{self, chat_type};

use super::command::{self, Command, Match, Say};
use super::policy::{self, Facts, Why};
use super::upkeep::{self, Leave};
use super::{Mem, Mode, Pending};
use crate::bot::cmd::GoalKind;
use crate::bot::ctx::Ctx;
use crate::bot::goals::social::{strip_gm, whisper_packet};
use crate::bot::party::{PartyMem, TeamLog};
use crate::bot::state::{BotState, ChatIn, UnitKind};
use crate::hub::unix_ms;
use crate::journal::Preference;

/// The pause before a yes (a person needs a moment to click), in milliseconds.
const ACCEPT_PAUSE_MS: (u64, u64) = (1_500, 3_500);
/// The pause before a whisper reply.
const REPLY_PAUSE_MS: (u64, u64) = (700, 2_200);
/// A bot that said yes and got no team within this long gives its place back.
const JOIN_WAIT_MS: u64 = 15_000;
/// A bot answers a request for an invitation from one player at most this often.
const ASK_GAP_MS: u64 = 60_000;
/// A bot tells one player why it said no at most this often.
const DECLINE_TELL_GAP_MS: u64 = 120_000;
/// How long an `assist` keeps the bot on the leader's target.
const ASSIST_MS: u64 = 20_000;
/// The pause between two leave requests.
const LEAVE_GAP_MS: u64 = 5_000;

fn jitter(ctx: &mut Ctx<'_>, (lo, hi): (u64, u64)) -> u64 {
    crate::bot::goals::jitter_ms(ctx.rng, lo, hi)
}

fn name_key(name: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = rustc_hash::FxHasher::default();
    name.trim().to_lowercase().hash(&mut h);
    h.finish()
}

/// The companion memory of the bot (made on first use).
pub fn mem_of(state: &mut BotState) -> &mut Mem {
    state
        .party_mem()
        .companion
        .get_or_insert_with(Default::default)
}

/// The companion memory of the bot when it has one (reading it makes none).
pub fn peek(state: &BotState) -> Option<&Mem> {
    state.party.as_deref()?.companion.as_deref()
}

/// Like [`peek`], to change.
fn peek_mut(state: &mut BotState) -> Option<&mut Mem> {
    state.party.as_deref_mut()?.companion.as_deref_mut()
}

fn team_of<'a>(ctx: &'a Ctx<'_>) -> Option<&'a TeamLog> {
    ctx.state.team().filter(|t| t.in_team())
}

/// Whether a player told the bot to stay: it then helps the party only close by (see the combat
/// controller) and does not follow.
pub fn holding(state: &BotState) -> bool {
    peek(state).is_some_and(|m| m.mode == Mode::Stay)
}

/// Whether the bot is in a team with a real player (a character that is not one of ours).
pub fn in_player_party(ctx: &Ctx<'_>) -> bool {
    let me = ctx.state.me.char_id;
    team_of(ctx).is_some_and(|t| {
        t.members
            .iter()
            .any(|m| m.char_id != me && !ctx.hub.is_bot_char(m.char_id))
    })
}

/// The character the bot follows: the real owner of its team, else the first real player online
/// (a team owned by a bot is handed to them).
pub fn leader_char(ctx: &Ctx<'_>) -> Option<u32> {
    let t = team_of(ctx)?;
    let members = upkeep_members(ctx, t);
    upkeep::leader_of(t.owner, &members, ctx.state.me.char_id)
}

fn upkeep_members(ctx: &Ctx<'_>, t: &TeamLog) -> Vec<upkeep::Member> {
    t.remote
        .iter()
        .map(|r| upkeep::Member {
            char_id: r.char_id,
            bot: ctx.hub.is_bot_char(r.char_id),
            online: r.online,
            level: r.level,
        })
        .collect()
}

/// Whether the dungeon goal's stray rule must leave the pending invitation alone: this feature is
/// on and the invitation is a real player's.
pub fn claims(ctx: &Ctx<'_>) -> bool {
    ctx.cfg.party.join_player_parties
        && ctx
            .state
            .team()
            .and_then(|t| t.invite.as_ref())
            .is_some_and(|i| !ctx.hub.is_bot_char(i.from_char))
}

/// The policy facts of this tick. `other` is the character the bot would join (or invite).
fn facts(ctx: &Ctx<'_>, running: Option<GoalKind>, other: u32) -> Facts {
    let st = &*ctx.state;
    let now = ctx.now.into_std();
    let cfg = &ctx.cfg.party;
    Facts {
        goal: running,
        in_team: team_of(ctx).is_some(),
        away: st.place.in_instance() || st.place.loading(),
        down: st.me.dead || !st.me.spawned,
        trading: st.trade.active(),
        dueling: ctx.combat.duel.in_duel(),
        travelling: st.travel.busy() || st.vehicle.seat().is_some() || st.vehicle.boat().is_some(),
        level: st.me.level,
        other_level: st
            .units
            .values()
            .find(|u| u.kind == UnitKind::Character && u.id == other)
            .map(|u| u.level)
            .filter(|&l| l > 0),
        max_gap: cfg.max_level_gap,
        companions: ctx.hub.companions.count(now),
        max_companions: cfg.max_companions as usize,
        with_player: ctx.hub.companions.of_player(other, now),
        max_per_player: usize::from(cfg.max_per_player),
    }
}

fn judge(ctx: &Ctx<'_>, running: Option<GoalKind>, other: u32) -> Result<(), Why> {
    policy::judge(&facts(ctx, running, other))?;
    check_trust(ctx, other)
}

fn check_trust(ctx: &Ctx<'_>, other: u32) -> Result<(), Why> {
    if ctx.hub.journal.read(ctx.persona.id, |j| {
        j.relationships.get(other).is_some_and(|r| r.trust <= -40)
    }) {
        return Err(Why::Distrusted);
    }
    Ok(())
}

/// What the brain asks every tick. Returns whether the bot should start the companion goal now
/// (it is in a real player's party and runs nothing it must not drop).
pub fn duty(ctx: &mut Ctx<'_>, running: Option<GoalKind>) -> bool {
    let cfg = &ctx.cfg.party;
    if !cfg.join_player_parties && !cfg.chat_commands {
        return false;
    }
    send_replies(ctx);
    if !cfg.join_player_parties {
        return false;
    }
    // (a dungeon run, a founding or a squad answers the invitations of its own bots; the invitation
    // of a real player gets its no at once: those goals are busy)
    answer_invitation(ctx, running);
    if !matches!(
        running,
        Some(GoalKind::Dungeon | GoalKind::Guild | GoalKind::Gank)
    ) {
        upkeep(ctx, running);
    }
    running.is_none_or(policy::preemptible) && in_player_party(ctx) && !leaving(ctx)
}

/// The bot has been told to leave or is leaving: no new goal for it.
fn leaving(ctx: &mut Ctx<'_>) -> bool {
    mem_of(ctx.state).leave
}

// ---- invitations -----------------------------------------------------------------------------

fn answer_invitation(ctx: &mut Ctx<'_>, running: Option<GoalKind>) {
    let now = ctx.now_ms;
    let inv = ctx.state.team().and_then(|t| t.invite.clone());
    let Some(inv) = inv else {
        // nothing to answer; a yes that never became a team gives its place back
        let wait = peek(ctx.state).and_then(|m| m.joining);
        if let Some((_, at)) = wait {
            if team_of(ctx).is_none() && now.saturating_sub(at) > JOIN_WAIT_MS {
                mem_of(ctx.state).joining = None;
                ctx.hub.companions.release(ctx.persona.id);
                ctx.log.say("party: the team never came; place given back");
            }
        }
        if let Some(m) = peek_mut(ctx.state) {
            m.pending = None;
        }
        return;
    };
    // the invitations of our own bots belong to the dungeon and guild goals
    if ctx.hub.is_bot_char(inv.from_char) {
        return;
    }
    let pending = peek(ctx.state)
        .and_then(|m| m.pending)
        .filter(|p| p.invite_ms == inv.at_ms);
    let Some(pending) = pending else {
        // first sight of this invitation: a no goes out at once, a yes after a pause
        match judge(ctx, running, inv.from_char) {
            Err(why) => decline(ctx, &inv, why),
            Ok(()) => {
                let due_ms = now + jitter(ctx, ACCEPT_PAUSE_MS);
                mem_of(ctx.state).pending = Some(Pending {
                    invite_ms: inv.at_ms,
                    due_ms,
                });
            }
        }
        return;
    };
    if now < pending.due_ms {
        return;
    }
    mem_of(ctx.state).pending = None;
    if let Err(why) = judge(ctx, running, inv.from_char) {
        decline(ctx, &inv, why);
        return;
    }
    let cfg = &ctx.cfg.party;
    if let Err(r) = ctx.hub.companions.reserve(
        ctx.persona.id,
        inv.from_char,
        cfg.max_companions as usize,
        usize::from(cfg.max_per_player),
        ctx.now.into_std(),
    ) {
        let why = match r {
            super::Refused::TooMany => Why::TooMany,
            super::Refused::FullParty => Why::FullParty,
        };
        decline(ctx, &inv, why);
        return;
    }
    ctx.send(cs::reply_to_join_team(
        inv.team_id,
        inv.is_party,
        inv.from_char,
        false,
        &inv.from_name,
        false,
    ));
    ctx.state.party_mem().team.invite = None;
    mem_of(ctx.state).joining = Some((inv.from_char, now));
    ctx.log
        .say(format!("party: joining the party of {}", inv.from_name));
}

/// Turns an invitation down at once and, now and then, says why.
fn decline(ctx: &mut Ctx<'_>, inv: &crate::bot::party::Invite, why: Why) {
    ctx.send(cs::reply_to_join_team(
        inv.team_id,
        inv.is_party,
        inv.from_char,
        true,
        &inv.from_name,
        false,
    ));
    ctx.state.party_mem().team.invite = None;
    mem_of(ctx.state).pending = None;
    ctx.log.say(format!(
        "party: turned down the invitation of {}: {}",
        inv.from_name,
        why.word()
    ));
    let key = name_key(&inv.from_name);
    let now = ctx.now_ms;
    let told = mem_of(ctx.state)
        .declined
        .iter()
        .any(|d| d.0 == key && now.saturating_sub(d.1) < DECLINE_TELL_GAP_MS);
    if told {
        return;
    }
    let m = mem_of(ctx.state);
    if m.declined.len() >= 3 {
        m.declined.remove(0);
    }
    m.declined.push((key, now));
    let say = match why {
        Why::LevelGap => Say::LevelGap,
        Why::TooMany | Why::FullParty => Say::Full,
        _ => Say::Sorry,
    };
    let pick = ctx.rng.usize(..8);
    let due = now + jitter(ctx, REPLY_PAUSE_MS);
    let text = command::line(say, pick, "");
    mem_of(ctx.state).reply(due, &inv.from_name, text);
}

// ---- upkeep ----------------------------------------------------------------------------------

fn upkeep(ctx: &mut Ctx<'_>, running: Option<GoalKind>) {
    let now = ctx.now_ms;
    let bot = ctx.persona.id;
    let (team_id, owner, members) = match team_of(ctx) {
        Some(t) => (t.team_id, t.owner, upkeep_members(ctx, t)),
        None => {
            // not in a team (any more): the place on the board goes back
            let was = peek_mut(ctx.state).is_some_and(|m| {
                let was = m.registered != 0;
                m.registered = 0;
                m.hand = (0, 0);
                m.gone_since_ms = 0;
                m.leave = false;
                m.leave_sent_ms = 0;
                was
            });
            if was {
                ctx.hub.companions.release(bot);
                ctx.log.say("party: out of the party");
            }
            return;
        }
    };
    let registered = mem_of(ctx.state).registered;
    if registered == 0 && !in_player_party(ctx) {
        // a team of bots (a dungeon party that is not running, a founding): not ours
        return;
    }
    let me = ctx.state.me.char_id;
    // the player whose party this is: the board counts the bot there
    let leader = upkeep::leader_of(owner, &members, me);
    let key = leader.unwrap_or(0);
    let cfg = &ctx.cfg.party;
    let reserved = ctx.hub.companions.reserve(
        bot,
        key,
        cfg.max_companions as usize + 1,
        usize::from(cfg.max_per_player) + 1,
        ctx.now.into_std(),
    );
    let first = registered == 0;
    if let Some(player) = leader.filter(|&id| id != registered && reserved.is_ok()) {
        let at = unix_ms() / 1000;
        ctx.hub.journal.update(bot, |j| {
            j.relationships.update(player, |r| r.joined_party(at));
        });
    }
    let (role_sent, hand, gone_since_ms, leave_asked) = {
        let m = mem_of(ctx.state);
        if reserved.is_ok() {
            m.registered = key.max(1);
        }
        if first {
            m.joining = None;
        }
        if m.hand_team != team_id {
            m.hand_team = team_id;
            m.hand = (0, 0);
        }
        (m.role_sent == team_id, m.hand, m.gone_since_ms, m.leave)
    };
    if first {
        let names: Vec<String> = ctx
            .state
            .team()
            .map(|t| t.remote.iter().map(|r| r.name.to_string()).collect())
            .unwrap_or_default();
        let lead = ctx
            .state
            .team()
            .and_then(|t| leader.and_then(|l| t.remote_of(l)))
            .map_or("?", |r| &*r.name)
            .to_owned();
        ctx.log.say(format!(
            "party: in a party of {} ({}), led by {lead}",
            names.len(),
            names.join(", "),
        ));
    }
    let plan = upkeep::plan(&upkeep::Facts {
        now_ms: now,
        me,
        my_level: ctx.state.me.level,
        owner,
        members: &members,
        role_sent,
        hand,
        gone_since_ms,
        leave_asked,
        max_gap: ctx.cfg.party.max_level_gap,
    });
    mem_of(ctx.state).gone_since_ms = plan.gone_since_ms;
    if plan.role {
        mem_of(ctx.state).role_sent = team_id;
        let role = role_of(ctx.persona.archetype.role);
        ctx.send(cs::set_team_member_role(team_id, me, role));
        ctx.log.debug(format!("party: role {role}"));
    }
    if let Some(target) = plan.hand_over {
        let m = mem_of(ctx.state);
        m.hand = (m.hand.0 + 1, now.max(1));
        ctx.send(cs::make_team_owner(team_id, target));
        ctx.log.say("party: handing the lead to the player");
    }
    if let Some(why) = plan.leave {
        leave_team(ctx, team_id, why, running);
    }
}

/// The party role of a class: tank, healer, or attacker.
pub fn role_of(role: crate::archetype::Role) -> u8 {
    use crate::archetype::Role;
    match role {
        Role::Tank => cs::team_role::TANK,
        Role::Healer => cs::team_role::HEALER,
        Role::Melee | Role::Ranged | Role::Caster => cs::team_role::ATTACKER,
    }
}

fn leave_team(ctx: &mut Ctx<'_>, team_id: u32, why: Leave, running: Option<GoalKind>) {
    let now = ctx.now_ms;
    let me = ctx.state.me.char_id;
    let m = mem_of(ctx.state);
    if m.leave_sent_ms != 0 && now.saturating_sub(m.leave_sent_ms) < LEAVE_GAP_MS {
        return;
    }
    let first = m.leave_sent_ms == 0;
    m.leave_sent_ms = now.max(1);
    m.leave = true;
    m.left_why = Some(why);
    ctx.send(cs::ask_risky_team_action(team_id, me, cs::RISKY_LEAVE));
    if first {
        ctx.log
            .say(format!("party: leaving the party ({})", why.word()));
        if why == Leave::LevelGap {
            if let Some(name) = leader_char(ctx)
                .and_then(|l| ctx.state.team().and_then(|t| t.remote_of(l)))
                .map(|r| r.name.clone())
            {
                let due = now + jitter(ctx, REPLY_PAUSE_MS);
                let text = command::line(Say::LevelGap, 0, "");
                mem_of(ctx.state).reply(due, &name, text);
            }
        }
    }
    let _ = running;
}

// ---- whispers --------------------------------------------------------------------------------

/// Sends the whispers whose time has come.
fn send_replies(ctx: &mut Ctx<'_>) {
    let now = ctx.now_ms;
    let due = match ctx
        .state
        .party
        .as_deref()
        .and_then(|p| p.companion.as_deref())
    {
        Some(m) => m.replies.first().is_some_and(|r| now >= r.due_ms),
        None => false,
    };
    if !due {
        return;
    }
    let reply = mem_of(ctx.state).replies.remove(0);
    if let Some(p) = whisper_packet(&reply.to, &reply.text) {
        ctx.send(p);
        let t = unix_ms();
        let note = format!("(me to {}) {}", reply.to, reply.text);
        ctx.log.with_slot(|g| g.social.push_chat(t, &note));
        ctx.log.say(format!("chat: {note}"));
    }
}

pub fn say_to(ctx: &mut Ctx<'_>, to: &str, say: Say, arg: &str) {
    let pick = ctx.rng.usize(..8);
    let due = ctx.now_ms + jitter(ctx, REPLY_PAUSE_MS);
    let text = command::line(say, pick, arg);
    mem_of(ctx.state).reply(due, to, text);
}

// ---- chat commands ---------------------------------------------------------------------------

/// Reads the chat lines before the social duty files them. Returns the names of the players whose
/// whisper this feature answers, so the social duty leaves them alone.
pub fn forward_chat(ctx: &mut Ctx<'_>, lines: &[ChatIn]) -> SmallVec<[Box<str>; 2]> {
    let mut claimed: SmallVec<[Box<str>; 2]> = SmallVec::new();
    if !ctx.cfg.party.chat_commands {
        return claimed;
    }
    for l in lines {
        if !matches!(
            l.kind,
            chat_type::WHISPER | chat_type::SAY | chat_type::PARTY
        ) {
            continue;
        }
        let who = strip_gm(&l.name);
        // (our own bots do not command each other)
        if who.is_empty() || ctx.hub.chat.names.contains(who) {
            continue;
        }
        if l.kind == chat_type::WHISPER && remember_preference(ctx, who, &l.text) {
            claimed.push(who.into());
            continue;
        }
        if l.kind == chat_type::WHISPER && ctx.cfg.party.trade_escorts {
            if let Some((sender, true)) = member_named(ctx, who) {
                if !ctx.hub.is_bot_char(sender) && super::escort::request(ctx, who, sender, &l.text)
                {
                    claimed.push(who.into());
                    continue;
                }
            }
        }
        let Some(cmd) = command::parse(&l.text) else {
            continue;
        };
        if handle(ctx, who, l.kind == chat_type::WHISPER, cmd) {
            claimed.push(who.into());
        }
    }
    claimed
}

fn remember_preference(ctx: &mut Ctx<'_>, who: &str, text: &str) -> bool {
    let request = text.trim().to_ascii_lowercase();
    let preference = if request == "forget preference" {
        None
    } else if let Some(activity) = request.strip_prefix("remember ") {
        let Some(preference) = Preference::parse(activity) else {
            return false;
        };
        Some(preference)
    } else {
        return false;
    };
    let Some((player, _)) = member_named(ctx, who) else {
        return false;
    };
    if ctx.hub.is_bot_char(player) {
        return false;
    }
    ctx.hub.journal.update(ctx.persona.id, |j| {
        j.relationships
            .update(player, |r| r.preference = preference);
    });
    let text = match preference {
        Some(p) => format!("I'll remember you like {}.", p.label()),
        None => "I've forgotten your activity preference.".to_owned(),
    };
    let due = ctx.now_ms + jitter(ctx, REPLY_PAUSE_MS);
    mem_of(ctx.state).reply(due, who, text);
    true
}

/// The member of the bot's team called `who`: character id and whether it leads.
fn member_named(ctx: &Ctx<'_>, who: &str) -> Option<(u32, bool)> {
    let t = team_of(ctx)?;
    let me = ctx.state.me.char_id;
    let r = t
        .remote
        .iter()
        .find(|r| r.char_id != me && r.name.eq_ignore_ascii_case(who))?;
    let leader = leader_char(ctx) == Some(r.char_id);
    Some((r.char_id, leader))
}

/// Runs one command. Returns whether the whisper is answered here.
fn handle(ctx: &mut Ctx<'_>, who: &str, whisper: bool, cmd: Command) -> bool {
    let now = ctx.now_ms;
    if cmd == Command::Invite {
        return whisper && invite_request(ctx, who);
    }
    // everything else only from a member of the bot's party
    let Some((sender, leads)) = member_named(ctx, who) else {
        return false;
    };
    if ctx.hub.is_bot_char(sender) {
        return false;
    }
    let reply = |ctx: &mut Ctx<'_>, say: Say, arg: &str| {
        if whisper {
            say_to(ctx, who, say, arg);
        }
    };
    match cmd {
        Command::Follow => {
            let m = mem_of(ctx.state);
            m.mode = Mode::Follow;
            m.come = 0;
            m.dungeon = None;
            reply(ctx, Say::Following, "");
            ctx.log.say(format!("party: {who} says follow"));
        }
        Command::Stay => {
            let m = mem_of(ctx.state);
            m.escort = None;
            m.mode = Mode::Stay;
            m.come = 0;
            m.dungeon = None;
            reply(ctx, Say::Staying, "");
            ctx.log.say(format!("party: {who} says stay"));
        }
        Command::Come => {
            let m = mem_of(ctx.state);
            m.come = sender;
            m.dungeon = None;
            reply(ctx, Say::Coming, "");
            ctx.log.say(format!("party: {who} says come"));
        }
        Command::Assist => {
            let has_target = team_of(ctx).is_some_and(|t| {
                let obj = t.remote_of(sender).map_or(0, |r| r.obj_id);
                t.member_targets_at(now).any(|(m, _)| m == obj)
            });
            mem_of(ctx.state).assist_until_ms = now + ASSIST_MS;
            reply(
                ctx,
                if has_target {
                    Say::Assisting
                } else {
                    Say::NoTarget
                },
                "",
            );
            ctx.log.say(format!("party: {who} says assist"));
        }
        Command::Leave => {
            if !leads {
                reply(ctx, Say::NotYours, "");
                return whisper;
            }
            mem_of(ctx.state).leave = true;
            reply(ctx, Say::Leaving, "");
            ctx.log.say(format!("party: {who} says leave"));
        }
        Command::Dungeon(query) => {
            if !leads {
                reply(ctx, Say::NotYours, "");
                return whisper;
            }
            mem_of(ctx.state).escort = None;
            dungeon_request(ctx, who, whisper, &query);
        }
        Command::Invite => {}
    }
    whisper
}

/// `dungeon <name>` from the leader.
fn dungeon_request(ctx: &mut Ctx<'_>, who: &str, whisper: bool, query: &str) {
    let found = {
        let names = ctx
            .world
            .dungeons()
            .iter()
            .enumerate()
            .map(|(i, d)| (i, &*d.name));
        command::find_dungeon(names, query)
    };
    let say = |ctx: &mut Ctx<'_>, say: Say, arg: &str| {
        if whisper {
            say_to(ctx, who, say, arg);
        }
    };
    match found {
        Match::None => say(ctx, Say::UnknownDungeon, ""),
        Match::Many => say(ctx, Say::WhichDungeon, ""),
        Match::One(i) => {
            let Some(d) = ctx.world.dungeons().get(i) else {
                return;
            };
            let raise = ctx.gm_allowed() && ctx.cfg.behavior.dungeon_level_up;
            let name = d.name.clone();
            if !crate::bot::goals::dungeon::level_fits(
                ctx.state.me.level,
                d.level_min,
                d.level_max,
                raise,
            ) || d.entrances.is_empty()
            {
                say(ctx, Say::TooLow, &name);
                return;
            }
            let group = d.zone_group;
            let m = mem_of(ctx.state);
            m.dungeon = Some(group);
            m.dungeon_told = false;
            m.come = 0;
            m.mode = Mode::Follow;
            say(ctx, Say::Heading, &name);
            ctx.log
                .say(format!("party: {who} asks for the dungeon {name}"));
        }
    }
}

/// A whisper "inv" from a player: invite them if the policy allows. Returns whether the whisper
/// is answered here.
fn invite_request(ctx: &mut Ctx<'_>, who: &str) -> bool {
    if !ctx.cfg.party.join_player_parties {
        return false;
    }
    let now = ctx.now_ms;
    // a member asking for what they have
    if member_named(ctx, who).is_some() {
        return true;
    }
    let key = name_key(who);
    let m = mem_of(ctx.state);
    m.asked.retain(|a| now.saturating_sub(a.1) < ASK_GAP_MS);
    if m.asked.iter().any(|a| a.0 == key) {
        // (asked a moment ago: the answer is on its way)
        return true;
    }
    if m.asked.len() >= 3 {
        m.asked.remove(0);
    }
    m.asked.push((key, now));
    // a bot in a party it does not lead cannot invite: it names the leader
    if let Some(t) = team_of(ctx) {
        let me = ctx.state.me.char_id;
        if t.owner != me {
            let leader = t
                .owner_remote()
                .map_or_else(|| "the leader".to_owned(), |r| r.name.to_string());
            say_to(ctx, who, Say::AskLeader, &leader);
            return true;
        }
        if t.size() >= crate::bot::party::MAX_PARTY {
            say_to(ctx, who, Say::Full, "");
            return true;
        }
    }
    let running = None;
    let other = ctx
        .state
        .units
        .values()
        .find(|u| {
            u.kind == UnitKind::Character
                && u.name
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(who))
        })
        .map_or(0, |u| u.id);
    let mut f = facts(ctx, running, other);
    // (the bot judges itself as the invitee would: only a bot in no team passes)
    f.in_team = false;
    // (a bot inside a dungeon may invite: that is how a player joins the run it leads)
    f.away = false;
    match policy::judge(&f).and_then(|()| check_trust(ctx, other)) {
        Ok(()) => {
            let team_id = team_of(ctx).map_or(0, |t| t.team_id);
            ctx.send(cs::invite_to_team(team_id, true, who));
            mem_of(ctx.state).inviting = Some((who.into(), now));
            say_to(ctx, who, Say::InviteSent, "");
            ctx.log.say(format!("party: invited {who} (asked for it)"));
        }
        Err(why) => {
            let say = match why {
                Why::LevelGap => Say::LevelGap,
                Why::TooMany | Why::FullParty => Say::Full,
                _ => Say::Sorry,
            };
            say_to(ctx, who, say, "");
            ctx.log
                .say(format!("party: did not invite {who}: {}", why.word()));
        }
    }
    true
}

// ---- the web snapshot ------------------------------------------------------------------------

/// The party of the bot as the web snapshot shows it, rebuilt when the team changed. `None` when
/// nothing changed since the last call, `Some(None)` when the bot left its party.
pub fn snapshot_if_changed(
    party: &mut Option<Box<PartyMem>>,
    is_bot: impl Fn(u32) -> bool,
    me: u32,
) -> Option<Option<Box<pb_web::PartyInfo>>> {
    let pm = party.as_deref_mut()?;
    let own_mode = pm.companion.as_deref().map_or(Mode::Follow, |m| m.mode);
    let team = &pm.team;
    let key = if team.in_team() {
        let roles = team.remote.iter().fold(0u32, |a, r| {
            a.wrapping_mul(7)
                .wrapping_add(u32::from(r.role) * 2 + u32::from(r.online))
        });
        (
            team.team_id,
            team.changes ^ team.owner,
            roles,
            u8::from(own_mode == Mode::Stay) + 1,
        )
    } else {
        (0, 0, 0, 0)
    };
    let last = pm.companion.as_deref().map_or((0, 0, 0, 0), |m| m.web_key);
    if key == last {
        return None;
    }
    let info = team.in_team().then(|| {
        let members = team
            .remote
            .iter()
            .map(|r| pb_web::PartyMember {
                name: r.name.to_string(),
                level: r.level,
                role: match r.role {
                    1 => "tank",
                    2 => "healer",
                    3 => "attacker",
                    _ => "",
                }
                .to_owned(),
                leader: r.char_id == team.owner,
                online: r.online,
                bot: is_bot(r.char_id) || r.char_id == me,
            })
            .collect();
        Box::new(pb_web::PartyInfo {
            id: team.team_id,
            leader: team
                .owner_remote()
                .map(|r| r.name.to_string())
                .unwrap_or_default(),
            mode: match own_mode {
                Mode::Follow => "follow",
                Mode::Stay => "stay",
            }
            .to_owned(),
            members,
        })
    });
    pm.companion.get_or_insert_with(Default::default).web_key = key;
    Some(info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aa_proto::sc_activity::{ActivityEvent, TeamMemberBrief};

    fn brief(char_id: u32, name: &str, level: u8, obj_id: u32) -> TeamMemberBrief {
        TeamMemberBrief {
            char_id,
            name: name.into(),
            level,
            role: 0,
            obj_id,
        }
    }

    fn join(party: &mut Option<Box<PartyMem>>, owner: u32, members: Vec<TeamMemberBrief>) {
        crate::bot::party::observe(
            party,
            &ActivityEvent::JoinedTeam {
                team_id: 9,
                owner_id: owner,
                is_party: true,
                members: members.into(),
            },
            1_000,
        );
    }

    const ME: u32 = 10;
    const PLAYER: u32 = 20;
    const BOT: u32 = 30;

    fn party() -> Option<Box<PartyMem>> {
        let mut p = None;
        join(
            &mut p,
            PLAYER,
            vec![
                brief(PLAYER, "Lead", 30, 200),
                brief(ME, "Me", 28, 100),
                brief(BOT, "Other", 29, 300),
            ],
        );
        p
    }

    #[test]
    fn the_web_party_is_rebuilt_only_when_the_team_changed() {
        let is_bot = |c: u32| c == BOT;
        let mut p = party();
        let first = snapshot_if_changed(&mut p, is_bot, ME)
            .expect("a new party")
            .expect("in a party");
        assert_eq!(
            (first.id, first.leader.as_str(), first.mode.as_str()),
            (9, "Lead", "follow")
        );
        let names: Vec<(&str, bool, bool)> = first
            .members
            .iter()
            .map(|m| (m.name.as_str(), m.leader, m.bot))
            .collect();
        // (the bot itself and the other bot of this process are bots, the player is not)
        assert_eq!(
            names,
            [
                ("Lead", true, false),
                ("Me", false, true),
                ("Other", false, true)
            ]
        );
        assert!(
            snapshot_if_changed(&mut p, is_bot, ME).is_none(),
            "nothing changed"
        );
        // a role announcement changes the key, so does the follow mode
        crate::bot::party::observe(
            &mut p,
            &ActivityEvent::TeamMemberRoleChanged {
                team_id: 9,
                char_id: ME,
                role: 2,
            },
            2_000,
        );
        let again = snapshot_if_changed(&mut p, is_bot, ME).unwrap().unwrap();
        assert_eq!(again.members[1].role, "healer");
        p.as_deref_mut()
            .unwrap()
            .companion
            .get_or_insert_with(Default::default)
            .mode = Mode::Stay;
        let stay = snapshot_if_changed(&mut p, is_bot, ME).unwrap().unwrap();
        assert_eq!(stay.mode, "stay");
        // leaving the party clears it, once
        crate::bot::party::observe(
            &mut p,
            &ActivityEvent::LeftTeam {
                team_id: 9,
                kicked: false,
                dismissed: true,
            },
            3_000,
        );
        assert_eq!(snapshot_if_changed(&mut p, is_bot, ME), Some(None));
        assert!(snapshot_if_changed(&mut p, is_bot, ME).is_none());
        // a bot that never met a team has nothing to show
        assert!(snapshot_if_changed(&mut None, is_bot, ME).is_none());
    }

    #[test]
    fn a_companion_memory_is_made_only_when_something_needs_it() {
        let mut p = party();
        assert!(p.as_deref().unwrap().companion.is_none());
        let mut st = BotState::default();
        assert!(peek(&st).is_none());
        assert!(!holding(&st));
        mem_of(&mut st).mode = Mode::Stay;
        assert!(holding(&st));
        assert!(peek(&st).is_some());
        // the snapshot reads the mode without making a memory
        let _ = snapshot_if_changed(&mut p, |_| false, ME);
        assert!(
            p.as_deref().unwrap().companion.is_some(),
            "it keeps its key there"
        );
    }

    #[test]
    fn a_name_is_remembered_by_its_hash_whatever_the_case() {
        assert_eq!(name_key("Wezukleix"), name_key("  wezukleix "));
        assert_ne!(name_key("Wezukleix"), name_key("Deitroth"));
    }

    #[test]
    fn a_bot_is_a_tank_a_healer_or_an_attacker() {
        use crate::archetype::Role;
        for (role, want) in [
            (Role::Tank, cs::team_role::TANK),
            (Role::Healer, cs::team_role::HEALER),
            (Role::Melee, cs::team_role::ATTACKER),
            (Role::Ranged, cs::team_role::ATTACKER),
            (Role::Caster, cs::team_role::ATTACKER),
        ] {
            assert_eq!(role_of(role), want, "{role:?}");
        }
    }
}
