# Server issue reports

Part of the operator documentation ([OPERATING.md](OPERATING.md)); the [README](../README.md#server-qa) has the overview.

While they play, the bots notice problems with the **server** (not with themselves): a quest that
gives no credit, an NPC or doodad that is not where the data says, a kick or an unexpected
disconnect, a character that dies without the server saying so, a malformed packet, a trade or a
sale that does not pay what it should, a warning in the server's log that names the bot. The
detectors file a report; the hub deduplicates them by *what went wrong* (kind and a stable key, for
example `quest=251 npc=3475`), so the same problem met by fifty bots is one **issue** with a count,
the bots that met it, the first and last time and up to three samples (the first and the latest
two, each with the bot's position, goal, packet trail, server log lines and own log).

What the bots look for (the code is `crates/playerbots/src/bot/watch/`; the judgement of each is a
pure function with tests in `watch/classify.rs`):

* **Requests that go unanswered:** a quest accept or turn-in, a loot window, a skill cast or a GM
  `/move` that gets no answer in 6 s (a cast is answered by `SCSkillStarted`, which the server sends
  only for casts with a casting time and for refusals; an accepted instant skill is answered by
  `SCSkillFired` alone and a plot skill by its `SCPlotEvent`s, which the bot reads from the raw packet;
  the default attacks, skill ids below 5000, repeat by themselves and the server accepts a repeat in
  silence, so they are not tracked). Dropped when the bot died, was teleported or walked away, or
  when the whole connection was quiet (that is a stall, not this).
* **The connection:** a packet the parser rejects (key = opcode, with the first bytes of the packet),
  a kick or an unexpected disconnect (key = phase and reason; the bot's own shutdown and the
  login flow's refusals are not issues), a stall (no packet for 8 s or a ping that took more than
  5 s, key = phase; blamed on the bot when its own loop was starved). One report per outage.
* **Quests:** a kill or a cast the server confirmed whose counter never moved (key = quest and
  component; a kill of a monster another character tagged is not one: the server credits the tag,
  and the bots read the first hit from `SCUnitDamaged`); a quest start the bot's own gates let
  through that the server refuses (key = quest and the server's code; `SCQuestUnitReqFailed` keeps
  its unit-requirement kind and value); a quest giver, turn-in NPC or quest doodad that is not at
  any spawn point the world data has for it; an NPC or doodad in view that is on another floor
  (a house storey, a cave under the terrain) with no walkable way within the server's 15 m
  (key = template and role).
* **Combat:** a monster that takes no damage from eight confirmed casts (or says it is immune); a
  skill refused for a reason the bot's own checks rule out (range, mana, cooldown, line of sight and
  moving are the bot's fault and stay local).
* **The bot's life:** hit points 0 without a death packet; a character that logs in dead and is not
  revived; a bot that died and is not resurrected after asking.
* **Money and goods:** a finished trade whose purse or bag moved by something other than the two
  offers; a sold trade pack whose payment mail is empty or cannot be read.
* **World events (rifts)**, with `[events] enabled` (`crates/playerbots/src/events/detect.rs`): a
  wave none of whose monsters a bot at the target saw (`event-wave-no-npcs def D wave N`; judged 60 s
  plus the controllers' walk after the wave began, and only after a bot stood at the target for 30 s);
  a wave that stayed after every kill it waits for was seen (`event-wave-stuck def D wave N`); an event
  still running 60 s past its time cap (`event-not-ended def D`). Each is told once and is filed for
  each of up to three bots that were in the event, so it counts as seen by several bots and gets the
  bots' trails. Suspect: server, area `systems/instances/tower_def.rs`.

Every report carries the **packet trail**: the last 48 packets sent and received, with their names
and ages (`trail.txt` in the evidence folder).

Every issue has a **suspect** (`server`, `data`, `bot`, `unknown`) and a **severity** (`low`,
`medium`, `high`), set by the detector. An issue is *reportable* when its suspect is not `bot` and

* it is a death without a packet, a malformed packet, a kick or an `ERROR` line in the server log
  about a bot, with high severity, seen **once**; or
* it was seen `min_occurrences` times (default 3), or by `min_bots` different bots (default 2).

Issues the bots suspect themselves for stay in the book and the web API, and never go to the
team's queue by themselves. All issues are listed by `GET /api/issues` and `GET /api/issues/{fp}`
(`pb:` and eight hex digits); `POST /api/issues/{fp}/action` with `{"action":"queue"}`,
`"ignore"`, `"resolve"` or `"reopen"` is the API behind the issue panel of the web UI. The book is kept in
`<dir>/issues.json` (default: `issues` next to `bots.roster`), so counts, states and the rate
limits below survive a restart. At most 500 issues are kept (the ones seen least, longest ago and
not queued go first).

For a reportable issue the bots write an **evidence folder**,
`<evidence_dir>/<YYYYMMDD-HHMMSS>-bot-<slug>/`, with `report.json` (the issue and its samples),
`trail.txt` (the packets before each sample), `server-log.txt` and `bot-log.txt`; it is rewritten
(a few times at most) when later samples arrive.

## The team's issue queue

If the team keeps its server issues in a text file that agents work from, point `queue_file` at it
and the bots **append** one entry per reportable issue:

```toml
[issues]
queue_file = '<path>/issues.txt'                  # the team's issue file (created if missing)
time_label = "EST"                                # the label after the time in the header
server_log = '<path>/game-server.log'             # the game server's log, when it runs on this machine
```

An entry is a header line `[YYYY-MM-DD HH:MM:SS <label>] <title> (playerbots)` (local time) and
a few lines of prose (what was observed and expected, how often, which bot was doing what where,
the likely server area as a hint, the server log line at the time, a reminder that the report
comes from an automated client and may be a bot-side misunderstanding, the evidence folder and a
marker `pb:<hex>`). Entries are separated by one blank line and the file uses CRLF line breaks. The
file is only ever opened in append mode and written with one `write` per entry: it is never
rewritten or truncated, so other people and agents can edit it (download, edit, upload) in
between. Agents delete an entry when its fix is done; if the issue happens again 30 minutes or
more after the entry was written, the bots read the file (it is small) and, when the marker is gone,
append a "Recurrence after the entry was removed" entry; while the marker is still there they do
nothing.

`queue_mode` is `auto` (a background task appends reportable issues), `manual` (only the web/API
`queue` action does) or `off`. Rate limits keep the file readable: at most `max_queue_per_hour`
(6) entries an hour and at most `max_queue_per_kind_per_day` (2) *new* entries of one kind of issue
in 24 hours; the others wait and stay visible in the web UI.

## The server log

With `server_log` set (the bots must run on the machine that writes the log, or see it through a
share) the log file is followed once a second: only new lines are read, truncation and rotation are
noticed, colour codes are removed, and the newest 4000 lines (1 MiB) are kept. A report gets the
`WARN` and `ERROR` lines within five seconds of it, plus the lines that name the bot's character,
and a new `WARN` or `ERROR` line that names one of our characters is handed to that bot, which
files a "server log during action" report with its own context (at most 20 lines a second go to
the bots). Lines that are ordinary events the server logs at `WARN` (a quest leaving the journal, a
cast refused for line of sight) are dropped by the bot, see `routine_server_line` in
`bot/watch/classify.rs`. Lines look like `HH:MM:SS [LEVEL] target - message`; other prefixes are tolerated.

## Privacy

A report holds no account, password, token or address of the bot: the bot password is never read
by this code, and nothing in a report carries it. As a second line of defence every text that is
stored, written to an evidence folder, appended to the queue file or served by the web API has
IPv4 addresses other than `127.x` replaced by `x.x.x.x` and the value of `password=`, `token:`,
`secret`, `cookie`, `ticket`, `authorization` and similar pairs replaced by `<redacted>`. Texts
are cut to a bounded length and flattened to one line, so a log line cannot forge an entry. The
evidence folders and `issues.json` contain character names, positions and server log lines: keep
them as private as the roster.

## For detector authors

`crates/playerbots/src/issues/mod.rs` has the tour. In short: build an `IssueReport` with
`IssueReport::new(kind, key, title).expected(..).actual(..).suspect(..).severity(..).area(..).fact(..)`
and file it with `ctx.report_issue(report)` in a goal or `self.file_issue(report)` in the bot
loop; the bot's identity, position, zone, goal and log are attached for you, and so is the packet
trail (`bot.state.watch.trail`, a `TrailRing`: `push(now_ms, dir, level, opcode)` per packet, names
are resolved only by `snapshot(now_ms)`), unless the report brought its own. `.packet_head(&bytes)`
keeps the first 96 bytes of the triggering packet. Report an episode once, not on every tick, and
pick the suspect honestly.

A new detector for a goal or the combat controller leaves a `watch::Finding` (a `Copy` note, ids and
counters only) with `ctx.find(..)`; `Finding::report` makes the report (key, title, suspect,
severity) and `Watch::push` keeps one finding per episode and session. Put the decision (when is it
a finding, whose fault is it) into a pure function in `watch/classify.rs` and test it there. A
request the server must answer is registered with `ctx.expect(..)` (or automatically from the
outgoing packet, see `watch/tracker.rs`).
