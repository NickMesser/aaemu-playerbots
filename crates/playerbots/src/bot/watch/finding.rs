//! What the detectors found, and the reports it becomes.
//!
//! A detector that lives in the state, a goal or the combat controller does not build an
//! [`IssueReport`] itself (it has neither the names nor the right to file): it pushes a small
//! `Copy` [`Finding`] (`BotState::watch`), and the bot loop turns every finding into a report with
//! [`Finding::report`] and files it with the packet trail and the bot's context. All keys, titles,
//! suspects and severities are decided here, in one place, from ids only, so two bots meeting the
//! same problem produce the same fingerprint.

use aa_proto::game::sc_name;
use aa_world::World;

use super::classify::{
    skill_result_name, Cause, ConnVerdict, Death, Fault, Phase, Stall, TradeFault,
};
use super::tracker::Req;
use crate::issues::{IssueKind, IssueReport, Severity, Suspect};

const AREA_QUESTS: &str = "rust/crates/aaemu-game/src/systems/quests/";
const AREA_SKILLS: &str = "rust/crates/aaemu-game/src/systems/skills/";
const AREA_ECONOMY: &str = "rust/crates/aaemu-game/src/systems/economy/";
const AREA_NPCS: &str = "rust/crates/aaemu-game/src/systems/npcs/";
const AREA_COMMANDS: &str = "rust/crates/aaemu-game/src/systems/services/commands/";
const AREA_WORLD: &str = "rust/crates/aaemu-game/src/world/";
const AREA_NET: &str = "rust/crates/aaemu-game/src/net/";
const AREA_LOGIN: &str = "rust/crates/aaemu-login/src/";

/// What an NPC or doodad is needed for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Hands out the quest.
    Giver,
    /// Takes the finished quest.
    TurnIn,
    /// An objective of the quest needs it.
    Objective,
}

impl Role {
    fn word(self) -> &'static str {
        match self {
            Role::Giver => "quest giver",
            Role::TurnIn => "quest turn-in",
            Role::Objective => "quest objective",
        }
    }
}

/// Why a trade pack's payment mail is missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayFault {
    /// The mail has neither money nor attachments.
    Empty,
    /// The server did not answer the requests for the mail or its attachments.
    Unanswered,
}

/// Something a detector found. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Finding {
    /// A request the server always answers got no answer.
    NoResponse {
        /// The kind of request.
        req: Req,
        /// Its quest, skill or object.
        id: u32,
        /// A second id (a corpse's template).
        aux: u32,
        /// How long the bot waited.
        waited_ms: u32,
    },
    /// The server confirmed the kill or cast an objective needs, the counter never moved.
    QuestCreditMissing {
        /// Quest template.
        quest: u32,
        /// Component of the objective.
        component: u32,
        /// The monster template killed, or the skill cast.
        source: u32,
        /// `source` is a skill (an item or doodad use), not a monster.
        by_skill: bool,
        /// The counter.
        have: i32,
        /// What the objective needs.
        need: i32,
    },
    /// The server refused a quest the bot's start gates let through (or its turn-in).
    QuestRefused {
        /// Quest template.
        quest: u32,
        /// The server's reason byte.
        code: u8,
        /// A turn-in, not an accept.
        turn_in: bool,
    },
    /// An NPC or doodad the world data puts here was not there.
    RoleMissing {
        /// Template id.
        template: u32,
        /// A doodad, not an NPC.
        doodad: bool,
        /// What for.
        role: Role,
        /// Spawn points the bot stood at.
        spots: u8,
    },
    /// An NPC or doodad is in view but the bot cannot get within the server's reach of it in
    /// 3D (a house storey or a cave floor the navigation mesh has no way to).
    RoleUnreachable {
        /// Template id.
        template: u32,
        /// A doodad, not an NPC.
        doodad: bool,
        /// What for.
        role: Role,
        /// Closest 3D distance the bot could get, in metres.
        dist_m: f32,
    },
    /// `SCQuestUnitReqFailed`: a start component's unit requirement refused the quest (for
    /// example "quest 1136 must be in progress first").
    QuestUnitReq {
        /// Quest template.
        quest: u32,
        /// `unit_reqs.kind_id`.
        kind: u8,
        /// Requirement value (the quest id for kind 0x25).
        value: u32,
    },
    /// Confirmed casts at a target never cost it a point (or the server said it is immune).
    TargetImmune {
        /// The monster template.
        npc: u32,
        /// Confirmed casts.
        casts: u8,
        /// The server's own `TargetImmune` refusal.
        refused: bool,
    },
    /// A cast was refused for a reason the bot's checks rule out.
    SkillRefused {
        /// Skill id.
        skill: u32,
        /// `SkillResult` byte.
        code: u8,
    },
    /// The bot's death, or its lack of one.
    Death(Death),
    /// A finished trade did not do what the offers say.
    Trade(TradeFault),
    /// A trade pack was sold and the payment mail is bad.
    Payment {
        /// What is wrong with it.
        fault: PayFault,
    },
}

/// The names of the static data, so a report reads well. [`World`] is the real one; the tests use
/// a fake (a `World` needs the server's database).
pub trait Names {
    /// A quest's name.
    fn quest(&self, id: u32) -> Option<&str>;
    /// An NPC template's name.
    fn npc(&self, id: u32) -> Option<&str>;
    /// A doodad template's name.
    fn doodad(&self, id: u32) -> Option<&str>;
    /// A skill's name.
    fn skill(&self, id: u32) -> Option<&str>;
    /// An item template's name.
    fn item(&self, id: u32) -> Option<&str>;
    /// Whether the bot's doodad filter keeps doodads of this template (a doodad without any use,
    /// loot or quest glow is dropped on arrival, see `keep_doodad` in the bot module), so the bot
    /// can only tell that one is missing if it keeps them.
    fn doodad_visible(&self, id: u32) -> bool;
}

impl Names for World {
    fn quest(&self, id: u32) -> Option<&str> {
        World::quest(self, id).map(|q| &*q.name)
    }

    fn npc(&self, id: u32) -> Option<&str> {
        World::npc(self, id).map(|n| &*n.name)
    }

    fn doodad(&self, id: u32) -> Option<&str> {
        World::doodad(self, id).map(|d| &*d.name)
    }

    fn skill(&self, id: u32) -> Option<&str> {
        World::skill(self, id).map(|s| &*s.name)
    }

    fn item(&self, id: u32) -> Option<&str> {
        self.item_name(id)
    }

    fn doodad_visible(&self, id: u32) -> bool {
        World::doodad(self, id).is_some_and(|t| t.flags != aa_world::DoodadFlags::NONE)
    }
}

fn named(id: u32, name: Option<&str>) -> String {
    match name {
        Some(n) => format!("{id} \"{n}\""),
        None => id.to_string(),
    }
}

fn quest_name(names: &dyn Names, quest: u32) -> String {
    named(quest, names.quest(quest))
}

fn npc_name(names: &dyn Names, t: u32) -> String {
    named(t, names.npc(t))
}

fn skill_name(names: &dyn Names, skill: u32) -> String {
    named(skill, names.skill(skill))
}

/// The server's name of a quest error byte (`locale.questContext.errors`, see the server's
/// `quest_error`).
pub fn quest_error_name(code: u8) -> &'static str {
    match code {
        1 => "already_have",
        2 => "invalid_quest",
        7 => "invalid_npc_or_quest",
        12 => "invalid_quest_status",
        19 => "invalid_doodad",
        _ => "quest_error",
    }
}

impl Finding {
    /// The report for this finding. `names` supplies the names of ids.
    pub fn report(&self, names: &dyn Names) -> IssueReport {
        match *self {
            Finding::NoResponse {
                req,
                id,
                aux,
                waited_ms,
            } => {
                let (key, what, area, severity) = match req {
                    Req::QuestAccept => (
                        format!("quest_accept quest={id}"),
                        format!("accepting quest {}", quest_name(names, id)),
                        AREA_QUESTS,
                        Severity::Medium,
                    ),
                    Req::QuestComplete => (
                        format!("quest_complete quest={id}"),
                        format!("turning in quest {}", quest_name(names, id)),
                        AREA_QUESTS,
                        Severity::Medium,
                    ),
                    Req::LootOpen => (
                        format!("loot_open npc={aux}"),
                        format!("opening the loot of NPC {}", npc_name(names, aux)),
                        AREA_ECONOMY,
                        Severity::Medium,
                    ),
                    Req::SkillCast => (
                        format!("skill_cast skill={id}"),
                        format!("casting skill {}", skill_name(names, id)),
                        AREA_SKILLS,
                        Severity::Low,
                    ),
                    Req::Teleport => (
                        "teleport".to_string(),
                        "a GM /move".to_string(),
                        AREA_COMMANDS,
                        Severity::Low,
                    ),
                };
                IssueReport::new(
                    IssueKind::NoResponse,
                    key,
                    format!("The server did not answer {what}"),
                )
                .expected("an answer (the result or a refusal) within a few seconds")
                .actual(format!(
                    "nothing for {} s, although the server kept sending other packets",
                    waited_ms / 1000
                ))
                .suspect(Suspect::Unknown)
                .severity(severity)
                .area(area)
                .fact("request", req.name())
                .fact("id", id)
            }
            Finding::QuestCreditMissing {
                quest,
                component,
                source,
                by_skill,
                have,
                need,
            } => {
                let (what, expected) = if by_skill {
                    (
                        format!("using skill {}", skill_name(names, source)),
                        "the cast was accepted by the server, the objective counter goes up",
                    )
                } else {
                    (
                        format!("killing NPC {}", npc_name(names, source)),
                        "the kill was confirmed by the server, the objective counter goes up",
                    )
                };
                IssueReport::new(
                    IssueKind::QuestCreditMissing,
                    format!("quest={quest} component={component}"),
                    format!("Quest {}: {what} gives no credit", quest_name(names, quest)),
                )
                .expected(expected)
                .actual(format!(
                    "the counter stayed at {have}/{need} for over 10 s{}",
                    if by_skill {
                        ""
                    } else {
                        " (no other character was within 40 m, but a monster tagged by someone out of                          view, or by an NPC, gives the killer no credit)"
                    }
                ))
                // (the server credits the tagger of a monster and their team: the bot cannot rule
                // out a tag it did not see, so it does not blame the server)
                .suspect(if by_skill {
                    Suspect::Server
                } else {
                    Suspect::Unknown
                })
                .severity(Severity::Medium)
                .area(AREA_QUESTS)
                .fact("quest", quest)
                .fact("component", component)
                .fact(if by_skill { "skill" } else { "npc" }, source)
                .fact("counter", format!("{have}/{need}"))
            }
            Finding::QuestRefused {
                quest,
                code,
                turn_in,
            } => {
                let (kind, verb) = if turn_in {
                    (IssueKind::QuestTurnInRefused, "turn in")
                } else {
                    (IssueKind::QuestRefused, "start")
                };
                // "invalid quest / NPC / doodad" after the bot's own gates passed means the data
                // the server uses differs from the data the bot read; "already have" means the
                // bot's journal is stale.
                let suspect = match code {
                    2 | 7 | 19 => Suspect::Data,
                    1 => Suspect::Bot,
                    _ => Suspect::Unknown,
                };
                IssueReport::new(
                    kind,
                    format!("quest={quest} code={code}"),
                    format!(
                        "The server refused to {verb} quest {} ({})",
                        quest_name(names, quest),
                        quest_error_name(code)
                    ),
                )
                .expected(if turn_in {
                    "the finished quest is completed"
                } else {
                    "the quest starts: the bot met its level, race, prior-quest and item gates"
                })
                .actual(format!("SCQuestContextFailed with error {code}"))
                .suspect(suspect)
                .severity(Severity::Medium)
                .area(AREA_QUESTS)
                .fact("quest", quest)
                .fact("error", code)
            }
            Finding::RoleMissing {
                template,
                doodad,
                role,
                spots,
            } => {
                let (kind, name, suspect) = if doodad {
                    let name = named(template, names.doodad(template));
                    // a doodad the bot's filter would not keep may well be there
                    let suspect = if names.doodad_visible(template) {
                        Suspect::Data
                    } else {
                        Suspect::Bot
                    };
                    (IssueKind::DoodadMissing, name, suspect)
                } else {
                    (IssueKind::NpcMissing, npc_name(names, template), Suspect::Data)
                };
                IssueReport::new(
                    kind,
                    format!("template={template}"),
                    format!(
                        "{} {} {name} is not at its spawn",
                        if doodad { "Doodad" } else { "NPC" },
                        role.word()
                    ),
                )
                .expected("the unit of the world data's static spawn is in view when the bot stands there")
                .actual(format!(
                    "the bot stood at {spots} spawn point(s) for a while and the server showed none"
                ))
                .suspect(suspect)
                .severity(Severity::Medium)
                .area(if doodad { AREA_WORLD } else { AREA_NPCS })
                .fact("template", template)
                .fact("role", role.word())
            }
            Finding::RoleUnreachable {
                template,
                doodad,
                role,
                dist_m,
            } => {
                let name = if doodad {
                    named(template, names.doodad(template))
                } else {
                    npc_name(names, template)
                };
                IssueReport::new(
                    IssueKind::TargetUnreachable,
                    format!("template={template} role={}", role.word()),
                    format!(
                        "{} {} {name} could not be reached (closest {dist_m:.0} m)",
                        if doodad { "Doodad" } else { "NPC" },
                        role.word()
                    ),
                )
                .expected("the bot walks within the server's quest reach (15 m) of the target")
                .actual(format!(
                    "the nearest walkable spot stayed {dist_m:.0} m away in 3D: the target is on \
                     another floor and the navigation mesh has no way there"
                ))
                .suspect(Suspect::Bot)
                .severity(Severity::Low)
                .area(if doodad { AREA_WORLD } else { AREA_NPCS })
                .fact("template", template)
                .fact("role", role.word())
                .fact("distance_m", format!("{dist_m:.1}"))
            }
            Finding::QuestUnitReq { quest, kind, value } => IssueReport::new(
                IssueKind::QuestRefused,
                format!("quest={quest} unit_req={kind}:{value}"),
                format!(
                    "The server refused to start quest {} (unit requirement {kind} value {value})",
                    quest_name(names, quest)
                ),
            )
            .expected("the quest starts: the bot's own start gates let it through")
            .actual(format!(
                "SCQuestUnitReqFailed with kind {kind} and value {value}"
            ))
            .suspect(if kind == 0x25 { Suspect::Bot } else { Suspect::Unknown })
            .severity(Severity::Low)
            .area(AREA_QUESTS)
            .fact("quest", quest)
            .fact("kind", kind)
            .fact("value", value),
            Finding::TargetImmune {
                npc,
                casts,
                refused,
            } => IssueReport::new(
                IssueKind::TargetImmune,
                format!("npc={npc}"),
                format!("NPC {} takes no damage", npc_name(names, npc)),
            )
            .expected("confirmed damaging casts lower the monster's hit points")
            .actual(if refused {
                "the server refused a cast with TargetImmune".to_string()
            } else {
                format!("{casts} casts were accepted and its hit points never changed")
            })
            .suspect(if refused { Suspect::Data } else { Suspect::Server })
            .severity(Severity::Medium)
            .area(AREA_SKILLS)
            .fact("npc", npc)
            .fact("casts", casts),
            Finding::SkillRefused { skill, code } => IssueReport::new(
                IssueKind::SkillRefusedUnexpectedly,
                format!("skill={skill} code={code}"),
                format!(
                    "The server refused skill {} ({})",
                    skill_name(names, skill),
                    skill_result_name(code)
                ),
            )
            .expected("a learned skill that is ready and affordable starts")
            .actual(format!("SCSkillStarted with result {code}"))
            .suspect(Suspect::Unknown)
            .severity(Severity::Medium)
            .area(AREA_SKILLS)
            .fact("skill", skill)
            .fact("result", code),
            Finding::Death(d) => match d {
                Death::NoDeathPacket => IssueReport::new(
                    IssueKind::DeathWithoutPacket,
                    "phase=world",
                    "The character's hit points reached 0 but the server never said it died",
                )
                .expected("SCUnitDeath within a moment of the hit points reaching 0")
                .actual("only SCUnitPoints with 0 hit points; no death packet for 3 s")
                .suspect(Suspect::Server)
                .severity(Severity::High)
                .area(AREA_SKILLS),
                Death::LoggedInDead => IssueReport::new(
                    IssueKind::LoggedInDead,
                    "phase=login",
                    "The character logged in dead and the server never revived it",
                )
                .expected("the temple resurrection works when the bot asks for it")
                .actual("still dead 45 s after entering the world, despite the resurrection requests")
                .suspect(Suspect::Server)
                .severity(Severity::Medium)
                .area(AREA_SKILLS),
                Death::NeverResurrected => IssueReport::new(
                    IssueKind::NoResponse,
                    "resurrect",
                    "The server never resurrected a dead character that asked for it",
                )
                .expected("SCCharacterResurrected after CSResurrectCharacter once the respawn time is over")
                .actual("still dead 45 s after the respawn time")
                .suspect(Suspect::Server)
                .severity(Severity::Medium)
                .area(AREA_SKILLS),
            },
            Finding::Trade(f) => {
                let (key, text, actual) = match f {
                    TradeFault::Money { expected, actual } => (
                        "kind=money".to_string(),
                        "The purse did not change by what the two offers say".to_string(),
                        format!("the purse changed by {actual} copper, the offers say {expected}"),
                    ),
                    TradeFault::ItemsIn {
                        template,
                        expected,
                        actual,
                    } => (
                        format!("kind=items_in item={template}"),
                        format!(
                            "The partner's item {} did not arrive in full",
                            names.item(template).unwrap_or("?")
                        ),
                        format!("{actual} of {expected} arrived"),
                    ),
                    TradeFault::ItemsOut { expected, actual } => (
                        "kind=items_out".to_string(),
                        "The bot lost another number of items than it put up".to_string(),
                        format!("{actual} items left the bag, {expected} were put up"),
                    ),
                };
                IssueReport::new(IssueKind::TradeMismatch, key, text)
                    .expected("both sides get exactly what the other put up")
                    .actual(actual)
                    .suspect(Suspect::Server)
                    .severity(Severity::High)
                    .area(AREA_ECONOMY)
            }
            Finding::Payment { fault } => {
                let (key, actual) = match fault {
                    PayFault::Empty => (
                        "pack_mail reason=empty",
                        "the payment mail of the sold trade pack has no money and no attachment",
                    ),
                    PayFault::Unanswered => (
                        "pack_mail reason=unanswered",
                        "the server did not answer the requests to read the payment mail or take its attachments",
                    ),
                };
                IssueReport::new(
                    IssueKind::PaymentMissing,
                    key,
                    "The payment for a sold trade pack did not arrive in full",
                )
                .expected("a system mail with the price after the sale")
                .actual(actual)
                .suspect(Suspect::Server)
                .severity(Severity::Medium)
                .area(AREA_ECONOMY)
            }
        }
    }
}

/// The report for a connection that ended or could not be made.
pub fn fault_report(fault: Fault, why: &str) -> IssueReport {
    let ConnVerdict {
        kind,
        key,
        suspect,
        severity,
    } = fault.verdict();
    let phase = fault.phase.name();
    let title = match fault.cause {
        Cause::Kicked(r) => format!("The server kicked the bot ({phase}, reason {r})"),
        Cause::Closed | Cause::Reset | Cause::WriteTimeout | Cause::Silent | Cause::Logout => {
            format!("The connection to the server ended unexpectedly ({phase}: {why})")
        }
        Cause::StepTimeout(what) => format!("The server did not answer: {what} ({phase})"),
        Cause::Denied(_) | Cause::JoinRefused(_) | Cause::EnterDenied(_) | Cause::NoWorld => {
            format!("The login was refused ({why})")
        }
        Cause::Refused(_) | Cause::CreateFailed(_) => {
            format!("The game server refused the bot ({why})")
        }
        Cause::ConnectFailed => format!("The bot could not connect to the server ({why})"),
        Cause::Protocol => format!("The server broke the protocol ({why})"),
    };
    let area = match fault.phase {
        Phase::Login => AREA_LOGIN,
        _ => AREA_NET,
    };
    IssueReport::new(kind, key, title)
        .expected("the bot stays connected until it logs out itself")
        .actual(why.to_string())
        .suspect(suspect)
        .severity(severity)
        .area(area)
        .fact("phase", phase)
}

/// The report for a stall of the server connection.
pub fn stall_report(s: Stall) -> IssueReport {
    let mut actual = Vec::new();
    if s.quiet_ms > 0 {
        actual.push(format!("no packet at all for {} s", s.quiet_ms / 1000));
    }
    if s.rtt_ms > 0 {
        actual.push(format!("a ping took {} s to be answered", s.rtt_ms / 1000));
    }
    IssueReport::new(
        IssueKind::ServerStall,
        "phase=world",
        "The server stalled: it stopped answering for several seconds",
    )
    .expected("the server answers a ping within a moment and sends something every few seconds")
    .actual(actual.join(", "))
    .suspect(s.suspect)
    .severity(Severity::Medium)
    .area(AREA_NET)
    .fact("quiet_ms", s.quiet_ms)
    .fact("rtt_ms", s.rtt_ms)
}

/// The report for a server packet the bot's parser rejects. `err` is the parser's message,
/// `body` the packet body (its first bytes are attached).
pub fn malformed_report(level: u8, opcode: u16, err: &str, body: &[u8]) -> IssueReport {
    let name = sc_name(level, opcode);
    IssueReport::new(
        IssueKind::MalformedPacket,
        format!("level={level} opcode={opcode:#05x}"),
        format!("Packet {name} ({opcode:#05x}) does not match its layout"),
    )
    .expected("every server packet parses with the layout the server's writer uses")
    .actual(format!(
        "{name} with a {} byte body: {err} (the bot's parser may be wrong, too)",
        body.len()
    ))
    .suspect(Suspect::Unknown)
    .severity(Severity::High)
    .fact("packet", name)
    .fact("opcode", format!("{opcode:#05x}"))
    .fact("level", level)
    .fact("body_len", body.len())
    .packet_head(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A world without names; doodads are visible when `visible` says so.
    struct Fake {
        visible: bool,
    }

    impl Names for Fake {
        fn quest(&self, id: u32) -> Option<&str> {
            (id == 251).then_some("Wild Boars")
        }

        fn npc(&self, _: u32) -> Option<&str> {
            None
        }

        fn doodad(&self, _: u32) -> Option<&str> {
            None
        }

        fn skill(&self, _: u32) -> Option<&str> {
            None
        }

        fn item(&self, _: u32) -> Option<&str> {
            None
        }

        fn doodad_visible(&self, _: u32) -> bool {
            self.visible
        }
    }

    fn world() -> Fake {
        Fake { visible: false }
    }

    #[test]
    fn keys_hold_ids_only_so_two_bots_file_the_same_issue() {
        let w = world();
        let a = Finding::NoResponse {
            req: Req::QuestAccept,
            id: 251,
            aux: 0,
            waited_ms: 6_100,
        }
        .report(&w);
        let b = Finding::NoResponse {
            req: Req::QuestAccept,
            id: 251,
            aux: 0,
            waited_ms: 9_900,
        }
        .report(&w);
        assert_eq!(a.fingerprint(), b.fingerprint());
        assert_eq!(a.key, "quest_accept quest=251");
        assert_eq!(a.kind, IssueKind::NoResponse);
        assert!(a.title.contains("251 \"Wild Boars\""), "{}", a.title);
        // a different quest is another issue
        let c = Finding::NoResponse {
            req: Req::QuestAccept,
            id: 252,
            aux: 0,
            waited_ms: 6_000,
        }
        .report(&w);
        assert_ne!(a.fingerprint(), c.fingerprint());
        // the loot request is keyed by the corpse's template, not its object id
        let l = Finding::NoResponse {
            req: Req::LootOpen,
            id: 4_321,
            aux: 3_475,
            waited_ms: 6_000,
        }
        .report(&w);
        assert_eq!(l.key, "loot_open npc=3475");
    }

    #[test]
    fn every_finding_makes_a_complete_report_with_a_fixed_key() {
        let w = world();
        let all = [
            Finding::QuestCreditMissing {
                quest: 251,
                component: 9,
                source: 3_475,
                by_skill: false,
                have: 1,
                need: 5,
            },
            Finding::QuestRefused {
                quest: 251,
                code: 7,
                turn_in: false,
            },
            Finding::QuestRefused {
                quest: 251,
                code: 12,
                turn_in: true,
            },
            Finding::RoleMissing {
                template: 77,
                doodad: false,
                role: Role::Giver,
                spots: 2,
            },
            Finding::RoleMissing {
                template: 78,
                doodad: true,
                role: Role::Objective,
                spots: 1,
            },
            Finding::RoleUnreachable {
                template: 5224,
                doodad: false,
                role: Role::TurnIn,
                dist_m: 46.0,
            },
            Finding::QuestUnitReq {
                quest: 1137,
                kind: 0x25,
                value: 1136,
            },
            Finding::TargetImmune {
                npc: 5,
                casts: 8,
                refused: false,
            },
            Finding::SkillRefused {
                skill: 14_001,
                code: 0x1E,
            },
            Finding::Death(Death::NoDeathPacket),
            Finding::Death(Death::LoggedInDead),
            Finding::Death(Death::NeverResurrected),
            Finding::Trade(TradeFault::Money {
                expected: 5,
                actual: 4,
            }),
            Finding::Trade(TradeFault::ItemsIn {
                template: 900,
                expected: 5,
                actual: 4,
            }),
            Finding::Trade(TradeFault::ItemsOut {
                expected: 2,
                actual: 1,
            }),
            Finding::Payment {
                fault: PayFault::Empty,
            },
            Finding::Payment {
                fault: PayFault::Unanswered,
            },
        ];
        let mut prints = std::collections::HashSet::new();
        for f in all {
            let r = f.report(&w);
            assert!(!r.title.is_empty() && !r.expected.is_empty() && !r.actual.is_empty());
            assert!(r.area.is_some(), "{f:?}");
            assert!(!r.key.is_empty());
            // a key never carries a number that changes between two occurrences
            assert!(
                !r.key.contains("ms=") && !r.key.contains("pos") && !r.key.contains("obj"),
                "{}",
                r.key
            );
            assert!(prints.insert(r.fingerprint()), "{f:?} collides");
        }
    }

    #[test]
    fn suspects_follow_what_the_bot_can_know() {
        let w = world();
        // the server says the quest or NPC is invalid although the gates passed: its data
        let data = Finding::QuestRefused {
            quest: 1,
            code: 7,
            turn_in: false,
        }
        .report(&w);
        assert_eq!(data.suspect, Suspect::Data);
        // "already have": the bot's journal is stale
        let stale = Finding::QuestRefused {
            quest: 1,
            code: 1,
            turn_in: false,
        }
        .report(&w);
        assert_eq!(stale.suspect, Suspect::Bot);
        assert!(!stale.suspect.is_reportable());
        // a doodad the bot's filter would not even keep cannot be called missing
        let missing = Finding::RoleMissing {
            template: 424_242,
            doodad: true,
            role: Role::Giver,
            spots: 1,
        };
        let doodad = missing.report(&w);
        assert_eq!(doodad.suspect, Suspect::Bot);
        assert_eq!(doodad.kind, IssueKind::DoodadMissing);
        // one it keeps is the data's problem
        let seen = missing.report(&Fake { visible: true });
        assert_eq!(seen.suspect, Suspect::Data);
        assert_eq!(seen.fingerprint(), doodad.fingerprint());
        // the server's own TargetImmune refusal is a data problem; silent invulnerability is not
        let refused = Finding::TargetImmune {
            npc: 5,
            casts: 1,
            refused: true,
        }
        .report(&w);
        assert_eq!(refused.suspect, Suspect::Data);
        let silent = Finding::TargetImmune {
            npc: 5,
            casts: 8,
            refused: false,
        }
        .report(&w);
        assert_eq!(silent.suspect, Suspect::Server);
        assert_eq!(refused.fingerprint(), silent.fingerprint());
        // a kill that gave no credit may be somebody else's tag; an accepted item cast has no such
        // excuse
        let credit = |by_skill| {
            Finding::QuestCreditMissing {
                quest: 1,
                component: 2,
                source: 3,
                by_skill,
                have: 0,
                need: 1,
            }
            .report(&w)
            .suspect
        };
        assert_eq!(credit(false), Suspect::Unknown);
        assert_eq!(credit(true), Suspect::Server);
        // a death without a packet is one of the always-report symptoms
        let d = Finding::Death(Death::NoDeathPacket).report(&w);
        assert!(d.kind.always_report() && d.severity == Severity::High);
    }

    #[test]
    fn reports_that_blame_the_bot_are_kept_but_never_queued() {
        use crate::config::QueueMode;
        use crate::issues::model::BotRef;
        use crate::issues::{IssueBook, Policy};
        const NOW: u64 = 1_800_000_000_000;
        let w = world();
        // the same quest refusal, once with a stale journal (the bot's fault, code 1) and once as
        // the data's fault (code 7), from five bots, many times
        let mine = Finding::QuestRefused {
            quest: 9,
            code: 1,
            turn_in: false,
        };
        let theirs = Finding::QuestRefused {
            quest: 9,
            code: 7,
            turn_in: false,
        };
        // a high-severity always-report kind from the bot's side must not get through either
        let bot_death = Finding::Death(Death::NoDeathPacket)
            .report(&w)
            .suspect(Suspect::Bot);
        let mut book = IssueBook::new(Policy::default());
        for bot in 1..=5u32 {
            for f in [mine, theirs] {
                for _ in 0..4 {
                    let mut r = f.report(&w).bot(BotRef {
                        id: bot,
                        name: format!("Bot{bot}"),
                        ..BotRef::default()
                    });
                    r.normalize(NOW);
                    book.record(r, NOW);
                }
            }
            let mut d = bot_death.clone().bot(BotRef {
                id: bot,
                ..BotRef::default()
            });
            d.normalize(NOW);
            book.record(d, NOW);
        }
        let mine_fp = mine.report(&w).fingerprint();
        let theirs_fp = theirs.report(&w).fingerprint();
        let death_fp = bot_death.fingerprint();
        let rec = book.get(&mine_fp).expect("a bot-side issue is recorded");
        assert_eq!((rec.count, rec.bots.total), (20, 5), "kept and counted");
        assert!(!book.is_reportable(rec), "but the team is not told");
        assert!(!book.is_reportable(book.get(&death_fp).unwrap()));
        assert!(book.is_reportable(book.get(&theirs_fp).unwrap()));
        // the queue plan has the data issue and neither of the bot-side ones
        let jobs = book.plan(NOW, QueueMode::Auto, true);
        let queued: Vec<&str> = jobs
            .iter()
            .filter(|j| j.queue.is_some())
            .map(|j| j.fp.as_str())
            .collect();
        assert_eq!(queued, [theirs_fp.as_str()], "{jobs:?}");
        assert!(jobs.iter().all(|j| j.fp != mine_fp && j.fp != death_fp));
    }

    #[test]
    fn the_connection_reports_carry_phase_and_reason() {
        let r = fault_report(
            Fault {
                phase: Phase::World,
                cause: Cause::Closed,
            },
            "game server closed the connection",
        );
        assert_eq!(r.kind, IssueKind::UnexpectedDisconnect);
        assert_eq!(r.key, "phase=world reason=closed");
        assert!(r.title.contains("world"));
        let kick = fault_report(
            Fault {
                phase: Phase::World,
                cause: Cause::Kicked(5),
            },
            "kicked (reason 5): idle",
        );
        assert_eq!(kick.kind, IssueKind::Kicked);
        assert_eq!(kick.severity, Severity::High);
        let stall = stall_report(Stall {
            quiet_ms: 12_000,
            rtt_ms: 0,
            suspect: Suspect::Server,
        });
        assert_eq!(stall.kind, IssueKind::ServerStall);
        assert_eq!(stall.key, "phase=world");
        assert!(stall.actual.contains("12 s"));
    }

    #[test]
    fn a_malformed_packet_report_keeps_the_head_of_the_body_and_its_opcode_as_key() {
        let r = malformed_report(1, 0x123, "unexpected end of packet", &[1, 2, 3, 4]);
        assert_eq!(r.kind, IssueKind::MalformedPacket);
        assert_eq!(r.key, "level=1 opcode=0x123");
        assert_eq!(r.packet_head_hex.as_deref(), Some("01020304"));
        assert_eq!((r.severity, r.suspect), (Severity::High, Suspect::Unknown));
        assert!(r.kind.always_report());
        let again = malformed_report(1, 0x123, "other text", &[9; 50]);
        assert_eq!(r.fingerprint(), again.fingerprint());
    }
}
