//! The guild duty: what a bot does for its guild whatever goal it is running.
//!
//! [`duty`] runs on every brain tick (cheaply: every part has its own clock in
//! [`Timers`](super::life::Timers)):
//!
//! * **Card.** Tells the [`GuildBook`](super::book::GuildBook) who the bot is every few seconds,
//!   and which guild it belongs to when that changes.
//! * **Invitations.** A guild invitation is answered after a human pause: from a bot of a bot
//!   guild with "join", from a real player only with `[guild] accept_player_invites`; a bot in a
//!   founding team declines everything.
//! * **Guild life** (see [`life`](super::life)): an officer recruits an unguilded bot of its
//!   faction now and then, the owner makes one or two officers, a newcomer is welcomed and says
//!   thanks, now and then a line of small talk, a real player's guild chat line gets an answer,
//!   and with `[guild] churn_per_hour` a member leaves (an owner hands the guild to another bot
//!   first).
//! * **Requests.** [`forward_chat`] reads the whispers of real players: "ginv", "guild invite",
//!   "can i join" make a bot officer invite the player (a member that cannot invite says whom to
//!   ask); the whisper is then not answered by the social duty as well.
//!
//! Every guild line goes through the limits of the book (per guild and for all guilds together)
//! and the `[social]` switches; the packets are the builders of [`aa_proto::guild`] and the chat
//! builders that refuse a line that could be taken for a command.

use aa_proto::cs::{self, chat_type};
use aa_proto::guild as wire;
use smallvec::SmallVec;

use super::book::{Card, Membership, CAN_FOUND, CAN_JOIN, WISHING};
use super::chat::{self, Kind};
use super::life::{
    self, Due, Heard, Leave, Reply, Request, View, ANSWER_DELAY_MS, CHURN_EVERY_MS, INVITE_GAP_MS,
    INVITE_STALE_MS, JOIN_WAIT_MS, LEAVE_WAIT_MS, LIFE_EVERY_MS, PROMOTE_EVERY_MS, PROMOTE_WAIT_MS,
    RECRUIT_EVERY_MS, REJOIN_PAUSE_MS,
};
use super::plan::{GuildWish, WISH_TTL};
use super::profession::{self, Profession};
use super::report::{self, Problem};
use crate::bot::cmd::GoalKind;
use crate::bot::ctx::Ctx;
use crate::bot::goals::social::canned;
use crate::bot::state::ChatIn;
use crate::hub::unix_ms;

/// The card is refreshed this often.
const CARD_EVERY_MS: u64 = 5_000;
/// The bots wait this long after the first card before they found a guild: the bots that are
/// logging in report their guilds first, so `max_guilds` counts them.
pub const FOUND_GRACE_MS: u64 = 90_000;
/// All guilds together say at most this many lines a minute.
const LINES_PER_MIN: u32 = 8;
/// A real player who asked for an invitation is answered once in this long.
const ASK_COOLDOWN_MS: u64 = 2 * 60_000;
/// Longest wait for the answer to a request for an invitation.
const REQUEST_DELAY_MS: (u64, u64) = (1_500, 4_000);
/// Small talk comes this often per bot (random in the range).
const TALK_EVERY_MS: (u64, u64) = (5 * 60_000, 12 * 60_000);
/// After a hand-over or leave the server ignored, the bot does not try again for this long.
const CHURN_BLOCK_MS: u64 = 10 * 60_000;
/// A promotion that did not show is not tried again for this long.
const PROMOTE_BLOCK_MS: u64 = 5 * 60_000;

// ---- what makes a bot able to join or to found -------------------------------------------------

/// What the card and the wish look at.
#[derive(Debug, Clone, Copy, Default)]
pub struct Facts {
    /// `[guild] enabled`.
    pub enabled: bool,
    /// `[guild] found`.
    pub found: bool,
    /// `[guild] max_guilds`.
    pub max_guilds: u32,
    /// `[guild] min_level`.
    pub min_level: u8,
    /// The bot's level.
    pub level: u8,
    /// In the world and alive.
    pub active: bool,
    /// In an instance or loading one.
    pub away: bool,
    /// In a guild.
    pub in_guild: bool,
    /// In a team (a party).
    pub in_team: bool,
    /// The goal that runs is one that must not be broken off for a guild.
    pub busy: bool,
    /// The bot is in a founding team already.
    pub founding: bool,
    /// The bot can pay the charter (it has the copper or may mint it).
    pub can_pay: bool,
    /// A founding failed lately and the bot rests.
    pub blocked: bool,
    /// The bot left a guild a moment ago.
    pub resting: bool,
    /// The bots have been reporting long enough.
    pub grace_over: bool,
    /// The book has room for one more guild.
    pub room: bool,
}

/// Whether the bot can accept an invitation now.
pub fn can_join(f: &Facts) -> bool {
    f.enabled
        && f.active
        && !f.away
        && !f.in_guild
        && !f.founding
        && !f.resting
        && f.level >= f.min_level.max(1)
}

/// Whether the bot can be a member of a founding team now.
pub fn can_found(f: &Facts) -> bool {
    can_join(f)
        && f.found
        && f.max_guilds > 0
        && !f.in_team
        && !f.busy
        && !f.blocked
        && f.can_pay
        && f.level >= wire::CREATE_LEVEL
        && f.grace_over
        && f.room
}

/// The facts of this tick. `running` is the goal that runs.
pub fn facts(ctx: &Ctx<'_>, running: Option<GoalKind>) -> Facts {
    let cfg = &ctx.cfg.guild;
    let st = &ctx.state;
    let me = &st.me;
    let now_unix = unix_ms();
    let t = st.guild.get().map(|m| &m.t);
    Facts {
        enabled: cfg.enabled,
        found: cfg.found,
        max_guilds: cfg.max_guilds,
        min_level: cfg.min_level,
        level: me.level,
        active: me.spawned && !me.dead,
        away: st.place.in_instance() || st.place.loading(),
        in_guild: st.guild.in_guild(),
        in_team: st.team().is_some_and(|t| t.in_team()),
        busy: matches!(
            running,
            Some(
                GoalKind::Dungeon
                    | GoalKind::Duel
                    | GoalKind::Trade
                    | GoalKind::Market
                    | GoalKind::Sail
                    | GoalKind::Auction
            )
        ),
        founding: t.is_some_and(|t| t.founding),
        can_pay: me.money >= wire::CREATE_COST || ctx.gm_allowed(),
        blocked: t.is_some_and(|t| ctx.now_ms < t.found_block_until_ms),
        resting: t.is_some_and(|t| ctx.now_ms < t.rejoin_after_ms),
        grace_over: ctx.hub.guilds.age_ms(now_unix) >= FOUND_GRACE_MS,
        room: ctx.hub.guilds.room_for_guild(cfg.max_guilds, now_unix),
    }
}

/// The mother faction of the bot (its own faction when the data lists none).
pub fn mother_of(ctx: &Ctx<'_>) -> u32 {
    let m = ctx.world.mother_faction(ctx.state.me.faction);
    if m != 0 {
        m
    } else {
        ctx.state.me.faction
    }
}

// ---- the duty --------------------------------------------------------------------------------

/// One brain tick of the guild duty. `running` is the goal that runs.
pub fn duty(ctx: &mut Ctx<'_>, running: Option<GoalKind>) {
    if !ctx.cfg.guild.enabled || !ctx.state.me.spawned || ctx.state.me.dead {
        return;
    }
    let _ = ctx.state.guild.mem();
    note_left(ctx);
    publish(ctx, running);
    answer_invitation(ctx);
    check_join(ctx);
    answer_requests(ctx);
    say_due(ctx);
    if ctx.state.guild.in_guild() {
        step_leave(ctx);
        life_tick(ctx);
    }
}

/// Tells the book who the bot is and which guild it is in.
fn publish(ctx: &mut Ctx<'_>, running: Option<GoalKind>) {
    let f = facts(ctx, running);
    let mut flags = 0u8;
    if can_join(&f) {
        flags |= CAN_JOIN;
    }
    if can_found(&f) {
        flags |= CAN_FOUND;
    }
    let now = ctx.now_ms;
    let (bot, char_id) = (ctx.persona.id, ctx.state.me.char_id);
    let (level, mother) = (ctx.state.me.level, mother_of(ctx));
    let name: Box<str> = ctx.state.me.name.clone();
    let mem = ctx.state.guild.mem();
    // (a wish that cannot be filled now, because the limit of guilds is reached or the bot is
    // busy, does not keep the guilds from recruiting the bot)
    let wished =
        mem.t.wished_ms != 0 && now.saturating_sub(mem.t.wished_ms) < WISH_TTL.as_millis() as u64;
    if wished {
        if flags & CAN_FOUND != 0 {
            flags |= WISHING;
        }
    } else if mem.t.wished_ms != 0 {
        mem.t.wished_ms = 0;
    }
    let guild = mem.log.id;
    let key = (guild, flags, level);
    let membership = (mem.log.version != mem.t.book_version).then(|| {
        mem.t.book_version = mem.log.version;
        mem.log.in_guild().then(|| Membership {
            id: mem.log.id,
            name: mem.log.name.clone(),
            mother,
        })
    });
    let due = key != mem.t.card_key || now >= mem.t.next_card_ms;
    if due {
        mem.t.card_key = key;
        mem.t.next_card_ms = now + CARD_EVERY_MS;
    }
    let book = &ctx.hub.guilds;
    if let Some(m) = membership {
        book.note_member(bot, m);
    }
    if due {
        book.update_card(
            Card {
                bot,
                char_id,
                name,
                mother,
                level,
                guild,
                flags,
                seen_ms: 0,
            },
            unix_ms(),
        );
        if profession::of(ctx, guild) == Some(Profession::Thief)
            && !ctx.hub.journal.read(bot, |j| j.thief_history)
        {
            ctx.hub.journal.update(bot, |j| j.thief_history = true);
        }
    }
    post_wish(ctx, &f);
}

/// A bot that can sign a charter tells the coordinator it wants a founding team, whatever it is
/// doing; the coordinator keeps the wish for [`WISH_TTL`] and, when a team forms, orders the bot to
/// the goal `guild` (see [`crate::bot::goals::guild`]). Asked once per wish.
fn post_wish(ctx: &mut Ctx<'_>, f: &Facts) {
    if !can_found(f) {
        return;
    }
    let now = ctx.now_ms;
    let ttl = WISH_TTL.as_millis() as u64;
    let wished = ctx
        .state
        .guild
        .get()
        .is_some_and(|m| m.t.wished_ms != 0 && now.saturating_sub(m.t.wished_ms) < ttl);
    // (the card above was published in the same tick or earlier: the coordinator reads it)
    if wished || ctx.hub.guilds.card(ctx.persona.id, unix_ms()).is_none() {
        return;
    }
    let wish = GuildWish {
        bot: ctx.persona.id,
        mother: mother_of(ctx),
        level: ctx.state.me.level,
        can_pay: f.can_pay,
    };
    if ctx.coord.request_guild(wish) {
        ctx.state.guild.mem().t.wished_ms = now.max(1);
        ctx.log
            .say("guild: wants to found a guild, waiting for a founding team");
    }
}

/// How a bot answers an invitation: `Some(true)` join, `Some(false)` decline.
pub fn policy(
    from_bot: bool,
    founding_team: bool,
    in_guild: bool,
    level: u8,
    min_level: u8,
    accept_player_invites: bool,
) -> bool {
    if in_guild || founding_team {
        return false;
    }
    if from_bot {
        level >= min_level.max(1)
    } else {
        accept_player_invites
    }
}

fn answer_invitation(ctx: &mut Ctx<'_>) {
    let now = ctx.now_ms;
    let (accept_players, min_level) =
        (ctx.cfg.guild.accept_player_invites, ctx.cfg.guild.min_level);
    let level = ctx.state.me.level;
    let bot = ctx.persona.id;
    let profession_allowed = ctx
        .state
        .guild
        .log()
        .and_then(|log| log.invite.as_ref())
        .and_then(|inv| profession::of(ctx, inv.guild))
        .is_none_or(|p| p.accepts(ctx.hub.journal.read(bot, |j| j.thief_history)));
    let Some(mem) = ctx.state.guild.get_mut() else {
        return;
    };
    if let Some(inv) = mem.log.invite.clone() {
        if now.saturating_sub(inv.at_ms) > INVITE_STALE_MS {
            mem.log.invite = None;
            mem.t.reply = None;
        } else if mem
            .t
            .reply
            .is_none_or(|r| r.guild != inv.guild || r.inviter != inv.inviter)
        {
            let from_bot = ctx.hub.is_bot_char(inv.inviter);
            let join = profession_allowed
                && policy(
                    from_bot,
                    mem.t.founding,
                    mem.log.in_guild(),
                    level,
                    min_level,
                    accept_players,
                );
            let due_ms = now + life::jitter(ctx.rng, ANSWER_DELAY_MS);
            mem.t.reply = Some(Reply {
                due_ms,
                join,
                guild: inv.guild,
                inviter: inv.inviter,
            });
            ctx.log.say(format!(
                "guild: {} invites me to {} ({}), will {}",
                inv.inviter_name,
                inv.guild_name,
                if from_bot { "a bot" } else { "a player" },
                if join { "join" } else { "decline" }
            ));
        }
    }
    let Some(r) = mem.t.reply else {
        return;
    };
    if now < r.due_ms {
        return;
    }
    mem.t.reply = None;
    mem.log.invite = None;
    let join = r.join && profession_allowed && !mem.log.in_guild() && !mem.t.founding;
    ctx.out.push(wire::reply_invite(r.guild, r.inviter, join));
    if join {
        mem.t.join_sent = Some((r.guild, now.max(1)));
        mem.t.wished_ms = 0;
    }
    ctx.hub.guilds.unclaim(bot);
}

/// After a "join" the guild must follow; if it does, the bot says thanks; if not, the server is
/// reported.
fn check_join(ctx: &mut Ctx<'_>) {
    let now = ctx.now_ms;
    let Some(mem) = ctx.state.guild.get_mut() else {
        return;
    };
    let Some((guild, at)) = mem.t.join_sent else {
        return;
    };
    if mem.log.in_guild() {
        mem.t.join_sent = None;
        mem.t.thanked = false;
        let delay = 5_000 + life::jitter(ctx.rng, (0, 8_000));
        mem.t.say = None;
        mem.t.next_talk_ms = now + life::jitter(ctx.rng, TALK_EVERY_MS);
        mem.t.thanks_due = Some(now + delay);
        ctx.log
            .say(format!("guild: joined {} (id {guild})", mem.log.name));
    } else if now.saturating_sub(at) > JOIN_WAIT_MS {
        mem.t.join_sent = None;
        ctx.report_issue(report::build(&Problem::JoinIgnored {
            waited_s: now.saturating_sub(at) / 1000,
        }));
    }
}

// ---- real players ------------------------------------------------------------------------------

/// Reads the chat the social duty is about to file: the guild chat lines of real players (to be
/// answered by one bot) and the whispers that ask for an invitation. Returns the names of the
/// players whose whisper this bot answers, so the social duty leaves them alone.
pub fn forward_chat(ctx: &mut Ctx<'_>, lines: &[ChatIn]) -> SmallVec<[Box<str>; 2]> {
    let mut claimed: SmallVec<[Box<str>; 2]> = SmallVec::new();
    if !ctx.cfg.guild.enabled || !ctx.state.guild.in_guild() {
        return claimed;
    }
    let now = ctx.now_ms;
    let wall = unix_ms();
    for l in lines {
        let who = crate::bot::goals::social::strip_gm(&l.name);
        if who.is_empty() || ctx.hub.chat.names.contains(who) {
            continue;
        }
        match l.kind {
            chat_type::CLAN => {
                ctx.log.with_slot(|g| {
                    g.social
                        .push_chat(wall, &format!("[guild] {}: {}", l.name, l.text));
                });
                if let Some(mem) = ctx.state.guild.get_mut() {
                    if mem.t.heard.len() >= 2 {
                        mem.t.heard.remove(0);
                    }
                    mem.t.heard.push(Heard {
                        who: who.into(),
                        text: l.text.clone(),
                        at_ms: now,
                    });
                }
            }
            chat_type::WHISPER if chat::is_join_request(&l.text) => {
                let key = chat::name_key(who);
                let delay = life::jitter(ctx.rng, REQUEST_DELAY_MS);
                let Some(mem) = ctx.state.guild.get_mut() else {
                    continue;
                };
                mem.t
                    .asked
                    .retain(|a| now.saturating_sub(a.1) < ASK_COOLDOWN_MS);
                if mem.t.asked.iter().any(|a| a.0 == key) {
                    // (asked a moment ago: the answer is on its way, nothing more is said)
                    claimed.push(who.into());
                    continue;
                }
                if mem.t.asked.len() >= 4 {
                    mem.t.asked.remove(0);
                }
                mem.t.asked.push((key, now));
                if mem.t.requests.len() < 2 {
                    mem.t.requests.push(Request {
                        name: who.into(),
                        due_ms: now + delay,
                    });
                }
                claimed.push(who.into());
            }
            _ => {}
        }
    }
    claimed
}

/// Answers the players who asked for an invitation when the time comes.
fn answer_requests(ctx: &mut Ctx<'_>) {
    let now = ctx.now_ms;
    let me = ctx.state.me.char_id;
    let hub = ctx.hub;
    let is_bot = |c: u32| hub.is_bot_char(c);
    let Some(mem) = ctx.state.guild.get_mut() else {
        return;
    };
    if mem.t.requests.first().is_none_or(|r| now < r.due_ms) {
        return;
    }
    let req = mem.t.requests.remove(0);
    let view = View {
        now_ms: now,
        me,
        log: &mem.log,
        is_bot: &is_bot,
    };
    let guild_name = mem.log.name.clone();
    let reply = if !mem.log.in_guild() {
        None
    } else if mem.log.can_invite(me) {
        ctx.out.push(wire::invite(&req.name));
        ctx.log.say(format!(
            "guild: {} asked for an invitation, invited",
            req.name
        ));
        Some(format!(
            "sent you an invitation to {guild_name}, accept it in the window"
        ))
    } else {
        view.inviter_other_than_me().map(|m| {
            format!(
                "i can't invite, but {} can, they are an officer of {guild_name}",
                m.name
            )
        })
    };
    if let Some(text) = reply {
        if let Some(p) = crate::bot::goals::social::whisper_packet(&req.name, &text) {
            ctx.out.push(p);
            let t = unix_ms();
            ctx.log.with_slot(|g| {
                g.social
                    .push_chat(t, &format!("(me to {}) {text}", req.name))
            });
        }
    }
}

// ---- chat --------------------------------------------------------------------------------------

/// Whether a bot may say a line in guild chat: the social switches and an audience.
fn chat_allowed(s: &crate::config::SocialCfg, real_player_near: bool, view: &View<'_>) -> bool {
    s.enabled
        && s.bot_chatter
        && (!s.chatter_needs_player || real_player_near || view.real_member_online())
}

/// Puts a guild chat line on the wire at once (no limits; the caller took them).
fn say_clan(ctx: &mut Ctx<'_>, text: &str) {
    let Some(p) = cs::try_chat(chat_type::CLAN, "", text) else {
        return;
    };
    ctx.out.push(p);
    ctx.log.say(format!("guild chat: (me) {text}"));
    let t = unix_ms();
    ctx.log
        .with_slot(|g| g.social.push_chat(t, &format!("[guild] (me) {text}")));
}

/// Plans a guild chat line `delay_ms` from now, within the limits of the book: at least `gap_ms`
/// since the guild last spoke. Returns whether a line was planned.
fn plan_line(ctx: &mut Ctx<'_>, kind: Kind, who: &str, delay_ms: u64, gap_ms: u64) -> bool {
    let now = ctx.now_ms;
    let (guild, name) = match ctx.state.guild.log() {
        Some(l) if l.in_guild() => (l.id, l.name.clone()),
        _ => return false,
    };
    if ctx.state.guild.get().is_some_and(|m| m.t.say.is_some()) {
        return false;
    }
    if !ctx
        .hub
        .guilds
        .take_line(guild, unix_ms(), gap_ms, LINES_PER_MIN)
    {
        return false;
    }
    let mem = ctx.state.guild.mem();
    let text = chat::line(kind, who, &name, &mut mem.t.recent, ctx.rng);
    mem.t.say = Some(Due {
        due_ms: now + delay_ms,
        text: text.into(),
    });
    true
}

/// Says the planned line when its time has come.
fn say_due(ctx: &mut Ctx<'_>) {
    let now = ctx.now_ms;
    let Some(mem) = ctx.state.guild.get_mut() else {
        return;
    };
    if mem.t.say.as_ref().is_none_or(|d| now < d.due_ms) {
        return;
    }
    let Some(due) = mem.t.say.take() else {
        return;
    };
    if mem.log.in_guild() {
        say_clan(ctx, &due.text);
    }
}

// ---- the life of a member ------------------------------------------------------------------------

/// What one look at the guild decided (plain values, so no borrow of the log is held while the
/// packets are made).
struct Decisions {
    greet: Option<(u32, Box<str>)>,
    welcome: bool,
    promote: Option<u32>,
    promoted_name: Option<Box<str>>,
    recruit: bool,
    may_leave: bool,
    allowed: bool,
    me_owner: bool,
    successor: Option<u32>,
    guild_id: u32,
}

fn life_tick(ctx: &mut Ctx<'_>) {
    let now = ctx.now_ms;
    let me = ctx.state.me.char_id;
    let hub = ctx.hub;
    let cfg = ctx.cfg;
    let real_near = ctx.real_player_near;
    let is_bot = |c: u32| hub.is_bot_char(c);
    let mother = mother_of(ctx);
    let bot = ctx.persona.id;
    let dominant = ctx.persona.temper.dominant();

    // a thank-you owed since the join
    let thanks = match ctx.state.guild.get_mut() {
        Some(mem) if mem.t.thanks_due.is_some_and(|d| now >= d) => {
            mem.t.thanks_due = None;
            let v = View {
                now_ms: now,
                me,
                log: &mem.log,
                is_bot: &is_bot,
            };
            chat_allowed(&cfg.social, real_near, &v)
        }
        _ => false,
    };
    if thanks && ctx.rng.f32() < 0.7 {
        plan_line(ctx, Kind::Thanks, "", 0, 6_000);
    }

    let d = {
        let Some(mem) = ctx.state.guild.get_mut() else {
            return;
        };
        if now < mem.t.next_life_ms {
            return;
        }
        mem.t.next_life_ms = now + life::jitter(ctx.rng, LIFE_EVERY_MS);
        if !mem.log.loaded {
            return;
        }
        // the first look: the small talk starts later
        if mem.t.next_talk_ms == 0 {
            mem.t.next_talk_ms = now + life::jitter(ctx.rng, TALK_EVERY_MS);
        }
        let view = View {
            now_ms: now,
            me,
            log: &mem.log,
            is_bot: &is_bot,
        };
        let me_owner = mem.log.is_owner(me);
        let may_leave = now >= mem.t.next_churn_ms
            && now >= mem.t.churn_block_until_ms
            && mem.t.leave.is_none()
            && view.may_leave();
        let d = Decisions {
            greet: view
                .newcomer_to_greet()
                .map(|m| (m.char_id, m.name.clone())),
            welcome: me_owner
                && !mem.t.welcomed
                && mem.log.founded_ms != 0
                && now.saturating_sub(mem.log.founded_ms) < 120_000
                && mem.log.members.len() >= 2,
            promote: if now >= mem.t.next_promote_ms && mem.t.promote_sent.is_none() {
                view.promotion()
            } else {
                None
            },
            promoted_name: None,
            recruit: now >= mem.t.next_recruit_ms && view.may_recruit(),
            may_leave,
            allowed: chat_allowed(&cfg.social, real_near, &view),
            me_owner,
            successor: if may_leave && me_owner {
                view.successor()
            } else {
                None
            },
            guild_id: mem.log.id,
        };
        let promoted_name = d
            .promote
            .and_then(|c| mem.log.member(c).map(|m| m.name.clone()));
        // an old newcomer nobody greeted is forgotten
        mem.log
            .newcomers
            .retain(|n| now.saturating_sub(n.at_ms) <= life::GREET_UNTIL_MS);
        if d.welcome {
            mem.t.welcomed = true;
        }
        Decisions { promoted_name, ..d }
    };

    // 1. the welcome of the founder
    if d.welcome && d.allowed {
        plan_line(ctx, Kind::Founded, "", 3_000, 5_000);
    }
    // 2. a newcomer is welcomed (the owner always; an officer now and then)
    if let Some((char_id, name)) = &d.greet {
        if d.me_owner || ctx.rng.f32() < 0.35 {
            if let Some(mem) = ctx.state.guild.get_mut() {
                mem.log.newcomers.retain(|n| n.char_id != *char_id);
            }
            if d.allowed {
                plan_line(ctx, Kind::Welcome, name, 1_500, 8_000);
            }
        }
    }
    // 3. the owner appoints an officer
    if let Some(target) = d.promote {
        let mem = ctx.state.guild.mem();
        mem.t.next_promote_ms = now + PROMOTE_EVERY_MS;
        mem.t.promote_sent = Some((target, now.max(1)));
        ctx.out
            .push(wire::set_member_role(wire::role::OFFICER, target));
        let name = d.promoted_name.as_deref().unwrap_or("");
        ctx.log.say(format!(
            "guild: making {} an officer",
            if name.is_empty() { "a member" } else { name }
        ));
        if d.allowed {
            plan_line(ctx, Kind::Promoted, name, 2_500, 8_000);
        }
    }
    // 4. an officer recruits
    if d.recruit {
        ctx.state.guild.mem().t.next_recruit_ms = now + life::jitter(ctx.rng, RECRUIT_EVERY_MS);
        let wall = unix_ms();
        let book = &ctx.hub.guilds;
        let floor = cfg.guild.min_level.max(1);
        if let Some(card) = book.pick_recruit(mother, floor, bot, wall, ctx.rng) {
            // (the guild invites at a pace of its own; a bot it cannot invite yet is let go)
            if book.take_invite_slot(d.guild_id, wall, INVITE_GAP_MS) {
                ctx.out.push(wire::invite(&card.name));
                ctx.log.say(format!("guild: invited {}", card.name));
            } else {
                book.unclaim(card.bot);
            }
        }
    }
    // 5. the chance to leave
    if d.may_leave {
        ctx.state.guild.mem().t.next_churn_ms = now + CHURN_EVERY_MS;
        let churn = cfg.guild.churn_per_hour;
        if churn > 0.0 && ctx.rng.f32() < churn / 60.0 {
            start_leave(ctx, d.successor);
        }
    }
    // 6. a real player's line in guild chat is answered by one bot
    answer_heard(ctx, dominant, d.allowed);
    // 7. small talk
    let talk = {
        let mem = ctx.state.guild.mem();
        let due = now >= mem.t.next_talk_ms && mem.t.next_talk_ms != 0;
        if due {
            mem.t.next_talk_ms = now + life::jitter(ctx.rng, TALK_EVERY_MS);
        }
        due && mem.log.members.len() >= 2
    };
    if talk && d.allowed && ctx.rng.f32() < 0.6 {
        plan_line(ctx, Kind::Talk, "", 1_000, 120_000);
    }
    // the appointment shows (or does not)
    check_promotion(ctx);
}

/// A real player spoke in guild chat: one bot of the guild answers with a canned line.
fn answer_heard(ctx: &mut Ctx<'_>, dominant: &'static str, allowed: bool) {
    let now = ctx.now_ms;
    let Some(mem) = ctx.state.guild.get_mut() else {
        return;
    };
    let Some(h) = mem.t.heard.pop() else {
        return;
    };
    mem.t.heard.clear();
    if !allowed || now.saturating_sub(h.at_ms) > 20_000 || mem.t.say.is_some() {
        return;
    }
    let key = chat::name_key(&format!("{}\n{}", h.who, h.text));
    let gate = &ctx.hub.chat.gate;
    if !gate.claim_line(key, ctx.now.into_std())
        || !gate.take_reply(ctx.now.into_std(), ctx.cfg.social.replies_per_min)
    {
        return;
    }
    let intent = canned::classify(&h.text);
    let who = crate::bot::goals::social::reply::flat_name(&h.who);
    let who = if who == "someone" { String::new() } else { who };
    let text = canned::reply(intent, dominant, &who, &mut mem.t.reply_recent, ctx.rng);
    mem.t.say = Some(Due {
        due_ms: now + 2_000 + life::jitter(ctx.rng, (0, 3_000)),
        text: text.into(),
    });
}

/// The owner's appointment: seen in the member list, or reported.
fn check_promotion(ctx: &mut Ctx<'_>) {
    let now = ctx.now_ms;
    let Some(mem) = ctx.state.guild.get_mut() else {
        return;
    };
    let Some((target, at)) = mem.t.promote_sent else {
        return;
    };
    let done = mem
        .log
        .role_of(target)
        .is_none_or(|r| r >= wire::role::OFFICER);
    if done {
        mem.t.promote_sent = None;
    } else if now.saturating_sub(at) > PROMOTE_WAIT_MS {
        mem.t.promote_sent = None;
        mem.t.next_promote_ms = now + PROMOTE_BLOCK_MS;
        ctx.report_issue(report::build(&Problem::PromoteIgnored {
            role: wire::role::OFFICER,
        }));
    }
}

// ---- leaving ------------------------------------------------------------------------------------

/// Starts leaving the guild: an owner hands the guild to `successor` first (and stays when it has
/// none).
fn start_leave(ctx: &mut Ctx<'_>, successor: Option<u32>) {
    let now = ctx.now_ms;
    let me = ctx.state.me.char_id;
    let Some(mem) = ctx.state.guild.get_mut() else {
        return;
    };
    if mem.log.is_owner(me) {
        let Some(to) = successor else {
            return;
        };
        let name = mem
            .log
            .member(to)
            .map(|m| m.name.clone())
            .unwrap_or_default();
        mem.t.leave = Some(Leave::Handover {
            to,
            sent_ms: now.max(1),
        });
        ctx.out.push(wire::change_owner(to));
        ctx.log
            .say(format!("guild: handing the guild to {name} before I leave"));
    } else {
        let guild = mem.log.id;
        mem.t.leave = Some(Leave::Leaving {
            sent_ms: now.max(1),
            guild,
        });
        farewell_and_leave(ctx);
    }
}

fn farewell_and_leave(ctx: &mut Ctx<'_>) {
    let allowed = {
        let (cfg, real_near) = (ctx.cfg, ctx.real_player_near);
        let me = ctx.state.me.char_id;
        let hub = ctx.hub;
        let is_bot = |c: u32| hub.is_bot_char(c);
        match ctx.state.guild.log() {
            Some(log) => {
                let v = View {
                    now_ms: ctx.now_ms,
                    me,
                    log,
                    is_bot: &is_bot,
                };
                chat_allowed(&cfg.social, real_near, &v)
            }
            None => false,
        }
    };
    if allowed {
        let mem = ctx.state.guild.mem();
        let name = mem.log.name.clone();
        let text = chat::line(Kind::Farewell, "", &name, &mut mem.t.recent, ctx.rng);
        say_clan(ctx, &text);
    }
    ctx.out.push(wire::leave());
    ctx.log.say("guild: leaving the guild");
}

/// Follows a hand-over or a leave under way.
fn step_leave(ctx: &mut Ctx<'_>) {
    let now = ctx.now_ms;
    let me = ctx.state.me.char_id;
    let Some(mem) = ctx.state.guild.get_mut() else {
        return;
    };
    match mem.t.leave {
        Some(Leave::Handover { sent_ms, .. }) => {
            if !mem.log.is_owner(me) && mem.log.owner != 0 {
                let guild = mem.log.id;
                mem.t.leave = Some(Leave::Leaving {
                    sent_ms: now.max(1),
                    guild,
                });
                farewell_and_leave(ctx);
            } else if now.saturating_sub(sent_ms) > LEAVE_WAIT_MS {
                mem.t.leave = None;
                mem.t.churn_block_until_ms = now + CHURN_BLOCK_MS;
                ctx.report_issue(report::build(&Problem::OwnerChangeIgnored));
            }
        }
        Some(Leave::Leaving { sent_ms, guild }) => {
            if !mem.log.in_guild() || mem.log.id != guild {
                mem.t.leave = None;
            } else if now.saturating_sub(sent_ms) > LEAVE_WAIT_MS {
                mem.t.leave = None;
                mem.t.churn_block_until_ms = now + CHURN_BLOCK_MS;
                ctx.report_issue(report::build(&Problem::LeaveIgnored));
            }
        }
        None => {}
    }
}

/// Takes note of a guild the bot lost (it left, was expelled or the guild was dismissed): the
/// bot rests a moment before another invitation, and a hand-over or leave under way is over.
fn note_left(ctx: &mut Ctx<'_>) {
    let now = ctx.now_ms;
    let Some(mem) = ctx.state.guild.get_mut() else {
        return;
    };
    let Some(left) = mem.log.left else {
        return;
    };
    if left.at_ms == mem.t.left_seen_ms {
        return;
    }
    mem.t.left_seen_ms = left.at_ms;
    mem.t.leave = None;
    mem.t.rejoin_after_ms = now + REJOIN_PAUSE_MS;
    let how = match left.how {
        super::log::LeftHow::Left => "left",
        super::log::LeftHow::Expelled => "was expelled from",
        super::log::LeftHow::Dismissed => "lost (dismissed)",
    };
    ctx.log.say(format!("guild: {how} guild {}", left.guild));
}

// ---- the web snapshot ------------------------------------------------------------------------------

/// The guild part of the web snapshot when it changed since the last call (`Some(None)`: the bot
/// left its guild), else `None`.
pub fn snapshot_if_changed(
    state: &mut super::GuildState,
    is_bot: impl Fn(u32) -> bool,
    me: u32,
) -> Option<Option<Box<pb_web::GuildInfo>>> {
    let mem = state.get_mut()?;
    let stamp = mem.log.version.wrapping_add(1);
    if mem.t.web_version == stamp {
        return None;
    }
    mem.t.web_version = stamp;
    let log = &mem.log;
    if !log.in_guild() {
        return Some(None);
    }
    let mut members: Vec<pb_web::GuildMember> = log
        .members
        .iter()
        .map(|m| pb_web::GuildMember {
            name: m.name.to_string(),
            role: wire::role::name(m.role).to_string(),
            level: m.level,
            online: m.online,
            bot: is_bot(m.char_id),
        })
        .collect();
    // the owner first, then by rank and name
    members.sort_by(|a, b| {
        let rank = |m: &pb_web::GuildMember| match m.role.as_str() {
            "General" => 0,
            "Commander" => 1,
            "Officer" => 2,
            "Member" => 3,
            _ => 4,
        };
        rank(a).cmp(&rank(b)).then_with(|| a.name.cmp(&b.name))
    });
    Some(Some(Box::new(pb_web::GuildInfo {
        id: log.id,
        name: log.name.to_string(),
        role: log
            .role_of(me)
            .map_or_else(String::new, |r| wire::role::name(r).to_string()),
        members,
    })))
}

/// Feeds the server's guild list to the book: the names that are taken.
pub fn observe(book: &super::GuildBook, ev: &wire::GuildEvent) {
    match ev {
        wire::GuildEvent::Factions(list) => {
            for f in list.iter() {
                book.note_name(&f.name);
            }
        }
        wire::GuildEvent::Created { faction, .. } => book.note_name(&faction.name),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready() -> Facts {
        Facts {
            enabled: true,
            found: true,
            max_guilds: 3,
            min_level: 5,
            level: 10,
            active: true,
            can_pay: true,
            grace_over: true,
            room: true,
            ..Facts::default()
        }
    }

    #[test]
    fn a_ready_bot_can_join_and_found() {
        assert!(can_join(&ready()));
        assert!(can_found(&ready()));
    }

    #[test]
    fn every_blocker_closes_joining_or_founding() {
        // (what changes, whether joining stays open, whether founding stays open)
        type Case = (fn(&mut Facts), bool, bool);
        let cases: [Case; 14] = [
            (|f| f.enabled = false, false, false),
            (|f| f.active = false, false, false),
            (|f| f.away = true, false, false),
            (|f| f.in_guild = true, false, false),
            (|f| f.founding = true, false, false),
            (|f| f.resting = true, false, false),
            (|f| f.level = 4, false, false),
            (|f| f.found = false, true, false),
            (|f| f.max_guilds = 0, true, false),
            (|f| f.in_team = true, true, false),
            (|f| f.busy = true, true, false),
            (|f| f.blocked = true, true, false),
            (|f| f.can_pay = false, true, false),
            (|f| f.room = false, true, false),
        ];
        for (i, (change, join, found)) in cases.iter().enumerate() {
            let mut f = ready();
            change(&mut f);
            assert_eq!(can_join(&f), *join, "case {i} join");
            assert_eq!(can_found(&f), *found, "case {i} found");
        }
        let mut f = ready();
        f.grace_over = false;
        assert!(can_join(&f) && !can_found(&f));
    }

    #[test]
    fn the_server_minimum_level_holds_even_with_a_lower_configured_one() {
        let mut f = ready();
        f.min_level = 1;
        f.level = 3;
        assert!(can_join(&f), "joining has no server minimum");
        assert!(!can_found(&f), "the charter needs level 5");
    }

    #[test]
    fn invitations_are_answered_by_who_asks() {
        // a bot's invitation is accepted at the minimum level
        assert!(policy(true, false, false, 5, 5, false));
        assert!(!policy(true, false, false, 4, 5, false));
        // a player's only when the switch is on
        assert!(!policy(false, false, false, 30, 5, false));
        assert!(policy(false, false, false, 1, 5, true));
        // a bot in a guild or a founding team declines everything
        assert!(!policy(true, false, true, 30, 5, true));
        assert!(!policy(true, true, false, 30, 5, true));
        assert!(!policy(false, true, false, 30, 5, true));
    }

    fn state_in_a_guild() -> super::super::GuildState {
        use aa_proto::guild::{GuildEvent, MemberBrief};
        let mut s = super::super::GuildState::default();
        s.on_own_guild(5, 1);
        let m = |c: u32, n: &str, role: u8, level: u8| MemberBrief {
            guild: 5,
            char_id: c,
            online: true,
            name: n.into(),
            level,
            role,
        };
        s.on_event(
            &GuildEvent::MemberList {
                total: 3,
                guild: 5,
                members: vec![m(3, "Zed", 1, 12), m(1, "Boss", 255, 30), m(2, "Cy", 2, 20)]
                    .into_boxed_slice(),
            },
            2,
            2,
        );
        s
    }

    #[test]
    fn the_web_snapshot_is_built_when_the_guild_changed() {
        let mut s = state_in_a_guild();
        let first = snapshot_if_changed(&mut s, |c| c < 100, 2).expect("changed");
        let info = first.expect("in a guild");
        assert_eq!((info.id, info.role.as_str()), (5, "Officer"));
        let names: Vec<&str> = info.members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(
            names,
            ["Boss", "Cy", "Zed"],
            "the owner first, then by rank"
        );
        assert!(info.members.iter().all(|m| m.bot));
        // nothing changed: nothing to publish
        assert!(snapshot_if_changed(&mut s, |c| c < 100, 2).is_none());
        // leaving is published once as "no guild"
        s.on_event(
            &aa_proto::guild::GuildEvent::UnitChanged {
                obj: 0,
                char_id: 2,
                kicker: "".into(),
                name: "Cy".into(),
                from: 5,
                to: 0,
                expel: false,
            },
            2,
            3,
        );
        assert_eq!(snapshot_if_changed(&mut s, |c| c < 100, 2), Some(None));
        assert!(snapshot_if_changed(&mut s, |c| c < 100, 2).is_none());
        // a bot that never heard of a guild has nothing to publish and nothing allocated
        let mut none = super::super::GuildState::default();
        assert!(snapshot_if_changed(&mut none, |_| true, 1).is_none());
        assert!(none.get().is_none());
    }

    #[test]
    fn the_server_guild_list_feeds_the_names_of_the_book() {
        use aa_proto::guild::{FactionBrief, GuildEvent};
        let book = super::super::GuildBook::default();
        let f = |id: u32, name: &str| FactionBrief {
            id,
            mother_id: 1,
            name: name.into(),
            owner_id: 1,
            owner_name: "O".into(),
            created: 0,
        };
        observe(
            &book,
            &GuildEvent::Factions(
                vec![f(1, "Iron Pilgrims"), f(2, "Silent Wardens")].into_boxed_slice(),
            ),
        );
        assert!(book.name_taken("iron pilgrims") && book.name_taken("Silent Wardens"));
        observe(
            &book,
            &GuildEvent::Created {
                faction: Box::new(f(3, "Hollow Order")),
                owner_obj: 1,
            },
        );
        assert!(book.name_taken("Hollow Order"));
        assert!(!book.name_taken("Other"));
    }
}
