# aaemu-playerbots — design

Player bots for the AAEmu Rust server (ArcheAge 1.2, r208022). Every bot is a **headless network
client**: it logs in through the login server, creates/selects a character and plays over the
normal client protocol. The server needs no changes; to it a bot is an ordinary player.

The behaviour is a re-implementation of the logic of
[NulightJens/ArcheAge-PlayerBots](https://github.com/NulightJens/ArcheAge-PlayerBots) (a C#
server-side mod, GPL-3.0-or-later) for a client-side bot, plus housing, trade runs, player trade
and an operator web UI, which the original does not have.

## Goals

- Hundreds of bots in one process, small memory per bot (target < 256 KiB of bot-side state).
- Bots quest, grind, duel each other, trade with each other, run trade packs and own houses.
- A web UI shows every bot on the world map with its activity, and accepts commands.
- An optional cheap LLM (any OpenAI-compatible endpoint, e.g. DeepSeek) gives bots chat replies
  and short plans of their next goals; an LLM outside the process can drive them through the
  agent API and MCP. Everything works without it.

## Crates

| Crate | Path | Role |
| --- | --- | --- |
| `aa-proto` | `crates/proto` | Wire protocol: framing, `PacketStream`, position codec, opcode tables, login client, game connection, CS packet builders, SC packet parsers → `ServerEvent`. No game logic. |
| `aa-world` | `crates/world` | Static world knowledge, loaded once and shared read-only (`Arc<World>`): compact.sqlite3 subset (NPCs, factions, quests and their start requirements, skills with mana by level, doodad and item use skills, crafts with their requirements, specialty buyers and pack prices, housing with garden radii), server spawn files, spatial index, terrain heights (mmap of the baked grid), the navigation meshes (`nav`: locate, weighted A* + funnel, component tables; the main world's and the lazily opened instance worlds'), baked extras (housing areas, quest spheres, zones, map calibration). |
| `aa-bake` | `crates/bake` | One-off extraction from the 1.2 client `game_pak` into the data dir: height grid, world map PNG + calibration, housing area polygons, quest sign spheres, zone sector grid, navigation meshes (the client's `.bai` navmesh as one memory-mappable file per world). |
| `pb-web` | `crates/web` | Operator web UI (axum): JSON API + an embedded single-page map UI. Knows nothing about bots except the `BotHub` trait and snapshot types it defines. |
| `pb-llm` | `crates/llm` | Optional OpenAI-compatible chat client with rate limit, concurrency cap and daily token budget. |
| `playerbots` | `crates/playerbots` | The binary: config, roster, bot runtime (connection lifecycle, world view, movement, combat), goals (quest, grind, duel, trade, trade run, housing, rest), coordinator, web hub, CLI (`run`, `bake`, `grant-gm-sql`). |

Dependency direction: `playerbots → {aa-proto, aa-world, aa-bake, pb-web, pb-llm}`; `aa-bake →
aa-world` (only for shared baked file types); nothing else depends on anything internal.

## Runtime model

- One tokio multi-thread runtime (2–4 workers). **One task per bot** owns its TCP connection and
  all its mutable state; it `select!`s over (a) the next server packet, (b) its brain tick timer,
  (c) its command channel. No per-bot reader task, no locks on the hot path.
- `Arc<World>` is shared read-only by all bots.
- Each bot publishes a small `BotSnapshot` (≈100 bytes) into the hub about once per second; the
  web UI only ever reads snapshots.
- Bots that need a partner (duel, trade, party) ask the **coordinator** (one task) over a channel;
  it pairs bots and sends both a command. Web UI commands go through the same per-bot command
  channels.
- Ticks are adaptive with jitter (combat 250 ms, moving 250 ms, idle 1–3 s, far from any real
  player and idle: 5 s), which avoids the C# version's 10-phase thundering herd and its serial
  loop over all bots. A walker that a real player can see also gets movement-only ticks between
  them, every `bots.move_update_ms` (see [Movement updates](#movement-updates-botmotionrs)).
- Bounded everything: entity view (only what the server shows), event log ring (32 lines),
  channels (small capacities).

## Protocol facts (verified against the server source)

- Frames: `[u16 LE len][payload]`. C→S level 1: `DD 01 00 <count> <u16 op> body`; level 2 (proxy):
  `DD 02 <u16 op> body`. No encryption, no checksum. S→C level 4 `DD 04 <u16 n> <deflate>` batches.
- Keep-alive: proxy `PingPacket` at least every 30 s (bots: every 5 s).
- Login: Trion ticket (XML, SHA-256 hex password), `AutoAccount` creates unknown accounts.
- Movement is not validated by the server (no speed clamp or collision check; > 25 m/s is only
  audit-logged). Bots must still move plausibly: run speed ≈ 5.4 m/s, Z from the terrain grid.
- The server checks distances (loot 5 m, specialty sale 2.5 m, skill range) against the position of
  the *last* `CSMoveUnit`, and a walking bot sends one only every second when no player is near. So
  the motion controller sends a final stopped update on arrival and `Motion::pin` puts an exact
  update in front of every packet the server range-checks (loot, talk, quest steps, craft, sell,
  casts); loot windows are opened from at most 3.5 m.
- GM commands are chat lines starting with `/`; the effective access level is
  max(account, character); most useful commands need 100. Bots use them only when
  `gm_bootstrap = true` (see "GM bootstrap").

## Goals and behaviour

Each bot has a **persona**: an archetype (class + rotation, from the original's
`BotArchetypes.json`/`BotRotations`) and temperament weights (quester, duelist, trader, merchant,
homebody). A utility scorer picks the next goal when the current one finishes or fails; the
optional LLM director may plan among the same eligible goals (and concrete variants of them)
instead (see [LLM control and the agent API](#llm-control-and-the-agent-api)).

| Goal | Summary |
| --- | --- |
| Quest | Pick an eligible quest (story first, then nearest side quest), walk to the giver, accept, do the objectives (talk, kill, gather from loot, use item, interact with doodad, reach sphere), turn in. Failures back off; repeated failures quarantine the quest (typed reasons, not strings). |
| Grind | Kill level-appropriate hostile NPCs near the bot (fallback when no quest is eligible). |
| Duel | Coordinator pairs two idle bots; they meet, challenge/accept, fight with their PvP rotation, end on the server's `SCDuelEnded`. |
| Trade | Coordinator pairs two bots; they meet, open a trade window, put up an item and/or money, lock, confirm. |
| Trade run | Walk to a specialty workbench, craft a trade pack (`CSExecuteCraft`), carry it to a specialty NPC that buys it, sell (`CSSellBackpackGoods`), collect the payment mail. A run starts only at 80 % hit points and skips routes through areas far above the bot's level or zones of the hostile alliance. While the pack is carried the run is the only goal (no event, quest, duel, dungeon or other fight, and no portal the pack buff blocks); a bot that dies with the pack goes back to the death spot for the dropped doodad and only writes the pack off when it is gone (logged, and counted in `Ledger::packs_lost`). |
| Housing | Get a design (and tax certificates), walk to a free spot inside a housing area, quote (`CSConstructHouseTax`) and place (`CSCreateHouse`); later "go home" when resting. |
| Farm | Buy seeds, plant them on own land or a public farm, remember the plots, come back when they are ripe, harvest and replant; grows what the population director wishes for or the economy board calls scarce (see [Farming](#farming-botfarm-goal-farm)). |
| Rest | Stand still out of combat until HP/MP recover. |
| Social | Reply to whispers/local chat from real players (LLM if enabled, canned lines otherwise). |

### GM bootstrap

Trade runs and housing need materials, labor, tax certificates or gold that a fresh level-1
character does not have. With `gm_bootstrap = true` and access level 100 on the bot accounts, bots
give themselves exactly what the goal needs (`/item add self …`, `/labor …`, `/gold add self …`)
and may use `/move` for long travel. Without it those goals are skipped with a visible reason.
`playerbots grant-gm-sql` prints the SQL that grants access level 100 to the bot accounts
(`WHERE username IN (...)` with exactly the accounts of the roster; `--game-db` / `--login-db` name
the MySQL databases when the server does not use `aaemu_game` / `aaemu_login`). The bot accounts
are GM accounts then, so `gm_bootstrap` is refused with the public default or an empty password;
without an explicit `server.password` / `$PLAYERBOTS_PASSWORD` the first run generates a random
one into `roster.secrets.json` (`secrets.rs`).

The server runs some GM commands on the sender's target and falls back to the sender only when
there is none, and a bot can end up targeting a real player (a duel). So every GM line is written
through `Ctx::gm` (`gm.rs`): an allowlist that rewrites `/level`, `/labor` and `/move` into the
explicit own-name form, passes only the `self` forms of `/item add` and `/gold add`, refuses
everything else, and puts `CSChangeTarget(0)` in front in the same flush. `/build` (which needs the
house as target) has its own entry point that targets the house first. The target is also cleared
when a fight against a character ends. Plain chat goes through `cs::chat`, which turns a
command-like text into nothing; `cs::gm_command` is the only builder that keeps a leading `/`.
Bots also decline trade requests from real players unless `behavior.trade_with_players` is on (and
never when they can mint goods).

## Quest progression (`goals/quest/progress.rs`, `aa-world::quest_atlas`)

Kills and NPC reach follow the server. The server credits a kill to the player that tagged the
monster first (its party instead once the party did more than half of its health), so the bots
read the first hit from the broadcast `SCUnitDamaged` packets and prefer monsters nobody else
tagged; a killing blow on somebody else's tag no longer expects (or reports) quest credit. A
quest request is measured in 3D by the server (15 m at an NPC, 20 m at a doodad): the goals walk
to the target's own floor through the navigation mesh (the search is asked for the NPC's z) and
report a target on an unreachable floor instead of stopping below it.

The quest picker looks for a giver within 300 m, then within 1500 m. A bot that has done everything
in that circle used to stay there (grinding and wandering filled in), so the Harani and Firran
hand-off from Villanelle to the Silent Forest, 2 km apart, never happened. Now a bot whose scan
finds nothing three times in a row (5 s apart) plans a **trip**, and the picker itself ranks quests
a little differently.

**The atlas.** `World::quest_atlas` is built once, by the first bot that asks (about 80 KiB, listed in the memory breakdown): per quest, at most three giver spawns (position,
zone, level, story flag), sorted by zone, plus each race's **story chain**: the open story category
with the most quests (Nuian 3, Elf 8, Harani 62, Firran 61), in chapter order. It holds only quests
whose objectives all have a plan (`objective::supported`). It is race-independent; whether a bot
may take a quest is judged per bot (`Eligibility`: level with the usual +1 slack, done, in the
journal, quarantined, prerequisites, the race/gender/quest gates of the start, a giver that is not
hostile).

**Choosing a destination** (`plan_target`, pure functions in `progress.rs`, in this order):

1. **Story.** The giver of the first quest of the bot's chain that it qualifies for and that lies
   beyond 1500 m. Chain order, then level; unsupported and quarantined quests are skipped.
2. **Zone.** The zone whose eligible quests, weighted by how well they fit the bot's level, add up
   to the most (at least 1.5), then the densest cluster of its givers (400 m), discounted by
   distance.
3. **Grind.** A spot with monsters of the bot's level near the zone whose quests it will be able to
   take within a few levels. Reaching it fails the goal with `NoTarget`, so the brain's grinding
   takes over; no further grinding trip is planned for ten minutes.

A candidate is dropped when it is blocked (a failed destination, below) or risky: monsters
10 or more levels above the bot at the destination, 14 or more along the route (bosses excluded
there, they stand off the road), or a destination on a part of the navigation mesh the bot cannot
reach. The picker ranks by story, chapter, then "worth the time", then distance: a quest more than
6 levels below the bot (a side quest, never a story quest) comes after every other one, and a
bot with only those near goes to a better place instead of farming them.

**Walking.** The goal has a fifth mode, `Travel`, that uses the existing `walk()` legs (the
navigation mesh in pieces of up to 1500 m, at most ten, the straight line where there is no mesh;
`Ctx::travel` and the portal journey for trips over 1200 m; `/move` only with
`long_travel_teleport_m`). Every five seconds it rescans; as soon as a non-trivial quest is within
1500 m the trip ends as `InRange` and the picker takes it.

**State and back-offs.** `Memory.progress: Option<Box<Progress>>` (one pointer per bot, allocated by
the first scan) holds the trip, the empty-scan count, failed destinations and the last few
trips. A trip has its own time limit (distance at 3 m/s plus 3 min, between 5 and 45 min) that the
run's end does not cut short, and survives a rest or a duel in between. A destination that timed
out, was blocked or killed the bot twice rests for 10 min, then 20, then 40, together with
everything within 400 m of it. A quest in the journal that is only resting after a failure and may
be tried again within 2 min keeps the bot where it is (no trip is planned: a short back-off must not
send a bot across the map). A search that finds no destination waits 2 min before the next. A scan
that comes back empty within 90 s of arriving marks the place as barren.

**Visible.** The goal detail reads "heading to Lilyut Hills for quest 1234 Name (2.1 km to go)" /
"... for quests suited to level 17" / "... to hunt monsters"; the log (`info`) has one line at the
start ("progress: heading to ...") and one at the end ("progress: trip to ... arrived after 612 s
(0 m of 2034 m to go)").

**Not covered.** Zone-to-zone connections that need a carriage, an airship or a ferry (the walk
fails and the destination rests); crossing a hostile zone is only avoided where the danger checks
see it. The real-data tests (`tests/quest_real_data.rs`, ignored, need `AA_COMPACT` and
`AA_SERVER_DATA`) run each race for 240 simulated minutes and print quests done, zones visited and
trips.

## Improvements over the original (from the audit)

- Immutable, shared compiled rotations (`Arc`) with a tiny per-bot state, instead of per-bot
  compiled object graphs with embedded mutable state.
- No per-tick allocation in the brain; no world scans (the bot's entity view is maintained
  incrementally from spawn/despawn/move packets; static lookups use a grid index).
- Scheduler with per-bot jittered deadlines instead of one serial loop with 10 phases.
- Typed failure reasons instead of string matching for quest quarantine.
- Buff checks keyed by buff ids actually observed (fixes `buffMissing` comparing a buff id with
  a skill id), zero-guarded HP percentages.
- Optional LLM, rate-limited and budgeted, never on the tick path.

## Server issue reports

Bots are the first players of a new server, so they also find its bugs. `crates/playerbots/src/issues/`
is the process-wide plumbing; the detectors (what counts as a problem) live in the bot loop and the
goals and file an `IssueReport` through `Ctx::report_issue` / `Bot::file_issue` → `Hub::report_issue`.

- `model.rs` the report (kind, stable key, title, expected/actual, suspect, severity, area hint, facts,
  and the bot context, packet trail and logs the bot side attaches), the fingerprint `pb:<8 hex>` (FNV-1a
  of kind and key; it is the marker in the team's queue file), `TrailRing`.
- `book.rs` `IssueBook`: dedup by fingerprint, counts, distinct bots, three samples, state
  (open / queued / ignored / resolved), the reportable policy, the queue rate limits, eviction at 500
  issues, JSON persistence. Plain data and no clock: tested without a runtime.
- `service.rs` `Issues`: the book behind a mutex (never held across an await) plus three background
  tasks: the debounced saver, the sink (evidence folders and queue entries, recurrence checks) and the
  server log tail. `report` never waits for I/O.
- `queue.rs` / `evidence.rs` the append-only entry for the team's issue file and the evidence folders;
  `logtail.rs` the server log follower and its bounded ring; `redact.rs` removes addresses and
  credential values from every text; `timefmt.rs` local times; `view.rs` the web types.

The detectors are `crates/playerbots/src/bot/watch/`, one `Box<Watch>` per bot in `BotState` (a single
allocation; `size_of::<Bot>()` grew by 24 bytes for it):

- `tracker.rs` a fixed table of eight requests the server must answer (quest accept/complete, loot
  open, skill cast, GM `/move`) with a 6 s deadline. Requests are registered from the outgoing packet
  in one place (`Tracker::on_sent`), cleared by the matching server event, and forgotten when the bot
  died, was teleported or moved away, or when nothing at all arrived since (then it is a stall).
- `classify.rs` the judgement as pure functions: whose fault a skill refusal is, when a quiet line is a
  sustained stall, whether a counter that did not move is a missing credit, whether a monster that
  takes no damage is immune, what a finished trade must have changed, what a bad payment mail is.
- `finding.rs` `Finding` (a `Copy` note a detector leaves) and its report: kind, id-only key,
  title, expected/actual, suspect and severity. The `Names` trait keeps it testable without a `World`.
- `mod.rs` `Watch` (trail, tracker, connection and death watches, findings, per-session episode
  dedupe) and the connection watch.

The flow is note, then drain: a detector pushes a finding, the bot loop drains them after every packet
and tick and files them with the packet trail attached (`TrailRing`: 8 bytes per packet, 48 packets,
allocated on the first packet, names resolved from the opcode tables only when a report is made).
Nothing runs per tick except a few comparisons; an episode is reported once.

The web crate only knows `IssueSummary` / `IssueDetail` / `IssueAction` and three defaulted `BotHub`
methods. Operator documentation: [`ISSUES.md`](ISSUES.md).

## Travel hooks (navigation motion, portals, dungeons, sailing)

The four travel features plug into the bot through fixed hooks, so each one edits only its own
files. Until a feature fills its hook it does nothing and the bots behave as before.

| Hook | Where | Owner |
| --- | --- | --- |
| `Motion::on_path_ready(ticket, PathResult)`, fed by `BotCmd::PathReady`; `BotDetail::path` (hub slot `path`) | `bot/motion.rs`, `bot/nav/`, `bot/cmd.rs`, `hub.rs`, pb-web `types.rs` | navigation motion (done) |
| `TravelState::on_event` (`BotState::travel`): the portal events | `bot/travel/` | portals |
| `Dungeon` goal, `goals::dungeon::{base_weight, score}`, `PartyOrder` / `BotCmd::Party` | `bot/goals/dungeon.rs`, `bot/party.rs` | dungeons |
| `Sail` goal, `goals::sail::{base_weight, score}`, `VehicleState::on_event` (`BotState::vehicle`): the slave and ship events | `bot/goals/sail.rs`, `bot/vehicle/` | sailing |
| `Place` (`BotState::place`): main world or instance world, `loading` between `SCProcessingInstance` / `SCLoadInstance` and the bot's `CSInstanceLoaded`; a change of world drops the entity view and the combat target; inside an instance motion gets no terrain heights (`Place::ground`) | `bot/place.rs` | shared (done) |

`[behavior] portals`, `dungeons` and `sailing` (default on) are read from `ctx.cfg.behavior` by the
features. `GoalKind::Dungeon` and `GoalKind::Sail` are scored only by their own `score` functions
(0 while a feature is a stub, so no bot picks it); forcing a stub from the web UI makes it fail with
`NotImplemented`, which switches it off for that bot. A ship's `SCOneUnitMovement` is decoded twice
(`Decoded::Ship`): the ordinary position event and `WorldEvent::ShipMoved` (heading, speed, helm).

## Market hooks (quests, auction, life-like movement, trade-chat sales)

Four more features plug in the same way. Shared and done: the goals, the configuration, the item
facts, the provenance ledger and the chat forwarding.

| Hook | Where | Owner |
| --- | --- | --- |
| `Auction` goal, `goals::auction::{base_weight, score}`, `AuctionMemory` (`BotState::market`, `Inner::auction`) | `bot/goals/auction.rs`, `bot/market/auction.rs` | auction house |
| `Market` goal, `goals::market::{base_weight, score}`, `AdState` (`Inner::ads`) fed by `AdState::on_chat` | `bot/goals/market.rs`, `bot/market/ads.rs`, `bot/goals/trade*` | trade-chat sales |
| `ItemFacts` / `World::item_facts(template)`: price, refund, stack, bind kind, level, fixed grade, auction categories and the quest-item flag of every item, 20 bytes each (about 0.5 MB for the whole table), `ItemFacts::tradable()` (sellable, unbound, not a quest item) | `aa-world` `items.rs` | shared (done) |
| `Ledger` (`MarketState::clean_count`, `sellable_surplus`): the quantity of each item the bot may sell, fed by `BotState::apply` from the item task reasons | `bot/market/ledger.rs`, `bot/state.rs` | shared (done) |
| `QuestNeeds::of(world, &state.quests)`: what the active quests still ask of each item (gather and use objectives), the part of the bag the sale paths keep back | `bot/market/needs.rs` | shared (done) |
| `MarketState::on_chat(kind, sender, text, now_ms)`: shouts, trade chat, group-finding chat and whispers, forwarded by the social duty before it files the inbox (`market::forward_chat`; which kinds: `ads::hears`) | `bot/market/`, `bot/goals/social.rs` | shared (done), kinds are the market feature's |
| `[behavior] lifelike`, `jump_rate`, `emotes` | `config.rs` | life-like movement (done, see below) |
| `[market]` | `config.rs`, `Config::may_sell_to_players` | shared (done) |

**What may change hands.** Every path that sells, asks for, buys or gives an item away goes through
`ItemFacts::tradable()`, and the paths that part with the bot's own goods also keep back
`QuestNeeds`: the sellable surplus (`Ledger::sellable_surplus`, behind the chat ads, the chat deals
and the auction listings), the trade-chat name index (`ads::names::global`), the WTB goods
(`goals::market::duty::wanted_goods`), the stand-in offers of random trades
(`goals::trade::choose_offer`, rule `keep_of`: unknown items are not offered) and the auction
buys (`auction::Env::tradable`). A quest item is detected from the compact database when the
item table loads (the quest item category, quest-only loot, and the items the quest tables hand
out except in the reward step); quest gather and use targets are ordinary materials and only
count in `QuestNeeds`. A chat deal re-checks `QuestNeeds` when the meeting starts, since a quest
taken since may need the goods.

`MarketState` is one pointer per bot (`Option<Box<Inner>>`, allocated by the first gain the ledger
counts), because the per-bot budget is tight (`size_of::<Bot>() < 3200`). The brain scores
`GoalKind::Auction` and `GoalKind::Market` only through their own `score` functions (0 while they
are stubs, so no bot picks them); forcing one from the web UI makes it fail with `NotImplemented`,
which switches it off for that bot. The brain's failure cooldowns are whole seconds in a `u32`
for the same reason.

## Wave-1 hooks (guilds, pvp, events, companion, farming, population)

Six more features plug in the same way as the market ones. Shared and done: the goal stubs, the
configuration, the goal mask and the zone conflict table. A feature fills in its own files and
reads its own `[section]`; the brain, the goal enum, the web goal lists and the director text
already name its goal.

| Hook | Where | Owner |
| --- | --- | --- |
| `Guild` goal (`GoalKind::Guild`, `goals::guild::{base_weight, score}`), `[guild]` | `bot/goals/guild.rs`, `bot/guild/`, `config.rs` | guilds (done, see [Guilds](#guilds-botguild-goal-guild)) |
| `Gank` goal (`goals::gank`), `[pvp]` `gank`, `attack_bots`, `attack_players` (done, see [PvP](#pvp-botpvp-goals-gank-and-warzone)) | `bot/goals/gank.rs`, `config.rs` | pvp |
| `Warzone` goal (`goals::warzone`), `[pvp]` `warzone`; reads the zone conflict table (done) | `bot/goals/warzone.rs`, `config.rs` | pvp |
| `Event` goal (`goals::event`, built: see [Events](#events-goal-event)), `[events]`; reads the zone conflict table (`avoid_war_zones`) | `bot/goals/event.rs`, `config.rs` | events |
| `Companion` goal (`goals::companion`, built: see [Parties of real players](#parties-of-real-players-botcompanion-goal-companion)), `[party]` | `bot/goals/companion.rs`, `bot/companion/`, `config.rs` | party with real players (done) |
| `Farm` goal (`goals::farm`, built: see [Farming](#farming-botfarm-goal-farm)), `[farm]` | `bot/goals/farm.rs`, `bot/goals/farm/`, `bot/farm/`, `config.rs` | farming (done) |
| `[population]` (the web UI's population director and the server API poll, see [Population](#population-census-economy-board-and-director)) | `config.rs`, `population/` | web director (done) |
| `[pvp]` `enabled`, `defend`, `never_names`, `only_names`, `max_squad`, `retreat_hp_pct`, `war_kill_cap`; `Config::may_attack_players()` (real players only, never while `bots.gm_bootstrap` is on) | `config.rs` | pvp |
| `WorldEvent::ConflictZoneState { zone_group, state, end_unix_s }` (`SCConflictZoneStatePacket`, opcode 0x0ee), `conflict_state::{TENSION .. PEACE, at_war, name}` | `aa-proto` `sc_world.rs` | shared (done) |
| `BotState::zone_states` (`ZoneStates`, at most 64 zone groups), `BotState::zone_state(zone_group) -> Option<(state, end_unix_s)>`, `BotState::at_war(zone_group)` (state conflict or war) | `bot/zones.rs`, `bot/state.rs` | pvp and events (shared, done) |

All six goals are built now (no stub is left). A goal added as a stub fails with `NotImplemented` (the
brain then switches it off for the bot) and its `score` is 0, so no bot picks it; forcing one from the
web UI does no harm. A feature scores its goal through its own `score(ctx, base)` (gate on its
`[section]`; the brain applies none of its factors to it) and turns the stub's methods into the real
thing (`owns_combat` for the pvp goals).

**The zone conflict table.** The server sends one `SCConflictZoneStatePacket` per zone group at world
entry and one on every change: a `u16` zone group, a `u8` state (0 tension, 1 danger, 2 dispute,
3 unrest, 4 crisis, 5 conflict, 6 war, 7 peace) and the `i64` Unix second the state ends. Each bot
keeps the latest of each zone group in `BotState::zone_states`, a table behind one pointer
(allocated by the first packet, bounded at 64 zone groups, cleared by a reconnect). The end time is
the server's schedule; the state holds until a new packet replaces it. `at_war` is true for
conflict and war only, and false for a zone group the server has not announced.

**Masks and the per-bot budget.** The brain's goal masks (`disabled`, `blocked`, the gate) are a
`GoalMask` (`u32`, 32 goals; a compile-time check fails the build when the goals outgrow it). The
failure cooldown table is allocated by a goal's first failure, so a bot that never failed a goal
pays one pointer for it. With eighteen goals, the zone table, the guild state, the pvp memory and the farm memory,
`size_of::<Bot>()` is 3192 bytes (limit 3200) and `Goal` stays 152: a new per-bot structure belongs
behind a pointer (the farm memory is one `Option<Box<FarmMem>>` in the market state, the running
farm machine one `Option<Box<Machine>>` in the goal).

## Living-world hooks (LLM control, gear, personalities, presence)

Four more features plugged in the same way as the wave-1 ones: finer LLM control, gear,
personalities and a world that looks played (presence, exploring, wild farms). The shared hooks
came first (the goals, the goal arguments, the personality profile, the configuration sections,
the journal and the web types); each feature then filled in its own files and reads its own
`[section]`. Their own sections below describe what they do.

| Hook | Where | Owner |
| --- | --- | --- |
| `GoalKind::Explore` (goal `explore`, `goals::explore::{base_weight, score}`), `[explore]` | `bot/goals/explore.rs`, `config.rs` | presence and exploring |
| `GoalKind::Gear` (goal `gear`, `goals::gear::{base_weight, score}`), `[gear]` | `bot/goals/gear.rs`, `config.rs` | gear |
| `GoalArgs` (`dest`, `zone`, `item`, `target`, `wild`, `why`), `BotCmd::GoalWith { kind, args }`, `Brain::queue_with`, `Goal::with_args`; read by `explore` (`dest`, `zone`), `gear` (`item`), `wander` (`dest`) and `farm` (`wild`, `item`) | `bot/cmd.rs`, `bot/brain.rs`, `bot/goals/mod.rs` | LLM control (wander, director), wild farms (farm) |
| `Profile` (`chatty`, `crude`, `joker`, `explorer`, `outlaw`, `gearhead`, `dedication`, `chronotype`) in `RosterEntry::profile` (derived from the bot id for an entry that has none), `Persona::profile`, `ProfilePatch`, `BotCmd::Profile` (applied to the persona and the roster), `[personality] enabled` | `roster.rs`, `bot/ctx.rs`, `bot/mod.rs`, `config.rs` | personalities (chat), and every feature that reads a trait |
| `JournalBook` (`hub.journal`, `journal.json` next to the roster, `bots.journal`): per bot `seen`, `wild`, `sessions`, `notes`, `gear`, each section in its own file | `journal/` | the section owners |
| `Presence` (`hub.presence`, ticked by `Hub::tick`, handed the roster by the supervisor), `[presence]` | `presence/` | presence |
| `[farm] wild` | `config.rs` | wild farms |
| Web: `Command::Act(ActArgs)` (`agent::act_command` turns it into `BotCmd::GoalWith`), `Command::Profile(ProfilePatch)`, `BotDetail::{gear, profile, journal}` (`GearView`, `ProfileView`, `JournalView`), `SlotInner::{gear, profile}` | pb-web `agent.rs`, `persona.rs`, `gear.rs`, `journal.rs`; `hub.rs`, `agent.rs` | LLM control (`act`), personalities (`profile`), gear (`gear`) |

**Per-bot budget.** The roster entry in `Bot` is boxed now (it is read at login and on renames
only), which took `size_of::<Bot>()` from 3192 to 3136 bytes with the profile in the persona and
the goal arguments in the brain. Of the four features only gear adds a field (`BotState::gear`, one
pointer): the director and the chat styles live in the brain's boxed chat state, presence and the
explore memory in the hub and the journal. `size_of::<Bot>()` is 3144 bytes with all four.

## LLM control and the agent API

Two ways for a model to steer the bots more finely than "pick a goal": the in-process **director**
(`bot/director.rs`, `[llm] director = true`) and the **agent API** for an LLM outside the process
(`pb-web` `agent.rs`, `playerbots` `agent.rs`, the `playerbots mcp` bridge in `agent/mcp.rs`).
Both end in the same place: a goal started once with [`GoalArgs`] (`Brain::queue_with`), which the
goal reads (`Wander` walks to `dest` and strolls there; `Explore` reads `zone`, `Farm` reads `wild`).

| Part | Where | What |
| --- | --- | --- |
| `Llm::decide`, `Decision` | pb-llm `decide.rs`, `client.rs` | A plan request: numbered options in, `{"steps":[{"choice":n}],"note":"…","reason":"…"}` out; tolerant parsing like `choose`, steps that name no option dropped, the note through `sanitize_reply`; counts plans, steps and notes (`LlmUsage`). |
| `Opt`, `What`, `menu`, `assemble`, `explore_candidates`, `step_allowed` | `bot/director.rs` | The options: the eligible goals best first plus variants (`explore <zone>`, `wander` to a town or home, `farm wild`), at most 10. |
| `Director` | `bot/director.rs` (in the brain's boxed `Chat`) | Timing (interval, triggers, minimum gap), the request in flight, the plan, the last goals, the level and zone of the last look. |
| `director_step`, `on_decision`, `next_step`, `director_wake`, `publish_agent` | `bot/brain.rs` | The glue: ask, keep the note and the plan, start the next step when a goal ends, wake on death and failure, fill the agent slot. |
| `Notes::push_clean` | `journal/notes.rs` | The one way a note gets into the journal (director and `note` command). |
| `Wander::with_args` | `bot/goals/wander.rs` | `dest`: travel there in legs (`pvp::walk::Walker`, so portals and `/move` work), then stroll around it. |
| `ZoneNames`, `resolve_zones`, `act_command`, `write_note`, `observe`, `AgentSlot` | `agent.rs`, `hub.rs` (`Hub::zones`, `SlotInner::agent`) | Zone names to keys, `act` to `BotCmd::GoalWith`, the `note` command, the observation. |
| `Tool`, `Observation`, `AgentBot`, `ZoneEntry`, `Command::Note`, `BotHub::{observe, zones}` | pb-web `agent.rs`, `types.rs`, `server.rs` | The catalog and the read routes `/api/agent/{tools,bots,observe/{id},zones}`. |
| `mcp::{handle, tool_request, call_result, serve, Bridge}` | `agent/mcp.rs`, `main.rs` | MCP over stdio (JSON-RPC 2.0) forwarding tool calls to the web API. |

**The director's loop.** At every brain tick (only with the director on, an LLM and no forced goal)
`director_step` first lets the `Director` look at the level and the zone key under the bot (a grid
lookup): a higher level wakes it with `Trigger::LevelUp`; a zone change reads the journal once, and a
zone the journal does not have, or has as its newest entry only since this change (the explore
feature may have recorded it a moment earlier), wakes it with `Trigger::NewZone` unless the bot is
exploring on purpose. The brain wakes it with `Died` (the dead branch of the tick) and `Failed`
(`end_goal` with a failure other than `NotImplemented`), and `next_step` with `PlanDone` when the
plan runs out. `due` answers `Trigger` when one waits (with `director_triggers`), `Timer` when the
jittered interval is up and no plan waits, and nothing within `director_min_gap_secs` of the last
request, before the bot's first minute, or while a request is in flight. A request builds the menu,
the option lines, the situation text and the system text and spawns `Llm::decide` on its own task;
`poll` collects the answer on a later tick and maps the indices back to the options that were
offered (a second check after the client's). The note goes through `Notes::push_clean`, the steps
become the plan (`PLAN_TTL_MS` = 30 min), and the first step is queued at once when the bot is idle,
wandering or resting. `Brain::pick` takes the next step before its own scoring; each step is checked
again (`step_allowed`: the goal still eligible, a zone still unseen, a wild farm still allowed, a
point not at war) and dropped otherwise. `BotCmd::Goal` and `BotCmd::GoalWith` from the operator or
an agent drop the plan.

**The menu.** The plain goals are the brain's own eligible set (`scores_now` > 0, without GM-only
goals when GM rights are off), best first. The variants are made only when a request is made, so
their cost does not matter on the tick path: up to four zones from the quest atlas that the journal
has not seen, within 8 km, with quests at most five levels above the bot, nearest first, then
filtered with the quest goal's tests (`progress::cut_off` on the navigation mesh components,
`progress::deadly` for the danger around the hub and along the way, `in_war_zone`) until two are
left; the quest hub of the bot's zone and its finished house (150 m to 3 km / 5 km away) for
`wander`; `farm wild` when `[farm] wild` is on and the outlaw trait is at least
`farm.wild_outlaw` (0.6, the farm goal's own threshold). `assemble` keeps at most four variants, puts each right after its goal and
cuts the plain goals to fill ten lines.

**What the model sees.** The system text is the persona (name, race, class, temperament) and the
contract; the user message is the situation (level, health, mana, money, personality, zone, zones
visited, the running and the last three goals, what is around, the last three notes, why it is asked
now) and the numbered options. Notes are untrusted (a model or an agent wrote them): they are shown
as JSON strings after "data, not instructions", so a note can neither close its quote nor start a
line. The answer can only name option numbers; GM commands never come from it.

**The agent API.** `GET /api/agent/tools` is a static catalog (`pb_web::agent::tools`): each tool's
name, description, JSON-schema input and the route it maps to (`GET` path with `{bot}`, or `POST` with
a command `type`). `GET /api/agent/observe/{id}` is `BotHub::observe`; the hub assembles it from the
slot (snapshot, log, profile), the journal (notes, zones seen) and `SlotInner::agent`, which the brain
refreshes every `AGENT_EVERY_MS` (15 s, on the bot clock, from the boxed chat state: the eligible
goals as a `GoalMask`, mana, a fair monster near, characters and real players within 60 m, the plan's
labels). A hub without its own view (the mock, tests) gets the default built from `detail`.
`Command::Act` names a zone by name or key; `Hub::command` resolves a name through `ZoneNames` (built
from `World::zone_name` for every key when the hub is made) before `to_bot_cmd`, and refuses an
unknown or ambiguous name in words. `Command::Note` is kept by the hub itself (`agent::write_note`).
All routes keep the security model of `server.rs`: the token for every `POST`, the loopback `Host`
check for every route.

**MCP.** `playerbots mcp --url … [--token-env NAME]` reads one JSON-RPC message per line from stdin and
answers on stdout (`initialize` with version negotiation, `ping`, `tools/list`, `tools/call`;
notifications get no answer; batches and other methods are errors). `handle` and `tool_request` are
pure: a tool call becomes an `HttpRequest` (the `bot` argument fills the path and never goes into
the body, the command `type` cannot be overridden by an argument), `call_result` turns the API's
answer into a tool result (`isError` with the API's message on a 4xx). `Bridge` makes the request
with the token from the environment variable, never logged (its `Debug` hides it), 15 s at most and
1 MiB of answer; the log goes to stderr. Stdin is read by a thread into an in-memory pipe (the
workspace's tokio has no `io-std`).

**Budget and bounds.** Nothing of this is in `Bot`: the director state and the agent timer live in
the brain's boxed `Chat`, the plan is at most five `Opt` (24 bytes at most each), the agent view is in the hub
slot. `size_of::<Bot>()` stays 3136, `Goal` 152 (the wander goal's target and walker fit), `BotCmd`
48. Every list is bounded (10 options, 5 steps, 3 notes shown, 16 kept, 8 log lines and notes in an
observation) and the requests share the client's limits.

**Tests.** pb-llm: `decide.rs` (parsing, the contract) and `tests.rs` (`decide` against the mock
endpoint: plan, note filter, counters, budget). `bot/director.rs`: triggers and the gap, the look at
level and zone, the plan, the mapping of answers, `step_allowed`, the options' arguments, labels and
lines, `assemble`, `explore_candidates`, `zone_level`, the example config. `agent.rs`: zone names,
`act` with a zone name, the observation. `agent/mcp.rs`: the protocol functions and a whole session
over stdio against the real router on 127.0.0.1 with a fake hub. pb-web: the catalog's schemas and
routes, `note`, and the agent routes end to end (`tests/api.rs`).

## Personalities (`bot/goals/social/`, `[personality]`)

Bots talk like players: some chat a lot and some hardly at all, some joke about the game, some
swear now and then. The chat reads three traits of the bot's `Profile` (`chatty`, `joker`,
`crude`); routing, replies and the old limits work as before.

* **Style** (`social/style.rs`, `Style`): plain arithmetic from the three traits and the operator's
  cap. Chattiness `c` scales the chance to answer an overheard line (`reply_chance` times
  `0.25 + 1.5c`), the reply gap (`1.25 - 0.5c`), the gap between two unasked lines
  (`chatter_gap_secs` times `2^(1-2c)`), the chance of a remark (`0.4 + 1.2c`), the mean gap
  between two ambient lines (3 minutes times `20^(1-c)`: 13 minutes at 0.5, 50 at 0.05), the
  gap between two channel lines (20 minutes times `30^(1-c)`, none below 0.3) and the per-bot cap
  of `1 + round(4c)` unasked lines in 10 minutes. 0.5 is the plain chat from before. Humor `j`:
  an ambient line is a joke with `0.1 + 0.8j`, small talk gets a quip with `0.35j`. Coarseness
  `k`: the bot's own level is clean below 0.4, mild below 0.75 and strong above, capped by
  `[personality] profanity` (default `mild`); a crude bot leans towards the coarse templates
  (weight `0.25 + 1.75k` against 1 for a clean one) and answers small talk coarsely with `0.3k`.
  With `[personality] enabled = false` every bot talks like the neutral profile and stays clean.
* **Lines** (`social/lines.rs`, `social/humor.rs`): static templates, each with the coarsest
  level it uses (`c`, `m`, `s`) and the slots `{n}` (a name), `{lv}` (a level), `{z}` (the zone)
  and `{x}` (an exclamation the bot's tongue allows, "ugh" to "fucking hell"). Remarks per
  `Chatter` kind (duel won, lost or drawn, trade, hello, ding, death, gank, quest, pack, harvest,
  house, loot); ambient small talk; shout, trade and group-finding lines, tested never to read as
  market talk, because the market and the economy board read those channels; 69 jokes on
  fourteen ArcheAge topics (labor, packs and gankers, Freedich and the Kraken, the queue,
  regrades, lunagems, farm drama, Mirage Isle, Diamond Shores, boats and pirates, Hasla, carrot
  farmers, the land rush, lag), quips and coarse answers. No repeats: per bot (`Recent`, 4 keys
  for replies and remarks, 8 for the rest) and across bots (`ChatGate::told`, the last 16 keys).
* **What happened** (`Tally`, `Social::observe`): on every tick the chat reads a few counters of
  the bot's state (level, dead, quests completed, `Ledger::packs_sold`, houses finished and
  under construction, `FarmMem::harvested`, `PvpMem::deaths`, the duel watch's last end through
  `DuelWatch::peek_end` before the duel goal takes it, and once a minute the bag count) and notes
  a `ChatEvent` when they change: a level up; a death, told at the resurrection, a gank when a
  character killed the bot; a quest; a pack; a harvest; a house that was being built and is
  finished; a bag 12 items fuller. A counter that jumps by more than two is a reload, and in
  the first 30 s after a spawn only a level or a death counts. The brain still notes the ends of
  duels and trades (`end_goal`); the duel's outcome picks the won or lost lines. Nothing outside
  the social module had to change for it. A fresh event of a higher rank is not replaced.
* **Ambient and channel lines** are tried at the look-around (every 15 to 30 s) when the bot's
  own schedule says so, a real player is near (with `chatter_needs_player`), the bot's cap has
  room and the process-wide budget gives a slot (`ambient_per_min`, `channel_lines_per_hour`,
  `ChatGate::take_ambient` / `take_channel`). With `[llm] enabled` and `llm_chatter` half of the
  ambient lines are asked of the model (`reply::build_remark_request`: the persona prompt and a
  fixed topic hint, never player text), at most two requests a minute for all bots
  (`ChatGate::take_llm_chatter`), with the canned line as the fallback.
* **The language filter** (`pb_llm::persona`): `blocked` refuses slurs, hate, sexual words,
  insults and threats (whole words after lower-casing and leetspeak, stretched letters, a plural
  `s`, word beginnings such as `nigg` or `retard`, phrases such as "kill yourself", and the worst
  words also spelled out with spaces). It is part of `sanitize_reply` and of `social::wire_text`,
  so every chat line of every feature passes it. `cap_profanity` grades the swear words (mild:
  damn, hell, crap, wtf, ffs and a few more; strong: the rest) and softens a word above the cap
  ("fuck" to "frick", "shit" to "crap" or "crud", "damn" to "dang"); a word without a softer form,
  or more than three to soften, rejects the line. `sanitize_chat(text, level)` is both; the social
  module runs it at the bot's level when it schedules a line, canned or written by the model, and
  once more just before the wire. A model reply in a foul-mouthed persona's voice cannot pass
  under `off`.
* **LLM persona**: `Persona::style` (`ChatStyle`: chattiness and humor in percent, the profanity
  level) adds a "Your style:" line to the system prompt (quiet or chatterbox; dry, or ArcheAge
  jokes never at another player's expense; clean, mild cussing, or gamer swearing, never aimed at a
  player); the rules stay, plus "never insult anyone and never use slurs".
* **Cost**: the new state is in `Social`, which the brain already boxes, so `size_of::<Bot>()` is
  unchanged. A tick reads the counters (a few loads; the bag once a minute); the tables are
  static; the filter runs once per line said.

The limits, worked out with the defaults: replies 20 a minute, remarks and bot-to-bot lines 4,
ambient lines 2, channel lines 4 an hour and LLM-written ambient lines 2 a minute (inside the LLM's
own budget), for all bots together. In two simulated hours next to a real player a chatterbox
(chatty 1) says about 45 unasked lines, a neutral bot about 11, a typical one (0.3) about 5 and a
quiet one (0.05) about 2; fifty chatterboxes together still say at most 2 ambient lines a minute.

## Farming (`bot/farm/`, goal `farm`)

Bots buy seeds, plant them, wait and harvest, and they grow what the economy is short of. Everything
is off until `[farm] enabled`.

* **Protocol** (`aa-proto`): `cs::create_doodad` (`CSCreateDoodad`: doodad id, fixed-point x and y,
  z, rotation, scale, the seed's item id), `cs::buy_items` (`CSBuyItems`: the merchant's object id and
  the stacks to buy, paid in money), `cs::request_common_farm_list`; the harvest is the ordinary
  `cs::start_skill_doodad`. `SCResponseCommonFarmListPacket` becomes `WorldEvent::CommonFarmList`
  (the character's crops on the public farms, which carry no object ids). All have byte-exact tests.
  `motion::range_checked` pins the bot's position before the two new packets because the server
  checks the distance (3 m to a merchant, 30 m to the planting spot).
* **Data** (`aa-world` `farm.rs`, `Farming`, from `World::farm()`): a seed is an item with a row in
  `item_spawn_doodads`; its `Crop` has the doodad, the phases (a phase is ripe when a use function
  with a harvest skill leads to a phase that hands out items), the total growth time (the sum of the
  growth delays, divided by the world's `GrowthRate` and shortened under a matching climate:
  `Crop::grow_ms_at(rate)`), the yield per produce item (`Yield { min, max }`, no drop chance:
  rare drops of a loot pack are overvalued), the labor per harvest skill (13980 to 20486), the seed
  vendors (`vendors_of`) and the farm kinds that allow it. The public farms (sub zones 966, 967,
  968, 974, 998; `FarmKind` Farm, Ranch, Nursery, Stable; ten crops per character on a field farm,
  five on the others) are polygons baked from `subzone_area.xml` into `farm_areas.json` (`aa-bake`
  `areas.rs`, `--only farms`); a world without them has gardens only.
* **Memory** (`bot/farm/mod.rs`, `FarmMem`): up to 64 `Plot`s (crop object id, doodad, seed, position,
  phase, ripe flag, the time to look again, when it was planted, whether it is honest, the land it
  stands on), the spots and seeds the server refused (with an expiry) and a few counters. It follows
  the server through `farm::observe`, which the bot loop calls for every world event before the
  `keep_doodad` filter drops the doodads it does not track: a `SCDoodadCreated` whose owner is the
  bot's character marks its own crop; a phase change marks it ripe; a crop that vanishes while the bot
  stands within 12 m is gone (harvested or destroyed), further away it only left the view; the common
  farm list replaces the plots on the public farms after a reconnect (such plots are not honest).
* **Planner** (`bot/farm/plan.rs`): `choose` scores every crop that grows on dry land,
  `(0.05 + profit + need) * time * reach * held`, with the profit per hour of produce value less half
  the seed price, the need per produce item `(3 if wished + 2 * scarcity) / (1 + units other bots
  grow / 40)`, `1 / (1 + hours / 6)` for the time to ripen, the distance to the seed vendor and a
  bonus when the bot already holds seeds. Everything it reads is behind the `Env` trait (the
  scarcity view, the wish list, the vendors, the bag, the farm board), so it is tested without a
  world. `make_plan` caps a run at five seeds and decides how to get them: buy (a vendor and the
  money), mint with a GM command (`bots.gm_bootstrap` only), or the seeds in the bag.
* **Sites** (`bot/farm/site.rs`): a `Site` is the garden of one of the bot's houses (the square of
  `garden_radius` around the house, from the housing data) or a public farm polygon; `pick_spot` draws
  a free point at least 1.6 m from any known crop, away from the polygon's edge, not on a refused
  spot. The spot is claimed in `contest` (`plot_key`, 3 m cells) so two bots do not pick one square.
* **Machine** (`bot/goals/farm/machine.rs`): a pure state machine over an `Input` (what the bot
  knows this tick) and the `FarmMem`, returning `Act`s (walk, buy, plant, cast, GM line, ...), so a
  whole run is tested against a scripted server (`goals/farm/tests.rs`) without a network. Phases:
  `Fetch` (ask for the public-farm crops once per session), then for a plant run `Shop` (walk next to
  the vendor, stay within its 3 m, buy, check the bag) or `Mint`, then `Sow` (draw a spot, walk
  there, face it, `CSCreateDoodad`, wait for the crop to appear, retry another spot on a typed
  refusal), and for a harvest run `Reap` (walk to the crop, cast the harvest skill, wait for the
  crop to change, wait for it to go, plant the seed on the same spot). The glue in `goals/farm.rs`
  (`RealOps`, `RealEnv`) turns `Act`s into packets and movement and builds the input from `Ctx`.
* **Honest produce.** The ledger counts the reasons an item arrives clean (`reason::*`); a harvest
  arrives as `DOODAD_INTERACTION` (17), which is not clean by itself. The machine opens
  `MarketState::open_harvest(honest)` for the length of a harvest cast, and `counts(17)` is true only
  inside an open honest window; `None` adds nothing and `Some(false)` counts nothing. A plot is honest
  when the ledger counted the planted seed as clean (`clean_count(seed) >= 1` at planting), so
  seeds bought from the vendor (`STORE_BUY`) give sellable produce and GM-minted ones never do. The
  seed that leaves the bag arrives as `DOODAD_CREATE` (14).
* **Scoring** (`goals::farm::score`): 0 unless `farm.enabled`, the bot is settled (90 s online),
  healthy (60 % hit points), in the main world and not within five minutes of its last plant run.
  Otherwise a due plot scores three times the weight, and a bot with room for plots and a crop it can
  get seeds for scores the weight plus half of the need of the best crop (capped at 3). The weight is
  `0.35 * (0.5 + 0.5 * homebody + 0.5 * merchant)`. The director's per-goal bias and its 1.5 times
  nudge for wishes apply on top, in `brain::scores_now`.
* **The board** (`bot/farm/board.rs`, `hub.farms`): per bot, what it expects its plots to give
  (item, count), published after each plant and harvest and dropped when the bot leaves. The planner
  damps the need by what the others grow; the Economy view's "Growing" column is its sum.
* **Web**: the bot snapshot gets `plots` (`PlotLine`: position, crop, state with the time left,
  honest, land; skipped when empty) and the economy rows `growing`; both are additive fields with
  defaults.

Live behaviour is in the farm notes of [OPERATING.md](OPERATING.md#farming-farm); the tests are the
unit and flow tests in `bot/farm/` and `bot/goals/farm/`, and `tests/farm_real_data.rs` (ignored,
needs `AA_COMPACT` and `AA_SERVER_DATA`) which runs the whole loop against the real data and a fake
server.

## Presence, exploring and wild farms (`presence/`, goals `explore` and `farm`)

Three pieces that make the world look played: bots log in and out on rhythms of their own, go and
see places, and the shady ones grow crops out in the wild (and take other people's). None of them
adds a byte to `Bot`: `size_of::<Bot>()` stays 3136 (presence lives on the hub, the explore trip is
boxed in its goal, the wild-farm state is in the boxed `FarmMem` and the journal).

**Presence** (`presence/`, `[presence]`, off by default).

* `schedule.rs` (pure): a bot's day from its `Profile` (`chronotype`, `dedication`) and its id.
  The chronotype sets the peak of the day (early bird 08:00, daytime 14:00, evening 20:30, night owl
  23:30, each bot shifted by up to 50 minutes); dedication sets how many sessions (one, sometimes two
  or three), how long (a casual player 20 to 60 minutes, a dedicated one hours, half again at the
  weekend, at most ten), breaks of 5 to 20 minutes in sessions over 100 minutes, and how often a day
  is skipped (casual players skip more, the weekend less). Every value is drawn from a generator
  seeded with the bot id and the local day, so a schedule is the same on every call and across
  restarts. `online_at`, `next_edge` and `curve` (bots scheduled per hour) answer from it.
* `plan.rs` (pure): one pass. Every bot is a `Row` (online, owned by presence, owned by the
  director, scheduled, held, its `stop_rank`). The target is the scheduled count, raised to
  `min_online` and capped at `max_online`, counting every online bot; presence then logs in
  scheduled bots of its pool first (then unscheduled ones when `min_online` asks for more, the ones
  whose next session is nearest first) and logs out its own bots whose session is over or that are
  above the cap, the safest first (`stop_rank`, then the longest online). A bot that may not be
  stopped now waits for the next pass.
* `mod.rs` (glue): the supervisor attaches the configuration and the roster; `Hub::tick` calls
  `Presence::tick` once a second and a pass runs every 20 s (3 s after the start). The pass reads
  the bot slots (online, activity, party), asks `population::select::stop_rank` whether a bot may be
  stopped (never in a trade, a duel, a dungeon, a market meeting, on a boat, or in the party of a real
  player), keeps to `max_starts_per_min` and `max_stops_per_min` and sends
  `SupCmd::StartIds { presence: true }` and `SupCmd::StopIds { presence: true }`. The supervisor
  queues the starts through the ordinary login ramp and checks the stop rules once more. With
  `[social]` on, a bot whose session ends may say a goodbye (`goodbye_chance`, a line that fits the
  hour) and leaves one pass later.
* **Who decides about which bot.** A bot the population director started (`Population::is_owned`)
  is the director's: presence never starts, stops or adopts it, but it counts towards the bounds. A
  bot the operator started (web UI, `Spawn`) is never stopped by presence. A bot the operator stopped
  is held: presence does not log it in again until its schedule ends the current session. Presence
  owns only the bots it started, and forgets a start that did not come online within five minutes.
  With presence on, the supervisor starts nobody at launch (`bots.count` only sizes the roster).
* The journal's `sessions` section (`journal/sessions.rs`) counts the sessions that reached the
  world, the play time and the last log-in and log-out (`Bot::world_loop`, `Bot::go_offline`).
* Web: `DirectorView::presence` (`PresenceView`, absent when presence is not attached): the local
  time, online against scheduled, the target and bounds, the next changes, the counts per chronotype
  and the curve of the day; the Director view shows it as a block below "Process health".
* The local time is the machine's (`chrono::Local`) or `utc_offset_hours`.

**Exploring** (goal `explore`, `[explore]`, on by default).

* Landmarks (`aa-world` `landmarks.rs`, `World::landmarks`, built at the first call from the zone
  grid, the terrain and the recall points, about 0.1 s and 25 KiB for `main_world`): per zone key
  the walkable ground nearest the middle, the highest walkable ground when it stands 25 m above the
  middle, the coast nearest the middle (walkable ground with water within 48 m) and the recall
  points. Keys without a zone group (test maps, cut-scene maps, unused areas) get none. A zone as
  players know it is the zone group (`Landmarks::region`): "Solzreed Peninsula" is three keys.
* The planner (`goals/explore/plan.rs`, pure over an `Env`): candidates within `max_trip_m` and at
  least 150 m away, a landmark in a zone (region) the bot has never stood in counting three times a
  new landmark of a known zone, a seen one a tenth; hills before the coast, recall points and zone
  middles; nearer counts more (half at 2 km). The best twelve are drawn by weight and at most six
  are checked: not at war (`ctx.in_war_zone`), on the bot's part of the navigation mesh or on a
  portal route (`progress::cut_off`, `travel::has_route`), and safe for its level
  (`WorldChecks::dangerous`: hostile aggressive monsters `level_gap` above it within 150 m of the
  place, normal and elite ones within 300 m, `level_gap + 4` above it along the straight line, or
  the quest goal's `progress::deadly`).
* The goal (`goals/explore.rs`, trip boxed): travels with `ctx.travel` in legs (a far trip may go
  by portal), gives up after its time budget (2 minutes plus the distance at 2 m/s, at most 40
  minutes) and avoids the place for six hours; at the place it looks around for 40 to 100 seconds
  (half again on a hill, at the coast or in a new zone) with short walks and pauses, and jumps for
  joy when the bot jumps at all. The place goes into the journal's `seen` (100 m cells) on arrival,
  the end of the trip sets `last_trip_unix`.
* Every zone a bot stands in is recorded in `seen.zones` by `Bot::publish`, whatever it does.
* Score (`goals::explore::score`): 0 unless `[explore] enabled`, settled (60 s), healthy (70 %),
  not in an instance, past the gap since the last trip (`trip_gap_mins` times `1.5 - explorer`) and
  with something new within `max_trip_m`. Else `(0.05 + 0.45 * explorer^2) * (1 + 0.1 * new places,
  up to 6) * (1 + 0.2 * the gaps it is overdue, up to 2)`: about a third of the quest weight for an
  average bot, more for a born explorer.
* `GoalArgs::dest` heads for that point, `GoalArgs::zone` for a landmark of that zone at any
  distance; the checks still apply.

**Wild farms** (`[farm] wild`, `steal`, off by default; `bot/farm/`, `goals/farm.rs`).

* What the server does (AAEmu-1.2-Rust, read-only): `systems/npcs/doodad/manager.rs`
  `handle_create_doodad` lets a seed go down anywhere on dry ground within 30 m unless
  `placement.rs` refuses the spot (276 area or restricted zone, 154 too far, 277 and 278 another
  doodad or a unit too close); on a public farm (`public_farm::can_place`) and on house land the
  seed costs no labor (house land needs the owner's permission, else `NO_PERM` 2), anywhere else it
  costs the labor of the seed's use skill (`consume_lp`: 10 for most seeds in `compact.sqlite3`, 1
  for some, 0 for a few) and too little labor is `LABOR_POWER_NEEDED` 9 (the old `refusal::NO_LABOR`
  29 never matched; it is 9 now, and `NO_PERM` costs only the spot). The crop is saved with its
  owner. `interactions.rs` `crop_harvest` and `funcs/skills.rs` let anybody harvest a crop out in
  the open (owner-only applies inside public farms during the guard time, house land needs house
  permission), and `phase.rs` `theft_evidence` with `social/crime.rs`
  `generate_evidence_from_theft` leaves footprints (a reportable crime) when somebody else harvests
  a character's crop with a crime-point harvest skill (all of 13980 to 20486) within the guard time
  of 24 hours after the planting.
* A new land type `WILD_LAND` (9) with `Site::wild` (a 4 m circle) beside the gardens (0) and the
  public farms (1 to 4); `site::wild_patch` (pure over `WildEnv`) draws 32 candidates 60 to 600 m
  from the bot and keeps the most secluded one that passes `wild_ok`: dry and flat ground across the
  patch (1.2 m at most), not a public farm, a housing area or a zone at war, no townsfolk (an NPC
  friendly to the bot or with a service) within 100 m, no recall or respawn point within 200 m, no
  hostile aggressive monster within 50 m, no quest giver within 150 m, no real player in view within
  120 m and not within 150 m of a patch where its crops were stolen. On the real data it finds a
  patch from 15 of 16 race starts and recall points.
* Who: `[farm] wild` and an `outlaw` trait of at least `wild_outlaw`, or a goal started with
  `GoalArgs::wild` (then only the wild patch). An outlaw tries its patch first, up to `wild_plots`
  (part of `max_plots`). `plan::labor_cap` cuts the run to the seeds its labor pays for; the machine
  checks the labor before every wild seed and a GM bot tops it up (`/labor`). `GoalArgs::item` picks
  that seed, or a crop that gives that item.
* The journal's `wild` section keeps the plots (position, seed, doodad, planted and ripe times,
  honest), the patch and the robbed patches. `Farm::start` puts the plots back into `FarmMem`
  (`restore_wild`), so a bot that restarted harvests them; `score` also counts a ripe wild plot of the
  journal. `FarmMem::observe` now tells the land of a crop found again (a public farm, the bot's
  garden or a housing area, else wild) and keeps the wild plots when the public-farm list arrives.
  A plot that leaves the memory is noted (`FarmMem::take_gone`); the goal writes harvested ones out
  of the journal, and a wild one that was gone when the bot came for it counts as stolen: logged,
  counted, and its patch is given up for another one.
* Honesty does not change: a plot is honest only when the ledger counted its seed clean (bought), and
  a minted seed never is.
* Stealing (`[farm] steal`): a farming bot keeps the ripe crops of others out in the wild that are in
  view (`FarmMem::foreign`, four at most). An outlaw with no real player near takes one
  (`steal_target`): the crop of another bot of this process, or of anybody with
  `steal_from_players` (never with `bots.gm_bootstrap`). The crop goes into the memory as
  `STOLEN_LAND` (10) for one harvest run: a dishonest window, no replanting, forgotten when the run
  ends. The bot does not know when the crop was planted, so a theft within the 24 hours leaves
  footprints for the owner.

Tests: `presence/tests.rs` and the `schedule` and `plan` unit tests; `landmarks` and
`goals/explore/plan.rs` unit tests; the wild cases in `bot/farm/tests.rs`, `bot/farm/site.rs`,
`bot/farm/plan.rs` and `goals/farm/tests.rs`; ignored real-data tests `tests/explore_real_data.rs`
(the landmarks of `main_world` and the trips a new bot plans from each race's start at levels 1 to
30) and `tests/wild_real_data.rs` (patches near the starts and recall points, seed labor).

## Gear (`bot/gear/`, goal `gear`)

Bots wear the best pieces they have for their level, get better ones as they level, socket lunagems
and, if the operator allows it, regrade. On with `[gear] enabled` (the default); minting and
regrading need `bots.gm_bootstrap`, regrading also `[gear] regrade`.

* **Protocol** (`aa-proto`, byte-exact tests in `tests/cs_gear.rs` and `tests/sc_gear.rs`):
  `cs::swap_items` (`CSSwapItemsPacket` 0x03a: `u64 from_item_id, u64 to_item_id, u8 from_slot_type,
  u8 from_slot, u8 to_slot_type, u8 to_slot`, the server's `MoveArgs::read`) with the helpers
  `equip_into_empty` (from the empty equipment slot, item id 0, to the bag item: the server's
  `DoEquipInEmptySlot`), `equip_swap` and `unequip`; `cs::start_skill_item_on_item` (`CSStartSkill`
  with an item caster, `u8 2, bc caster, u64 item id, u32 template, u8 0, u32 0`, and an item target,
  `u8 3, bc caster, u64 target item id, u32 0, u8 0`: the socketing and the regrade). The equipment
  detail (type 1, 55 bytes: image, durability, `i16`, lunastone, charge time, dye, seven gem ids,
  two `u16`s) is read for its gems: `OwnState::equipment_gems` (parallel to `equipment`, at log-in),
  `ItemChange::Updated::gems` (an item task's update of a piece). `SCItemSocketingLunagemResult`
  (0x09d: `u8 result, u64 item id, u32 gem, bool install`) is `WorldEvent::LunagemResult`,
  `SCGradeEnchantResult` (0x09c: `u8 result, Item, u8 grade before, u8 after`) is
  `WorldEvent::GradeEnchantResult` (`grade_enchant_result`: 0 break, 1 downgrade, 2 fail, 3 success,
  4 great success).
* **Data** (`aa-world` `gear.rs`, `World::gear()`): one 8-byte `GearItem` per equippable template
  (slot type, armor/weapon/accessory, armor kind or holdable, item level, level requirement, fixed
  grade, gender, gradable and regradable flags; about 8 500 templates, 124 KB with the rest), the
  lunagems (where they fit, the lowest item level), sockets by slot type and grade, the socket
  chances (100 % for the first gem, 50 % after), the grade facts (order, stat multiplier, regrade
  odds), the copper offers of spawned merchants at the grade the server normalizes them to, the plain
  regrade scrolls and the reward choices of every quest in the order of the client's index. The
  server checks none of a client's rules (level requirement, gem fit, socket count): the bots do.
* **Value** (`bot/gear/eval.rs`, pure): `(item level + 4) * grade factor * affinity * (1 + 4 % per
  gem)`. The affinity (`Fit::of` the class build: armor kind, weapon kinds in order, shield, bow)
  is 1 for armor of the build's kind, cloaks and accessories, 0.55 for another armor kind, 1 down to
  0.6 for the build's weapon kinds, 0 for other weapons and for a shield the build does not use.
  Both hands count together (a two-handed weapon 1.8 times). A bag piece goes on when it adds at
  least 3 % (`best_move`), the bot meets its level requirement (an item without one at most two
  levels above the character), and a two-handed weapon that pushes the off-hand piece out has a free
  bag slot. Tiers are bands of ten levels: band `b` wants item level `10b - 3` and a grade (common,
  common, uncommon, rare, ancient, heroic); `deficit` is how far the eight core slots are below it.
* **Memory** (`GearMem`, `BotState::gear`, one pointer): the grade of bag pieces and the gems of the
  pieces the bot has seen (at most 48 facts, by item id), the equip the duty waits for, the refused
  pieces (ten minutes), the templates bought or minted this session, what the goal could do, the
  session's numbers, the last results. The state feeds it: `SCCharacterState` (worn gems, bag size),
  the bag chunks (grades), the item tasks (grades, gems, moves) and the two results. `BotState`
  itself now moves pieces between the bag and the equipment with their grades (`move_item`), keeps
  the grade of a piece added to the equipment and of a regrade, and keeps the gear memory across
  `reset` (item ids outlive a connection).
* **Equip duty** (`gear::duty`, one line in `brain.rs` `tick`): every `check_secs` (20 s), not in a
  fight, a trade, an instance, while loading, travelling by portal, or while the gear, trade,
  market, auction or dungeon goal runs. It sends one `CSSwapItems`, waits until the piece is worn
  (4 s), and keeps a refused piece out for ten minutes; then it works out what the goal could do
  (`plan::want`: cheap, the merchant offers and the worn pieces) and publishes the view.
* **Goal** (`bot/goals/gear.rs`, `plan.rs`, `machine.rs`): one job per run, in this order: buy a
  better piece (worth it, at most `max_spend_pct` of the money, the merchant within `max_trip_m`;
  while minting is allowed only a piece that meets the tier), mint one (`/item add self <template> 1
  <grade>`: the best piece of a wanted slot that suits the build well, at most the bot's level, at
  the tier's grade and never of a fixed grade above it, no quest, test or NPC item, a core slot
  first), socket a gem (only a piece whose gems are known, a free socket by the grade's limit, a gem
  that fits; always at 100 %, a riskier gem only for a bot with gearhead 0.75 or more, from 30 % at
  gearhead 1), regrade (a grade a failure cannot destroy; a failure that can lower it only from
  gearhead 0.8). `GoalArgs::item` names a piece (put on even when worse) or a gem. The machine walks
  to the merchant, buys one piece and checks the bag, or mints and waits, puts the piece on (one more
  try after a refusal), or casts the gem's or scroll's use skill on the worn piece and waits for the
  result. `score` = `weight * (0.3 + 2 * deficit)` for a piece, 0.7 times the weight for a gem, 0.4
  for a regrade, with `weight = 0.3 * (0.4 + 1.2 * gearhead)`; a failed run rests five minutes.
* **Quest rewards** (`gear::reward_choice`, at the two places the quest goal sends
  `CSCompleteQuestContext`): the reward that adds most to the worn set; for the boxes the gear data
  does not know, a hint from the English name (the build's weapon kind, then its armor kind);
  otherwise the quest code's own choice.
* **Journal** (`journal/gear.rs`): the last change, pieces put on, bought (and the copper spent),
  minted, gems socketed, the last eight failed socketings, regrades.
* **Web**: `BotDetail::gear` (`GearView`: summary, the worn pieces with slot, name, item level,
  grade and gems, the tier, the average item level, the deficit, what the goal could do, the last
  change, the session's numbers), rendered in the bot detail by `renderGear` in `app.js`.

Open points: the client's `SkillCastItemTarget` for a socketing is not known from a capture; the
bots send the caster's object id, a zero `type1` and `type2` (the server reads only the item id).
A bag piece's gems are not parsed (its details are skipped), so a piece that comes back to the bag
socketed counts as unknown and is not socketed again until the server updates it.

## Parties of real players (`bot/companion/`, goal `companion`)

A bot in the party of a real player: it joins when invited, follows the leader, helps in the fights,
obeys a closed set of chat commands and goes through a dungeon door behind the leader. The pieces
are pure decision code, tested without a connection, and thin glue that turns them into packets.

| File | What it is |
| --- | --- |
| `aa-proto` `sc_activity.rs`, `cs.rs` | The team packets a companion needs besides the join ones: `SCTeamRemoteMembersEx` (0xe4: zone key, instance id, level, hit points, mana and position of every member), `SCTeamMemberDisconnected`, `SCTeamMemberRoleChanged`, `SCRefreshTeamMember`, `SCTeamLootingRuleChanged`; the requests `make_team_owner` and `set_team_member_role`. Byte-exact tests were written from the server's encoders; counts are bounded by the bytes left. |
| `bot/party/remote.rs` | `Remote` per team member (role, online, level, object id, zone key, instance id, vitals, position, when it was seen) next to the `TeamLog`; a pure function of the packets, boxed with the rest of `PartyMem`. |
| `policy.rs` | `judge(&Facts)`: may the bot join (or invite) now? In a team, away, dead, trading, dueling, on a journey, busy with a goal it does not drop, the level gap, the caps. |
| `command.rs` | `parse(text)` into the closed set (`follow`, `stay`, `come`, `assist`, `leave`, `dungeon <name>`, `inv`), the dungeon name search (word prefixes), the short lines the bot answers with. |
| `follow.rs` | `decide(&Inputs)`: hold, rest, keep the trip or walk (to the leader, a different stop distance per bot, catching up beyond 20 m, a trip beyond 120 m, rest below 40 % until 70 % with hysteresis); `should_assist(&AssistIn)`: the leader's last target. |
| `upkeep.rs` | `plan(&Facts)`: announce the role once, hand the lead to the first real player online (four tries), leave (asked, no real player, offline for three minutes, level gap plus five). |
| `lead.rs` | `leader_state(&In)`: main world, a dungeon (instance id) or *vanished at an entrance* from the unit in view and the position records; `position` says where to walk. |
| `enter.rs` | `Machine`: the dungeon follow (outside, door, cast, load, inside, exit, away) with its deadlines, typed reasons (`Why`) and the refusals answered by kind through the dungeon goal's `entry::reaction`. `step(&Facts) -> Intent` is pure. |
| `duty.rs` | The glue the brain calls every tick: answer invitations, upkeep, queued whispers, the chat commands, the web snapshot. |
| `mod.rs` | `Mem` (the per-bot state, one box inside `PartyMem`) and `Board` (the hub's count of companions per process and per player, places expire after 45 s). |
| `bot/goals/companion.rs` | The goal: facts for the machine from the tick context, the intents carried out (walk, cast the entrance and exit skills, `/level`, leave the team), the follow step, the assist. |

**Size.** `size_of::<Bot>()` did not grow: the companion memory is an `Option<Box<Mem>>` inside the
existing `PartyMem` box (allocated by the first invitation or command), the caps are on the hub, and
the goal is one pointer.

**Hooks in shared files** (all additive): `Brain::tick` calls `companion::duty` (and starts the goal
when the bot is in a real player's party and runs nothing it must not drop), `route_inbox` hands the
chat lines to `companion::forward_chat`, `dungeon::unsolicited` leaves a real player's invitation to
the party feature, `pick_assist` in the combat controller shrinks the help radius to 12 m for a bot
told to `stay`, `publish` fills `BotSnapshot.party`.

**What the server does that the design works around** (live-verified, see OPERATING.md):

* An invitation never expires and a pending one blocks every later invitation to the same
  character: every invitation is answered within seconds, a no at once.
* Accepting does not check the membership of another team, so a bot never accepts while it is in one.
* A late joiner of an existing team receives no party chat: replies are whispers.
* A character that logs in again is still a member of its team (the server keeps the offline
  member), and gets the join packet again: a restarted bot follows its party at once.
* A team member that casts the entrance of a dungeon its team has opened joins the team's
  instance; the entry limit (`enter_count` per four hours and character) still counts when
  `[dungeons] entry_limit` is on (off by default: the 1.2 server has no dungeon entry limit).
* The `0xe4` record carries the zone *key* (the instance's key inside a dungeon), not `zones.id`.
* The server announces a target change only: a fight that began before a bot joined, or that goes
  on for minutes, is invisible to an event-driven assist, so the goal looks at the leader's last
  announced target itself.
* Whispers between the factions are refused by the server (`CHAT_CANNOT_WHISPER_TO_HOSTILE`).

**Not done.** Healing allies: the rotation's heals are `Aim::SelfOnly` and `rotation::Inputs` knows
only the bot's own hit points; a healer fights like a damage dealer and heals itself.

## Guilds (`bot/guild/`, goal `guild`)

A guild is what the server calls an expedition (`systems/social/expedition.rs`). The feature is
split into pure decision code, tested without a connection, and thin glue that turns the decisions
into packets.

| File | What it is |
| --- | --- |
| `aa-proto` `guild.rs` | The requests (`create`, `invite`, `reply_invite`, `leave`, `kick`, `set_member_role`, `change_owner`, `dismiss`), the `GuildEvent` parser for the nine packets a bot needs (faction list, created, member list in chunks, member status, invitation, join/leave/expel, role, owner, dismissed), the role and error-code constants. Counts are bounded by the bytes left, byte-exact tests were written from the server's writers. `cs::chat_type::CLAN` is guild chat. |
| `log.rs` | `GuildLog`: id, name, owner, members (`since_ms`, role, online), the pending invitation, how the bot last lost a guild, the newcomers to greet. A pure function of the packets (`apply`), idempotent where the server repeats itself (`SCUnitExpeditionChanged` may arrive twice on a leave). |
| `book.rs` | `GuildBook` in the `Hub`: one mutex around plain data, the clock is an argument. A card per bot (character, mother faction, level, guild, can-join / can-found / wishing, refreshed every 5 s, stale after 20 s), the guilds the bots belong to, the foundings under way (`reserve` / `release`, so `max_guilds` counts them), the guild names that are taken, the claim on an invited bot, the pace of invitations and chat lines per guild. |
| `plan.rs`, `forming.rs` | `plan` picks `party_size` wishes of one mother faction (longest wait first, the founder is the highest-level bot that can pay); `Wishes` is the coordinator's list (`CoordMsg::Guild`), which first drops the wishes of bots that left, joined a guild or changed faction (the book knows) and reserves the place in the book. |
| `found.rs` | `Machine`: the founder (wait for the others, invite one after the other, name, create, dismiss the team) and the member (answer the leader's invitation, turn others down, wait for the guild, leave the team). `step(&Snap) -> (Status, Acts)` is pure; every wait has a deadline and a typed `Fail` with a back-off. |
| `life.rs` | `View` over the log: who may recruit, whom the owner promotes, who succeeds a leaving owner, who may leave, whom to greet; the clocks and plans in `Timers`. |
| `duty.rs` | The glue every brain tick (`guild::duty`): card and membership to the book, invitation answers (a human pause first), the join-ignored detector, life steps, requests of real players, the web snapshot (`snapshot_if_changed`). |
| `chat.rs`, `names.rs`, `report.rs` | The lines (no repeat of a recent one, all pass the safety filter), the request detector for whispers ("ginv", "guild invite", "can i join"; not "party", "group", "dungeon", ...), the name generator (mirrors the server's `^[a-zA-Z ]{3,32}$`, five shapes, a Roman numeral from the third try), the issue reports. |
| `goals/guild.rs` | The goal: with a `PartyPurpose::Guild` order the founding (without one, when an operator forces it, a wish). Its score is 0: the wish itself (`CoordHandle::request_guild`) is posted by the duty. |

**Per-bot cost.** `BotState::guild` is `GuildState(Option<Box<Mem>>)`: one pointer until the first
guild packet or duty tick with the feature on. `size_of::<Bot>()` is 3192 with it and the other features (limit 3200).

**Founding through the coordinator.** The duty posts a `GuildWish` when the bot can sign a charter; the
bot goes on with its day and the coordinator keeps the wish for 20 minutes. `Coordinator::form_guilds` runs on
every wish and every sweep; it sends every member of a team the same `PartyOrder` (leader first,
purpose `Guild`), which the brain turns into the goal with the order. Nobody walks anywhere: the
team invitation and `CSInviteToExpedition` go by name and the server checks no distance. The founder
releases the reservation in `stop`; one that is never released ends after ten minutes.

**Server facts the bots keep to** (read from `expedition.rs`, `team.rs`):

* A charter needs a team of `PartyMemberCount` (five), every member level 5 or more, unguilded and of
  the founder's mother faction, and 10000 copper from the founder; the name is compared exactly
  and must match the regex. Refusals are `SCErrorMsg` codes 293, 294, 295, 296, 297, 299, 321,
  330 and 761; each has a back-off (`Fail::back_off_ms`).
* An owner who leaves leaves the guild without a leader: the bots hand the guild over first
  (`CSChangeExpeditionOwner`) and wait for `SCExpeditionOwnerChanged` before they leave.
* By default only role 2 and up may invite, only the general (255) may promote and expel. Refused
  invitations and replies are silent, so every request has a deadline and a detector.
* An invitation lasts 60 s and a target has one pending; a team invitation never expires and
  blocks later ones, so a founding member answers at once and turns down strays
  (`dungeon::unsolicited` spares the goal `guild` for the same reason).

**Detectors.** A "join" that did not become a membership within 15 s, a creation without an
answer, a leave or hand-over or promotion that changed nothing, and a creation refused for a
reason the bots' own checks exclude are reported with the keys `guild join-ignored`,
`guild create-no-answer`, `guild create-refused code=N`, `guild leave-ignored`,
`guild owner-change-ignored` and `guild promote-ignored` (area `systems/social/expedition.rs`).

**Tests.** Unit tests cover every pure part (the parser with byte-exact packets, the log, the
book, the planner, the coordinator's forming, the machine, the decisions, the lines, the names, the
reports, the config). `tests/guild_real_data.rs` (ignored without the server data, see its header)
drives the real brain against the real world: an owner promoting and recruiting its own faction
only, the invitation policy, whispers for an invitation, a hand-over before a leave and the rest
after it, a founder gathering a team, a wish reaching the coordinator.

## PvP (`bot/pvp/`, goals `gank` and `warzone`)

Everything is behind `[pvp] enabled`. The pieces:

| Piece | Where | What |
| --- | --- | --- |
| Rules | `bot/pvp/rules.rs` | Pure: may the bot attack a character, for a defence, a hunt or a gank? Mirrors the server's `can_attack`: the hostile relation is by mother faction (Nuia 148 against Haranya 149); a character is protected where its alliance owns the zone (the target's mother faction is the zone's faction or its mother; a zone without an owner counts as the neutral faction 2); the town peace buffs (2149, 2221, 4828) forbid combat skills. Then the operator's limits. |
| Self-defence | `bot/pvp/defence.rs`, `bot/pvp/fight.rs`, `bot/combat.rs` | `defence::pick` finds the character that attacks the bot (the controller's threat list, or a character that lost hit points next to the bot); `fight::judge` ends a fight with a character (retreat, the protected-zone signature, time); `fight::start_flee` runs. The controller has a `fleeing` flag, so goals that do not own combat wait. |
| Post planner | `bot/pvp/post.rs` | Pure: walks the line between each workbench and the best buyer of its best pack, keeps the points outside the victims' land and scores them (pack price, conflict zone, war, the first point past the home land, minus the way). |
| Squads | `bot/pvp/squad.rs`, `bot/coordinator.rs`, `bot/party.rs` | A waiting bot sends a `Standby` offer; the coordinator groups offers of one alliance and sends every member a `BotCmd::Party` with `PartyPurpose::Ambush { x, y }`, which the brain turns into a gank goal that knows its squad. The squad needs no further messages: all members pick the same carrier (`squad::pick_target`) and take turns at the doodad. |
| Gank | `bot/goals/gank.rs` | `Plan -> ToPost -> Lurk -> Fight -> Collect`. `verdict` and `CollectJob` are pure. A finished gank with a pack in the backpack slot continues as a trade run (`Gank::resumes_run`, the same hand-over the rest goal uses), whose carry-only plan sells it. |
| Warzone | `bot/goals/warzone.rs` | `Plan -> Go -> Patrol <-> Fight -> Regroup`; pure `wars`, `pick_war`, `engage`, `patrol_spot`. |

**State.** `BotState::pvp` is one pointer (`Option<Box<PvpMem>>`, allocated by the first use): the
backpack templates of the characters in view, the shun table, blocked posts, the flee in progress,
war kill counts and the last honor the server said. The one cheap per-unit fact is the `U_PACK`
flag in `Unit::flags` (the backpack slot is occupied). `size_of::<Bot>()` is 3192 bytes with the guild and the other features (limit
3200).

**Protocol.** The backpack is equipment slot 26 (`BACKPACK_SLOT`); a character's state packet carries
it in its equipment block, `SCUnitEquipmentsChangedPacket` (0x094: `u8 count`, then `u8 slot` and an
item or four zero bytes per slot) updates it, and `SCUnitPvPPointsChangedPacket` (0x204: `u8 kind`,
`i32 points`; kind 0 honor, kind 1 hostile faction kills) follows every kill of a hostile character.
When a character dies its pack drops as a doodad (one of the `put_down_backpack_effects` doodads,
`World::is_pack_doodad`) that anybody with an empty backpack slot picks up with the doodad's func
skill (11361) within a few metres; the pack goes straight to slot 26. The doodad is kept in the
view although it has no flags the bots otherwise care about.

**Zone groups.** `World::zone_info_at` gives the zone group and the owning faction of a point,
`World::zone_group_spots` the spots (every fourth sector centre) of a conflict zone group, and
`World::is_conflict_group` the groups that take part in the zone conflicts (`conflict_zones`, not
closed).

## Events (goal `event`)

The server runs *rifts*: tower defence events (its `tower_defs` tables) in which controller NPCs
summon waves of monsters. An event is keyed by (definition id, zone group) in world 0; the waves are
"steps", a 1-based index into the definition's progs ordered by id. A wave ends when its timer ran out
(if it has one) and/or every kill target it names has died (`cond_comp_by_and` picks AND or OR); kills
count for anyone in the zone group. The event ends after its last wave, by `/towerdef end` or at
`force_end_time` (3600 s for the Crimson Rift).

* **Packets** (`aa-proto` `sc_event.rs`, wired in `sc_world.rs`): `SCTowerDefList` 0x16b, `Start` 0x16c,
  `End` 0x16d, `WaveStart` 0x16e become `WorldEvent::{TowerDefList, TowerDefStart, TowerDefEnd,
  TowerDefWave}`. A list entry is 41 bytes (def `u32`, zone group `u16`, zone `u32`, spot `u32`, target
  object `bc`, x and y as fixed-point `i64`, z `f32`, step `u32`); the count is checked against the body.
  The server sends a list on world entry and after each start and wave. Its position is where the
  *target unit* stood at that moment, and the target of the Crimson Rift **walks a path** (about 1 m/s),
  so a bot that sees the unit (`state.unit(obj)`) follows its live position (`live_rally`).
* **Data** (`aa-world` `event.rs`): `Events::load` joins `tower_defs`, `tower_def_progs`, spawn targets,
  kill targets, `npc_spawner_npcs` and the conflict zones with the NPC spawn data. An `EventDef` has the
  controllers (spawner, step, template, position, zone group), the kill targets per step (template and
  count), the timer, the time cap, the level range (the levels of the kill-target NPCs; a definition
  without kill targets has 0 and is not joinable) and the war zone group of a conflict definition.
* **The board** (`playerbots` `events/board.rs`, `hub.events`): the process-wide `EventBoard`. The first
  bot to hear a packet posts it (`on_server_event`, called by the bot loop after `state.apply`); the
  others find it there. One short mutex plus an atomic count, so a bot with no event running reads one
  atomic. It keeps the running events, the step and its start, the target, the slots (leases that
  the goal renews every tick, `LEASE_MS` 25 s), the join delays, the bars (hurt 5 min, idle 10 min, three
  deaths) and the evidence of the detectors. Flood control: `max_bots_per_event`; `max_share` of the
  online bots that are high enough for the event (`Population`, rebuilt by `Hub::tick` once a second
  while an event runs; unknown means no share cap); the join delay is a deterministic draw in
  `join_delay_secs` from (bot, definition, first seen). It logs `event started / wave / target / ended`
  on target `pb_events`.
* **The goal** (`bot/goals/event.rs`, decisions in `event/plan.rs` and `event/pick.rs`):
  `Event{run: Option<Box<Run>>}` is one pointer, so `Bot` stays within its size budget. `duty` (called by
  the brain every tick, one atomic read when idle) starts it in place of none, wander, rest, grind or
  quest; `score` (12) does the same when the brain draws. `offer` is the eligibility: `enabled`, a
  running event, level at least the definition's, hit points 70 % or more, not away (instance, loading,
  dead), not a war definition, the zone group not at war with `avoid_war_zones`, a way short enough
  (4000 m on foot, 15000 m with portals, any with GM `/move`), the join delay over, a free slot.
  Phases: *Go* (`Ctx::travel`, so a long trip takes portals or `/move`; trips are limited in time and
  continued while they gain) and *Hold* (within 40 m of the target, at an own spot in a ring of 6 to
  18 m so bots spread). In *Hold* the bot fights the nearest unit that is a kill target of the current
  wave, else a monster of another wave of the event (a straggler), within 55 m of the target and 70 m of
  the bot, one nobody holds before one somebody holds (`hub.contest` claims). Controllers (the Rift
  itself, the summoners) and every monster that is not the event's own are never attacked: a wave that
  names no kill targets (the first, a timer) is waited out. It leaves when the event ends, below
  40 % hit points, after three deaths, or after 8 minutes of nothing to fight (the wave timer and three
  minutes when that is longer; a new wave restarts the wait).
* **War zones** (`bot/war.rs`): `Ctx::in_war_zone(x, y)` is `[events] enabled`, `avoid_war_zones` and the
  zone group at (x, y) (`World::zone_key_at`, `travel.zone_groups`) at conflict or war in
  `BotState::zone_states`. Grind (`find_spawn`), wander (`random_point` filter) and the quest planner
  (`plan_target`, `find_candidate`) do not choose such a destination; the `warzone` and `gank` goals never
  ask. A bot already inside is not moved out.
* **Detectors** (`events/detect.rs`, pure `judge`, run by `EventBoard::sweep_witnessed` about once a
  second): *no wave monster*: a participant stood at the target for 30 s, 60 s plus the walk from the
  farthest controller (4 m/s, at most 240 s) after the wave began, and no unit of the wave and no
  kill was seen (`event-wave-no-npcs def D wave N`); *stuck wave*: every kill the wave waits for was
  seen but the event stayed in the wave 30 s later (`event-wave-stuck def D wave N`); *not ended*: the
  event outlived its cap by 60 s (`event-not-ended def D`). Each is told once per wave (per event for
  the last) and is filed for each of up to three bots that stood in the event (so it is an issue seen
  by several bots, with their trail), suspect server, area `systems/instances/tower_def.rs`. A stale
  event is dropped from the board 120 s later.

Deviations from the brief, on purpose: the "no wave monster" wait is 60 s *plus the walk* (the
summoners stand up to 1500 m from the target); `max_share` counts the online bots that are high enough
for the event, not all bots (so a share of 0.3 of 20 such bots is 6 whatever else is online); a bot
does not fight monsters that are merely near the target when the wave names no kill targets (the first
live run did, and the camp around the Grimghast Rift killed the bots in under a minute).

## Navigation motion (`bot/nav/`)

Bots walk routes on the navigation mesh (`aa_world::nav`) instead of straight lines;
`[behavior] navmesh = false` brings the straight lines back.

- **Where it plugs in.** `Motion` keeps its straight-line walker, the stuck detector, the stopped update
  on arrival and `Motion::pin`. A bot has a `NavLink` (`Motion::attach_nav`, built by
  `nav::link_for_bot`) and `Motion::go_to` starts a `NavState` trip; each tick the trip either moves
  the bot along its route or says "walk the straight line" (`NavStep::Legacy`). The goals' API
  (`go_to`, `travel_to`, `stop`, `face`, `status`, `remaining`, `pin`) did not change; `sync_place`
  tells the controller which world's mesh to use (main, instance, or none).
- **Planning** (`trip.rs`, `planner.rs`). A trip whose goal is within 150 m is searched on the bot's
  own task (cap 12k expansions). A longer one goes to the planner pool: one or two `std` threads and
  a bounded queue (512), owned by the hub (`Hub::nav`, started with the first bot). The answer
  comes back as `BotCmd::PathReady { ticket }` on the bot's own command channel. A full queue, an
  answer later than 1.5 s, `Unreachable`, `NoMesh` (after a second try that snaps from 24 m) all
  mean the straight line. While it waits the bot stands and faces the goal. A partial answer, or a
  goal beyond 2 km, is walked in pieces: the search for the next piece starts the moment a route is
  installed, from its end (at most 10 pieces). Tickets make stale answers harmless.
- **Walking** (`follow.rs`). `Route` is a parametric curve over the path's corners: outdoor corners
  are rounded by a quadratic fillet (start up to 2 m before, never more than 0.3 m inside the
  corner, because the funnel's agent-radius clearance must survive), indoor and waypoint corners are
  exact, long stretches get a small per-bot lateral offset that fades at both ends and is only used
  where the mesh has room beside the line. Heights come from the terrain outdoors and from the
  path indoors and in instances. Run outdoors, walk indoor segments (unless longer than 25 m), slow
  to walk speed over the last 5 m before the stop point. The stop point is the circle of the stop
  radius around the goal, like the straight line.
- **Stuck.** The old detector (under 0.3 m in 3 s) first re-plans twice with the nodes on and
  ahead of the bot made expensive, then falls back to the straight line's side steps and finally
  `Failed(Stuck)`.
- **Time limits.** A planned route may take `elapsed + length / 3 m/s + 30 s` (never less than the
  90 s of the straight line), so legs of 1.5 km work; goals that cut walks into legs
  (`trade_run`) ask `Motion::leg_m()` (1500 m with the mesh, 280 m without).
- **Numbers.** `NavMetrics` (atomics in `Hub::nav`): searches inline / pooled, outcomes, latency
  histograms (p50 / p99), replans, stuck events, straight-line fallbacks by cause, queue depth,
  trips and how they ended, detour ratio. They are in `HubStats::nav` (`/api/stats`) and logged
  once a minute.
- **Web.** `Bot::publish` writes the rest of the route (at most 32 points, Ramer-Douglas-Peucker
  decimated) into the hub slot; the map draws it for the selected bot.

## Life-like movement (`bot/manner.rs`)

A bot that only walks and stands still looks like a bot. With `[behavior] lifelike` (default on) the
movement controller adds what a person does, **for the eyes of a real player only**: nothing happens
unless a character that is not one of our bots is within the watch radius (150 m; the movement
update rate goes up for every real player who can see the bot, see
[Movement updates](#movement-updates-botmotionrs)), and nothing while the bot fights, loots, casts,
trades, sails, is in
an instance, dead or loading (`Bot::life_busy`). `jump_rate` scales the jumps (0 = none), `emotes`
switches the emotes.

- **Where it plugs in.** `Life` (traits, jump state, schedule, mannerisms, emote state; one
  `Box` per bot, `Option<Box<Life>>` in `Motion`, so the bot gained 8 bytes) is owned by `Motion`.
  `Motion`'s public API did not change; its tick calls the parts and `Bot::on_tick` calls
  `manner::tick` for the emotes and to tell the controller the moment (`Gate`: audience, busy, town).
  The parts are plain values without clock or socket (`Traits`, `Arc`/`Jump`, `JumpSched`, `Fidget`,
  `Dawdle`, `Emotes`, `Budget`), so the rates and limits are unit-tested over simulated hours.
- **Wire fixes** (`aa_proto::cs::move_unit`): velocity shorts are `m/s * 32767/60` (the client's
  scale; they were `m/s * 1000`, which a remote client reads as 1.8 times the speed), a
  walk has actor flag `0x01` and half the stick (`delta` 63), a stop has actor `0x04` and no
  velocity, a swimming bot sends stance 2.
- **Jumps.** The client's physics: 1.3 m, gravity 25 m/s^2, so 8.06 m/s up, apex after 0.32 s, 0.65 s
  in the air (a hop over a step: 0.55 m). Packets go at take-off (header `0x04`, `0x06` with a stick
  held, actor `0x10`, `vel.z` up), at the apex and on the way down (header `0x00`/`0x02`, actor
  `0x00`, height in `pos.z`), at touchdown (header `0x04`, actor `0x14`, no vertical speed) and 90 ms
  later as an ordinary ground update. The bot's real position stays on the ground; the height is only
  in the packets. A running bot keeps its horizontal speed. A jump wakes the bot loop for its next
  packet (`Motion::wake_in`, at least 15 ms apart); the regular updates wait while it lasts.
  Triggers: a straight stretch of 6 m or more while running (one per 30 to 60 s times the bot's
  multiplier, 10 s cooldown, one in ten is two in a row), a step of 0.3 to 1.2 m ahead (a hop, 15 s
  cooldown), after the stuck detector's side-step, in place after 90 to 180 s of standing, and to
  cheer a level or a finished quest. A bot's playfulness comes from its id: 12 % never jump, the rest
  jump 0.35 to 1.65 times as often as the rate. A pin, a stop, a teleport or a new height ends a jump
  (a bot never jumps with a range-checked packet queued, in combat, swimming, sitting, or while a
  teleport waits).
- **Mannerisms.** After a trip a standing bot turns in place one to three times (30 to 120 degrees,
  2 to 6 s apart) and sometimes shuffles 0.5 to 1.5 m at walk pace; at some corners of a long route it
  stops for 0.3 to 1 s; in a town (the peace aura or a crowd of eight) a quarter of the short trips
  are walked; every bot runs within 3 % of the configured speed. (The tick period no longer
  varies in front of a player: uneven updates looked like stutter, see
  [Movement updates](#movement-updates-botmotionrs).)
- **Emotes** (`CSExpressEmotion`, `SCEmotionExpressed`). Greeting (a player within 6 m, 60 % of the
  time, once per player per 10 min), an answer to an emote of a real player (90 % when played at the
  bot, 30 % nearby, at most one per player per 30 s), ambient emotes in towns (at most one per 45 s
  per bot), a celebration with a jump for a level or a quest, and the sit emote (id 48) 8 to 25 s into
  a rest; the stand-up is a stopped update. Each bot waits 8 s between emotes, and a process-wide
  allowance (5 ambient and 15 social a minute) keeps the server log quiet, because the server logs
  every emote at WARN.
- **Ground truth.** What the real 1.2 client sends was captured with a bot that logs the relayed
  packets (`RUST_LOG=pb_move=trace`): a standing client sends header `0x00`, actor `0x04` about every
  3 s; walking, about seven updates a second with header `0x02`, actor `0x04` and a vertical speed
  on slopes; a stop is two updates 47 ms apart (`0x04`, the second without velocity; bots send one);
  a fall is header `0x00`, actor `0x00` and ends in `0x04`, actor `0x14`. The **take-off flags of a
  jump and the emote packets are not confirmed by a capture** (the client driver could not press the
  jump key or run an emote); they follow the server's `MoveTypeFlags` (`Moving | Stopping` is a
  jump), the touchdown and the client code. `RUST_LOG=pb_life=debug` logs every jump and emote a bot
  makes.
- **Live check** (20 bots for 16 minutes near the Nuian start with real characters in range, against
  the same bots with `lifelike = false`). Jumps: 258 started, 48 per bot-hour (31 on the run, 17
  hops), 18 of the 20 bots jumped. Emotes: 36, about 2.3 a minute, one server WARN line each (19
  celebrations, 11 ambient, 3 greetings, 3 sits). Outgoing packets per bot stayed within the
  run-to-run noise (1.6 to 1.9 a second with it on, 1.7 off, in the same place); the server log had no
  speed-audit or landing lines. The relayed packets (`pb_move`) showed every jump as the sequence
  above, the take-off and the touchdown always in pairs.

## Movement updates (`bot/motion.rs`)

What another player sees of a bot is the stream of its `CSMoveUnit` updates, which the server
passes on unchanged (`SCOneUnitMovement`). The real 1.2 client sends about seven updates a second
while it walks, the server's own NPCs ten (one per AI tick), both at an even pace.

- **The pace.** A walking bot that a real player can see sends an update every
  `bots.move_update_ms` (125 ms, about eight a second), evenly: between two brain ticks
  (`bots.tick_ms`) the bot loop wakes for a movement-only tick (`Bot::on_motion_tick`; the
  schedule is the pure `bot::next_wake_ms`) that moves the bot on and sends the update, nothing
  else. A moving update is due within 20 ms of its gap (`SEND_SLACK_MS`), so a timer that wakes a
  little early does not skip one. A bot nobody can see sends one update a second.
- **Who can see it.** The server shows a unit to the players in its own 64 m region and the eight
  around it, up to about 181 m away; a real character within `VIEW_RADIUS` (190 m) counts. The
  watch radius of the life-like movement and the chat stays 150 m.
- **Chases.** The combat controller aims a chase again when the target stands more than
  `CHASE_RETARGET_M` (1 m) from the point the bot runs to (`combat::chase_again`; it was 2 m). With
  2 m a bot chasing a moving target stopped where the target had been, short of it, and started
  again at once.
- **Measured** (`tests/movement_real_data.rs`, ignored, needs the real data with the navigation
  mesh: one bot with the mesh, the planner pool and the life-like movement, ticked like the bot
  task; every update decoded and compared with where the one before said the bot would be). Before,
  in view of a player: 3.9 updates a second, 210 to 300 ms apart on purpose (the tick period varied
  by 20 % either way); out of the 150 m watch radius but still in view: one a second, and at a
  corner the bot was up to 10.7 m from where its last update put it. After: 7.7 to 8.0 updates a
  second, every one 125 ms after the one before (140 ms at most, a jump), at most 0.06 m off the
  dead reckoning of the update before on 99 % of the updates of a trip (1.3 m at the worst corner,
  from 2.9 m). A melee chase of a target that keeps moving at 4 m/s stopped and started again 115
  times in three minutes with the 2 m rule, 46 times with 1 m: what is left is the stop of a
  fighter that caught up with a slower target and stands in reach until it is out of reach again.
  The update `time` stays the bot clock in milliseconds: the server only requires it to grow
  (`move_verify.rs`, reason 0).

## Portal travel (`bot/travel/`)

Bots record Teleport Book entries, open gates to them and use gates; `[behavior] portals` switches it
all off.

- `book.rs` the bot's picture of the book: private entries (id, zone key, wire offsets, world
  position once known), visited recall districts (their positions come from `recalls.json`) and the
  bound recall point. The wire sends an entry as an offset from the origin cell of its zone
  (`ZoneOrigins` in `aa-world`, one cell per zone, learned from a recall district or from the spot the
  bot stood on when it recorded, shared by all bots and kept in `portal_zone_origins.txt` next to the
  roster).
- `plan.rs` the route as a pure function: walk, gate to the entry nearest the destination (within
  400 m) or recall, by estimated seconds; trips under 1.2 km are walked.
- `journey.rs` `Journey`, the state machine any goal or the party code drives: `Journey::new(dest)`
  opens a gate (top-up of the book, stones and ink with `Ctx::gm`, cooldown wait, cast, wait for the
  bot's own gate NPC 3891, get within 3.5 m, `CSUsePortal`, detect the move), or recalls;
  `Journey::follow_gate(owner)` goes through the gate that character opened. `step(&Snap) -> Acts +
  JourneyStatus` is pure and tested; every wait has a deadline and a typed `JourneyFail`
  (`NoRoute`, `NoBook`, `NoReagents`, `OpenRefused(code)`, `NoGate`, `GateGone`, `UseRefused(error)`,
  `NoTeleport`, ...). A failed journey never retries: the caller walks and the destination is walked
  for two minutes.
- `drive.rs` the glue: `Journey::tick(ctx)`, `far_trip` (the hook of `Ctx::travel`: a far trip goes to
  an implicit journey when the planner finds a route, goals need no change; `has_route` lets the trade
  run ask for a far trip in one piece instead of walking legs), `duty` (called by the brain every tick:
  steps the implicit journey, records entries, visits recall districts with `CSNotifySubZone` like the
  client, lets an idle bot hop through another bot's gate now and then, and keeps the goal waiting while
  a cast or a gate is under way) and the detectors that file issues (record cast with no entry, gate
  cast accepted and stones taken but no gate, `SCUnitPortalUsed` without a teleport, `CSUsePortal`
  refused or unanswered, refused skill starts).
- `places.rs` where a spot is worth an entry (a recall point within 120 m, two quest givers within
  60 m, a pack buyer within 40 m, a dungeon entrance within 40 m, the bot's house), the recording rules
  (`decide`: one record per 10 minutes, calm, no entry within 600 m, ink, book of at most 20 entries,
  the most redundant entry goes first) and the entry names.

Server rules the live run showed (all of them are data, not bugs): a bot with a trade pack gets
`SCErrorMsg` 610 on `CSUsePortal` and result 50 on the open skill (a buff of the pack), so a laden bot
plans no gate and opens or follows none (`JourneyFail::Laden`); the record skill 11215 is not on the
`skill_reqs` whitelist of the peace-zone buffs (2149 around a Nui statue, 2221, 4828), so it is refused
with result 50 there. `Portals::blocking_buffs(skill)` reads such buffs (and stun, landing, ...) from
`skill_reqs` for the record, open and recall skills; the bot waits for one to go (`JourneyFail::Blocked`
after 15 s) instead of casting into a refusal; with `bots.long_travel_teleport_m` set a GM
`/move` is faster than any gate, so far trips are left to it.

`TravelState` is one pointer (the data is a lazily created `Box<Inner>`); `size_of::<Bot>()` grew by 8
bytes. The server events it needs are routed by `state::route` (the portal events and the answers to
skills 11215, 11216, 10594 and 16841); `apply_unit_state` tells it about gate NPCs (with their owner).

## Mounts and gliders (`bot/mount/`)

A bot rides a mount on long trips and glides down cliffs. The pieces:

| Piece | Where | What |
| --- | --- | --- |
| [`plan`](../crates/playerbots/src/bot/mount/plan.rs) | pure | `Facts` -> at most two `Act`s: summon / board / leave / dismiss the mate, equip / deploy the glider, and the ledge rule. Every wait has a deadline and a refusal back-off. |
| `duty` | `bot/mount/mod.rs` | The glue the brain calls every tick: reads the bag, the equipment and (with a worn glider) the terrain ahead, applies the acts, keeps the motion speeds and the movement object in sync. |
| `Mem` | `bot/travel/mod.rs` (`Inner::mount`) | One per bot behind the travel pointer, so `size_of::<Bot>()` does not grow. The phase, the mate (`tl` / object id / position), the glider and the clocks. |
| Motion | `bot/motion.rs` | `set_speeds` (mount pace), `set_move_obj` (movement packets name the mate while riding) and the glide (`TripMode::Glide`): a fixed heading, a descent, a landing and a navmesh replan. |
| `Mounts` | `aa-world` `travel/mount.rs` | The server data: rideable `item_summon_mates` items (a driver seat in `model_bindings`), glider items (`item_backpacks.backpack_type_id = 2` with a use skill) and the lowest-id defaults. |

**Server facts.** A mount is the summon item's `SpawnPet` skill (special effect type 24;
`item_summon_mates` names the mate NPC), `CSMountMatePacket` 0x0a7 with the driver point, and
`SCUnitAttached` back. A riding player's own `CSMoveUnit` is ignored (`world/session.rs:1191`), so
the bot sends its movement updates with the mate's object id (the server accepts an owned mate as
the moved target and guards its speed) and the character follows the attachment. `SCUnMountMate`
0x0a8 and `CSRemoveMate` 0x0a4 end the ride. A glider is a back-slot item whose use skill the
server accepts only with that item as caster (`systems/skills/glider.rs`); the flight itself is
client physics, so the bot integrates it and sends the glide stance (4, `skills/glider.rs:71`) in
its updates. The 1.2 client's exact mount speed and glider descent are not captured: `[mounts]
speed`, `glide_speed` and `glide_descent` are operator settings, defaults conservative.

**Tests.** The plan (summon threshold, combat/stop/blocked dismounts, refusals and timeouts, the
far-mate dismissal, the idle despawn, the ledge rule, the glider grant) and the motion (glide
descent, landing, stance packet, speed change, mate object id) are unit-tested in
`bot/mount/tests.rs`; the packets have byte-exact tests in `crates/proto/tests/` (mount, unmount
and remove builders; the `SCMateSpawned` reader); `travel_real_data.rs` checks the 1.2 defaults
(4177 / 16385) against `compact.sqlite3`.

## Sailing (goal `sail`)

A bot gets a boat summon scroll, summons a boat, sails it along a planned route and comes ashore.
Only the sea is planned: lakes and rivers are out of scope (the sea mask knows nothing of them).
The boat is the rowboat unless `[sail] ship` names another hull; the bigger hulls (clipper, cutter,
junk, yawl, schooner) launch, steer, plan and berth differently, see [Bigger hulls](#bigger-hulls).

```text
Gear -> Plan -> Shore -> Summon -> Board -> Sail -> Land            Abort (every failure, then Failed)
```

- **Gear**: a scroll in the bag is used; otherwise, with `gm_bootstrap`, `/item add self <scroll> 1`
  with the cheapest scroll of the hull family (`ShipFamily`, `Boats::scroll_for`): the default is
  17906, the rowboat (the cheapest scroll of the data with a driver seat and a top speed of 2.5 m/s
  or more). `[sail] ship = "auto"` takes the family of a scroll the bag holds, else a boat of the
  bot's own (a roll seeded by the character id; merchants favour the schooner). A boat left over
  from an earlier trip is despawned first. Without GM and without a scroll the goal fails
  (`NoTarget`) and the brain's 60 s failure cooldown applies.
- **Plan** (`goals/sail/trip.rs`): the nearest beach with open water in front of it is the launch
  (`find_shore`: land 3 m behind the stand point, water 12 to 28 m off the beach); the landing is a
  beach near a random open-water point 1 to 3 km away (twice as far for a hull that does 6 m/s)
  whose sea route is at most 2.2 times the straight line; otherwise a sightseeing loop out to a
  point at sea and back to the launch. Aggressive monsters that live on the water (`danger_zones`:
  the spawns of NPC templates that attack on sight and that the bot's faction is hostile to, e.g.
  8563 / 13012 near (15344, 16346); at most 400) close a 100 m disc each to the planner (140 m
  shuts the whole of Castaway Strait to a trip). A failed plan says why in the bot's log.
- **Sea map** (`aa-world::water`, `World::water()`): 16 m cells from the 4 m height grid, built on
  first use by up to four threads (about 1.4 s in a debug build, a tenth in release) and shared by
  all bots: a cell is sea when every height sample it touches is a real terrain sample under 98 m.
  35 x 2431 cells of bits, **664 KiB**. The large hulls use a second mask of the same size
  (`World::water_deep()`, terrain under 95.5 m: 4.5 m of water), built at their first trip. The
  planner is an A* over open cells (a cell is open when its square neighbourhood of `clear` cells
  is all sea) with hashed scratch, a node bound, a shore penalty and line-of-sight smoothing; the
  rowboat's route keeps two cells (32 m) off the beach, one as a fallback.
- **Shore / Summon**: the bot walks to the stand point, faces the water, waits 0.8 s and casts the
  scroll's skill on itself (`start_skill_item`). Refusals of the place (`SkillResult` 0x23, error
  messages 100 / 583) pick another beach, at most two more times; an unexpected refusal is a
  `SkillRefused` finding; a boat that does not appear within the cast time plus 10 s is an issue.
- **Board**: the bot swims to the hull (the server measures the bind reach to the hull's mass box,
  under 4 m of surface gap; for the bigger hulls the bot aims at a point 0.3 m outside the box and
  binds from a gap of 1.4 m) and sends `CSBindSlave` from 2.8 m, closer after a "too far" answer.
- **Sail** (`bot/vehicle/{helm,follow}.rs`): pure-pursuit on the waypoints (rowboat: lookahead 70 m,
  waypoint reach 40 m, arrival 18 m) and a hull-aware rate-tracking heading controller. The server
  turns slowly (a rowboat 4.35 degrees per second, angular acceleration 0.125 rad/s^2 at full helm;
  both from `ship_models.turn_accel` and the kind's turn cap, `SlaveTemplate::turn`), so the
  controller asks for the yaw rate that stops on the target heading (`sqrt(2 a err)`), converts the
  rate error to a steering value and slows down for sharp turns. The turn factor of the server
  (0.5 at a standstill, 1 from 2.5 internal units, which are twice the real speed) enters the rate
  conversion. A steering probe at the start (80 steering with 100 throttle for 2.5 s) measures which
  way the wheel turns (the documented direction is a positive steering that lowers the yaw), and a
  divergence watch flips the sign once more if the heading runs away *and* the hull turns against
  the helm (a hull that turns the right way but too slowly for the route keeps its sign). A
  `ShipRequest` is sent when the throttle changes, the steering moves by 6 or more, or as a 1 s
  heartbeat (the server keeps a request until the next one).
- **Speed**: the throttle is an acceleration request, not a speed: the server accelerates the hull
  for as long as the throttle is ahead, up to its cap, and brakes it with an astern throttle (2.4
  times as hard as it accelerates). So `throttle(speed, wanted)` goes ahead in proportion to the
  shortfall, to 0 within a small band and astern in proportion to the excess, and `wanted_speed`
  is the top speed (1.1 times: the server caps the hull itself) unless the route asks for less
  (`Guide.v_limit`) or the bow is far off the wanted heading.
- **Stuck**: less than 3 m of progress in 8 s with the engine on, or the server's `stuck` flag
  (which is never set), or an impact (a hull that ran at 1.5 m/s or more within the last 6 s and
  now stands still with the engine on), backs the boat off astern (4 s and a tenth of a second per
  metre of hull) and plans the route again from where it is; the fourth episode aborts.
- **Land**: brake (astern while above 2.5 m/s), set the throttle to 0, `CSDiscardSlave`, then
  `CSDespawnSlave`, then walk (swim) to the landing stand point. Each request is repeated once
  after a timeout; a missing answer is an issue and the trip goes on.
- **Never leave a boat behind**: every way the goal ends (done, failed, interrupted by a rest, a
  forced goal, death) passes through `Sail::stop`, which stops the helm, leaves the seat and
  despawns the boat. A cast that is still in flight when the goal is interrupted can still put a
  boat on the water; the next sail goal despawns it first and the server removes it at logout.
- **Scoring** (`goals::sail::score`): 0 unless `[behavior] sailing`, outside instances, hit points
  60 percent or more, online 3 minutes or more, 30 minutes since the last trip of this bot, a scroll
  obtainable (bag or GM) and a coast within 450 m (terrain probes under 98 m); the weight is
  `0.2 * (0.3 + 0.7 * (1 - homebody) + 0.7 * merchant)`, so explorer- and merchant-like bots go
  more often than homebodies. `base_weight` stays 0 (the goal owns its weight).
- **Fast ticks**: `VehicleState::fast_tick` keeps the 250 ms bot tick while the goal has a boat or a
  seat (the idle tick is 1.5 s). On the boat the goal owns the combat decisions
  (`owns_combat`): a fight does not pause the helm; the combat controller only defends.
- **Detectors** (`ctx.report_issue`, one per key and bot): `sail:no_slave:<skill>` (cast accepted,
  no boat), `sail:bind_no_attach` (`SCSlaveBound` names the bot, no `SCUnitAttached`),
  `sail:bind_no_response`, `sail:no_ship_movement`, `sail:no_thrust` (engine on 5 s, deep water,
  not aground, speed 0), `sail:dead_wheel` (helm hard over at speed for 10 s, no turn),
  `sail:slow_boat` (the top speed seen is under 75 percent of the data's, on a route over 600 m),
  `sail:despawn_ignored`, `sail:discard_ignored`, `sail:error_code:<id>` (an unexpected
  `SCErrorMsg` after a boat action).
- **Memory**: `VehicleState` is one pointer (the `Sailor` is allocated by the first sail goal or
  summon and kept), `Sail` is one boxed `Machine` (a few hundred bytes while a trip runs), the sea
  map is 664 KiB once (and the deep one 664 KiB more once a large hull sails). `size_of::<Bot>()`
  is 3192 bytes (limit 3200), `Goal` is 152 bytes (limit 160); the hull data and the route live
  in the boxed machine.
- **Live behaviour of the rowboat** (QA server, scroll 17906, slave 15): the boat takes 10 to 20 s
  from the cast to the bind, the helm probe finds the documented steering sign, the boat holds its
  heading within a degree on the straights and its top speed is 3.2 m/s: `ship_models.velocity`
  6.4 times `MoveSpeedMul` squared (2.0) over 8. (This document used to say velocity times
  `MoveSpeedMul` over 2, which is twice the speed; the server moves the body at the internal
  speed times `MoveSpeedMul / 4` and caps the internal speed at `velocity * MoveSpeedMul / 2`.) A
  crossing of 1.6 to 3.4 km takes 9 to 18 minutes, landing, despawn and the swim ashore about 20
  seconds. The server keeps a despawned (or logged out with) boat in its `slaves` table and puts it
  back on the water at login, at its old position: the Gear phase despawns such a boat first. A
  scroll is not consumed by the cast.
- **Limits**: sea only; the bot that is made to leave the goal at sea (a forced goal, a rest) is left
  swimming where it is; passengers (`take_passenger_seat` on seat 2 of the rowboat) are not done;
  the bigger hulls sail with their driver only (no crew, no cannons, no cargo yet).
- **Tests**: the water mask, the planner (detour, no route, discs), the shore and launch finders,
  the heading controller (also with an inverted wheel and other hulls), the route follower, and the
  whole machine against a scripted server (`goals/sail/tests.rs`: a rowboat, a clipper and a
  cutter with the server's turn and speed physics (`vehicle/sim.rs`), GM scroll, refused places, no
  boat, no seat, ignored bind / discard / despawn, a dead engine, a boat aground, a hull that hits
  a wall without the server saying so, rounding the end of a wall, an interruption at every point of
  the trip, a late boat of an interrupted cast). Real-data checks: `AA_BAKED=... cargo test -p
  aa-world --test water_real -- --ignored` and `--test travel_real_data` (hull classes).

### Bigger hulls

`[sail] ship` picks the scroll family: `rowboat` (default, today's behaviour), `clipper`, `cutter`,
`junk`, `yawl`, `schooner` or `auto`. The hull's numbers come from the server data
(`aa_world::travel::ShipClass::of`, the constants `ROWBOAT`, `CLIPPER` and `CUTTER` are checked
against the data by `ship_classes` in `travel_real_data.rs`):

| Hull (scroll) | Kind | Top speed | Turn rate / radius at top speed | Box (beam x length x height) | Summon (ahead / side) | Portal | Guns / racks |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Rowboat (17906) | boat | 3.2 m/s | 4.35 deg/s, 42 m | 1 x 3 x 0.5 | 2 / 2 m | none | 0 / 0 |
| Clipper (17626; the harpoon and adventurer's clippers 27266 / 23618 spawn 25 / 15) | speedboat | 6.5 m/s | 5.85 deg/s, 64 m | 4 x 8 x 7 | 5 / 5 m | 7 s | 0 / 0 |
| Eznan cutter (5223), Lutesong junk (25739) | small sailing ship | 6.3 m/s | 2.6 deg/s, 139 m | 6.5 x 30 x 14.5 | 30 / 20 m | 7 s | 8 / 4 |
| Growling yawl (13711) | small sailing ship | 9.8 m/s | 2.6 deg/s, 216 m | 6.5 x 30 x 14.5 | 30 / 20 m | 7 s | 8 / 4 |
| Merchant schooner (23398) | small sailing ship | 6.5 m/s | 2.6 deg/s, 143 m | 6.5 x 30 x 14.5 | 30 / 20 m | 7 s | 2 / 20 |

The top speed is `velocity * MoveSpeedMul^2 / 8` (see the rowboat note above) and the sailing ships
add the wind: with the server's default (official) wind model a small or big sailing ship or a
fishing boat goes 15 percent faster while its bow is within 15 degrees of north or south, which the
bots see as a cutter at 7.2 m/s or a schooner at 7.0 on a northward leg (the table is the speed
without it). The radius is the top speed over the turn rate cap, so a cutter needs a circle of 280
m across; the helm is ignored for the portal time (7 s) after the summon.

- **Launch**: the server places the hull in `summon_candidate_1_2` (40 steps ahead of the caster,
  starting at the "ahead" offset, on the line and to both sides by the "side" offset, the hull laid
  across the caster's view with its bow to the caster's left) and refuses the first place where
  the hull box touches the terrain or the line of sight is blocked. `find_launch` /
  `launch_ok` (`aa-world::water::shore`) run the same search on the sea mask and the height grid
  (footprint sampled in 15 points, `LAUNCH_DEPTH_MEDIUM_M` 2.5 m and `LAUNCH_DEPTH_LARGE_M` 6 m of
  water under it, a 2 m margin) so a beach is only chosen when the server will have room. The planner
  tries up to six beaches (a beach without open sea within 260 m of it is skipped). The Large depth
  is a guess until the server's mass box test is known (the cutter's mass box reaches 13.5 m down,
  its model less): the live runs below saw no refusal at 6 m.
- **Berth**: the hull ends the trip at `moor_point`: the first point, going straight out to sea
  from the landing beach (or the launch, for a loop) in 8 m steps up to 260 m, with `plan_clear`
  cells of sea around it. The bot leaves the hull there, despawns it and swims the rest.
- **Route**: a wider margin to the shore (`ShipClass::route_clear`: half the hull, a third of the
  radius and half a cell, 2 to 6 cells; the wider margin falls back cell by cell, and a route more
  than 1.35 times the planned one plus 300 m, as when the wide margin closes a passage, is not
  taken while a narrower margin has a shorter one), the deep mask for the large hulls, then
  `round_corners`: every corner becomes an arc of 1.15 times the radius (smaller where the legs
  are short; kept sharp where the arc would leave the water) and a hairpin, as the far end of a loop
  trip, becomes a half circle with the way back `2 * radius` beside the way out. The route is
  resampled every 20 m so that the follower sees the curvature.
- **Follower** (`Params::for_hull`): lookahead 0.9 radius (70 m at least), waypoint reach 10 m,
  arrival 0.6 hull length (18 m at least), and a speed plan: `Guide.v_limit` is the lowest of the
  end braking (0.4 of the astern deceleration, to 0.8 m/s at the end) and the corner limits (the
  route's discrete curvature against 0.85 of the turn cap, 1.2 m/s at least for steerage).
- **Helm**: the angular response uses the hull's own acceleration and cap (`Hull`), the probe lasts
  2.5 to 7 s depending on the acceleration, the first sample is waited for `6 s + portal time`.
  The trip limit is `max(40 min, 8 min + 2.5 * route / top speed)`.
- **Aground**: the server never sets `stuck`, so the machine infers an impact from the speed (see
  Stuck) and backs off at once; a hull stopped at the beach for 8 s is the old stuck window plus 4 s
  for the big hulls.

## Population: census, economy board and director

Everything lives in `crates/playerbots/src/population/` and hangs on the hub as `hub.pop`
(`Population`); the web types are in `crates/web/src/population.rs`.

| Piece | File | What it does |
| --- | --- | --- |
| `Geo`, `alliance_of_*`, `place_at` | `region.rs` | Side of a faction or race (mother faction 148 / 149), continent of a zone (`zone_groups.target_id`: 3 west, 4 east, 5 Auroria, 1 / 2 / 6 seas). A trait, so the tests use a table instead of a `World`. |
| `Census` | `census.rs` | Real players in view of a bot: a bounded map (2048) keyed by character id, 60 s time to live. `observe` runs in the bot's once-a-second housekeeping and takes the characters in `state.units` that are not the bot and not ours (`Hub::is_bot_char`). |
| `Economy` | `economy.rs` | Items (1500 at most): per-bot price observations, price history, chat lines, the server's lots; `score` -> 0..1; `ScarcityView` for the farming goal. Chat is counted once per line (hash of speaker and text within 20 s). |
| `ServerApi`, `poll_once` | `serverapi.rs` | The optional poller of the game server's web API; key in a `Secret`, fail-soft, bounded body. |
| `world_view` | `view.rs` | The World panel from bot rows (one per slot, read without copying text) and the census: cells, zones, heat squares (512 m), the watched area (128 m cells), coverage. |
| `Selector`, `stop_rank`, `pick_stops` | `select.rs` | A checked spawn filter; how willingly a running bot is stopped; the despawn choice. |
| `Director`, `plan`, `targets` | `director.rs` | The policy, the pure planner, pacing, back-off, the action log, the policy file. |
| `GoalBias` | `mod.rs` | Per-goal multipliers (atomics); the brain applies them at the end of `scores_now`. |

**Feeding the board.** A bot does three small things in `housekeeping` / `route_inbox` / `publish`:
hands the players in view to the census (once a second, taking the census lock only when there are
any), hands its fresh `PriceNote`s to the economy (every ten seconds, staggered by bot id) and files
the trade chat it hears through the same `forward_chat` sink the market uses. `publish` fills the
snapshot's `mother_faction`, `alliance`, `continent` and `zone_key`. No new per-bot state exists:
the board deduplicates by bot id instead of keeping a watermark in the bot, so
`size_of::<Bot>()` is unchanged.

**The director's loop.** `Hub::tick` (once a second) calls `Population::tick`: rows from the slots,
the census per side, the director's own bots (a set kept by the supervisor, with a count of those
that may be stopped now: up for 3 minutes and doing something `stop_rank` allows), the process
health from `stats()`, then `Director::step`. `plan` is a pure function of the policy and those
numbers (see the rule and the safety list in
[OPERATING.md](OPERATING.md#world-metrics-and-the-population-director)); `step` adds the pacing
(gap, bots per minute), the dry-run bookkeeping, a floor for bots it just queued (they count
towards their side for 2 minutes, until they show as online) and the log. A live order goes to the
supervisor as `SupCmd::SpawnWhere` or `DespawnWhere`; the supervisor writes a `result` line into
the log when it has acted.

**Supervisor.** `Book::spawn_matching` starts only roster entries that match the selector and grows
the roster (`ensure_roster_races`) with the races the selector allows among `bots.races`; the
`MAX_BOTS` limit counts the whole roster. `despawn_where` takes matching bots out of the start queue
first, then asks `pick_stops` for the running ones. Ownership (`Population::own`, `disown`) is kept
by the supervisor, so a bot the operator started is never the director's.

**Deviations and choices.** The census is a lower bound by nature (about 128 m around each bot); the
server API gives the exact count when a key is set. A policy file never restores the kill switch or
`dry_run`: a restart is as safe as the configuration. Wishes only record the want for the farming
goal and nudge its weight; the goal itself is another feature. The economy treats a bot's chat
line as a quarter of a player's so that the bots' stand-in advertisements do not drive the
scarcity they then react to.

**Tests.** The census, economy, director, selector, view and server-API parser have unit tests next to
the code; the web crate's `tests/api.rs` drives the new endpoints against a mocked hub; the
supervisor tests cover filtered spawn, despawn and roster growth. `tests/population_real_data.rs`
(ignored, needs `AA_COMPACT` and `AA_SERVER_DATA` like the other real-data tests) checks the sides
and continents of real factions and zones, the census through a real hub, the filtered commands and
the policy.

## Data directory

`playerbots bake` writes (see `docs/DATA-FORMATS.md`):

```
data/
  heights_main_world.bin   # u16 grid, header + samples, mmap'd at runtime
  map_main_world.png       # painted world map (or relief fallback)
  map_main_world.json      # calibration: world rect covered by the image
  housing_areas.json       # polygons per housing area id
  farm_areas.json          # polygons of the public farm sub zones
  quest_spheres.json       # quest sign spheres (quest id, component id, pos, radius)
  zones_main_world.bin     # zone key per 64 m sector (optional)
  nav_main_world.bin       # navigation mesh for path finding (mmap'd), one more per instance world
  nav_<instance>.bin
```

Runtime also reads the server's `compact.sqlite3` (read-only) and `app/Data/Worlds/*/` spawn
files, paths given in `playerbots.toml`.

## Travel data (`aa-world::travel`)

Static data for the portal, instance-dungeon and boat abilities, loaded once with the world
(about 180 KiB of heap) and read through `World::travel()` or the `World` shortcuts
(`recall_points`, `nearest_recall`, `portal_reagents`, `dungeons`, `dungeon_by_entrance_doodad`,
`instance_world`, `instance_world_by_zone_key`, `boat_scrolls`, `slave_template`,
`boat_for_scroll`).

- **Portals** (`portal.rs`): `Data/Portal/{recalls,worldgates,respawns}.json`, the open-portal
  reagent tables (1 stone on the same continent, 3 across), the Teleport Book / record / recall /
  Memory Tome skills with their timings, the gate NPCs 3891 / 6949 and the Memory Tome doodad
  spawns of the main world with the recall point each binds per faction.
- **Dungeons** (`dungeon.rs`, `instance.rs`, `funcs.rs`): the `DoodadFuncEnterInstance` entrance
  doodads that spawn in the main world, the `indun_zones` rules (level, party, item, `closed`),
  the entry limit (3 per zone group per 4 h) and every `Worlds/<name>/` instance world: zone keys,
  spawn points, NPC / usable doodad spawns and the exit doodads. The zone key an `SCLoadInstance`
  carries is matched to a world by `zones.name` (the client's `world.xml` is not server data).
- **Boats** (`boat.rs`): items whose use skill has the `SpawnSlave` effect and summon a boat-kind
  slave, with seats (`model_bindings`), `ship_models.velocity` and the slave's `MoveSpeedMul`
  (top speed = velocity x MoveSpeedMul^2 / 8, `SlaveTemplate::top_speed`), the model's box,
  turn data, spawn offsets, portal time, cannon and rack attach points and an estimated hit point
  pool. `ShipClass::of(&template)` condenses a template into what driving it needs (top speed,
  turn rate and radius, hull box, draft, sizes `Small` / `Medium` / `Large`, route margins, the
  launch request); `ShipFamily` groups the scrolls of one hull (`Boats::scroll_for`).

Fixed ids (Teleport Book 4045, skills 11215 / 11216 / 10594 / 14602 / 16841 / 15802, ...) are
checked against `compact.sqlite3` at load and only warn when missing. Real-data checks:
`AA_COMPACT=... AA_SERVER_DATA=... cargo test -p aa-world --test travel_real_data -- --ignored`.
