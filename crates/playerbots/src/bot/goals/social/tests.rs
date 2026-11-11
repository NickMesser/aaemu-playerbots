//! Tests of the chat logic: canned replies, rate limits, the send path, the system-line ring and an
//! end-to-end LLM reply against a local mock endpoint (no external host is ever contacted).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use pb_llm::{Llm, LlmConfig};
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::gate::ChatGate;
use super::*;
use crate::config::PersonalityCfg;

// ---- fakes -----------------------------------------------------------------------------------

#[derive(Default)]
struct Fake {
    bot_chars: FxHashSet<u32>,
    bot_names: FxHashSet<String>,
    llm: Option<Arc<Llm>>,
    gate: ChatGate,
}

impl Services for Fake {
    fn is_bot_char(&self, id: u32) -> bool {
        self.bot_chars.contains(&id)
    }

    fn is_bot_name(&self, name: &str) -> bool {
        self.bot_names.contains(&name.to_lowercase())
    }

    fn llm(&self) -> Option<Arc<Llm>> {
        self.llm.clone()
    }

    fn gate(&self) -> &ChatGate {
        &self.gate
    }
}

const BOT_POS: [f32; 3] = [100.0, 100.0, 10.0];

/// One bot's surroundings for a test.
struct Env {
    units: FxHashMap<u32, Unit>,
    cfg: SocialCfg,
    pcfg: PersonalityCfg,
    style: Style,
    zone: Option<&'static str>,
    rng: fastrand::Rng,
    now_ms: u64,
    now: Instant,
    real_near: bool,
    active: bool,
    dominant: &'static str,
}

impl Env {
    fn new(seed: u64) -> Env {
        Env {
            units: FxHashMap::default(),
            cfg: SocialCfg::default(),
            // (the tests of the replies and the remarks count every line: no ambient or channel
            // lines unless a test asks for them)
            pcfg: PersonalityCfg {
                ambient_per_min: 0,
                channel_lines_per_hour: 0,
                ..PersonalityCfg::default()
            },
            style: Style::NEUTRAL,
            zone: Some("Solzreed"),
            rng: fastrand::Rng::with_seed(seed),
            now_ms: 1_000,
            now: Instant::now(),
            real_near: true,
            active: true,
            dominant: "quester",
        }
    }

    fn with_unit(mut self, obj: u32, kind: UnitKind, id: u32, dist: f32, name: &str) -> Env {
        self.units.insert(
            obj,
            Unit {
                kind,
                id,
                pos: [BOT_POS[0] + dist, BOT_POS[1], BOT_POS[2]],
                hp: 100,
                faction: 0,
                level: 1,
                flags: 0,
                target: 0,
                name: Some(name.into()),
            },
        );
        self
    }

    /// Advances the clocks by `ms`.
    fn advance(&mut self, ms: u64) {
        self.now_ms += ms;
        self.now += Duration::from_millis(ms);
    }

    fn step(&mut self, social: &mut Social, svc: &dyn Services, heard: Vec<ChatIn>) -> Vec<Action> {
        let mut inp = Inputs {
            now_ms: self.now_ms,
            now: self.now,
            active: self.active,
            real_near: self.real_near,
            pos: BOT_POS,
            units: &self.units,
            cfg: &self.cfg,
            pcfg: &self.pcfg,
            style: self.style,
            facts: Facts {
                name: "Aria",
                race: "Nuian",
                class: "Darkrunner",
                dominant: self.dominant,
                zone: self.zone,
            },
            rng: &mut self.rng,
        };
        let mut out = SmallVec::new();
        social.step(&mut inp, svc, heard.into_iter().collect(), &mut out);
        out.into_vec()
    }

    /// Steps every 250 ms for up to `ms` and returns the first actions that come out.
    fn run(&mut self, social: &mut Social, svc: &dyn Services, ms: u64) -> Vec<Action> {
        for _ in 0..ms / 250 {
            self.advance(250);
            let out = self.step(social, svc, Vec::new());
            if !out.is_empty() {
                return out;
            }
        }
        Vec::new()
    }
}

fn line(kind: i16, obj: u32, name: &str, text: &str) -> ChatIn {
    ChatIn {
        kind,
        sender_obj: obj,
        name: name.into(),
        text: text.into(),
        at_ms: 1,
    }
}

fn whisper(name: &str, text: &str) -> ChatIn {
    line(-3, 0, name, text)
}

fn say_from(obj: u32, name: &str, text: &str) -> ChatIn {
    line(0, obj, name, text)
}

fn only_text(actions: &[Action]) -> &str {
    match actions {
        [Action::Say(t)] | [Action::Whisper { text: t, .. }] => t,
        other => panic!("expected one action, got {other:?}"),
    }
}

// ---- canned replies --------------------------------------------------------------------------

#[test]
fn a_whisper_gets_a_canned_whisper_back_after_a_pause() {
    let mut env = Env::new(1);
    let svc = Fake::default();
    let mut social = Social::default();
    let first = env.step(&mut social, &svc, vec![whisper("Bob", "hello Aria!")]);
    assert!(first.is_empty(), "a reply is not instant");
    assert!(social.busy());
    let out = env.run(&mut social, &svc, 6_000);
    match out.as_slice() {
        [Action::Whisper { to, text }] => {
            assert_eq!(to, "Bob");
            assert!(!text.is_empty() && !text.starts_with('/'));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(svc.gate.stats.canned_replies.load(Ordering::Relaxed), 1);
    assert!(!social.busy());
}

#[test]
fn the_gm_prefix_is_stripped_from_the_whisper_target() {
    let mut env = Env::new(2);
    let svc = Fake::default();
    let mut social = Social::default();
    env.step(&mut social, &svc, vec![whisper("GM Nick", "hi there")]);
    let out = env.run(&mut social, &svc, 6_000);
    assert!(
        matches!(&out[..], [Action::Whisper { to, .. }] if to == "Nick"),
        "{out:?}"
    );
    assert_eq!(strip_gm("GM Nick"), "Nick");
    assert_eq!(strip_gm("gm nick"), "nick");
    assert_eq!(strip_gm("Gmork"), "Gmork");
    assert_eq!(strip_gm("  GM  Nick "), "Nick");
}

#[test]
fn a_line_with_the_bots_name_gets_a_local_reply_even_from_far_away() {
    let mut env = Env::new(3).with_unit(7, UnitKind::Character, 700, 90.0, "Bob");
    env.cfg.reply_chance = 0.0;
    let svc = Fake::default();
    let mut social = Social::default();
    env.step(
        &mut social,
        &svc,
        vec![say_from(7, "Bob", "hey aria, nice armor")],
    );
    let out = env.run(&mut social, &svc, 6_000);
    assert!(matches!(&out[..], [Action::Say(_)]), "{out:?}");
}

#[test]
fn an_unaddressed_line_is_answered_only_when_near_and_the_dice_say_so() {
    let svc = Fake::default();
    // near, chance 1: answered
    let mut env = Env::new(4).with_unit(7, UnitKind::Character, 700, 10.0, "Bob");
    env.cfg.reply_chance = 1.0;
    let mut social = Social::default();
    env.step(
        &mut social,
        &svc,
        vec![say_from(7, "Bob", "anyone know where the vendor is?")],
    );
    assert!(!env.run(&mut social, &svc, 6_000).is_empty());
    // too far: never
    let mut env = Env::new(4).with_unit(7, UnitKind::Character, 700, 45.0, "Bob");
    env.cfg.reply_chance = 1.0;
    let mut social = Social::default();
    env.step(
        &mut social,
        &svc,
        vec![say_from(7, "Bob", "anyone know where the vendor is?")],
    );
    assert!(env.run(&mut social, &svc, 6_000).is_empty());
    // chance 0: never
    let mut env = Env::new(4).with_unit(7, UnitKind::Character, 700, 10.0, "Bob");
    env.cfg.reply_chance = 0.0;
    let mut social = Social::default();
    env.step(
        &mut social,
        &svc,
        vec![say_from(7, "Bob", "anyone know where the vendor is?")],
    );
    assert!(env.run(&mut social, &svc, 6_000).is_empty());
    // out of view: cannot be checked, so not answered without the name
    let mut env = Env::new(4);
    env.cfg.reply_chance = 1.0;
    let mut social = Social::default();
    env.step(&mut social, &svc, vec![say_from(9, "Ghost", "hello?")]);
    assert!(env.run(&mut social, &svc, 6_000).is_empty());
}

#[test]
fn our_own_bots_and_npcs_are_not_answered() {
    let mut svc = Fake::default();
    svc.bot_chars.insert(700);
    svc.bot_names.insert("friendbot".into());
    let mut env = Env::new(5)
        .with_unit(7, UnitKind::Character, 700, 5.0, "Friend")
        .with_unit(8, UnitKind::Npc, 12, 5.0, "Guard")
        .with_unit(9, UnitKind::Character, 900, 5.0, "Real");
    env.cfg.reply_chance = 1.0;
    // (no hellos to the bots in view either: this is about answers)
    env.cfg.bot_chatter = false;
    // a bot in view
    let mut social = Social::default();
    env.step(&mut social, &svc, vec![say_from(7, "Friend", "hello Aria")]);
    assert!(env.run(&mut social, &svc, 8_000).is_empty());
    // a bot out of view, known by name (whisper)
    let mut social = Social::default();
    env.step(&mut social, &svc, vec![whisper("FriendBot", "hello")]);
    assert!(env.run(&mut social, &svc, 8_000).is_empty());
    // an NPC that names the bot
    let mut social = Social::default();
    env.step(&mut social, &svc, vec![say_from(8, "Guard", "Aria, halt!")]);
    assert!(env.run(&mut social, &svc, 8_000).is_empty());
    // a real player does get an answer
    let mut social = Social::default();
    env.step(&mut social, &svc, vec![say_from(9, "Real", "hello Aria")]);
    assert!(!env.run(&mut social, &svc, 8_000).is_empty());
    // unless the test switch for talking to our own bots is on
    env.cfg.reply_to_bots = true;
    let mut social = Social::default();
    env.step(&mut social, &svc, vec![say_from(7, "Friend", "hello Aria")]);
    assert!(!env.run(&mut social, &svc, 8_000).is_empty());
}

#[test]
fn nothing_is_said_while_dead_or_switched_off() {
    let svc = Fake::default();
    let mut env = Env::new(6);
    env.active = false;
    let mut social = Social::default();
    env.step(&mut social, &svc, vec![whisper("Bob", "hi")]);
    assert!(env.run(&mut social, &svc, 8_000).is_empty());
    assert!(!social.busy());

    let mut env = Env::new(6);
    env.cfg.enabled = false;
    let mut social = Social::default();
    env.step(&mut social, &svc, vec![whisper("Bob", "hi")]);
    assert!(env.run(&mut social, &svc, 8_000).is_empty());
}

#[test]
fn canned_replies_do_not_repeat_back_to_back() {
    let svc = Fake::default();
    let mut env = Env::new(7);
    env.cfg.replies_per_min = 1000;
    let mut social = Social::default();
    let mut last = String::new();
    for i in 0..12 {
        env.advance(15_000);
        // a different sender each time so the per-conversation quota never applies
        env.step(
            &mut social,
            &svc,
            vec![whisper(&format!("Bob{i}"), "so what do you think")],
        );
        let out = env.run(&mut social, &svc, 6_000);
        let text = only_text(&out).to_string();
        assert_ne!(text, last);
        last = text;
        // keep the burst window clear
        env.advance(121_000);
    }
}

// ---- rate limits -----------------------------------------------------------------------------

#[test]
fn a_bot_waits_between_replies_and_has_a_burst_cap() {
    let svc = Fake::default();
    let mut env = Env::new(8);
    env.cfg.replies_per_min = 1000;
    let mut social = Social::default();
    env.step(&mut social, &svc, vec![whisper("A1", "hi")]);
    assert!(!env.run(&mut social, &svc, 6_000).is_empty());
    // a second whisper right away is not answered
    env.step(&mut social, &svc, vec![whisper("A2", "hello?")]);
    assert!(env.run(&mut social, &svc, 4_000).is_empty());
    // after the gap it is
    env.advance(15_000);
    env.step(&mut social, &svc, vec![whisper("A3", "hello again")]);
    assert!(!env.run(&mut social, &svc, 6_000).is_empty());
    // the burst cap: BURST replies per BURST_SPAN_MS, however patient the other side is
    let mut sent = 2;
    for i in 0..4 {
        env.advance(15_000);
        env.step(&mut social, &svc, vec![whisper(&format!("B{i}"), "hi")]);
        if !env.run(&mut social, &svc, 6_000).is_empty() {
            sent += 1;
        }
    }
    assert_eq!(sent, BURST, "no more than the burst cap within the window");
    // later the window has passed
    env.advance(BURST_SPAN_MS + 1_000);
    env.step(&mut social, &svc, vec![whisper("C", "hi")]);
    assert!(!env.run(&mut social, &svc, 6_000).is_empty());
}

#[test]
fn one_conversation_is_capped() {
    let svc = Fake::default();
    let mut env = Env::new(9);
    env.cfg.replies_per_min = 1000;
    let mut social = Social::default();
    let mut answered = 0;
    for _ in 0..12 {
        // slow enough for the per-bot limits (gap, burst cap); only the quota of the
        // conversation is left to stop a player who never stops talking
        env.advance(30_000);
        env.step(
            &mut social,
            &svc,
            vec![whisper("Chatty", "and another thing")],
        );
        if !env.run(&mut social, &svc, 6_000).is_empty() {
            answered += 1;
        }
    }
    assert_eq!(
        answered,
        memory::REPLIES,
        "of 12 lines in about seven minutes"
    );
}

#[test]
fn all_bots_together_answer_at_most_the_configured_number_per_minute() {
    let svc = Fake::default();
    let mut answered = 0;
    for seed in 0..4 {
        let mut env = Env::new(100 + seed);
        env.cfg.replies_per_min = 2;
        let mut social = Social::default();
        env.step(&mut social, &svc, vec![whisper("Bob", "hello")]);
        if !env.run(&mut social, &svc, 6_000).is_empty() {
            answered += 1;
        }
    }
    assert_eq!(answered, 2);
    // and a limit of zero means silence
    let svc = Fake::default();
    let mut env = Env::new(1);
    env.cfg.replies_per_min = 0;
    let mut social = Social::default();
    env.step(&mut social, &svc, vec![whisper("Bob", "hello")]);
    assert!(env.run(&mut social, &svc, 6_000).is_empty());
}

#[test]
fn of_all_the_bots_that_overhear_a_line_only_one_answers() {
    let svc = Fake::default();
    let mut answered = 0;
    for seed in 0..5 {
        let mut env = Env::new(200 + seed).with_unit(7, UnitKind::Character, 700, 8.0, "Bob");
        env.cfg.reply_chance = 1.0;
        // all of them live at the same wall-clock moment
        let t0 = Instant::now();
        env.now = t0;
        let mut social = Social::default();
        env.step(
            &mut social,
            &svc,
            vec![say_from(7, "Bob", "is anybody here?")],
        );
        if !env.run(&mut social, &svc, 6_000).is_empty() {
            answered += 1;
        }
    }
    assert_eq!(answered, 1);
}

// ---- unsolicited chatter ---------------------------------------------------------------------

fn chatter_with(seed: u64, env_setup: impl Fn(&mut Env)) -> Option<String> {
    let svc = Fake::default();
    let mut env = Env::new(seed);
    env_setup(&mut env);
    let mut social = Social::default();
    social.note(ChatEvent::DuelDone(Some("Bob".into())), env.now_ms);
    env.step(&mut social, &svc, Vec::new());
    let out = env.run(&mut social, &svc, 8_000);
    out.first().map(|a| match a {
        Action::Say(t) => t.clone(),
        other => panic!("chatter is local chat, got {other:?}"),
    })
}

#[test]
fn a_duel_is_followed_by_a_gg_most_of_the_time_but_only_with_an_audience() {
    let said: Vec<String> = (0..100).filter_map(|s| chatter_with(s, |_| {})).collect();
    assert!((40..95).contains(&said.len()), "{} of 100", said.len());
    assert!(said
        .iter()
        .all(|t| { t.contains("gg") || t.contains("well fought") || t.contains("nice one") }));
    assert!(said.iter().any(|t| t.contains("Bob")), "{said:?}");
    // nobody is near: nothing is said
    assert!((0..100).all(|s| chatter_with(s, |e| e.real_near = false).is_none()));
    // switched off
    assert!((0..100).all(|s| chatter_with(s, |e| e.cfg.bot_chatter = false).is_none()));
    // an audience is not needed when the config says so
    assert!((0..100).any(|s| chatter_with(s, |e| {
        e.real_near = false;
        e.cfg.chatter_needs_player = false;
    })
    .is_some()));
}

#[test]
fn chatter_is_limited_per_bot_and_across_bots() {
    // per bot: a second remark inside the gap is not made
    let svc = Fake::default();
    let seed = (0..50)
        .find(|s| chatter_with(*s, |_| {}).is_some())
        .expect("some seed talks");
    let mut env = Env::new(seed);
    let mut social = Social::default();
    social.note(ChatEvent::DuelDone(None), env.now_ms);
    env.step(&mut social, &svc, Vec::new());
    assert_eq!(env.run(&mut social, &svc, 8_000).len(), 1);
    social.note(ChatEvent::TradeDone(None), env.now_ms);
    assert!(
        env.run(&mut social, &svc, 40_000).is_empty(),
        "inside the gap"
    );
    // across bots: at most `chatter_per_min` lines in a minute
    let svc = Fake::default();
    let mut lines = 0;
    for s in 0..60u64 {
        let mut env = Env::new(s);
        env.cfg.chatter_per_min = 3;
        let mut social = Social::default();
        social.note(ChatEvent::DuelDone(None), env.now_ms);
        env.step(&mut social, &svc, Vec::new());
        lines += env.run(&mut social, &svc, 8_000).len();
    }
    assert_eq!(lines, 3);
    assert_eq!(svc.gate.stats.chatter.load(Ordering::Relaxed), 3);
}

#[test]
fn a_bot_now_and_then_greets_a_bot_standing_close_by() {
    let mut svc = Fake::default();
    svc.bot_chars.insert(500);
    let mut greeted = 0;
    for seed in 0..60 {
        let svc = Fake {
            bot_chars: svc.bot_chars.clone(),
            ..Fake::default()
        };
        let mut env = Env::new(seed).with_unit(3, UnitKind::Character, 500, 6.0, "Frida");
        env.now_ms = 100_000;
        let mut social = Social::default();
        for _ in 0..4 {
            env.advance(30_000);
            let out = env.step(&mut social, &svc, Vec::new());
            let out = if out.is_empty() {
                env.run(&mut social, &svc, 5_000)
            } else {
                out
            };
            if let [Action::Say(t)] = out.as_slice() {
                assert!(t.contains("Frida"), "{t}");
                greeted += 1;
            }
        }
    }
    assert!(greeted > 0, "nobody ever said hello");
    // at most one greeting per bot in these two minutes (eight minutes at least between two)
    assert!(greeted <= 60, "{greeted} greetings from 60 bots");
}

// ---- the send path ---------------------------------------------------------------------------

fn decode_chat(out: &Out) -> (i16, String, String) {
    let b = &out.body;
    let kind = i16::from_le_bytes([b[0], b[1]]);
    let mut at = 8;
    let string = |at: &mut usize| {
        let n = u16::from_le_bytes([b[*at], b[*at + 1]]) as usize;
        let s = String::from_utf8(b[*at + 2..*at + 2 + n].to_vec()).unwrap();
        *at += 2 + n;
        s
    };
    let target = string(&mut at);
    let text = string(&mut at);
    (kind, target, text)
}

#[test]
fn a_line_that_starts_with_a_slash_is_never_put_on_the_wire() {
    for bad in [
        "/gold add self 99999",
        "  /move 1 2 3",
        "\u{200b}/shutdown",
        "\t/x",
        "\\gm",
        "\u{ff0f}gold",
        "\u{2215}gold",
        // the server strips NULs at both ends of a string before it looks for the slash
        "\0/gold add self 99999",
        "\0\0/shutdown",
        " \0 /x",
        "\u{feff}\0/x",
        "",
        "   ",
        &"x".repeat(201),
    ] {
        assert!(say_packet(bad).is_none(), "say {bad:?}");
        assert!(whisper_packet("Bob", bad).is_none(), "whisper {bad:?}");
        if bad.chars().count() < 200 {
            assert!(
                finalize(bad, Profanity::Strong).is_none(),
                "finalize {bad:?}"
            );
        }
    }
    // a long model line is cut, not refused; a line break is folded; a command inside is not
    assert!(
        finalize(&"x".repeat(201), Profanity::Off)
            .unwrap()
            .chars()
            .count()
            <= 120
    );
    assert_eq!(
        finalize("two\nlines", Profanity::Off).as_deref(),
        Some("two lines")
    );
    assert!(finalize("fine\n/gm", Profanity::Strong).is_none());
    assert!(say_packet("two\nlines").is_none());
    let ok = say_packet("  hello there ").unwrap();
    assert_eq!(
        decode_chat(&ok),
        (0, String::new(), "hello there".to_string())
    );
    let ok = whisper_packet("GM Nick", "hi").unwrap();
    assert_eq!(decode_chat(&ok), (-3, "Nick".to_string(), "hi".to_string()));
    // the target must be a plain name
    for bad_target in ["", "A B", "/Nick", "Ni\nck", "Ni/ck", &"N".repeat(33)] {
        assert!(whisper_packet(bad_target, "hi").is_none(), "{bad_target:?}");
    }
    // a slash further in is just text
    assert!(say_packet("what is 1/2 of 10?").is_some());
}

#[test]
fn an_unsafe_line_that_reaches_the_send_stage_is_dropped() {
    // Even if something put a command into the queue, flush_due refuses to emit it.
    let svc = Fake::default();
    let mut env = Env::new(1);
    let mut social = Social {
        due: Some(Due {
            at_ms: 0,
            action: Action::Say("/gold add self 5".into()),
            how: Sent::Canned,
            who: "Bob".into(),
            told: 0,
            joke: false,
        }),
        ..Social::default()
    };
    env.advance(10);
    let out = env.step(&mut social, &svc, Vec::new());
    assert!(out.is_empty());
    assert!(!social.busy());
}

// ---- system lines ----------------------------------------------------------------------------

#[test]
fn system_lines_are_routed_to_the_ring_and_not_lost() {
    let mut ring = SlotSocial::default();
    let mut heard = SmallVec::new();
    let lines: SmallVec<[ChatIn; 2]> = [
        line(-2, 0, "", "House construction complete"),
        say_from(7, "Bob", "hi all"),
        line(-1, 0, "", "Server notice"),
        whisper("Bob", "psst"),
        line(1, 7, "Bob", "SHOUT"),
    ]
    .into_iter()
    .collect();
    route_lines(lines, &mut ring, 42, &mut heard);
    // chat goes to the reply logic; shouts are dropped
    assert_eq!(heard.len(), 2);
    assert_eq!(heard[0].kind, 0);
    assert_eq!(heard[1].kind, -3);
    // system and notice lines stay readable, with their arrival time
    let sys = ring.system_since(0);
    assert_eq!(sys.len(), 2);
    assert_eq!(&*sys[0].text, "House construction complete");
    assert_eq!(sys[0].kind, -2);
    assert_eq!(sys[1].kind, -1);
    // the panel shows what was said to the bot
    let chat = ring.chat_lines();
    assert_eq!(chat.len(), 2);
    assert_eq!(chat[0].text, "Bob: hi all");
    assert_eq!(chat[1].text, "Bob (whisper): psst");
    assert_eq!(chat[0].t, 42);
}

// ---- LLM replies (local mock endpoint) -------------------------------------------------------

struct Mock {
    base: String,
    hits: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<String>>>,
}

async fn mock(content: &'static str) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let base = format!("http://{}", listener.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let (h, b) = (hits.clone(), bodies.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let (h, b) = (h.clone(), b.clone());
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let head_end = loop {
                    let n = stream.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break p + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_lowercase();
                let len: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                while buf.len() < head_end + len {
                    let n = stream.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                b.lock()
                    .push(String::from_utf8_lossy(&buf[head_end..]).to_string());
                h.fetch_add(1, Ordering::SeqCst);
                let body = serde_json::json!({
                    "choices": [{"index": 0, "message": {"role": "assistant", "content": content}}],
                    "usage": {"prompt_tokens": 30, "completion_tokens": 10, "total_tokens": 40}
                })
                .to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    Mock { base, hits, bodies }
}

fn llm_for(mock: &Mock, per_minute: u32) -> Arc<Llm> {
    Llm::with_key(
        &LlmConfig {
            enabled: true,
            base_url: mock.base.clone(),
            max_requests_per_minute: per_minute,
            daily_token_budget: 1_000_000,
            timeout_secs: 5,
            ..LlmConfig::default()
        },
        "test-key".into(),
    )
}

/// Steps until an action comes out (the LLM answers on its own task, so real time passes too).
async fn run_async(env: &mut Env, social: &mut Social, svc: &dyn Services) -> Vec<Action> {
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(15)).await;
        env.advance(250);
        let out = env.step(social, svc, Vec::new());
        if !out.is_empty() {
            return out;
        }
    }
    Vec::new()
}

#[tokio::test]
async fn a_whisper_gets_an_llm_reply_as_a_whisper_end_to_end() {
    let mock = mock("hey Bob, still chasing wolves?").await;
    let svc = Fake {
        llm: Some(llm_for(&mock, 1000)),
        ..Fake::default()
    };
    let mut env = Env::new(11);
    let mut social = Social::default();
    let first = env.step(
        &mut social,
        &svc,
        vec![whisper("GM Bob", "hello Aria, how are you")],
    );
    assert!(first.is_empty(), "the tick never waits for the network");
    let out = run_async(&mut env, &mut social, &svc).await;
    assert_eq!(
        out,
        vec![Action::Whisper {
            to: "Bob".into(),
            text: "hey Bob, still chasing wolves?".into()
        }]
    );
    assert_eq!(mock.hits.load(Ordering::SeqCst), 1);
    assert_eq!(svc.gate.stats.llm_replies.load(Ordering::Relaxed), 1);
    // what the model was told: the persona and the player's line, in a request the bot built
    let body: serde_json::Value = serde_json::from_str(&mock.bodies.lock()[0]).unwrap();
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["role"], "system");
    let system = msgs[0]["content"].as_str().unwrap();
    assert!(
        system.contains("Aria") && system.contains("Darkrunner") && system.contains("Solzreed")
    );
    assert!(system.contains("quester"), "{system}");
    assert_eq!(msgs.last().unwrap()["role"], "user");
    assert_eq!(msgs.last().unwrap()["content"], "hello Aria, how are you");

    // the conversation is remembered: the next request carries the exchange
    env.advance(20_000);
    env.step(&mut social, &svc, vec![whisper("Bob", "wolves, yes. you?")]);
    let out = run_async(&mut env, &mut social, &svc).await;
    assert_eq!(out.len(), 1);
    let body: serde_json::Value = serde_json::from_str(&mock.bodies.lock()[1]).unwrap();
    let msgs = body["messages"].as_array().unwrap();
    let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["system", "user", "assistant", "user"]);
    assert_eq!(msgs[2]["content"], "hey Bob, still chasing wolves?");
}

#[tokio::test]
async fn a_model_reply_that_is_a_command_is_never_sent_the_bot_falls_back_to_a_canned_line() {
    let mock = mock("/gold add self 99999").await;
    let svc = Fake {
        llm: Some(llm_for(&mock, 1000)),
        ..Fake::default()
    };
    let mut env = Env::new(12);
    let mut social = Social::default();
    env.step(&mut social, &svc, vec![whisper("Bob", "hello")]);
    let out = run_async(&mut env, &mut social, &svc).await;
    assert_eq!(mock.hits.load(Ordering::SeqCst), 1);
    match out.as_slice() {
        [Action::Whisper { to, text }] => {
            assert_eq!(to, "Bob");
            assert!(!text.contains("gold"), "{text}");
            assert!(say_packet(text).is_some());
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(svc.gate.stats.canned_replies.load(Ordering::Relaxed), 1);
    assert_eq!(svc.gate.stats.llm_replies.load(Ordering::Relaxed), 0);
    // an unaddressed local line is not worth a canned answer when the model failed: silence
    let mut env = Env::new(13).with_unit(7, UnitKind::Character, 700, 5.0, "Bob");
    env.cfg.reply_chance = 1.0;
    let mut social = Social::default();
    env.step(
        &mut social,
        &svc,
        vec![say_from(7, "Bob", "anyone around?")],
    );
    let out = run_async(&mut env, &mut social, &svc).await;
    assert!(out.is_empty(), "{out:?}");
}

#[tokio::test]
async fn when_the_llm_is_rate_limited_the_bot_answers_from_its_table() {
    let mock = mock("sure thing").await;
    let svc = Fake {
        llm: Some(llm_for(&mock, 1)),
        ..Fake::default()
    };
    let mut env = Env::new(14);
    env.cfg.replies_per_min = 1000;
    let mut social = Social::default();
    env.step(&mut social, &svc, vec![whisper("Bob", "first")]);
    let out = run_async(&mut env, &mut social, &svc).await;
    assert_eq!(only_text(&out), "sure thing");
    // the one request of the minute is used up: the next whisper is answered from the table
    env.advance(20_000);
    env.step(&mut social, &svc, vec![whisper("Alice", "second")]);
    let out = run_async(&mut env, &mut social, &svc).await;
    assert!(
        matches!(out.as_slice(), [Action::Whisper { to, .. }] if to == "Alice"),
        "{out:?}"
    );
    assert_ne!(only_text(&out), "sure thing");
    assert_eq!(
        mock.hits.load(Ordering::SeqCst),
        1,
        "no request beyond the limit"
    );
    assert_eq!(svc.gate.stats.canned_replies.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn a_dead_endpoint_never_blocks_the_bot() {
    // nothing listens on the port
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let svc = Fake {
        llm: Some(Llm::with_key(
            &LlmConfig {
                enabled: true,
                base_url: format!("http://{addr}"),
                timeout_secs: 1,
                ..LlmConfig::default()
            },
            "k".into(),
        )),
        ..Fake::default()
    };
    let mut env = Env::new(15);
    let mut social = Social::default();
    let t = Instant::now();
    env.step(&mut social, &svc, vec![whisper("Bob", "hello")]);
    assert!(
        t.elapsed() < Duration::from_millis(500),
        "the step must not wait"
    );
    let out = run_async(&mut env, &mut social, &svc).await;
    assert!(
        matches!(out.as_slice(), [Action::Whisper { .. }]),
        "{out:?}"
    );
}

// ---- personalities ---------------------------------------------------------------------------

/// A style with these traits under the strongest cap.
fn talker(chatty: f32, crude: f32, joker: f32) -> Style {
    Style {
        chatty,
        crude,
        joker,
        cap: Profanity::Strong,
    }
}

/// Steps every `step_ms` for `ms` and returns every action with the bot clock it came out at.
fn simulate(
    env: &mut Env,
    social: &mut Social,
    svc: &dyn Services,
    ms: u64,
    step_ms: u64,
) -> Vec<(u64, Action)> {
    let mut out = Vec::new();
    for _ in 0..ms / step_ms {
        env.advance(step_ms);
        for a in env.step(social, svc, Vec::new()) {
            out.push((env.now_ms, a));
        }
    }
    out
}

fn text_of(a: &Action) -> &str {
    match a {
        Action::Say(t) | Action::Whisper { text: t, .. } | Action::Channel { text: t, .. } => t,
    }
}

/// With the ambient and channel lines on, as the config has them by default.
fn lively(env: &mut Env) {
    env.pcfg = PersonalityCfg::default();
}

#[test]
fn a_chatterbox_talks_far_more_than_a_quiet_bot_and_stays_under_its_own_cap() {
    let run = |chatty: f32, seed: u64| {
        let svc = Fake::default();
        let mut env = Env::new(seed);
        lively(&mut env);
        env.style = talker(chatty, 0.3, 0.7);
        let mut social = Social::default();
        let out = simulate(&mut env, &mut social, &svc, 2 * 3_600_000, 1_000);
        (out, svc)
    };
    let (loud, loud_svc) = run(1.0, 1);
    let (quiet, _) = run(0.05, 2);
    // two hours with a real player around: a chatterbox says a line every few minutes, a quiet
    // bot one or two
    assert!((20..=60).contains(&loud.len()), "{} lines", loud.len());
    assert!(quiet.len() <= 6, "{} lines from a quiet bot", quiet.len());
    assert!(loud.len() >= 5 * quiet.len().max(1));
    // the chatterbox never says more than five unasked lines in any ten minutes, and keeps a
    // gap of at least half a minute between two
    for (i, (t, _)) in loud.iter().enumerate() {
        let window = loud[i..].iter().filter(|(u, _)| *u < t + 600_000).count();
        assert!(window <= 5, "{window} lines in ten minutes");
        if let Some((next, _)) = loud.get(i + 1) {
            assert!(next - t >= 30_000, "{} ms apart", next - t);
        }
    }
    // jokes, small talk and a channel line or two, all counted
    let stats = &loud_svc.gate.stats;
    assert!(stats.jokes.load(Ordering::Relaxed) > 5);
    let ambient = stats.ambient.load(Ordering::Relaxed);
    let channel = stats.channel.load(Ordering::Relaxed);
    assert_eq!((ambient + channel) as usize, loud.len());
    assert!((1..=8).contains(&channel), "{channel} channel lines");
    // nothing is said unasked without an audience
    let svc = Fake::default();
    let mut env = Env::new(3);
    lively(&mut env);
    env.style = talker(1.0, 0.3, 0.7);
    env.real_near = false;
    let mut social = Social::default();
    assert!(simulate(&mut env, &mut social, &svc, 3_600_000, 5_000).is_empty());
}

#[test]
fn a_crowd_of_chatterboxes_stays_within_the_process_wide_limits() {
    let svc = Fake::default();
    let t0 = Instant::now();
    let mut bots: Vec<(Env, Social)> = (0..50)
        .map(|i| {
            let mut env = Env::new(1000 + i);
            lively(&mut env);
            env.style = talker(1.0, 0.5, 0.8);
            env.now = t0;
            (env, Social::default())
        })
        .collect();
    let mut said = Vec::new();
    for _ in 0..600 {
        for (env, social) in bots.iter_mut() {
            env.advance(1_000);
            said.extend(env.step(social, &svc, Vec::new()));
        }
    }
    let stats = &svc.gate.stats;
    let ambient = stats.ambient.load(Ordering::Relaxed);
    let channel = stats.channel.load(Ordering::Relaxed);
    // ten minutes: two ambient lines a minute and four channel lines an hour for all of them
    assert!((10..=22).contains(&ambient), "{ambient} ambient lines");
    assert!(channel <= 4, "{channel} channel lines");
    assert_eq!(said.len() as u64, ambient + channel);
}

#[test]
fn a_chatterbox_answers_more_overheard_lines_than_a_quiet_bot() {
    let count = |chatty: f32| {
        (0..300)
            .filter(|seed| {
                let svc = Fake::default();
                let mut env =
                    Env::new(2000 + seed).with_unit(7, UnitKind::Character, 700, 10.0, "Bob");
                env.style = talker(chatty, 0.0, 0.5);
                let mut social = Social::default();
                env.step(
                    &mut social,
                    &svc,
                    vec![say_from(7, "Bob", "the wolves are everywhere today")],
                );
                !env.run(&mut social, &svc, 6_000).is_empty()
            })
            .count()
    };
    let (loud, quiet) = (count(1.0), count(0.05));
    assert!(
        loud > 170 && quiet < 70 && loud > 3 * quiet,
        "{loud} vs {quiet}"
    );
}

fn spawned(level: u8) -> Tally {
    Tally {
        spawned: true,
        level,
        ..Tally::default()
    }
}

#[test]
fn the_chat_notices_what_happened_and_remarks_on_it() {
    let svc = Fake::default();
    let mut env = Env::new(21);
    env.style = talker(1.0, 0.0, 0.5);
    let mut social = Social::default();
    let mut t = spawned(10);
    social.observe(&t, env.now_ms);
    // a level up
    env.advance(40_000);
    t.level = 11;
    social.observe(&t, env.now_ms);
    let out = env.run(&mut social, &svc, 8_000);
    assert!(only_text(&out).contains("ding"), "{out:?}");
    // a death to a character: nothing while dead, the remark after the resurrection
    env.advance(60_000);
    t.dead = true;
    t.pvp_deaths = 1;
    social.observe(&t, env.now_ms);
    env.active = false;
    assert!(env.run(&mut social, &svc, 10_000).is_empty());
    env.active = true;
    t.dead = false;
    social.observe(&t, env.now_ms);
    let out = env.run(&mut social, &svc, 8_000);
    let text = only_text(&out).to_lowercase();
    assert!(
        ["gank", "road", "travel", "coming"]
            .iter()
            .any(|w| text.contains(w)),
        "{text}"
    );
    // a plain death is no gank
    env.advance(60_000);
    t.dead = true;
    social.observe(&t, env.now_ms);
    t.dead = false;
    social.observe(&t, env.now_ms);
    assert_eq!(social.event.as_ref().map(|e| &e.0), Some(&ChatEvent::Died));
    let out = env.run(&mut social, &svc, 8_000);
    assert!(!only_text(&out).contains("gank"), "{out:?}");
    // a duel the bot won colours the remark the duel goal's end asks for
    env.advance(60_000);
    t.duel = Some((Outcome::Won, env.now_ms));
    social.observe(&t, env.now_ms);
    social.note(ChatEvent::DuelDone(Some("Bob".into())), env.now_ms);
    let out = env.run(&mut social, &svc, 8_000);
    let text = only_text(&out);
    assert!(
        lines::Chatter::DuelWon
            .table()
            .iter()
            .any(|l| canned::render(l.text, "Bob") == text),
        "{text}"
    );
    assert_eq!(svc.gate.stats.chatter.load(Ordering::Relaxed), 4);
}

#[test]
fn counters_that_jump_or_change_at_login_are_no_achievement() {
    let mut social = Social::default();
    let mut t = spawned(20);
    t.houses_done = 1;
    t.bag_units = Some(10);
    social.observe(&t, 1_000);
    // inside the settle time a quest is not remarked on
    t.quests_done = 1;
    social.observe(&t, 5_000);
    assert!(social.event.is_none());
    // a reload of the quest log after the settle time is not either
    t.quests_done = 31;
    social.observe(&t, 60_000);
    assert!(social.event.is_none());
    // one more quest is
    t.quests_done = 32;
    social.observe(&t, 61_000);
    assert_eq!(
        social.event.as_ref().map(|e| &e.0),
        Some(&ChatEvent::QuestDone)
    );
    // a level up beats the quest that gave it
    t.level = 21;
    social.observe(&t, 61_100);
    t.quests_done = 33;
    social.observe(&t, 61_200);
    assert_eq!(
        social.event.as_ref().map(|e| &e.0),
        Some(&ChatEvent::LevelUp(21))
    );
    // a house counts when it was being built and is finished now
    social.event = None;
    t.houses_building = 1;
    social.observe(&t, 62_000);
    assert!(social.event.is_none());
    t.houses_building = 0;
    t.houses_done = 2;
    social.observe(&t, 63_000);
    assert_eq!(
        social.event.as_ref().map(|e| &e.0),
        Some(&ChatEvent::HouseDone)
    );
    // a bag that filled up between two counts
    social.event = None;
    t.bag_units = None;
    social.observe(&t, 64_000);
    t.bag_units = Some(30);
    social.observe(&t, 65_000);
    assert_eq!(social.event.as_ref().map(|e| &e.0), Some(&ChatEvent::Loot));
    // a reconnect starts over: the first counts after the new spawn are the baseline, and a
    // death of the old session is not told
    social.event = None;
    t.dead = true;
    social.observe(&t, 69_000);
    assert!(social.died.is_some());
    social.observe(&Tally::default(), 70_000);
    let mut back = spawned(25);
    back.quests_done = 40;
    social.observe(&back, 80_000);
    back.level = 26;
    social.observe(&back, 80_100);
    assert_eq!(
        social.event.as_ref().map(|e| &e.0),
        Some(&ChatEvent::LevelUp(26))
    );
    assert!(social.died.is_none());
}

#[test]
fn no_line_is_coarser_than_the_bots_level_and_the_coarse_lines_are_used() {
    use pb_llm::persona::profanity_of;
    for cap in Profanity::ALL {
        let mut worst = Profanity::Off;
        for seed in 0..30 {
            let svc = Fake::default();
            let mut env = Env::new(3000 + seed);
            lively(&mut env);
            env.pcfg.ambient_per_min = 60;
            env.style = Style {
                cap,
                ..talker(1.0, 1.0, 0.0)
            };
            let mut social = Social::default();
            social.note(ChatEvent::Died, env.now_ms);
            for (_, a) in simulate(&mut env, &mut social, &svc, 30 * 60_000, 2_000) {
                let p = profanity_of(text_of(&a));
                assert!(p <= cap, "{a:?} under {cap:?}");
                worst = worst.max(p);
            }
        }
        assert_eq!(worst, cap, "a crude bot uses what the cap allows");
    }
    // a polite bot under the strongest cap stays clean
    let svc = Fake::default();
    let mut env = Env::new(9);
    lively(&mut env);
    env.style = talker(1.0, 0.0, 0.5);
    let mut social = Social::default();
    for (_, a) in simulate(&mut env, &mut social, &svc, 60 * 60_000, 2_000) {
        assert_eq!(profanity_of(text_of(&a)), Profanity::Off, "{a:?}");
    }
}

#[test]
fn only_chatty_bots_speak_on_the_public_channels_and_never_advertise() {
    use crate::bot::market::ads::grammar::parse;
    let channel_lines = |chatty: f32, seed: u64, hours: u64| {
        let svc = Fake::default();
        let mut env = Env::new(seed);
        lively(&mut env);
        env.style = talker(chatty, 0.5, 0.5);
        let mut social = Social::default();
        simulate(&mut env, &mut social, &svc, hours * 3_600_000, 5_000)
            .into_iter()
            .filter_map(|(_, a)| match a {
                Action::Channel { kind, text } => Some((kind, text)),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert!(channel_lines(0.05, 41, 6).is_empty());
    assert!(channel_lines(0.25, 42, 6).is_empty());
    let loud = channel_lines(1.0, 43, 3);
    assert!((3..=12).contains(&loud.len()), "{loud:?}");
    for (kind, text) in &loud {
        assert!(lines::CHANNELS.contains(kind));
        assert!(!parse(text).is_market(), "{text}");
        assert!(channel_packet(*kind, text).is_some());
    }
    // the channel builder refuses everything but the three public channels and unsafe lines
    assert!(channel_packet(cs::chat_type::PARTY, "hi").is_none());
    assert!(channel_packet(cs::chat_type::SAY, "hi").is_none());
    assert!(channel_packet(cs::chat_type::SHOUT, "/gm").is_none());
    let ok = channel_packet(cs::chat_type::SHOUT, "anyone lagging?").unwrap();
    assert_eq!(decode_chat(&ok).0, cs::chat_type::SHOUT);
}

#[test]
fn the_wire_refuses_slurs_whoever_wrote_the_line() {
    for bad in ["you fag", "kys", "nice b00bs", "heil hitler"] {
        assert!(wire_text(bad).is_none(), "{bad}");
        assert!(say_packet(bad).is_none() && whisper_packet("Bob", bad).is_none());
    }
    assert!(
        say_packet("damn, the lag").is_some(),
        "swearing is the cap's business"
    );
}

#[tokio::test]
async fn a_foul_model_reply_is_softened_to_the_bots_level_and_a_slur_never_sent() {
    let foul = mock("fuck this lag, damn").await;
    let svc = Fake {
        llm: Some(llm_for(&foul, 1000)),
        ..Fake::default()
    };
    let mut env = Env::new(31);
    env.style = Style {
        cap: Profanity::Off,
        ..talker(0.5, 1.0, 0.5)
    };
    let mut social = Social::default();
    env.step(&mut social, &svc, vec![whisper("Bob", "how is it going")]);
    let out = run_async(&mut env, &mut social, &svc).await;
    assert_eq!(only_text(&out), "frick this lag, dang");
    assert_eq!(svc.gate.stats.softened.load(Ordering::Relaxed), 1);
    // the prompt told the model to keep it clean
    let body: serde_json::Value = serde_json::from_str(&foul.bodies.lock()[0]).unwrap();
    let system = body["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("no swearing at all"), "{system}");

    let slur = mock("lol you f4g").await;
    let svc = Fake {
        llm: Some(llm_for(&slur, 1000)),
        ..Fake::default()
    };
    let mut env = Env::new(32);
    let mut social = Social::default();
    env.step(&mut social, &svc, vec![whisper("Bob", "hello there")]);
    let out = run_async(&mut env, &mut social, &svc).await;
    let text = only_text(&out);
    assert!(!pb_llm::persona::blocked(text), "{text}");
    assert_eq!(svc.gate.stats.canned_replies.load(Ordering::Relaxed), 1);
    assert_eq!(svc.gate.stats.llm_replies.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn the_llm_writes_some_ambient_lines_and_the_table_is_the_fallback() {
    let m = mock("the kraken is up again, of course").await;
    let svc = Fake {
        llm: Some(llm_for(&m, 1000)),
        ..Fake::default()
    };
    let mut asked = false;
    for seed in 0..20 {
        let mut env = Env::new(4000 + seed);
        lively(&mut env);
        env.style = talker(1.0, 0.0, 0.5);
        let mut social = Social::default();
        for _ in 0..400 {
            env.advance(1_000);
            let out = env.step(&mut social, &svc, Vec::new());
            if social.pending.is_some() || !out.is_empty() {
                break;
            }
        }
        if social.pending.is_none() {
            continue;
        }
        asked = true;
        let out = run_async(&mut env, &mut social, &svc).await;
        assert_eq!(
            out,
            vec![Action::Say("the kraken is up again, of course".into())]
        );
        break;
    }
    assert!(asked, "no ambient line was ever asked of the model");
    assert_eq!(m.hits.load(Ordering::SeqCst), 1);
    assert_eq!(svc.gate.stats.llm_ambient.load(Ordering::Relaxed), 1);
    let body: serde_json::Value = serde_json::from_str(&m.bodies.lock()[0]).unwrap();
    let system = body["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("Nobody asked you anything"), "{system}");
    assert!(system.contains("chatterbox"), "{system}");
    // switched off: the lines come from the table and the model is never asked
    let mut env = Env::new(4100);
    lively(&mut env);
    env.pcfg.llm_chatter = false;
    env.style = talker(1.0, 0.0, 0.5);
    let mut social = Social::default();
    let said = simulate(&mut env, &mut social, &svc, 20 * 60_000, 1_000);
    assert!(!said.is_empty());
    assert!(social.pending.is_none());
    assert_eq!(m.hits.load(Ordering::SeqCst), 1);
}
