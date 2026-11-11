//! Who may be attacked: pure functions over the facts of a character in view.
//!
//! The rules follow the server's `can_attack` (`systems/skills/relation.rs`):
//!
//! * a character of a hostile alliance is a target; a friendly or neutral one never is, and the
//!   bots never use the force attack (`CSSetForceAttackPacket`) to change that;
//! * a character whose alliance owns the zone it stands in is protected there (45 home zones; the
//!   conflict zones and the open sea have no owner): the casts are accepted and the damage is
//!   dropped silently, so the bots do not start such a fight;
//! * the town peace aura (buff 2149 and its kin) forbids combat skills.
//!
//! On top of that come the operator's limits from `[pvp]`: `never_names` (game masters, friends)
//! are never attacked, `only_names` narrows the real players that may be, other bots are attacked
//! only with `attack_bots` and real players only with [`Config::may_attack_players`]
//! (`attack_players`, never while the bots are GM-bootstrapped).
//!
//! A bot defends itself ([`Purpose::Defend`]) against a hostile character that attacks it whatever
//! `attack_bots` and `attack_players` say, but still not against a name in `never_names`. A hunt
//! ([`Purpose::Hunt`]: the warzone goal) and an ambush ([`Purpose::Gank`]: only carriers of trade
//! packs) obey all of them.

use aa_world::{Relation, World};

use crate::config::{Config, PvpCfg};

use super::super::state::{Unit, UnitKind, U_PACK};

/// `system_factions` id of the Nuian alliance (the mother faction of the Nuian races).
pub const NUIA: u32 = 148;
/// `system_factions` id of the Haranyan alliance.
pub const HARIHARA: u32 = 149;
/// The neutral root faction; the faction of a zone nobody owns.
const FACTION_NEUTRAL: u32 = 2;
/// Buffs that forbid combat skills: the peace auras of towns (2149 around a Nui statue, 2221,
/// 4828), which the server's `skill_reqs` whitelist lets only a few skills through.
pub const PEACE_BUFFS: [u32; 3] = [2149, 2221, 4828];

/// The alliance a bot of alliance `mine` is hostile to: the other of the two player alliances;
/// 0 for a bot of no alliance (nobody is its enemy).
pub fn enemy_alliance(mine: u32) -> u32 {
    match mine {
        NUIA => HARIHARA,
        HARIHARA => NUIA,
        _ => 0,
    }
}

/// The static facts the rules need. Implemented by [`World`]; the tests use a synthetic one.
pub trait Geo {
    /// How faction `a` regards faction `b`.
    fn relation(&self, a: u32, b: u32) -> Relation;
    /// The alliance (mother faction) of `faction`; 0 when it has none.
    fn mother(&self, faction: u32) -> u32;
    /// The faction that owns the zone at (x, y): `Some(0)` when nobody does (a conflict zone, the
    /// open sea), `None` when the position cannot be judged (no baked zone grid).
    fn zone_owner(&self, x: f32, y: f32) -> Option<u32>;
    /// Whether item `template` is a trade pack some NPC buys.
    fn is_pack(&self, template: u32) -> bool;
}

impl Geo for World {
    fn relation(&self, a: u32, b: u32) -> Relation {
        World::relation(self, a, b)
    }

    fn mother(&self, faction: u32) -> u32 {
        self.mother_faction(faction)
    }

    fn zone_owner(&self, x: f32, y: f32) -> Option<u32> {
        self.baked().zones.as_ref()?;
        Some(self.zone_info_at(x, y).map_or(0, |z| z.faction))
    }

    fn is_pack(&self, template: u32) -> bool {
        !self.specialty_buyers(template).is_empty()
    }
}

/// Why a fight is started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// The character attacked the bot.
    Defend,
    /// A fight in a zone at war.
    Hunt,
    /// An ambush on a carrier of a trade pack.
    Gank,
}

/// What kind of character a target is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Who {
    /// One of our bots.
    Bot,
    /// A real player.
    Player,
}

/// Why a character may not be attacked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// `[pvp] enabled` is off.
    Disabled,
    /// Not a character (a monster, a vehicle).
    NotACharacter,
    /// Already dead.
    Dead,
    /// The name is in `never_names`.
    NeverName,
    /// Our bots do not attack each other (`attack_bots` is off).
    BotsOff,
    /// Real players are not attacked (`attack_players` is off, or the bots are GM-bootstrapped).
    PlayersOff,
    /// `only_names` is set and the player is not in it.
    NotListed,
    /// The character is not of a hostile alliance (friendly, neutral or unknown).
    NotHostile,
    /// The character stands in a zone its alliance owns: the server drops the damage.
    HomeZone,
    /// The zone of the character is not known (no baked zone grid).
    UnknownZone,
    /// An ambush needs a carrier of a trade pack.
    NoPack,
    /// The bot gave up on this character a moment ago.
    Shunned,
    /// The bot itself stands in a town peace aura, which forbids combat skills.
    PeaceArea,
}

impl Why {
    /// A short word for the log.
    pub fn word(self) -> &'static str {
        match self {
            Why::Disabled => "pvp is off",
            Why::NotACharacter => "not a character",
            Why::Dead => "dead",
            Why::NeverName => "name is never attacked",
            Why::BotsOff => "attacking bots is off",
            Why::PlayersOff => "attacking players is off",
            Why::NotListed => "not in only_names",
            Why::NotHostile => "not hostile",
            Why::HomeZone => "in its home zone",
            Why::UnknownZone => "zone unknown",
            Why::NoPack => "no trade pack",
            Why::Shunned => "given up lately",
            Why::PeaceArea => "in a peace area",
        }
    }
}

/// The facts about a unit in view the rules read.
#[derive(Debug, Clone, Copy)]
pub struct Target<'a> {
    /// The unit is a player character.
    pub is_character: bool,
    /// Dead.
    pub dead: bool,
    /// Character name (empty when unknown).
    pub name: &'a str,
    /// Faction id (0 when the server did not say).
    pub faction: u32,
    /// Position.
    pub pos: [f32; 3],
    /// One of our bots.
    pub is_bot: bool,
    /// Template of the item in its backpack slot; 0 for none.
    pub pack: u32,
}

impl<'a> Target<'a> {
    /// The facts of `unit`; `is_bot` says whether our hub knows it as one of our bots, `pack` is
    /// the template in its backpack slot (see [`PvpMem::pack_of`](super::PvpMem::pack_of)).
    pub fn of(unit: &'a Unit, is_bot: bool, pack: u32) -> Target<'a> {
        Target {
            is_character: unit.kind == UnitKind::Character,
            dead: unit.dead(),
            name: unit.name.as_deref().unwrap_or(""),
            faction: unit.faction,
            pos: unit.pos,
            is_bot,
            pack: if unit.flags & U_PACK != 0 { pack } else { 0 },
        }
    }
}

/// The rules for one bot. See the module docs.
pub struct Rules<'a, G: Geo + ?Sized> {
    geo: &'a G,
    cfg: &'a PvpCfg,
    players_ok: bool,
    own_faction: u32,
}

impl<'a> Rules<'a, World> {
    /// The rules of the bot of faction `own_faction` under `cfg`.
    pub fn new(world: &'a World, cfg: &'a Config, own_faction: u32) -> Rules<'a, World> {
        Rules::with(world, &cfg.pvp, cfg.may_attack_players(), own_faction)
    }
}

impl<'a, G: Geo + ?Sized> Rules<'a, G> {
    /// Rules over any [`Geo`]; `players_ok` is [`Config::may_attack_players`].
    pub fn with(geo: &'a G, cfg: &'a PvpCfg, players_ok: bool, own_faction: u32) -> Rules<'a, G> {
        Rules {
            geo,
            cfg,
            players_ok,
            own_faction,
        }
    }

    /// `Ok` (and what kind of character it is) when the bot may attack `t` for `purpose`.
    pub fn check(&self, t: &Target<'_>, purpose: Purpose) -> Result<Who, Why> {
        if !self.cfg.enabled {
            return Err(Why::Disabled);
        }
        if !t.is_character {
            return Err(Why::NotACharacter);
        }
        if t.dead {
            return Err(Why::Dead);
        }
        if listed(&self.cfg.never_names, t.name) {
            return Err(Why::NeverName);
        }
        let who = if t.is_bot { Who::Bot } else { Who::Player };
        if purpose != Purpose::Defend {
            match who {
                Who::Bot if !self.cfg.attack_bots => return Err(Why::BotsOff),
                Who::Player if !self.players_ok => return Err(Why::PlayersOff),
                Who::Player
                    if !self.cfg.only_names.is_empty() && !listed(&self.cfg.only_names, t.name) =>
                {
                    return Err(Why::NotListed);
                }
                _ => {}
            }
        }
        if !self.hostile(t.faction) {
            return Err(Why::NotHostile);
        }
        match home_protected(self.geo, t.faction, t.pos) {
            Some(true) => return Err(Why::HomeZone),
            None if purpose != Purpose::Defend => return Err(Why::UnknownZone),
            _ => {}
        }
        if purpose == Purpose::Gank && !(t.pack != 0 && self.geo.is_pack(t.pack)) {
            return Err(Why::NoPack);
        }
        Ok(who)
    }

    /// Whether a character of `faction` belongs to an alliance hostile to the bot's.
    pub fn hostile(&self, faction: u32) -> bool {
        let (mine, theirs) = (self.geo.mother(self.own_faction), self.geo.mother(faction));
        mine != 0
            && theirs != 0
            && mine != theirs
            && self.geo.relation(self.own_faction, faction) == Relation::Hostile
    }
}

/// Whether the server protects a character of `faction` standing at `pos`: its alliance owns the
/// zone (the server's `can_attack`: the target's mother faction is the zone's faction or its
/// mother). `None` when the zone cannot be judged.
pub fn home_protected<G: Geo + ?Sized>(geo: &G, faction: u32, pos: [f32; 3]) -> Option<bool> {
    let owner = geo.zone_owner(pos[0], pos[1])?;
    let zone_faction = if owner == 0 { FACTION_NEUTRAL } else { owner };
    let mother = geo.mother(faction);
    if mother == 0 {
        return Some(false);
    }
    Some(mother == zone_faction || mother == geo.mother(zone_faction))
}

/// Whether `name` is in `list` (compared without case and surrounding blanks).
pub fn listed(list: &[String], name: &str) -> bool {
    let name = name.trim();
    !name.is_empty() && list.iter().any(|n| n.trim().eq_ignore_ascii_case(name))
}

/// Whether one of the bot's own buffs `(index, buff id)` is a town peace aura.
pub fn in_peace_area(buffs: &[(u32, u32)]) -> bool {
    buffs.iter().any(|b| PEACE_BUFFS.contains(&b.1))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// 101, 102 are Nuian races (mother 148), 108, 109 Haranyan (mother 149); 5 has no mother and
    /// no relation; zones: x < 1000 Nuian home, x >= 5000 Haranyan home, between them no owner;
    /// x > 9000 is outside the grid.
    pub struct Fake {
        pub grid: bool,
    }

    impl Default for Fake {
        fn default() -> Self {
            Fake { grid: true }
        }
    }

    impl Geo for Fake {
        fn relation(&self, a: u32, b: u32) -> Relation {
            let (ma, mb) = (self.mother(a), self.mother(b));
            match (ma, mb) {
                (148, 149) | (149, 148) => Relation::Hostile,
                (x, y) if x != 0 && x == y => Relation::Friendly,
                _ => Relation::Neutral,
            }
        }

        fn mother(&self, f: u32) -> u32 {
            match f {
                101 | 102 => NUIA,
                108 | 109 => HARIHARA,
                _ => 0,
            }
        }

        fn zone_owner(&self, x: f32, _y: f32) -> Option<u32> {
            if !self.grid {
                return None;
            }
            Some(if x < 1000.0 {
                NUIA
            } else if (5000.0..9000.0).contains(&x) {
                HARIHARA
            } else {
                0
            })
        }

        fn is_pack(&self, template: u32) -> bool {
            template == 28_091
        }
    }

    pub fn cfg() -> PvpCfg {
        PvpCfg {
            enabled: true,
            attack_bots: true,
            gank: true,
            warzone: true,
            ..PvpCfg::default()
        }
    }

    pub fn target(name: &str, faction: u32, x: f32) -> Target<'_> {
        Target {
            is_character: true,
            dead: false,
            name,
            faction,
            pos: [x, 0.0, 0.0],
            is_bot: true,
            pack: 0,
        }
    }

    fn check(
        g: &Fake,
        c: &PvpCfg,
        players_ok: bool,
        t: &Target<'_>,
        p: Purpose,
    ) -> Result<Who, Why> {
        Rules::with(g, c, players_ok, 101).check(t, p)
    }

    #[test]
    fn only_a_hostile_alliance_is_a_target() {
        let (g, c) = (Fake::default(), cfg());
        let mid = 3000.0;
        // Haranyan vs Nuian in a conflict zone
        assert_eq!(
            check(&g, &c, false, &target("Foe", 108, mid), Purpose::Hunt),
            Ok(Who::Bot)
        );
        // the same alliance, another race
        assert_eq!(
            check(&g, &c, false, &target("Pal", 102, mid), Purpose::Hunt),
            Err(Why::NotHostile)
        );
        // an unknown faction (the packet did not say, a duel's temporary faction)
        for f in [0, 5, 999] {
            assert_eq!(
                check(&g, &c, false, &target("Odd", f, mid), Purpose::Hunt),
                Err(Why::NotHostile),
                "faction {f}"
            );
            assert_eq!(
                check(&g, &c, false, &target("Odd", f, mid), Purpose::Defend),
                Err(Why::NotHostile),
                "faction {f}"
            );
        }
        // a bot without a faction attacks nobody
        let r = Rules::with(&g, &c, false, 0);
        assert_eq!(
            r.check(&target("Foe", 108, mid), Purpose::Hunt),
            Err(Why::NotHostile)
        );
    }

    #[test]
    fn a_character_in_its_home_zone_is_protected() {
        let (g, c) = (Fake::default(), cfg());
        // a Haranyan in Haranyan land: the server drops the damage
        let home = target("Foe", 108, 6000.0);
        for p in [Purpose::Hunt, Purpose::Gank, Purpose::Defend] {
            assert_eq!(check(&g, &c, false, &home, p), Err(Why::HomeZone));
        }
        // a Haranyan in Nuian land is fair game, and in a conflict zone too
        assert!(check(&g, &c, false, &target("Foe", 108, 500.0), Purpose::Hunt).is_ok());
        assert!(check(&g, &c, false, &target("Foe", 108, 3000.0), Purpose::Hunt).is_ok());
        // the open sea (outside the grid) has no owner either
        assert!(check(&g, &c, false, &target("Foe", 108, 9500.0), Purpose::Hunt).is_ok());
    }

    #[test]
    fn without_a_zone_grid_only_a_defence_goes_on() {
        let g = Fake { grid: false };
        let c = cfg();
        let t = target("Foe", 108, 3000.0);
        assert_eq!(
            check(&g, &c, false, &t, Purpose::Hunt),
            Err(Why::UnknownZone)
        );
        assert_eq!(
            check(&g, &c, false, &t, Purpose::Gank),
            Err(Why::UnknownZone)
        );
        // (the fight watch ends a defence that hurts nobody)
        assert!(check(&g, &c, false, &t, Purpose::Defend).is_ok());
    }

    #[test]
    fn bots_and_players_have_their_own_switches() {
        let g = Fake::default();
        let mut c = cfg();
        let mut player = target("Real", 108, 3000.0);
        player.is_bot = false;
        let bot = target("Bot", 108, 3000.0);
        // players: off unless the config says so (may_attack_players)
        assert_eq!(
            check(&g, &c, false, &player, Purpose::Hunt),
            Err(Why::PlayersOff)
        );
        assert_eq!(check(&g, &c, true, &player, Purpose::Hunt), Ok(Who::Player));
        // bots: attack_bots
        c.attack_bots = false;
        assert_eq!(check(&g, &c, true, &bot, Purpose::Hunt), Err(Why::BotsOff));
        assert_eq!(check(&g, &c, true, &bot, Purpose::Gank), Err(Why::BotsOff));
        // a defence needs neither
        assert_eq!(check(&g, &c, false, &bot, Purpose::Defend), Ok(Who::Bot));
        assert_eq!(
            check(&g, &c, false, &player, Purpose::Defend),
            Ok(Who::Player)
        );
        // the master switch beats everything
        c.enabled = false;
        assert_eq!(
            check(&g, &c, true, &bot, Purpose::Defend),
            Err(Why::Disabled)
        );
    }

    #[test]
    fn never_names_protect_everybody_and_only_names_narrow_players() {
        let g = Fake::default();
        let mut c = cfg();
        c.never_names = vec!["Game Master".into(), " bob ".into()];
        let mut gm = target("game master", 108, 3000.0);
        gm.is_bot = false;
        for p in [Purpose::Hunt, Purpose::Gank, Purpose::Defend] {
            assert_eq!(check(&g, &c, true, &gm, p), Err(Why::NeverName));
        }
        let bob = target("BOB", 108, 3000.0);
        assert_eq!(
            check(&g, &c, true, &bob, Purpose::Hunt),
            Err(Why::NeverName)
        );
        // only_names: a player must be in it, a bot is not asked
        c.only_names = vec!["Rival".into()];
        let mut other = target("Stranger", 108, 3000.0);
        other.is_bot = false;
        assert_eq!(
            check(&g, &c, true, &other, Purpose::Hunt),
            Err(Why::NotListed)
        );
        let mut rival = target("rival", 108, 3000.0);
        rival.is_bot = false;
        assert_eq!(check(&g, &c, true, &rival, Purpose::Hunt), Ok(Who::Player));
        let bot = target("Stranger", 108, 3000.0);
        assert_eq!(check(&g, &c, true, &bot, Purpose::Hunt), Ok(Who::Bot));
        // a defence is not narrowed by only_names
        assert_eq!(
            check(&g, &c, true, &other, Purpose::Defend),
            Ok(Who::Player)
        );
        // an unnamed character matches no list
        assert!(!listed(&c.never_names, ""));
    }

    #[test]
    fn an_ambush_needs_a_trade_pack() {
        let (g, c) = (Fake::default(), cfg());
        let mut t = target("Carrier", 108, 3000.0);
        assert_eq!(check(&g, &c, false, &t, Purpose::Gank), Err(Why::NoPack));
        t.pack = 777; // a glider or some other backpack
        assert_eq!(check(&g, &c, false, &t, Purpose::Gank), Err(Why::NoPack));
        t.pack = 28_091;
        assert_eq!(check(&g, &c, false, &t, Purpose::Gank), Ok(Who::Bot));
        // a hunt does not care
        t.pack = 0;
        assert_eq!(check(&g, &c, false, &t, Purpose::Hunt), Ok(Who::Bot));
    }

    #[test]
    fn dead_and_non_characters_are_no_targets() {
        let (g, c) = (Fake::default(), cfg());
        let mut t = target("Foe", 108, 3000.0);
        t.dead = true;
        assert_eq!(check(&g, &c, false, &t, Purpose::Hunt), Err(Why::Dead));
        t.dead = false;
        t.is_character = false;
        assert_eq!(
            check(&g, &c, false, &t, Purpose::Hunt),
            Err(Why::NotACharacter)
        );
    }

    #[test]
    fn peace_auras_are_found_in_the_bots_buffs() {
        assert!(!in_peace_area(&[]));
        assert!(!in_peace_area(&[(1, 100), (2, 200)]));
        assert!(in_peace_area(&[(1, 100), (7, 2149)]));
        assert!(in_peace_area(&[(3, 4828)]));
    }

    #[test]
    fn unit_facts_come_from_the_view() {
        use super::super::super::state::{U_DEAD, U_PACK};
        let u = Unit {
            kind: UnitKind::Character,
            id: 77,
            pos: [1.0, 2.0, 3.0],
            hp: 10,
            faction: 108,
            level: 30,
            flags: U_PACK,
            target: 0,
            name: Some("Foe".into()),
        };
        let t = Target::of(&u, false, 28_091);
        assert!(t.is_character && !t.dead && !t.is_bot);
        assert_eq!((t.name, t.faction, t.pack), ("Foe", 108, 28_091));
        // no flag: whatever the memory says is stale
        let mut u2 = u.clone();
        u2.flags = U_DEAD;
        let t = Target::of(&u2, true, 28_091);
        assert!(t.dead && t.is_bot);
        assert_eq!(t.pack, 0);
    }
}
