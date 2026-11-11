//! Compiled rotations: the immutable form of `data/rotations/*.json`.
//!
//! A rotation file is a list of **rules** (a condition and the rows to cast when it holds, each
//! row with a relevance) plus a weighted **default** list used as filler. [`Rotation::compile`]
//! parses one file, resolves the skill aliases to skill ids, interns group and chain names and
//! flattens the rules into entries ordered by relevance. The result is shared between all bots of
//! an archetype through an `Arc`; the per-bot mutable parts (cooldown stamps, combo, chain) live
//! in [`RotState`](super::RotState).
//!
//! Anything the runtime cannot interpret (an unknown condition kind, an unknown action, a skill
//! alias that is not in the file's `skills` map) makes the affected row be ignored and puts a
//! line in [`Rotation::warnings`]; the rest of the file still works.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{Map, Value};

/// Most distinct group names / chain ids / timers a rotation may use (they index small arrays).
const MAX_INTERNED: usize = 250;

/// How a `move` row positions the bot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveMode {
    /// Walk next to the target.
    Melee,
    /// Walk into the range of the anchor skill.
    SpellRange,
    /// Step behind the target (not simulated: the server has no facing rules for bots).
    Behind,
    /// Face the target.
    Facing,
    /// Back off a few metres.
    Away,
}

/// What a row does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Cast a skill on the target.
    Cast,
    /// Cast a melee skill on the target.
    CastMelee,
    /// Cast a buff (on the bot).
    CastBuff,
    /// Cast a debuff or crowd control on the target.
    CastDebuff,
    /// Cast an area skill.
    CastAoe,
    /// Cast a heal (on the party member with the fewest hit points, the bot itself without a party).
    CastHeal,
    /// Cast, walking into range first when needed.
    ReachAndCast,
    /// Keep the home range of the anchor skill.
    MaintainRange,
    /// Position the bot.
    Move(MoveMode),
    /// Use the basic attack.
    AutoAttack,
}

impl Action {
    /// Whether the row casts `skill`.
    pub fn casts(self) -> bool {
        matches!(
            self,
            Action::Cast
                | Action::CastMelee
                | Action::CastBuff
                | Action::CastDebuff
                | Action::CastAoe
                | Action::CastHeal
                | Action::ReachAndCast
        )
    }
}

/// A resource a `resource` condition reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stat {
    /// Hit points.
    Hp,
    /// Mana.
    Mp,
    /// Labor points.
    Labor,
}

/// A condition of a rule or row.
#[derive(Debug, Clone, PartialEq)]
pub enum Cond {
    /// Always true (a row without `when`).
    Always,
    /// Never true (a condition that could not be compiled sensibly).
    Never,
    /// The skill can be cast now: learned, off cooldown, enough mana, target in range.
    CanCast(u32),
    /// Same check as [`Cond::CanCast`] (the original differs only in name).
    CooldownReady(u32),
    /// The target is within `min..=max` metres (defaults: the skill's own range, else unbounded).
    Range {
        /// Skill whose range is the default.
        skill: Option<u32>,
        /// Lower bound.
        min: Option<f32>,
        /// Upper bound.
        max: Option<f32>,
    },
    /// At least `min` hostile units within `radius` metres of the bot.
    EnemyCount {
        /// Needed count.
        min: u32,
        /// Radius in metres.
        radius: f32,
    },
    /// The skill's buff is not on the bot (or runs out within `refresh_ms`).
    BuffMissing {
        /// Skill that creates the buff.
        skill: u32,
        /// Counts as missing when it expires within this many ms.
        refresh_ms: u32,
    },
    /// The skill's buff is on the bot.
    BuffPresent(u32),
    /// The skill's debuff is not on the target (or runs out within `min_life_ms`).
    DebuffMissing {
        /// Skill that creates the debuff.
        skill: u32,
        /// Counts as missing when it expires within this many ms.
        min_life_ms: u32,
    },
    /// The bot's hit points are within `min..=max` percent.
    HealthBand {
        /// Lower bound.
        min: f32,
        /// Upper bound.
        max: f32,
    },
    /// The party member with the fewest hit points is within `min..=max` percent (the bot itself
    /// without a party).
    PartyLowest {
        /// Lower bound.
        min: f32,
        /// Upper bound.
        max: f32,
    },
    /// There is a living target.
    HasTarget,
    /// The target is a player character.
    Pvp,
    /// The bot is stunned (best effort: the server refused a cast with a stun result).
    Stunned,
    /// The bot cannot act normally (stunned, rooted, silenced; best effort).
    Controlled,
    /// The bot has a debuff a cleanse skill would remove (best effort, same as `Controlled`).
    Cleansable,
    /// No row of the group cast within the last `ms`.
    GroupCooldown {
        /// Interned group name.
        group: u8,
        /// Quiet time in ms.
        ms: u32,
    },
    /// A combo is running (with this opener when given).
    ComboActive(Option<u32>),
    /// The chain is at stage `steps` for at least `delay_ms`.
    ChainStep {
        /// Interned chain id.
        id: u8,
        /// Stage that must be reached.
        steps: u8,
        /// Time since the previous stage.
        delay_ms: u32,
    },
    /// Fires at most every `every_ms`, with probability `probability`.
    Timer {
        /// Index into the per-bot timer stamps.
        idx: u8,
        /// Minimum time between two firings.
        every_ms: u32,
        /// Chance to fire when due.
        probability: f32,
    },
    /// A resource of the bot is within `min..=max`.
    Resource {
        /// Which resource.
        stat: Stat,
        /// Lower bound.
        min: f32,
        /// Upper bound.
        max: f32,
    },
    /// The target is casting (best effort: a cast the bot saw start).
    TargetCasting,
    /// All children hold.
    All(Box<[Cond]>),
    /// At least one child holds.
    Any(Box<[Cond]>),
    /// The child does not hold.
    Not(Box<Cond>),
}

/// One castable row, flattened out of a rule or the default list.
#[derive(Debug, Clone)]
pub struct Entry {
    /// `as` of the file, else `<action>:<skill alias>`; for logs.
    pub name: Box<str>,
    /// What the row does.
    pub action: Action,
    /// The skill (0 for rows without one).
    pub skill: u32,
    /// Relevance: higher goes first (rules are 12 and above, the filler is 11).
    pub rel: f32,
    /// Filler weight (>= 1 after rounding).
    pub weight: f32,
    /// The row ignores the global cooldown between skills.
    pub ignore_gcd: bool,
    /// The row may be cast while stunned or silenced.
    pub while_controlled: bool,
    /// Rule condition and row condition together.
    pub cond: Cond,
    /// `(opener, follow_up)` skill ids: casting this row starts that combo.
    pub combo: Option<(u32, u32)>,
    /// `(chain id, stage)` this row sets.
    pub chain: Option<(u8, u8)>,
    /// Group cooldowns stamped when the row is cast.
    pub claims: Box<[u8]>,
}

/// A compiled rotation file. See the module docs.
#[derive(Debug, Clone)]
pub struct Rotation {
    id: Box<str>,
    /// Rule entries, highest relevance first.
    pub(crate) rules: Box<[Entry]>,
    /// Weighted filler rows.
    pub(crate) filler: Box<[Entry]>,
    anchor: Option<u32>,
    skills: Box<[u32]>,
    buff_skills: Box<[u32]>,
    max_enemy_radius: f32,
    timers: u8,
    warnings: Vec<String>,
}

impl Rotation {
    /// Parses and compiles one rotation file. An error means the text is not a rotation at all;
    /// problems with single rows are reported through [`Rotation::warnings`].
    pub fn compile(json: &str) -> Result<Rotation, String> {
        let raw: RawRotation = serde_json::from_str(json).map_err(|e| e.to_string())?;
        if raw.id.trim().is_empty() {
            return Err("the rotation has no id".into());
        }
        Ok(Compiler::new(&raw).run(&raw))
    }

    /// The rotation id (`abolisher.tank`).
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Rows ignored and other oddities found while compiling.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// The `homeAnchorSkill`: the skill whose range the bot keeps to its target.
    pub fn anchor(&self) -> Option<u32> {
        self.anchor
    }

    /// Every skill the rotation can cast, ascending.
    pub fn skills(&self) -> &[u32] {
        &self.skills
    }

    /// Skills named by buff and debuff conditions, ascending (their casts are time-stamped).
    pub fn buff_skills(&self) -> &[u32] {
        &self.buff_skills
    }

    /// The largest radius an `enemyCount` condition asks about (0 when there is none).
    pub fn max_enemy_radius(&self) -> f32 {
        self.max_enemy_radius
    }

    /// How many `timer` conditions there are.
    pub fn timer_count(&self) -> u8 {
        self.timers
    }

    /// Whether the rotation can cast `skill`.
    pub fn uses_skill(&self, skill: u32) -> bool {
        self.skills.binary_search(&skill).is_ok()
    }

    /// The first skill the rotation casts as a heal (`castHeal`), for the rest goal's self heal.
    pub fn heal_skill(&self) -> Option<u32> {
        self.rules
            .iter()
            .chain(self.filler.iter())
            .find(|e| e.action == Action::CastHeal && e.skill != 0)
            .map(|e| e.skill)
    }
}

// ---- the file format -------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawRotation {
    id: String,
    #[serde(default)]
    meta: RawMeta,
    #[serde(default)]
    skills: HashMap<String, u32>,
    #[serde(default)]
    default: Vec<RawRow>,
    #[serde(default)]
    rules: Vec<RawRule>,
}

#[derive(Debug, Default, Deserialize)]
struct RawMeta {
    #[serde(default, rename = "homeAnchorSkill")]
    home_anchor_skill: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawRule {
    #[serde(default)]
    when: Option<RawWhen>,
    #[serde(default)]
    then: Vec<RawRow>,
}

#[derive(Debug, Deserialize)]
struct RawRow {
    action: String,
    #[serde(default)]
    skill: Option<String>,
    #[serde(default)]
    rel: f32,
    #[serde(default)]
    weight: Option<f32>,
    #[serde(default, rename = "as")]
    name: Option<String>,
    #[serde(default, rename = "castWhileControlled")]
    cast_while_controlled: bool,
    #[serde(default)]
    when: Option<RawWhen>,
    #[serde(default)]
    combo: Option<RawCombo>,
    #[serde(default, rename = "ignoreGlobalDelay")]
    ignore_global_delay: bool,
    #[serde(default)]
    chain: Option<RawChain>,
}

#[derive(Debug, Deserialize)]
struct RawCombo {
    opener: String,
    #[serde(rename = "followUp")]
    follow_up: String,
}

#[derive(Debug, Deserialize)]
struct RawChain {
    id: String,
    #[serde(default)]
    stage: u32,
}

#[derive(Debug, Deserialize)]
struct RawWhen {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    children: Vec<RawWhen>,
    #[serde(flatten)]
    args: Map<String, Value>,
}

impl RawWhen {
    fn str_arg(&self, name: &str) -> Option<&str> {
        self.args.get(name).and_then(Value::as_str)
    }

    fn num_arg(&self, name: &str) -> Option<f64> {
        self.args.get(name).and_then(Value::as_f64)
    }
}

// ---- compiling -------------------------------------------------------------------------------

struct Compiler {
    skills: HashMap<String, u32>,
    groups: Vec<String>,
    chains: Vec<String>,
    timers: u8,
    max_enemy_radius: f32,
    buff_skills: Vec<u32>,
    warnings: Vec<String>,
}

impl Compiler {
    fn new(raw: &RawRotation) -> Compiler {
        Compiler {
            skills: raw
                .skills
                .iter()
                .map(|(k, v)| (k.to_ascii_lowercase(), *v))
                .collect(),
            groups: Vec::new(),
            chains: Vec::new(),
            timers: 0,
            max_enemy_radius: 0.0,
            buff_skills: Vec::new(),
            warnings: Vec::new(),
        }
    }

    fn run(mut self, raw: &RawRotation) -> Rotation {
        let mut filler = Vec::new();
        for row in &raw.default {
            if let Some(e) = self.entry(row, None, None) {
                filler.push(e);
            }
        }
        let mut rules = Vec::new();
        for rule in &raw.rules {
            let rule_skill = rule
                .then
                .iter()
                .find_map(|r| r.skill.as_deref().and_then(|s| self.resolve(s)));
            let cond = match &rule.when {
                Some(w) => match self.cond(w, rule_skill) {
                    Ok(c) => c,
                    Err(why) => {
                        self.warnings
                            .push(format!("rule ignored ({} rows): {why}", rule.then.len()));
                        continue;
                    }
                },
                None => Cond::Always,
            };
            for row in &rule.then {
                if let Some(e) = self.entry(row, Some(&cond), rule.when.as_ref()) {
                    rules.push(e);
                }
            }
        }
        // Highest relevance first; equal relevance keeps the file order.
        rules.sort_by(|a, b| b.rel.total_cmp(&a.rel));
        let anchor = raw
            .meta
            .home_anchor_skill
            .as_deref()
            .and_then(|s| self.resolve(s));
        let mut skills: Vec<u32> = rules
            .iter()
            .chain(filler.iter())
            .filter(|e| e.skill != 0)
            .map(|e| e.skill)
            .collect();
        skills.sort_unstable();
        skills.dedup();
        self.buff_skills.sort_unstable();
        self.buff_skills.dedup();
        Rotation {
            id: raw.id.clone().into_boxed_str(),
            rules: rules.into_boxed_slice(),
            filler: filler.into_boxed_slice(),
            anchor,
            skills: skills.into_boxed_slice(),
            buff_skills: self.buff_skills.into_boxed_slice(),
            max_enemy_radius: self.max_enemy_radius,
            timers: self.timers,
            warnings: self.warnings,
        }
    }

    fn resolve(&self, alias: &str) -> Option<u32> {
        self.skills.get(&alias.to_ascii_lowercase()).copied()
    }

    /// Compiles one row; `rule` is the enclosing rule's condition (already compiled) and
    /// `rule_when` its source, whose group cooldowns the row claims on success.
    fn entry(
        &mut self,
        row: &RawRow,
        rule: Option<&Cond>,
        rule_when: Option<&RawWhen>,
    ) -> Option<Entry> {
        let action = match self.action(row) {
            Ok(a) => a,
            Err(why) => {
                self.warnings.push(why);
                return None;
            }
        };
        let skill = match (action.casts(), row.skill.as_deref()) {
            (true, Some(alias)) => match self.resolve(alias) {
                Some(id) => id,
                None => {
                    self.warnings
                        .push(format!("row ignored: unknown skill '{alias}'"));
                    return None;
                }
            },
            (true, None) => {
                self.warnings
                    .push(format!("row ignored: {:?} without a skill", row.action));
                return None;
            }
            (false, Some(alias))
                if matches!(action, Action::MaintainRange | Action::AutoAttack) =>
            {
                self.resolve(alias).unwrap_or(0)
            }
            (false, _) => 0,
        };
        let row_cond = match &row.when {
            Some(w) => match self.cond(w, Some(skill).filter(|s| *s != 0)) {
                Ok(c) => Some(c),
                Err(why) => {
                    self.warnings.push(format!("row ignored: {why}"));
                    return None;
                }
            },
            None => None,
        };
        let cond = match (rule, row_cond) {
            (Some(Cond::Always) | None, None) => Cond::Always,
            (Some(r), None) => r.clone(),
            (None, Some(c)) => c,
            (Some(Cond::Always), Some(c)) => c,
            (Some(r), Some(c)) => Cond::All(vec![r.clone(), c].into_boxed_slice()),
        };
        let combo = match &row.combo {
            Some(c) => match (self.resolve(&c.opener), self.resolve(&c.follow_up)) {
                (Some(o), Some(f)) => Some((o, f)),
                _ => {
                    self.warnings.push(format!(
                        "combo of '{}' ignored: unknown skill",
                        row.skill.as_deref().unwrap_or("?")
                    ));
                    None
                }
            },
            None => None,
        };
        let chain = row.chain.as_ref().and_then(|c| {
            let id = self.intern_chain(&c.id)?;
            Some((id, c.stage.min(255) as u8))
        });
        let mut claims = Vec::new();
        for w in [rule_when, row.when.as_ref()].into_iter().flatten() {
            self.collect_groups(w, &mut claims);
        }
        claims.sort_unstable();
        claims.dedup();
        let name = row
            .name
            .clone()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| format!("{}:{}", row.action, row.skill.as_deref().unwrap_or("-")));
        Some(Entry {
            name: name.into_boxed_str(),
            action,
            skill,
            rel: row.rel,
            weight: row.weight.filter(|w| *w > 0.0).unwrap_or(1.0),
            ignore_gcd: row.ignore_global_delay,
            while_controlled: row.cast_while_controlled,
            cond,
            combo,
            chain,
            claims: claims.into_boxed_slice(),
        })
    }

    fn action(&self, row: &RawRow) -> Result<Action, String> {
        Ok(match row.action.as_str() {
            "cast" => Action::Cast,
            "castMelee" => Action::CastMelee,
            "castBuff" => Action::CastBuff,
            "castDebuff" => Action::CastDebuff,
            "castAoe" => Action::CastAoe,
            "castHeal" => Action::CastHeal,
            "reachAndCast" => Action::ReachAndCast,
            "maintainRange" => Action::MaintainRange,
            "autoAttack" => Action::AutoAttack,
            "move" => match row.skill.as_deref().map(str::to_ascii_lowercase).as_deref() {
                Some("melee") | None => Action::Move(MoveMode::Melee),
                Some("spellrange" | "spell-range") => Action::Move(MoveMode::SpellRange),
                Some("behind") => Action::Move(MoveMode::Behind),
                Some("facing") => Action::Move(MoveMode::Facing),
                Some("away" | "flee") => Action::Move(MoveMode::Away),
                Some(other) => return Err(format!("row ignored: unknown move mode '{other}'")),
            },
            other => return Err(format!("row ignored: unknown action '{other}'")),
        })
    }

    fn intern_group(&mut self, name: &str) -> Option<u8> {
        intern(&mut self.groups, name, &mut self.warnings, "group")
    }

    fn intern_chain(&mut self, name: &str) -> Option<u8> {
        intern(&mut self.chains, name, &mut self.warnings, "chain")
    }

    fn collect_groups(&mut self, w: &RawWhen, out: &mut Vec<u8>) {
        if w.kind.eq_ignore_ascii_case("groupCooldown") {
            if let Some(g) = w.str_arg("group").and_then(|g| self.intern_group(g)) {
                out.push(g);
            }
        }
        for c in &w.children {
            self.collect_groups(c, out);
        }
    }

    /// The skill a condition is about: its own `skill` / `spell` / `opener` argument, else the
    /// enclosing rule's (first) skill. `Err` when the argument names an unknown alias.
    fn skill_arg(&self, w: &RawWhen, fallback: Option<u32>) -> Result<Option<u32>, String> {
        match ["skill", "spell", "opener"]
            .iter()
            .find_map(|k| w.str_arg(k))
        {
            Some(alias) => self
                .resolve(alias)
                .map(Some)
                .ok_or_else(|| format!("unknown skill '{alias}' in '{}'", w.kind)),
            None => Ok(fallback),
        }
    }

    fn cond(&mut self, w: &RawWhen, rule_skill: Option<u32>) -> Result<Cond, String> {
        let kind = w.kind.to_ascii_lowercase();
        let num = |name: &str| w.num_arg(name);
        // `range` may name pseudo skills ("behind", "facing") that only carry min and max.
        let skill = if kind == "range" {
            self.skill_arg(w, rule_skill).unwrap_or(None)
        } else {
            self.skill_arg(w, rule_skill)?
        };
        let need_skill = |what: &str| skill.ok_or_else(|| format!("'{what}' needs a skill"));
        Ok(match kind.as_str() {
            "cancast" => Cond::CanCast(need_skill("canCast")?),
            "cooldownready" => Cond::CooldownReady(need_skill("cooldownReady")?),
            "range" => {
                let (min, max) = (num("min").map(|v| v as f32), num("max").map(|v| v as f32));
                if skill.is_none() && min.is_none() && max.is_none() {
                    return Err("'range' without a skill or bounds".into());
                }
                Cond::Range { skill, min, max }
            }
            "healthband" => Cond::HealthBand {
                min: num("min").unwrap_or(0.0) as f32,
                max: num("max").unwrap_or(100.0) as f32,
            },
            "partylowest" => Cond::PartyLowest {
                min: num("min").unwrap_or(0.0) as f32,
                max: num("max").unwrap_or(100.0) as f32,
            },
            "hastarget" => Cond::HasTarget,
            "pvp" => Cond::Pvp,
            "stunned" => Cond::Stunned,
            "controlled" => Cond::Controlled,
            "hascleansabledebuff" => Cond::Cleansable,
            "not" => match w.children.as_slice() {
                [only] => Cond::Not(Box::new(self.cond(only, rule_skill)?)),
                _ => return Err("'not' needs exactly one child".into()),
            },
            "all" | "any" => {
                let mut kids = Vec::with_capacity(w.children.len());
                for c in &w.children {
                    kids.push(self.cond(c, rule_skill)?);
                }
                let kids = kids.into_boxed_slice();
                if kind == "all" {
                    Cond::All(kids)
                } else {
                    Cond::Any(kids)
                }
            }
            "groupcooldown" => {
                let group = w
                    .str_arg("group")
                    .and_then(|g| self.intern_group(g))
                    .ok_or("'groupCooldown' needs a group")?;
                let ms = num("ms").or(num("cooldownMs")).unwrap_or(0.0).max(0.0);
                Cond::GroupCooldown {
                    group,
                    ms: ms.min(f64::from(u32::MAX)) as u32,
                }
            }
            "comboactive" => Cond::ComboActive(skill),
            "chainstep" => {
                let id = w
                    .str_arg("id")
                    .and_then(|c| self.intern_chain(c))
                    .ok_or("'chainStep' needs an id")?;
                Cond::ChainStep {
                    id,
                    steps: num("steps").unwrap_or(0.0).clamp(0.0, 255.0) as u8,
                    delay_ms: num("stepDelayMs").unwrap_or(0.0).clamp(0.0, 3.6e6) as u32,
                }
            }
            "timer" => {
                let idx = self.timers;
                self.timers = self.timers.saturating_add(1);
                Cond::Timer {
                    idx,
                    every_ms: num("every").unwrap_or(1000.0).clamp(1.0, 3.6e6) as u32,
                    probability: num("probability").unwrap_or(1.0).clamp(0.0, 1.0) as f32,
                }
            }
            "resource" => {
                let stat = match w.str_arg("stat").map(str::to_ascii_lowercase).as_deref() {
                    Some("mp" | "mana") => Stat::Mp,
                    Some("labor") => Stat::Labor,
                    Some("hp" | "health") | None => Stat::Hp,
                    Some(other) => return Err(format!("unknown stat '{other}'")),
                };
                Cond::Resource {
                    stat,
                    min: num("min").map_or(f32::MIN, |v| v as f32),
                    max: num("max").map_or(f32::MAX, |v| v as f32),
                }
            }
            "enemycount" => {
                let radius = num("radius").unwrap_or(10.0).clamp(0.5, 60.0) as f32;
                self.max_enemy_radius = self.max_enemy_radius.max(radius);
                Cond::EnemyCount {
                    min: num("min").unwrap_or(1.0).clamp(0.0, 255.0) as u32,
                    radius,
                }
            }
            "targetcasting" => Cond::TargetCasting,
            "buffmissing" | "hasnoaura" => {
                let skill = need_skill("buffMissing")?;
                self.buff_skills.push(skill);
                Cond::BuffMissing {
                    skill,
                    refresh_ms: num("refreshBefore").unwrap_or(0.0).clamp(0.0, 3.6e6) as u32,
                }
            }
            "buffpresent" | "hasaura" => {
                let skill = need_skill("buffPresent")?;
                self.buff_skills.push(skill);
                Cond::BuffPresent(skill)
            }
            "debuffmissing" => {
                let skill = need_skill("debuffMissing")?;
                self.buff_skills.push(skill);
                Cond::DebuffMissing {
                    skill,
                    min_life_ms: num("minLifetime").unwrap_or(0.0).clamp(0.0, 3.6e6) as u32,
                }
            }
            other => return Err(format!("unknown condition kind '{other}'")),
        })
    }
}

fn intern(
    list: &mut Vec<String>,
    name: &str,
    warnings: &mut Vec<String>,
    what: &str,
) -> Option<u8> {
    if let Some(i) = list.iter().position(|n| n.eq_ignore_ascii_case(name)) {
        return Some(i as u8);
    }
    if list.len() >= MAX_INTERNED {
        warnings.push(format!("too many {what} names; '{name}' ignored"));
        return None;
    }
    list.push(name.to_owned());
    Some((list.len() - 1) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile(json: &str) -> Rotation {
        Rotation::compile(json).unwrap()
    }

    #[test]
    fn a_small_rotation_compiles_in_relevance_order() {
        let r = compile(
            r#"{
              "id": "t", "meta": {"homeAnchorSkill": "bolt"},
              "skills": {"Bolt": 10, "heal": 20, "Shield": 30},
              "default": [
                {"action": "cast", "skill": "bolt", "rel": 11, "weight": 3},
                {"action": "cast", "skill": "heal", "rel": 11, "when": {"kind": "pvp"}}
              ],
              "rules": [
                {"when": {"kind": "buffMissing", "spell": "shield"},
                 "then": [{"action": "castBuff", "skill": "shield", "rel": 25}]},
                {"when": {"kind": "healthBand", "min": 0, "max": 50},
                 "then": [{"action": "castHeal", "skill": "heal", "rel": 40}]}
              ]
            }"#,
        );
        assert!(r.warnings().is_empty(), "{:?}", r.warnings());
        assert_eq!(r.id(), "t");
        assert_eq!(r.anchor(), Some(10));
        assert_eq!(r.skills(), &[10, 20, 30]);
        assert_eq!(r.buff_skills(), &[30]);
        let order: Vec<u32> = r.rules.iter().map(|e| e.skill).collect();
        assert_eq!(order, [20, 30], "the heal (40) comes before the buff (25)");
        assert_eq!(
            r.rules[1].cond,
            Cond::BuffMissing {
                skill: 30,
                refresh_ms: 0
            }
        );
        assert_eq!(r.filler.len(), 2);
        assert_eq!(r.filler[0].weight, 3.0);
        assert_eq!(r.filler[1].cond, Cond::Pvp);
        assert_eq!(&*r.filler[0].name, "cast:bolt");
        assert_eq!(r.heal_skill(), Some(20), "the castHeal row's skill");
    }

    #[test]
    fn unknown_things_are_warned_about_and_dropped() {
        let r = compile(
            r#"{
              "id": "t", "skills": {"a": 1, "b": 2},
              "default": [
                {"action": "dance", "skill": "a", "rel": 11},
                {"action": "cast", "skill": "nope", "rel": 11},
                {"action": "cast", "rel": 11},
                {"action": "cast", "skill": "a", "rel": 11}
              ],
              "rules": [
                {"when": {"kind": "moonPhase"}, "then": [{"action": "cast", "skill": "a", "rel": 30}]},
                {"when": {"kind": "canCast", "skill": "ghost"}, "then": [{"action": "cast", "skill": "a", "rel": 31}]},
                {"when": {"kind": "not", "children": []}, "then": [{"action": "cast", "skill": "a", "rel": 32}]},
                {"when": {"kind": "canCast", "skill": "b"}, "then": [{"action": "cast", "skill": "b", "rel": 33}]}
              ]
            }"#,
        );
        assert_eq!(r.filler.len(), 1);
        assert_eq!(r.rules.len(), 1);
        assert_eq!(r.rules[0].skill, 2);
        assert_eq!(r.warnings().len(), 6, "{:?}", r.warnings());
        assert!(r.warnings().iter().any(|w| w.contains("moonphase")));
        assert!(r.warnings().iter().any(|w| w.contains("dance")));
        assert!(r.warnings().iter().any(|w| w.contains("ghost")));
    }

    #[test]
    fn rule_and_row_conditions_are_combined_and_groups_are_interned() {
        let r = compile(
            r#"{
              "id": "t", "skills": {"a": 1, "b": 2},
              "rules": [
                {"when": {"kind": "all", "children": [
                   {"kind": "canCast", "skill": "a"},
                   {"kind": "groupCooldown", "group": "G", "ms": 4000}]},
                 "then": [
                   {"action": "cast", "skill": "a", "rel": 20},
                   {"action": "cast", "skill": "b", "rel": 19, "when": {"kind": "groupCooldown", "group": "H", "ms": 10}}]}
              ]
            }"#,
        );
        assert!(r.warnings().is_empty(), "{:?}", r.warnings());
        assert_eq!(&*r.rules[0].claims, &[0]);
        assert_eq!(&*r.rules[1].claims, &[0, 1]);
        assert!(matches!(r.rules[1].cond, Cond::All(ref k) if k.len() == 2));
        // the rule condition alone for a row without its own
        assert!(matches!(r.rules[0].cond, Cond::All(ref k) if k.len() == 2));
    }

    #[test]
    fn move_rows_modes_and_pseudo_skill_ranges() {
        let r = compile(
            r#"{
              "id": "t", "skills": {"a": 1},
              "rules": [
                {"when": {"kind": "range", "skill": "behind", "min": 0, "max": 2},
                 "then": [{"action": "move", "skill": "behind", "rel": 13}]},
                {"when": {"kind": "range", "skill": "a"},
                 "then": [{"action": "move", "skill": "facing", "rel": 12}]},
                {"when": {"kind": "hasTarget"},
                 "then": [{"action": "move", "skill": "sideways", "rel": 12}]}
              ]
            }"#,
        );
        assert_eq!(r.rules.len(), 2);
        assert_eq!(r.rules[0].action, Action::Move(MoveMode::Behind));
        assert_eq!(
            r.rules[0].cond,
            Cond::Range {
                skill: None,
                min: Some(0.0),
                max: Some(2.0)
            }
        );
        assert_eq!(r.rules[1].action, Action::Move(MoveMode::Facing));
        assert_eq!(r.warnings().len(), 1);
    }

    #[test]
    fn combo_chain_and_timer_arguments() {
        let r = compile(
            r#"{
              "id": "t", "skills": {"open": 1, "next": 2, "s1": 3},
              "rules": [
                {"when": {"kind": "comboActive", "opener": "open"},
                 "then": [{"action": "castMelee", "skill": "next", "rel": 49, "ignoreGlobalDelay": true}]},
                {"when": {"kind": "chainStep", "id": "tri", "steps": 1, "stepDelayMs": 800},
                 "then": [{"action": "castMelee", "skill": "s1", "rel": 44, "chain": {"id": "tri", "stage": 2}}]},
                {"when": {"kind": "timer", "every": 5000, "probability": 0.5},
                 "then": [{"action": "cast", "skill": "open", "rel": 20,
                           "combo": {"opener": "open", "followUp": "next"}}]},
                {"when": {"kind": "resource", "stat": "mp", "min": 20},
                 "then": [{"action": "cast", "skill": "s1", "rel": 19}]}
              ]
            }"#,
        );
        assert!(r.warnings().is_empty(), "{:?}", r.warnings());
        let by_skill = |s: u32, rel: f32| {
            r.rules
                .iter()
                .find(|e| e.skill == s && e.rel == rel)
                .unwrap()
        };
        assert_eq!(by_skill(2, 49.0).cond, Cond::ComboActive(Some(1)));
        assert!(by_skill(2, 49.0).ignore_gcd);
        assert_eq!(
            by_skill(3, 44.0).cond,
            Cond::ChainStep {
                id: 0,
                steps: 1,
                delay_ms: 800
            }
        );
        assert_eq!(by_skill(3, 44.0).chain, Some((0, 2)));
        assert_eq!(by_skill(1, 20.0).combo, Some((1, 2)));
        assert_eq!(
            by_skill(1, 20.0).cond,
            Cond::Timer {
                idx: 0,
                every_ms: 5000,
                probability: 0.5
            }
        );
        assert_eq!(r.timer_count(), 1);
        assert_eq!(
            by_skill(3, 19.0).cond,
            Cond::Resource {
                stat: Stat::Mp,
                min: 20.0,
                max: f32::MAX
            }
        );
    }

    #[test]
    fn a_condition_without_a_skill_uses_the_rules_first_skill() {
        let r = compile(
            r#"{
              "id": "t", "skills": {"a": 1},
              "rules": [{"when": {"kind": "canCast"}, "then": [{"action": "cast", "skill": "a", "rel": 20}]}]
            }"#,
        );
        assert_eq!(r.rules[0].cond, Cond::CanCast(1));
    }

    #[test]
    fn broken_files_are_errors() {
        assert!(Rotation::compile("not json").is_err());
        assert!(Rotation::compile(r#"{"skills": {}}"#).is_err());
        assert!(Rotation::compile(r#"{"id": "  "}"#).is_err());
        assert!(Rotation::compile(r#"{"id": "x"}"#)
            .unwrap()
            .rules
            .is_empty());
    }

    #[test]
    fn enemy_radius_and_nulls_in_the_shipped_style_parse() {
        let r = compile(
            r#"{
              "id": "t", "skills": {"a": 1},
              "default": [{"action": "cast", "skill": "a", "rel": 11.0, "weight": 81.8, "as": null, "castWhileControlled": false}],
              "rules": [{"when": {"kind": "enemyCount", "children": [], "min": 3, "radius": 8},
                         "then": [{"action": "castAoe", "skill": "a", "rel": 27.0, "weight": 1.0, "as": "aoe", "castWhileControlled": false}]}]
            }"#,
        );
        assert!(r.warnings().is_empty(), "{:?}", r.warnings());
        assert_eq!(r.max_enemy_radius(), 8.0);
        assert_eq!(
            r.rules[0].cond,
            Cond::EnemyCount {
                min: 3,
                radius: 8.0
            }
        );
        assert_eq!(&*r.rules[0].name, "aoe");
        assert_eq!(&*r.filler[0].name, "cast:a");
    }
}
