# Changelog

All notable user-facing changes to darkmux are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

darkmux follows semver, stable since **1.0.0**; breaking changes are called out
explicitly in each entry (pre-1.0, the no-compat-baggage policy shipped breaks
without deprecation shims). Roadmap **milestones** (`M1`/`M2`/`M3`…) are
intentionally decoupled from these version numbers, and the `RULES_SCHEMA` /
`FLOW_SCHEMA` / `LEDGER_SCHEMA` data-shape contracts version on their own
cadence (see `CLAUDE.md`) — a major bump in one of those is a breaking change
to that payload, called out in the entry, and does not by itself force a major
darkmux release.

## [Unreleased]

### Added

- **A relayed run is one run** (#3016). Work asked on one machine and executed
  on another (radio's answering seat on a peer, `dispatch --profile p@peer`, a
  fleet job) is one row on the runs board, owned by the machine that ran it: it
  counts there and the Machine filter matches it (the asking side writes only a
  `dispatch.route` record, so it never makes a second row). `GET /runs` rows gain
  an additive `relay: { asked_on_machine, sender_run? }`, `sender_run` set only
  when the asker was a mission; the row subtitle ends "from
  <asker>" and the run page header reads "on <executor>, from <asker>".
- **Cross-machine order is the hub's receive order** (#3017). Records read off
  the hub carry the Redis stream id as an additive `hub_id` (documented on the
  flow record wire type); the mission graph
  orders a step's attempts and folds its span by it, so a peer whose clock runs
  slow no longer loses its live retry to a failed attempt or stretches a
  "finished" span across two clocks. A reporting peer's quiet session is judged
  against that peer's own clock (its presence beat), not this daemon's.
- **Clock skew is said in `machine list` and `doctor`, nowhere else** (#3017).
  `clock 10m behind the hub` appears when a machine's presence beat is more
  than 30 seconds off the hub's clock (twice the beat TTL, so a healthy clock
  never reads skewed); `FleetMachine` gains an additive `clock_skew_ms`.
- **Wire contracts are additive-only from 5.0 (D2).** `route-table.golden` now lists
  every JSON response type's fields and their types (read from the generated
  TypeScript twins), and `scripts/contract-additive-guard.py` fails a PR that removes
  or changes a line of it, of `tests/cli-json.golden` or of `ui/src/types/generated/*.ts`
  unless the PR carries the `breaking-v6` label. It starts at the first `v5.*` tag.
- **A mod names its change, its proposer and its site** (MOD schema `2` to
  `2.1`, additive; `mod list --json` and `mod show --json` gain three optional
  fields, `tests/cli-json.golden` regenerated). `change_key` is `chg-<blake3>`
  over the sorted `for` keys, the kit bytes (with a kind derived from them,
  never the `--kit-kind` label) and the attachment hashes, taken from the
  bytes as they are stored, so byte-identical proposals share it and any
  different diff does not;
  `proposer` is `{role, profile, model}` when a darkmux role proposed the mod
  (`by` is unchanged); `site` is `{source, sha, file, start_line, end_line}`
  when the finding it answers sat in a planned crawl or review site. `mod show`
  prints each when present. A schema-2 record still reads, with all three
  absent.
- **`step.error` says why** (FLOW 2.0.0, additive). A step's error
  record carries `{cause}`: the message on one line, control and invisible
  characters dropped, URL userinfo and token-looking query values redacted,
  bounded to 400 columns. `mission show` prints the cause
  under an errored step, and `mission show --json` carries it as the step row's
  `error` (additive, absent for a step that did not error;
  `tests/cli-json.golden` regenerated for the new field).

### Fixed (5.0): isolation
- **A remote read is redacted as it streams, with no size cap, and the serve token compares as digests** (#3073).
  The redaction layer reloaded `fleet.json` and canonicalized the home directories on
  every remote request; it now reuses the last derivation while `fleet.json`'s mtime and
  length, `HOME` and `DARKMUX_HOME` are unchanged (10 s at most). It also parsed the whole
  response into one JSON value (about 5 to 7 times the body) and withheld anything over a
  cap; it now rewrites each string and key as it passes and sends the result in 64 KiB
  slices, so memory is bounded by the largest value and a busy day is never answered 500.
  A body that is not JSON, or stops being JSON partway, is redacted line by line as text.
  Measured on a 64 MB `/flow/:date`-shaped body: peak memory +222 MB before, +1.6 MB now.
  A wrong-length bearer token no longer returns early.

- **`darkmux dispatch --workspace-read-only` mounts the workspace read-only** (#3074).
  The crew-of-one hop dropped the flag, so the agent's tools could write into the tree
  the operator asked to protect. A `dispatch.internal` step reads it from the config key
  `workspace_read_only`, and the hop that copies a dispatch into a step names every
  field of the dispatch, so a new one cannot be dropped silently.
- **`review`'s mod wait takes its values as environment variables** (#3074). The finding
  key and `mod_wait_seconds` were spliced into a shell command, so a quote in the value
  ran as shell. `procedural.shell` gains an `env` map; a mission input can declare
  `whole_number`, and the launcher refuses anything but digits for `mod_wait_seconds`.
  No shipped command splices a placeholder any more.
- **A symlinked `tree` or `mirror` directory under a workspace root is refused** (#3074),
  instead of being deleted into and checked out under.
- **The agent's `bash` tool caps each stream at 256 KiB** (#3073). The rest is read and
  dropped, and the result says how many bytes of stdout or stderr were not shown.
- **A Redis password holding `#`, `/` or `?` is masked** (#3074) in `flow status`, doctor
  and the flow-status panel, and `SinkInfo`'s `Debug` no longer prints the raw URL. A URL
  with an `@` after the host now masks up to the last `@`.

### Removed (breaking, 5.0): readers of pre-5.0 shapes

5.0 is the compatibility break. Code whose only job was to read or report a
pre-5.0 shape is deleted. The retired config-key and environment-variable
refusals stay (`RETIRED_SETTINGS`). The renames a 3.x home has to apply by
hand are in the one-time upgrade skill (`docs/upgrade/darkmux-upgrade/SKILL.md`).

- **Retired verbs and flags get clap's own error.** The table that refused
  them by name is gone: `swap`, `status`, `fleet`, `lessons`, `model`,
  `crew sync`, `mission run`, `mission dispatch|add-phase|start|pause|resume`,
  `lab eval`, `lab run list|inspect|stats|compare`, `finding list --dispatch`
  and the retired `--session-id`, `--session`, `--runs`, `--phase-id` and
  `--missions` flags now fail with "unrecognized subcommand" or "unexpected
  argument" instead of a line naming the replacement (and `lab run list` now
  launches a workload called `list`). The replacements are listed in the
  upgrade skill.
- **A mission state file in an old spelling is no longer refused or reported.**
  A `mission.json` with `sprint_ids` loads as a mission with no phases. A task
  file with `sprint_id` does not load, because `phase_id` is required, and the
  load of that phase's whole task list fails with it. A `sprints/` directory
  is not read, and a status of `closed` or `paused` reads as an unknown status (the
  `paused` alias is gone). `darkmux doctor` no longer has the `mission state
  files` row. A state file from a newer darkmux is still refused.
- **`darkmux doctor` stops reporting the old layouts.** The rows `beat-33
  crew/ layout`, `mission state files`, `lab runs location` and `retired roles
  (mission-compiler, scribe)` are gone, and the skills row no longer warns on
  an installed skill the binary stopped shipping. State under `<root>/crew/`
  stays invisible and is not named.
- **Lab runs left in `<root>/runs` are not read and not named.** 3.13 wrote
  lab runs to `<root>/runs`, so every 3.x upgrader has them there. Every lab
  verb, `darkmux run list --kind lab` and `darkmux serve` used to refuse or
  warn with the `mv` that moves them; now `run list --kind lab` shows none of
  them, with no warning. Move the directory yourself (step 5 of the upgrade
  skill has the exact commands). `GET /lab/runs` loses its `pending_move` field and the `serve` banner
  its move line.
- **`darkmux init` no longer prunes retired skills.** A leftover `darkmux-*`
  skill directory the binary stopped shipping stays installed until you delete
  it, and `init --dry-run` no longer prints a "would prune" line.
- **`profiles.json` retired keys read as unknown keys.** `crews`, `hooks`, a
  model's `role`, `runtime.config_path`, `runtime.contextTokens` and the
  openclaw `runtime.compaction.*` keys are still refused, as unknown keys with
  the closest valid key suggested, rather than with a line naming what
  replaced them.
- **A role manifest with `"role_family": "admin"`** gets the generic "not a
  recognized family" error instead of a message naming `"utility"`.
- **An archived step record naming a retired kind or output label is not
  mapped.** A mission run before #2430 shows its `crawl.unit` and
  `crawl.summary` steps under those names, and an archived `crawl.unit-outcome`
  output is refused as the wrong producer. A mission config that names a retired
  step kind is still refused, naming the new id.
- **3.x session ids attribute to nothing.** The free-form strings
  (`task-<id>`, `step-<id>[-<mission>]`, `mission-run-<mission>-<phase>`) in an
  old flow archive are not sessions: `flow tail` shows `-` for their run,
  `/flow-mission` does not name their step, the mission graph does not fold
  their tokens into a step, and `memory correction list` and the coder brief
  do not read their corrections.
- **Old lab artifacts read less.** A run recorded before its trajectory was
  copied into the run directory (#364) shows zero turns instead of reading the
  sandbox it names, a pre-2.0 openclaw reply envelope is no longer unwrapped,
  and an openclaw trajectory line is no longer forced to an unknown event.
- **The viewer draws no host-load track from a retired record.** A
  `telemetry.process` record in a 3.x archive no longer feeds the machine
  drawer, the mission header's host readout or a run's SYSTEM pane (current
  `machine.telemetry` is unchanged), and a mission step whose kind starts with
  `review.` gets no AI styling from its name.
- **The `remote` rename aliases are gone** (#3064). An archived envelope's
  `remote_budgets` key, a `remote` seat flag on an archived staffing snapshot
  or `step result` record, and a peer card's `seats.hosted` key are no longer
  read as their `dispatch_budgets` / `unmanaged` successors: the budget rows
  and the unmanaged flag read as empty or false (every review envelope already
  on disk spells `remote_budgets`, so none can read as a partial run on a
  judge-stage skip any more, and that dead branch of `review_outcome` is
  deleted), and a card still carrying
  `hosted` shows its peer as unavailable. The 1.0 and 1.1 card fixtures are
  deleted with the alias.
- **A pre-2.6 audit file is no longer verified.** `darkmux flow integrity-check`
  reports a file whose header has no `hash_format` marker (every file written
  before 2.6.0, or one naming another format) as a break at line 1 and exits 2;
  `darkmux doctor` fails the `audit integrity` row for it, where both used to
  report it as a legacy file that was readable but not verified. `--strict` and
  exit 3 are gone, and the `legacy_format` and `note` fields leave
  `integrity-check --json`. The doctor warning for a torn audit tail is kept.
  Archive the file so a fresh chain starts.
- **Internal migrations for old stores are gone.** The SQLite crew index no
  longer drops its pre-rename `capability*` and `knowledge` tables when it opens
  (it is rebuilt from the manifests on every change, so nothing is
  observable), and a residency lease with no start stamp is described as one
  whose writer could not read its start time, not as an older binary's.

### Changed (5.0)

- **Flow hub and hook outbox fixes** (5.0, #3073, #3074). A record whose session
  is a lab run no longer reaches the fleet hub, live or in an outage backfill
  (the lab/fleet sink boundary now has a conformance test). The Redis sink keeps
  one bounded connection instead of opening a connection and a thread per record
  (300 records through the sink: 77 ms before, 11 ms after, on loopback), and
  reconnects after any error; the first record after a hub restart is retried on a
  fresh connection instead of lost. A writer that finds the connection busy writes
  on a connection of its own instead of queuing, so a silent hub costs each writer
  at most one timeout, in parallel, not each in turn (8 writers: waits grew 1 to
  8 s before, about 1 s after), and the backfill runs on its own connection. The outage backfill re-sends every day file from
  the outage start through today, so a multi-day outage is covered. Hook outbox
  compaction can no longer delete an undelivered line when the cursor reset
  fails or the process dies after the repack (a marker file makes the pending
  reset recoverable), and a non-UTF-8 outbox line is quarantined with
  `hook.failed` instead of wedging its rule. `flow integrity-check` gains
  `chain_restarted` and a warning line when a day file's first line was set aside
  and the chain reseeded; `doctor` names it.

- **Read routes never show host facts to a stranger** (5.0, #3072). A caller
  that is neither this machine nor a token holder no longer sees an address, a
  tailnet name or a home directory in any JSON read (`/fleet/view`,
  `/fleet/roster`, `/machine/*`, `/lab/runs`, `/flow/*`, `/runs`, ...) or in the
  live stream. One module owns the rule, shared with the console panels, and
  applies it in one pass to every key and value (a non-JSON reply on a JSON
  route is redacted as text): roster addresses and their hosts, the fleet hub's
  host, private and tailnet IPv4 literals and any IPv4 literal in a URL or
  `host:port` position (a version like `0.3.30.1` in prose stays), tailnet,
  unique-local, global and link-local IPv6 literals (`::1` stays), any `.ts.net`
  name, and home directories (`/Users/<name>`, `/home/<name>`,
  `/var/home/<name>`, `/root`) read "(address hidden)" or `~`. A roster host
  with no dot (`studio`) is hidden only where it addresses (`://studio`,
  `studio:8765`), so "LM Studio" survives. `specs.redis_url_redacted` is gone
  from `GET /machine/specs` and the machine card (it put the hub's tailnet
  address on every machine's card); `specs.hub_configured` says whether a hub
  is configured, never where. The card keeps schema 1.2 (unreleased).
- **Machine telemetry has its own hub stream, so work records keep their window** (#2101).
  `machine.telemetry` was 87% of the hub's records, so the one capped stream held
  about 41 hours and a relayed run's usage was trimmed away before its sender could
  be asked (12 of 30 relayed runs on one day). Samples now go to
  `<redis.stream>:telemetry` under their own cap, `redis.telemetry_maxlen`
  (env `DARKMUX_REDIS_TELEMETRY_MAXLEN`, default 10000, `0` unbounded; `init`
  writes it, `config set` validates it, `doctor` and `flow status` show it).
  `GET /flow/<date>`, a dispatch replay and the live tail read both streams
  and merge them in hub order (the listings read the work stream only, below); the outage backfill never re-sends a sample (the
  next one supersedes it). `flow status --json` gains `telemetry_stream`,
  `telemetry_max_len`, `telemetry_xlen`, `telemetry_oldest_ts` and `telemetry_newest_ts`
  (`tests/cli-json.golden` regenerated); the schema-skew sample reads both streams.
  `/runs`, `/flow-missions` and a mission replay read the work stream only (a full
  read of both doubled the parse cost, 300 ms to 610 ms for 10k entries each); a
  dispatch replay reads both for its host charts. A live tail on one stream only is closed
  so the client reconnects.
  **Trade:** the outage backfill re-sends work records only, so a relayed run's
  host chart has a hole over the outage window. `redis.telemetry_maxlen` is part of
  config schema 2.3; an older binary refuses it as an unknown key.
  **Upgraders:** the old samples stay on the work stream until they age out; no
  action needed.
- **Hub outage watermark hardening** (#3062 follow-ups). The watermark's generation
  never restarts at 1 when its file is lost (it starts at the clock's nanoseconds),
  its temp file is fsynced before the rename, a torn-tail sidecar's directory is
  fsynced so its name survives a power cut, and the sink's disable warning names
  what failed last (`write` or `backfill`) instead of counting both as writes.

- **Fleet compatibility remnants removed** (5.0). `doctor`'s roster identity
  check no longer treats a flow record without a `machine_uid` as a known name
  (a record with no uid names no machine), and the retired Redis-queue
  and mixed-version narration is gone from the fleet code and docs.
- **One rule for a retired setting's leftover, on both channels** (5.0).
  A leftover env var or `config.json` key is refused only when ignoring it is
  unsafe (a spend cap); otherwise it warns. `dirs.notebook`, `radio.router_profile`,
  `remote.step_budget_policy`, `runtime.log_level` and `machine_rollup.enabled`
  used to warn as env vars but refuse as `config.json` keys; they warn in both.
  A command with no preflight (a read such as `run list`) now prints a
  `config.json` problem as a `warning:` instead of staying silent. `RENAMED_SETTINGS`
  (always empty after 5.0) is gone; a rename is a `RETIRED_SETTINGS` entry.
- **One owner for an unmanaged endpoint's seat** (5.0). The key a call claims
  (`ModelEndpoint::seat_key`) and how many calls run at once
  (`concurrent_width`) are each derived once, shared by the scheduler and a
  fleet receiver's seat book.
- **A run status reads one word on every surface** (5.0). The run page pill, the
  runs board, its Status filter and the fleet timeline share one map: `error`
  (no more `errored` or `killed` on the page), `unparseable`, `aborted` or `no
  ending` for an abandoned run, and `not reporting` for a run recorded as running
  on a machine the fleet view holds as down with no live session beat (it used to
  read `unknown`, the same word as a status that is not one). The daemon decides
  that once, on the row (`GET /runs` rows gain an additive `not_reporting`), so
  the board, its filter, the run page, the fleet timeline and `darkmux run list`
  agree. `run list`'s STATUS column (now 13 wide, so its narrowest pane grew by 2
  columns) reads the same words from one shared fixture, and its subtitle no
  longer leads with the abandon reason. `run list` looks at the fleet view only
  when a row is running on another machine, quietly and within a bound (the fleet view from its local daemon, which on a cold
  cache may wait out one peer-card timeout, and the live beats read directly from
  Redis); if
  the view or the live beats cannot be read it marks nothing.
- **`darkmux machine list` words a machine's status as its card does** (5.0).
  Under each row: `status: idle`, `dispatch in flight`, `online` (with `not
  streaming` on the next line), `not streaming` or `offline`, then `why:` and the
  card tooltip's reason. A card nothing could read now names the typed reason
  (`not streaming: not listening`) instead of a fixed sentence.
- **Fleet cards read the same from any serving machine** (5.0). Cards are ordered
  by machine uid, no longer this machine first, and show only their own machine's
  facts. A card of unknown standing that nothing has read holds the no-reading
  line `—` instead of a confident `0 running`.
- **The panel accepts this machine's own name for `profile list --machine`**
  (5.0), as the CLI does: both resolve against the fleet view's own names
  (`FleetView::selector_names`), this machine's included.
- **The runs board orders by the hub's receive order** (5.0). `GET /runs` rows
  gain `receive_key` (the newest record's hub stream id as `ms * 1024 + seq`; a
  row with no hub record carries this machine's own receive time on the same
  scale), and the board and `darkmux run list` sort on it alone, so a peer whose
  clock runs ahead cannot sit above work the hub received later, and a mission
  sorts alike on the machine that ran it and on a peer.
- **A relayed run reads `from <machine>` everywhere** (5.0): the board subtitle,
  the run page header (it said `asked on`) and `darkmux run list`'s subtitle.
- **Console panels fail gently and keep to their audience** (5.0).
  - `doctor` gets its own 25s spawn bound (it measures 7s with unreachable
    peers) and a timeout is worded "slow", not "wedged"; a timed-out run no
    longer uses up the 30s manual-run wait (it releases only its own claim), so
    an immediate retry works.
  - A 429, a 504 or a network error keeps the last good output on screen and
    says so in the header's meta slot (`failed: slow · 00:40:57`, the full message
    in its tooltip), instead of replacing the output with the error.
  - `flow-status` joins `doctor` and `config-list` as served only to this machine
    or a token holder: it prints the flow directories, hook target URLs and the
    Redis URL. `mission status` rendered in the console links root-relative.
  - One output filter on every panel for a caller that is neither this machine
    nor a token holder: `stderr_tail` is replaced by "diagnostics are shown on
    this machine only", every roster address (and its host part) in stdout reads
    "(address hidden)", and the daemon user's home prefix reads `~` (a
    `DARKMUX_HOME` outside it reads `$DARKMUX_HOME`). Matches are whole tokens
    only (`mac` is not hidden inside `macos`; punctuation and color codes are
    boundaries; an OSC 8 link to a hidden target loses the target, not its label; only SGR and OSC 8 escapes are sent, and `HOME` and `DARKMUX_HOME` are matched in their symlink-resolved form too), and a machine id or name is never
    hidden: only the address behind it. The sets come from the roster and the
    environment at request time.
    This machine and token holders see the output unchanged.
  - A console link that carried something unusable (an unknown panel, option or
    value, a machine with `remote`, a repeated key) says what was not used in the
    header (`2 unused`, the list in its tooltip). A repeated query key on `GET /panel/:id` is a 400 naming the key.
  - `machine status` is run as `machine status -- <id>`, and a machine id may not
    start with `-` (`machine add` and the roster refuse it, and an unknown
    flag-shaped id is not suggested for adding), so no roster id can read as a
    flag.
  - `run-list` and `mission-status` cache for 8s (a cold fleet read is about 5s,
    longer than the old 3s TTL); `stderr_tail` drops `[darkmux-liveness]` lines
    and keeps error lines first; a panel's stdout is capped at 512 KiB with a
    visible note; the console reads SGR 22/23/24/39/49 and 256-color and
    truecolor sequences as whole sequences.
  - The console's panel table is generated from `panel.rs` into
    `ui/src/lenses/console/panel-table.generated.json` and the console's tests
    read it, so the two cannot drift.

- **One token total, from usage records only** (#3067). Every surface sums the
  `telemetry.tokens` usage records and nothing else: a `dispatch.complete`
  (and `remote_tokens`, the old hosted-item spelling) is never read for tokens, so
  a pre-5.0 run with no usage record reads as unmeasured. A run's tokens
  count under each record's own session and mission, so a resumed dispatch
  (same execution id, new session) no longer lends its tokens to the first
  session and leaves its own blank. The run page's tiles, the mission
  graph's step meter and `GET /runs` now show the same total as
  `run list` (utility calls included), with the utility part named: a hover on
  the run page tiles, `tokensUtility` on each graph step (additive), and a
  `no_run` line in `run list --usage` for calls that belong to no run (radio
  routing, `doctor --probe`: `doctor --probe` now records its usage). A call
  that reported only one half of its counts adds that half to every run total,
  as the usage record's reader already did. `legacy_completes` leaves
  `--usage --json`; `no_run` joins it. The `remote` flag of a map item's
  usage record and the `remote_tokens` field of a `dispatch.complete` are
  removed.
- **One half-reported rule, and the no-run tokens named everywhere** (#3067). A
  call whose provider reported `total_tokens: 0` beside non-zero prompt or
  completion counts is read as unreported, so every sum counts the halves
  (the usage record's reader and the run total used to disagree on it). `GET
  /runs` gains additive `no_run` and `unlisted` (`calls`, `tokens` each): the
  usage that names no run, and the usage that names a run with no row in the
  listing (a start record outside the window), so the rows plus both equal the
  total (a usage entry two rows could read, such as a task session shared by two peer
  missions, is counted on one row only, chosen by whose span holds the entry's first record, then receive key and id; an entry
  is never split between rows, and `--since` can move its first record and so its
  winner; the same records give the same rows whichever machine serves them, and a
  mission's `dispatch_id` no longer varies between two serves of one archive); `run list --usage` prints a line for each when non-zero (`unlisted` joins
  `--usage --json`). A reported total of 0 beside both halves is their sum and beside one half is no total (a step budget charges it as a missing one). The fleet hero's hover says
  how many tokens it includes with no run or on unlisted runs. The run page's token tiles name the
  run's total on hover when INPUT + GENERATED fall short of it (a total-only
  record), so the page and the row agree; with no split reported at all the tiles read a dash.
- **`run list --usage` keys each row on the machine that executed the call**
  (#3067). `localhost` means a different machine to whoever made the call, so
  two machines' LM Studios serving the same model merged into one row (a relayed
  radio answer from the Studio read as the laptop's). Rows are now (machine,
  endpoint, model): a MACHINE column, and `--json` groups gain additive
  `machine` and `endpoint_id` fields (a named endpoint shows its registry id, not
  its URL; the machine is keyed on its hardware uid, so a rename does not split
  it). A hosted endpoint used from two machines is two rows, the `all` row still
  sums both. Calls whose reply reported no usage are counted in CALLS at 0
  tokens, as the endpoint window budget counts them, and a line under the totals
  says how many (`unreported` on each split in `--json`). Tokens of sessionless
  utility calls (radio routing) sit in the table but on no run's TOKENS cell.
- **Informational stderr lines stay out of an interactive terminal** (5.0).
  `[darkmux-liveness]` markers, the `flow: ... sink enabled` banners, and the
  dispatch progress headers print only when stderr is not a terminal (CI logs,
  pipes, the daemon log, ACP), under the new global `--verbose` / `-v` flag
  (`doctor -v` is the same flag), with `DARKMUX_VERBOSE=1`, or with
  `runtime.verbose: true`. Warnings and errors always print, and the liveness
  heartbeat file is written either way.
- **A retired `config.json` key at its old default warns; a value you set is
  still refused** (#3057). The retired `remote` block, `machine_rollup` block,
  `runtime.log_level`, `runtime.daemon_auth_enabled`, `runtime.telemetry_record_every_samples`,
  `orchestrator`, `review`, `gh` and `dirs.ack` are judged by their VALUE.
  A leftover holding what `darkmux init` wrote (`log_level: "info"`, the `remote`
  block with `max_tokens_per_step: null` and `step_budget_policy: "warn"` and any
  `concurrent_cap`, `machine_rollup.enabled: false`) prints one
  `warning: <key> in config.json is ignored: removed in 5.0 ...; delete it` per
  command and shows as a warning in `darkmux doctor`; every command still
  starts, `serve` included. A value ignoring which would change something is
  refused at preflight until you move it: a spend cap that was set
  (`remote.max_tokens_per_step` or `max_tokens_per_execution` other than the old
  500000 default), `machine_rollup.enabled: true`, `runtime.daemon_auth_enabled: true`.
  `config set` on a retired key still refuses. **Upgraders:** a leftover at its old
  default is safe to delete; a value you set is refused until you move it.
- **Loose JSON fields are typed (#3035).** Typing, not a wire change: every value
  already written still reads, and a typed field writes back the JSON it was read from. A flow payload's `context` is a `RecordContext` (`workspace`, `source`,
  `sha`, `rule`, `rules`, `confirm`, `unit`, `model`, `locality`, `profile`, and on a
  finding `site`; a key it does not name is kept), so the generated TypeScript
  changes from `Record<string, unknown>` to `RecordContext`, and `FindingRecord.context`
  and `ForFinding.context` change from `any` to it. A knob's `value` is a `KnobValue`
  (`boolean | number | string`, or `null`) instead of `any`. The lab run
  `manifest.json` is one `RunManifest` type that the three providers, the fixture
  and work-gate enrichers and every reader share. The free-form fields that stay
  `any` (a finding's `emitted`, a mission input's `default`, `Profile.use_when`)
  are listed in `DESIGN.md` with a reason each. Each typed field reads leniently on its
  own: a wrong-typed key is dropped (`None`) and the rest of the payload, manifest or
  finding still reads.
- **Additive-only schema changes are enforced, not hoped for** (#3035). These
  authored and persisted shapes carry a `schema_version` (role, skill, crew,
  rule, workload and lab-fixture manifests; `mission.json` and each phase, task
  and step file; `graph-report.json`; `lab-registry.json`; `resume_origin`; the
  lab run `manifest.json` as `manifest_schema_version`; the trajectory as a
  first-line `trajectory.header` event), written on every save and read
  leniently (absent means written before the marker). `config.json`,
  `profiles.json`, mission configs, the workspace spec, `envelope.json` and a
  mission's config snapshot already carried their own; `fleet.json` keeps its
  advisory `version`, which nothing enforces. A file whose marker is newer than the binary's is refused with
  `this file was written by a newer darkmux (...). Upgrade darkmux.` and its keys
  are not reported as typos; at the same or an older version an unknown key is
  still a typo. `lessons.db` refuses a newer `user_version` instead of
  re-stamping it down (`memory lesson list`, `export` and `recall` refuse it
  rather than read it as empty), and runs ordered migrations on an older one.
  `run stats`, `lab loop` and `lab inspect` refuse a trajectory whose header is
  newer.
- **A status or enum value from a newer darkmux reads as `unknown`, not as an
  error.** `MissionStatus`, `PhaseStatus` and `NodeStatus` gain `unknown`
  (`mission status` lists such missions in their own section, the runs board
  reads them `unparseable`, no verb moves them, the scheduler never runs or
  counts them), and so do the trajectory, model-ledger, lab and flow enums a
  peer or a recorded run can send. `mission debrief --json`'s phase status
  gains `unknown` (`tests/cli-json.golden` regenerated).
- **`fleet.json` keeps top-level fields it does not know** across `machine add`,
  a pin and an identity update, as a machine entry already did.
- **CI: production code reads flow records through `darkmux_flow::reader`**
  (`scripts/flow-reader-guard.py`).
- **Radio routes a phrase that names a mission** (F12). "Launch the review mission
  on my branch" was refused although `review` was in the catalog: the router
  prompt now says that naming a listed command, with extra words around it, asks
  for that command, and `coder-phase` leads with a plain sentence ("fixing a
  failing test") so a fix-the-test request finds it. Tests pin that no retired
  verb, `lab eval` included, appears in the router catalog or the verb index.
- **`darkmux doctor`'s one-line help says what it checks** (config, profiles,
  LM Studio and endpoints, runtime image, fleet, flow sinks, state files), so
  radio describes it correctly.
- **A review whose records were partly unreadable says so** (#2425). The delivered
  comment's scope line now names what `records.gather` could not read (a failed
  phase or step listing, a unit whose output did not parse) under `Unreadable:`,
  and a run with nothing else to say but an unreadable input is `degraded`, not a
  clean no-op.
- **`machine list` words each machine's status and utility model, and the console
  reaches it** (5.0). Under each row it prints the status in the fleet card's
  words (idle, running, online · not streaming, not streaming, offline) with the
  reason when the word has one ("offline: not listening"), and the card's utility
  model with whether it is resident (`job: not shown here`, since a utility job is
  a flow record). `machine status` prints the same utility line, for this machine
  or a roster peer. The status comes from one Rust derivation (`src/card_status.rs`)
  pinned to the card's own (`cards.ts`) by a shared fixture both test suites read.
  The console gains a `machine list` panel (a tenth, so a phone has no tooltip to
  miss), and its `machine status` panel takes a roster machine (`opt.machine`),
  the same roster-validated opt `profile list` has.
- **Residency judges the copy the seat addresses** (#3074). A seat dispatches
  to its own placement's identifier, so only that exact instance can be reused;
  another `darkmux:` copy of the same model is replaced, never reused at
  whatever context it holds. A lease on an explicit alias that a placement also
  names now blocks an unload mid-generation, as a `darkmux:` lease already did.
- **A sibling seat's copy is never unloaded to make room for another seat** (#3076).
  When one wave holds two seats on the same model, the copy a seat addresses stays
  whichever seat is planned first; the other seat loads beside it, or is refused
  naming the budget. A replaced `darkmux:` copy now carries its own reason instead
  of an "insufficient context" one, and `mission config show` words it the same way.
- **A failed `lms ps` no longer paints the model ledger green** (#3074). The
  machine verdict reads unknown, attribution reads unavailable, and the worker
  footprint stays in the machine current.
- **An all-error phase never reads complete** (#3074). The mission graph shows a
  phase whose every task errored (or was abandoned) as error (or abandoned) even
  when the phase was persisted complete.

### Changed (breaking, 4.0)

- **`GET /flow/:date` answers the `FlowRecordsResponse` envelope.** It returned a
  bare `FlowRecord[]`; it now returns `{records, count, truncated, generated_at_ms, meta}`,
  the shape `/flow-mission/:id` and `/flow-dispatch/:id` already had (`truncated` is
  true when the day read hit its record cap; `meta.sources.fleet` is `ok`, `off` or
  `unavailable` for the Redis half; `meta.cut` says per source, `local` and `fleet`, whether its read was cut short, and `truncated` is true when either was). The SSE stream `/flow/:date/stream` still sends
  one record per event. **Migration:** read `.records` where you read the array
  (`curl .../flow/$(date +%F) | jq .records`).
- **A lab run's deep link is `#lens=runs&lab=<dir>`.** It was `run=<dir>`, which named
  the umbrella, not the kind it opens (CLAUDE.md contract 8). There is no alias: an old
  `run=` link lands on the runs board. **Migration:** rewrite bookmarks to `lab=`.

- **The shared step kinds `crawl.unit` and `crawl.summary` are now `dispatch.unit` and
  `dispatch.summary`** (#2430). Both the `crawl` and `review` configs use them, and the new
  names describe the procedure: one bounded dispatch per planned site, and a fold over unit
  outcomes. `plan.sites` and `crawl.plan` keep their names. There is no alias: a mission
  config naming an old id is refused when it loads, with a message naming the new id. The
  output labels moved with them (`crawl.unit-outcome` is `dispatch.unit`,
  `crawl.summary` is `dispatch.summary`, and a plan is `plan.sites`). Missions and step files
  already on disk still read: old ids map to the new ones on read, and nothing writes an old
  id back. The viewer's mission graph now treats `dispatch.unit` as a model-dispatching step
  (it shows the token and turn meter) and `dispatch.summary` as a procedural one.
- **Upgrading a config that names `crawl.unit` or `crawl.summary`** (#2430). One such file in
  your `mission-configs/` directory blocks every `mission launch`, the built-in configs
  included, because the registry-wide preflight refuses it. The refusal and `darkmux doctor`
  name the file and the fix: rename `crawl.unit` to `dispatch.unit` and `crawl.summary` to
  `dispatch.summary`. The upgrade skill (`docs/upgrade/darkmux-upgrade/SKILL.md`, step 8) has
  a `sed` one-liner that touches only those two ids. Mission history on disk is not rewritten.
- **A mission config whose wiring cannot work is refused before anything is minted** (#2312).
  Each step kind declares the outputs it `requires` and `provides`; for every task, the first
  step's requirements must be met by the last step of a task named in `depends_on`, `reads`
  or `grow.from`. A miss is an error naming both tasks and both kinds (a `dispatch.unit`
  grown from a `procedural.shell` task, for instance, which used to mint and then fail every
  unit reading its plan). `mission launch`, `mission config show` and `darkmux doctor` all
  run the check. `procedural.shell` and `dispatch.internal` provide untyped `text`;
  `mission.verify` no longer declares a `worktree` data port it could never receive.
- **`RunStatus` gains `degraded` and `escalated`** (`/runs`, `run list`, `run list --json`).
  A run whose envelope says `degraded` (a unit cut at its bound, a step that
  completed with partial work, the wall-clock bound) now reads `degraded` instead
  of `complete`, and a lab run whose dispatch escalated on purpose reads
  `escalated` instead of `error`. `lab loop` gains an `escalated` verdict and an `escalation` field on its
  `--json` report for the same reason; `lab run`, `characterize` and `tune` still
  exit 1 on an escalation (unfinished work is no pass), but label it an escalation. A run's status is decided in one place
  (`MissionOutcomeStatus::decide`) and its exit code is derived from it.
- **The registered utility model stays loaded across dispatches.** A dispatch
  that does not name it no longer unloads it, and neither do the budget and
  pool-pressure evictions; a load that would fit only by evicting it is
  refused with a reason naming the utility model. `darkmux machine eject` is
  how to release it.
- **CLI `--json` output is a contract** (C4). Every verb's `--json` output is now
  one serialized, named type (`src/cli_json.rs`), pinned by
  `tests/cli-json.golden`, which lists each verb and the fields and types of
  everything it prints. The golden is derived from the types, so changing a
  shape fails a test until the golden is regenerated on purpose. An output
  carries no `schema_version` of its own (its version is darkmux's), except a
  document that is also written to disk, which already named its schema. From
  this release a shape change is a semver-visible change like a daemon route.
  No field spells `session`. These fields changed, one line per verb; every
  other output keeps its bytes:
  - `role list`: each role's `skills` (a count) is now `skill_count`, so `skills`
    is a list of skill ids in `role show` and absent from `role list`.
  - `mission status`: the drift `kind` `running-phase-session-dead` is
    `running-phase-execution-dead`; `budget_waits[].session_id` is
    `execution_id`.
  - `mission config show`: a role's `provenance` is now `overridden`, `mapped` or
    `default_fallback` (it was the phrase `launch override (--param)`,
    `role_profiles map` or `default_profile fallback`); the text view prints the
    same phrases as before.
  - `run stats` (several runs, or `--baseline`): each `errors` entry is
    `{"run": ..., "error": ...}` (it was a `[run, error]` pair).
  - `run stats`: `bounds` is typed as a map from a knob's name to
    `{value, source, configured_value?}` and is read from the record's own
    `bounds` map, so a knob a newer darkmux writes is kept (reading it through
    `RuntimeBounds` would have dropped it). The JSON of a well-formed entry is
    unchanged, so `RUN_STATS_SCHEMA_VERSION` stays; an entry that is not a knob
    is now skipped instead of copied through.
  - `memory correction list`: prints `{"corrections": [...]}` (it was a bare list).
  - `flow integrity-check`: prints `{"reports": [...]}` (it was a bare list).
  - `machine status`: every answer carries `machine_id` and `lms_unreachable`
    (`false` when LMStudio answered). It was two shapes: the unreachable one had
    both, the normal one neither.
  - `machine status` (for a roster peer): a peer that answers in a shape this
    darkmux does not read is refused with an error that names what the peer
    reports (its `schema_version` or `darkmux_version`, when it gives one). It
    was printed as raw JSON.
  - `machine resources <peer>` prints the daemon's own response: the ledger plus
    `cache_ttl_ms` and, when the peer's sampler has a reading, `load`. Head of
    this release dropped both. A body this darkmux does not read (a newer peer's
    unknown variant) is refused under `--json` with the peer's version named, and
    in text mode prints a note naming it, exit 0. It was printed as raw JSON.
  - `machine list`: prints the fleet view (`FleetView`), one row per roster
    machine with the card it states about itself. It is no longer the reachability
    probe plus `--deep` specs: `probe_ms`, `resolved_address`, `dialed_address`,
    `reachable`, `specs` and the four `specs_*` flags are gone, and each row is
    `{entry, is_this_machine, machine_uid, uid_source, liveness, last_beat_ms,
    received_at_ms, fetch_ms, card, accepts}`, with a row for this machine always
    present (`entry` is `null` when the roster has none). See "Fleet awareness"
    under Added.
  - `dispatch` (`--json`): the runtime's keys print in the order `result`,
    `final_assistant`, `trajectory_path`, `failed_tool_invocations`,
    `resumed_from` (they were alphabetical), and a key the runtime does not
    define is dropped. `result` and `trajectory_path` are absent when the runtime
    did not send them (they printed as `""`). A runtime stdout that does not parse
    as an envelope goes out as written, with one line on stderr saying why.
    `detections`, `bounds` and `host_window` are the flow payload types
    themselves (`TelemetryDetectorPayload`, `RuntimeBounds`, `HostWindow`), so
    the golden pins them and two things changed inside them. A key that was
    printed as an explicit `null` is now omitted: in `detections[]`,
    `generated_chars` on a stream-gate observation and `tail_ratio` on a gate
    abort (the same omission the flow records got). Key order follows the
    struct's field order: in `bounds`, `reasoning_checkpoint_interval_tokens`
    moves from second to fifth; `host_window.power_mw_total` prints `mean`,
    `p95`, `max` (it was `mean`, `max`, `p95`); `detections[]` prints `kind`,
    `severity`, `detail` and then the fields in the type's declared order
    instead of the order each detector happened to add them. The golden lists
    only values the verb can print: `DetectorKind`, `DetectorSeverity` and
    `KnobSource` no longer offer `"unknown"`, which only a reader of another
    build's archive can meet.
  **Migration:** rename the fields above in any script that reads them, and
  regenerate a golden you keep of these outputs.

- **A mission config's step `config` is checked against its kind** (B1). It
  was open JSON, so a typo inside a step's config passed the unknown-key
  gate and silently did nothing. Each of the fifteen kinds darkmux ships
  (`dispatch.internal`, `dispatch.single_shot`, `dispatch.map`,
  `procedural.shell`, `procedural.noop`, `mods.gate`, `records.gather`,
  `deliver.github_review`, `crawl.plan`, `crawl.unit`, `crawl.summary`,
  `plan.sites`, `mission.worktree`, `mission.coder`, `mission.verify`) now
  has one typed config. A misspelled, wrong-type or missing key in a step
  config, a `grow.config` key no step of the task reads, or a step `kind`
  that is none of the fifteen, is refused at preflight and failed by
  `darkmux doctor`, naming the file, the key path and the closest valid key.
  **Migration:** run `darkmux doctor` and fix what it names; a key that did
  nothing has no replacement. Numbers and flags still accept their text form
  (`"draws": "3"`), and the open fields (`dispatch.map`'s `collection`, the
  `findings`, `mods` and `scope` a `deliver.github_review` step embeds) stay
  free-form. A step config the kind cannot load now fails the step naming the
  key, where a wrong-typed value used to fall back to a default.
- **A step config is also checked by value, before anything runs** (B1). The
  rules each kind's own reader enforces are now checked by the same code
  before a launch starts, on the document where no `{{param}}` is involved
  and again with the launch's `--param` values substituted (`--dry-run`
  included, and each grown copy as it is minted). The refusal names the step,
  the key and the rule: `crawl.unit` `draws` outside `1..=8` and
  `timeout_seconds` of `0`; `plan.sites` `source: "diff"` with no `diff_file`,
  or with neither `workspace` nor `github` plus `head_sha`, or with a `github`
  that is not `owner/repo` or a GitHub URL; `plan.sites` on its default tree
  source with no `workspace` (`github` plus `head_sha` derive one for a diff
  only); `crawl.plan`, `plan.sites` and `crawl.unit` with a `rule` that is not
  a safe path component (`crawl.unit` checks each part of a `+`-joined rule);
  `crawl.plan` and `plan.sites` with a blank `rule` (and `crawl.plan` a blank
  `workspace`) or a `sizing.*` of `0`; `mods.gate` with a blank `for_key`; `dispatch.map` retry
  budgets past `u32`; `deliver.github_review` with `findings` but no `mods` or
  `diff`, or records that are not the record types. The launch substitutes
  params exactly as the mint does, `{{mission_id}}` included, and a refusal
  after substitution names the step and its kind whatever the problem. Still
  refused only when the step runs, because no config alone decides them: a
  role named by neither the task nor `dispatch.internal`'s config, a
  profile-registry endpoint id, a `rule` id that names no known rule (or a
  diff-only rule on a tree plan), a directory or file that must exist (a
  workspace spec, a diff, a plan or an intent file, a workdir), a
  `dispatch.map` collection read from a dependency's output, and a
  `deliver.github_review` with no embedded `findings` and no `records.gather`
  output to read. A step `config` that is not an object (a string, a
  number or a list) is refused; a list used to load as its first values in
  field order. `temperature` now accepts its text form (`"0.5"`), and a
  `{{param}}` reference counts as a number or flag only when it is the whole
  string (`"n={{n}}"` is refused).
- **`mission launch --dry-run` labels the `mission_id` it prints** (B1). The id
  is minted before the launch check so `{{mission_id}}` can be checked, and a
  real launch mints a fresh one; the dry run now says `example id` beside it.
- **Some step config values now fail the step, and some now read as unset**
  (B1). Now refused, where they were silently dropped: a `records.gather`
  `not_attempted` entry that is not a string; a `deliver.github_review`
  `emit`, `attribution` or (with no `findings`) `diff` that is not a string; a
  `crawl.unit` `rule` that is not a string. Now read as unset, where they were
  errors: `null` for a `sizing.max_*` or `no_progress_turns`. And a
  `deliver.github_review` `findings: null` now reads as absent, so the step
  takes its records from a `records.gather` step. **Migration:** delete the
  key or fix its type; `darkmux doctor` names each.
- **A `mission.verify` task takes no `role_id`** (A19, #2953). The step always
  dispatches `code-reviewer`, so the key never did anything; the shipped
  `coder-phase` config no longer sets it and a config that does is refused.
  **Migration:** delete the `role_id` from the task; to review with another
  role, `darkmux dispatch <role> ...`.

- **Read auth and execution auth are separate switches** (#2988). A serve
  token used to close the whole read surface to peers, so a hub that took
  fleet work (which needs the token) could not also serve its viewer over
  `tailscale serve`. Now fleet work submission always requires the token
  plus a network-verified sender, unchanged, and READS are governed by the
  new `serve.read_auth` (`DARKMUX_SERVE_READ_AUTH`), default `false`: the
  viewer and every JSON route stay tailnet-open, token or no token. With it
  on, a read not from this machine needs the token, proxied requests
  included, so a browser viewer over the tailnet gets 401. `darkmux doctor`
  shows both postures (`serve daemon token`, `serve reads`), and so does the
  `serve` banner. `runtime.daemon_auth_enabled` is retired (CONFIG 2.0),
  replaced by `serve.token_keychain`; `init` writes both new keys visibly
  as `false`. **Migration:** move `runtime.daemon_auth_enabled` to
  `darkmux config set serve.token_keychain <value>` (a leftover key is
  refused, naming the replacement). If you relied on a token closing reads,
  `darkmux config set serve.read_auth true`. A non-loopback `--bind` now
  also requires `serve.read_auth true`: a token alone no longer licenses
  it, and `serve` refuses to start with read auth on and no token.
  "This machine" is one predicate: a loopback peer (`::ffff:127.0.0.1`
  included), no proxy header, and a single `Host` naming the daemon
  (`localhost`, `127.0.0.1`, `[::1]` or the bound address, with no port or
  the bound port). A page rebound by DNS to loopback, and a browser reaching
  the daemon through a header-less `tailscale serve --tcp` proxy, are no
  longer local. Limit: `Host` is client-set, so a non-browser client behind a
  TCP forward that adds no headers can send `Host: localhost` and cannot be
  told apart from this machine; for that setup use the HTTPS `tailscale
  serve` (it adds headers) or keep read auth on with a non-loopback bind. The `doctor` and
  `config-list` panels describe the fleet listener and its allow-list, so
  they are served only to this machine or a token holder even with read
  auth off; the other panels follow `serve.read_auth`. `darkmux serve` runs
  the config gate before it binds, so a wrong-typed value (`serve.read_auth:
  "true"`) or a retired key refuses the start. **Migration:** a tailnet
  viewer with read auth off loses the `doctor` and `config-list` panels
  (401); run them on the hub, or present the token.

- **`internal.utility` is the object `{ "id", "n_ctx" }` only (PROFILES 2.0).**
  The bare-string spelling (`"utility": "<model-id>"`) is refused, and the
  registry does not load with it: the error names the object to write.
  **Migration:** change `"utility": "<id>"` to
  `"utility": { "id": "<id>", "n_ctx": <the window it is loaded at> }`
  (the shipped `profiles.example.json` already uses it); an object with no
  `n_ctx` still declares no window and is nudged by `darkmux doctor`.

- **A profile model's inline `endpoint` object is refused, and an endpoint
  declares its kind (PROFILES 2.0).** A model names an `endpoints` entry by
  id (`"endpoint": "azure-east"`); the object form is gone, and so is the
  implicit-kind rule (no `url` meant managed, a `url` meant unmanaged): an
  `endpoints` entry declares `"managed": "lmstudio"` or a `url`, and one
  with neither is refused at use. Every dispatching preflight and `darkmux
  doctor` name each inline object with the exact rewrite. **Migration:**
  for each `"endpoint": { ... }` on a model, move the object to
  `endpoints."<id>"` and write `"endpoint": "<id>"` on the model (the
  refusal prints the id); a model on the LM Studio darkmux manages needs no
  `endpoint` at all.

- **`dirs.crew` and `DARKMUX_CREW_DIR` are removed; `DARKMUX_HOME` is the one
  relocation.** "Crew" is a retired concept: roles, missions, phases, crews
  and skills live directly under the darkmux root, and the knob meant two
  things (the preamble-override directory `<root>/crew`, and the root of
  that state). `dirs.crew` in `config.json` is an unknown key, refused by
  the gate, and a set `DARKMUX_CREW_DIR` is refused by every command
  (`doctor` and `config` excepted, so you can find and fix it) and failed by
  `darkmux doctor`. The autonomous-dispatch preamble override is now
  `<root>/AUTONOMOUS_DISPATCH_PREAMBLE.md` (it was `<root>/crew/...`).
  **Migration:** delete `dirs.crew`, unset `DARKMUX_CREW_DIR`, and if you
  relocated darkmux with it set `DARKMUX_HOME` instead. `darkmux doctor`'s
  `beat-33 crew/ layout` row prints the move for a preamble override left
  under `<root>/crew/`.

- **A project-local `./.darkmux/` is no longer adopted.** The darkmux root is
  `$DARKMUX_HOME` when set, else `~/.darkmux`, and nothing else: a `.darkmux/`
  in the working directory used to become the root for flows, lab runs,
  sandboxes and profiles while missions and roles stayed at home. It is now
  ignored: only the per-repo `lessons.db` and `conventions.json` are still
  read from it. The same goes for a `./.darkmux/profiles.json` or
  `./.darkmux.json` registry, which used to be searched ahead of
  `~/.darkmux/profiles.json`: the registry now comes from the root
  (`DARKMUX_HOME` or `~/.darkmux`), or `--profiles-file` /
  `DARKMUX_PROFILES`. **Migration:** to keep using such a directory, run
  darkmux with `DARKMUX_HOME=<that directory>`; otherwise move what you need
  into `~/.darkmux` (a registry is `~/.darkmux/profiles.json`). `darkmux
  doctor`'s `project-local .darkmux` row warns when the working directory
  holds anything besides those per-repo files, naming what is stranded.

- **A retired setting's env var is checked once, at CLI entry, by whether
  ignoring it is safe.** A leftover whose silent loss would change behavior
  is refused by every command except `doctor` and `config` (and `--help` /
  `--version`): the renamed `DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION` (a
  token cap that would quietly vanish) and `DARKMUX_CREW_DIR` (a state
  location that would quietly move). A leftover that nothing reads and whose
  loss changes nothing, `DARKMUX_NOTEBOOK_DIR` and
  `DARKMUX_RADIO_ROUTER_PROFILE`, is ignored with one warning on stderr, and
  the command runs. These used to be refused only by the dispatch,
  mission-launch, lab and fleet entry points. **Migration:** remove the
  export from your shell rc; `darkmux doctor` lists each one, failing for a
  refused one and warning for an ignored one, with the exact change to make.

- **An unknown key in a user file is refused (CONFIG 2.0).** `config.json`,
  `profiles.json`, role, skill and crew manifests, mission configs, rule
  files, workload documents, lab fixture manifests and a crawl's workspace
  spec used to ignore a key they did not know, so a typo silently did
  nothing. Now every entry point that reads the file refuses to start,
  naming the file, the key's path and the closest valid key, and `darkmux
  doctor` fails it. Files still load, so doctor always runs. A retired key
  is named with what replaced it; the tables come from `git log`, so every
  key an older darkmux read or `init` wrote is covered: in `config.json`,
  `remote.max_tokens_per_execution`, `remote.stage_budget_policy`, `gh`,
  `orchestrator`, `review`, `dirs.notebook`, `dirs.openclaw_config`,
  `dirs.runtime_agents`, `radio.router_profile` and
  `runtime.telemetry_record_every_samples` (the first three used to be warned
  about and ignored, the rest silently ignored); in `profiles.json`, `crews`,
  `hooks`, a model's `role`, a profile's `runtime.config_path` /
  `configPath` / `contextTokens` and the openclaw compaction keys (`mode`,
  `model`, `customInstructions`, `maxHistoryShare`, `recentTurnsPreserve`);
  a role's `capabilities` and `tier`; a mission config's `gh_verb` and a
  task's `expand`; a workload's `agent` and `expected.test_count_baseline`;
  a fixture manifest's `hash_exclude` and `hash_include`. `_comment` is accepted anywhere as a note. A value of the wrong
  type (`"port": "x"`) is refused the same way, naming the expected type and
  what it got: one used to make `config.json` silently fall back to every
  default (Redis and audit off), and made a user role, skill or rule
  silently lose to the builtin of the same id. A mistyped `profiles.json`
  entry keeps its loud per-entry quarantine instead. **Migration:** run
  `darkmux doctor` and delete or rename each key its `user file keys` rows
  name. A preflight refuses only over files the run would load: the
  effective copy of each mission config and workload (a shadowed copy is
  reported by doctor, not refused), and the fixture the run binds. A fixture
  registered from an older darkmux checkout keeps its old
  `.fixture.json`; delete `hash_exclude` from it, or re-run
  `scripts/lab-init.sh --force` from a current checkout.
- **A mission config's `source_input` and `ticket` are declared fields.**
  They were read out of the unknown-key overflow; a non-string value is now a
  parse error. **Migration:** none for a string value.
- **`tool-bench` workload knobs are declared fields** (`trials`,
  `taskTimeoutSeconds`, `chainDepths`, `seed`); a bad value's message reads
  ``workload `trials` must be …`` rather than `workload extras.trials must be …`.
  **Migration:** none.

- **A session id is a typed identity that names its run.** Every flow
  record's `session_id` is now `<run>[.lab|.solo].<kind>[.<field>...]`
  (FLOW 2.0.0; see its schema entry): a task session reads
  `review-1790000000-ab12cd.task.probe`, not `task-probe`, so two launches
  of one config never share a session, a presence key or a budget record,
  and `mission_id` always agrees with the session. `mission-<m>` and the
  bare `<m>` of the whole-run bookend are one session, `<m>.run`. A fleet
  receiver runs a submitted job under a relay of the sender's session in a
  standalone run (WORK_JOB 7), never one of its own missions, so the
  `-from-` rule on machine names is gone. `darkmux dispatch --name
  <name>` now names the dispatch within its crew-of-one run
  (`<run>.adhoc.<role>.<name>`). Archives are never rewritten: an old id
  still reads for step attribution and corrections. **Migration:** a script
  that matched session prefixes (`task-`, `step-`, `mission-run-`,
  `crew-dispatch-`) should key on `mission_id`, or on the printed session
  id as a whole; a `flow note --execution` names the execution `darkmux
  dispatch` printed.

- **A lab run with no verify spec reports verify "not checked", not a pass**
  (#2982). A `prompt` workload that declares no verify used to record
  `verify=pass (no verify spec)`; its outcome is now no verify at all, so
  `lab run` prints no verify note, `lab run inspect`'s note reads `verify:
  not checked — no verify spec`, and `lab loop` reads such a run with no
  tool calls as `failed` rather than `inert-false-pass` (both exit 1). **Migration:** a script that
  grepped for `verify=pass` on a no-verify workload should key on the exit
  code instead.
- **`lab characterize` and `lab tune` exit 1 on a failed verify** (#2982),
  through the same gate as `lab run`; they used to exit 0 whenever every
  dispatch completed. They, and `lab loop`, also exit 130 when a signal
  ends the run, as `lab run` already did. **Migration:** a script that
  treated exit 0 from these verbs as "the dispatch ran" should expect 1
  when the workload's verify fails.
- **A provider error fails one lab run, not the batch** (#2986). When run
  k of N errored, `lab run`, `lab characterize` and `lab tune` stopped and
  discarded runs 1..k-1. Now the errored run is recorded (its lifecycle
  reads `error`, and stderr names it), returned as a failed outcome, and
  the batch goes on; only a signal stops it. The exit code is still 1.
  `lab tune`'s stats cover the runs that completed, it names each errored
  run, and its header reads `× N run(s), K completed`. `lab run`'s summary
  line reads `N run(s): K completed, E errored` (it was `N run(s)
  complete:`). `lab run inspect` on an errored run shows the error its
  lifecycle recorded, which now keeps the whole cause chain. **Migration:**
  a script that read "exit 1 with an error message" as "nothing after this
  ran" should read the per-run lines, and one that matched `run(s)
  complete:` should match the new summary.
- **A prompt run's manifest records its verify, so `lab run list` shows a
  failed one as `FAIL`** (#2494). It used to show a plain tick. A manifest
  written before this reads as not checked (`—`), never as a pass.
- **Two lab runs in the same second no longer share a run dir, so a run id
  can carry a claim suffix** (#2981). The second run used to overwrite the
  first's artifacts. A run whose `<workload>-<profile>-<epoch>-<n>`
  directory already exists now claims
  `<n>.2`, `<n>.3`, … instead of writing into it. **Migration:** a tool that
  parses the last segment of a run id as an integer must accept `<n>.<k>`.
- **Every viewer surface judges a run the same way, by the daemon's own
  staleness rule.** The fleet card, the activity timeline, the run page's
  pill, clock and pulse, the live token scope, the mission graph's step
  meter and playback all read one lifecycle (`ui/src/lib/lifecycle.ts`), so
  the same run states the same phase on each at the same moment. A run
  silent for twice the runtime's inactivity budget (`runtime.inactivity_
  timeout_seconds`, 20 minutes by default, the rule `/runs` already used)
  reads as stopped with no ending recorded everywhere: the fleet card
  waited 5 minutes before, and the run page's pill said RUNNING forever.
  The STALLED word is the earlier signal, after 30 seconds of silence. A
  session id two missions share is two runs on every surface (#2125); a
  relaunch under the same id is its own attempt; a terminal with an
  unparsable timestamp closes the run on the fleet card too. A hosted call
  held by its endpoint's budget is running while it waits, on `/runs` as
  well (it no longer reads Abandoned after 20 minutes of a longer wait), and
  a wait the operator stopped reads **aborted**. A wait that lapses (its
  announced resume time plus a minute of grace passes with no resume)
  now stays running for a further 20 minutes before it reads stopped,
  matching `/runs`; before, the viewer called it stopped the moment the
  grace ran out. The mission graph's step meter follows the same rule, so
  a step held by a budget wait keeps its pulse. A link to a run's detail
  view carries its mission (`#dispatch=<sid>&dispatch.mission=<id>`), so a
  session id several missions share opens the run that was clicked, and
  the event log beside it lists that run alone; a link naming only the
  session still opens the run that started last. On `/runs` and in
  radio's busy check, a mission whose task session another mission shares
  (#1918) is judged by its own attempts on that session: it reads Running
  while it runs, where it used to read Abandoned after 20 minutes. Its
  role, model, machine and endpoint are still not read from a shared
  session. `/runs` gains
  `policy: {stale_after_ms, budget_wait_grace_ms}`, the numbers it judged
  by, and `/health` gains the same object as `lifecycle_policy`, which is
  where the viewer reads it (both additive). **Migration:** none.
- **A call that reports no prompt count has an unknown spend, and is
  never charged as small.** A usage record whose provider sent a
  completion count but no prompt count (and no total) now carries no
  `total_tokens`: a split missing a half is not a total. A hosted
  `dispatch.single_shot`, `dispatch.map` item or `darkmux dispatch` to an
  endpoint settles such a call (and one with no usage at all) against its
  per-step cap at the whole granted `max_tokens` PLUS the prompt it sent,
  estimated from the request at four characters a token (it had been
  settling the completion alone, or the cap alone: the cap bounds only the
  completion). Under an endpoint's `limits.window` token budget, the
  halves such a call did report still count toward `warn_at`, the budget
  and a `wait`, as a floor, and the window is flagged as not fully metered:
  `budget.warn` and `budget.wait` carry `unmetered_calls` beside any level
  they report (a warning whose only news is the flag has `level: null`),
  and `darkmux doctor`'s endpoints check reads "spent at least". A calls
  budget is never flagged: a call count is exact. The limit: a call whose
  provider reported NO usage at all adds nothing to the window's known
  spend, so under a token `wait` budget such calls warn "not fully
  metered" but never, on their own, make it wait (the conservative charge
  applies only to a step's per-step cap). **Migration:** none; an
  endpoint that reports full usage reads exactly as before.
- **The per-step cap on hosted tokens is renamed, has no default, and
  never stops a step: a step that used to stop at 500,000 hosted tokens now
  runs to completion unless you set a cap** (#2902 step 5).
  `remote.max_tokens_per_execution` is now `remote.max_tokens_per_step`
  (env `DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION` is now
  `DARKMUX_REMOTE_MAX_TOKENS_PER_STEP`): a per-step cap on hosted tokens,
  where `dispatch.map` steps naming the same `bucket_group` share one
  allowance. Clean break, no alias: `darkmux config set` refuses the old key
  naming the new one, and a leftover old key in `config.json` or old env var
  is refused at every preflight and failed by `darkmux doctor`, naming the
  exact rename. It has no built-in default any more (unset means no cap), and
  reaching a cap never stops the step: the new `remote.step_budget_policy`
  (env `DARKMUX_REMOTE_STEP_BUDGET_POLICY`) is `warn` (the default: a CLI
  line and a `budget.warn` flow record, and the step keeps going) or `off`.
  There is no `wait` for a step (a step has no rolling window to wait on;
  `wait` is an endpoint budget's value) and `wait` there is refused at
  preflight. Before, a step that reached its cap skipped its remaining
  hosted calls, and each call's `max_tokens` was clamped to what was left;
  neither happens now. A cap of `0` is no cap (a `0` on a darkmux bound
  means unbounded), where before it refused every hosted call. `init`
  writes both keys visibly as `null`. CONFIG 1.32. **Migration:** a
  `config.json` written by an earlier `init` carries
  `"max_tokens_per_execution": 500000` in its `remote` block; darkmux now
  refuses it. Delete it, or, to keep a cap, run
  `darkmux config set remote.max_tokens_per_step <n>` (and rename an
  exported `DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION`).
- **Flow actions have one spelling per event** (FLOW 2.0.0). Every action
  is `<scope>.<event>[.<detail>]`: `dispatch start` is now `dispatch.start`,
  `step result` is `step.result`, `mission close` is `mission.close`,
  `note` is `operator.note`, `tier-decision` is `tier.decision`, and
  `verdict: <v>` is `phase.review.verdict` with the verdict in
  `payload.verdict` (the full list is in `crates/darkmux-flow/src/schema.rs`).
  darkmux's own readers, the daemon routes included, upgrade pre-4.0
  archives on read and never rewrite them. An action retired with no
  current equivalent (`telemetry.process`, `funnel.*`, the old
  `mission.run.*`, `crawl.*` launcher records) still reads, as retired; an
  action this build does not know still reads and `darkmux doctor` names it. `darkmux flow record
  --action` accepts only known actions. **Migration:** a consumer of the
  flow stream outside darkmux (a hook receiver, a script over the day files
  or Redis) must read the dotted spellings. A hook rule whose `match.action`
  names an old spelling, an exact one (`dispatch complete`) or a spaced glob
  (`step *`), is refused: the hook sink does not load (the run itself
  continues without hooks, like any other bad hook rule) and `darkmux doctor`
  fails the rule (`RETIRED SPELLING`), each naming the spelling to write
  (`dispatch.complete`, `step.*`). A glob's dotted twin can match more than
  the old spelling did (`dispatch.*` also matches every `dispatch.turn` and
  `dispatch.tool` record); the refusal says so. A rule's outbox is keyed by a
  hash of its `match`, so a rewritten rule starts a new outbox: records still
  pending under the old spelling are not delivered (they stay in the outbox
  directory as `<key>.outbox.jsonl`; delete the stale files). The key is not
  derived to survive the rewrite because that would mean hashing the retired
  spelling forever. Only the hook outbox, an archive of records already
  written, is still read leniently.
- **A whole run has its own bookends: `run.start` / `run.complete` /
  `run.error`** (FLOW 2.0.0, CLAUDE.md contract 8). `mission launch` and an
  ACP panel run used to bracket the whole run in `dispatch.start` /
  `complete` / `error` with `source: "mission"` and `payload.runtime`
  (`mission` / `ephemeral`); they now write `run.*` on the run's own session
  and neither field (`payload.runtime` is gone from every record; see below). `dispatch.*` means one role execution only. On the
  viewer, the fleet card's DISPATCHES chip and the status line's "last
  dispatch" no longer count a run as a dispatch, and the event log files the
  run records as `run.start` / `run.complete` / `run.error` under MISSION.
  darkmux's own readers read a pre-4.0 archive's whole-run pair (`source`
  `mission`, or the retired review launcher's `review`) as `run.*`, and never
  rewrite it. **Migration:** a hook rule or external reader that watched
  `dispatch.complete` with `source: "mission"` to learn that a run ended
  must match `run.complete` / `run.error` instead; one that counted
  `dispatch.start` records as runs now counts role executions.
- **Every record of a role execution names it: `execution_id`** (FLOW 2.0.0,
  CLAUDE.md contract 8). One id is minted per execution (a container or hosted
  dispatch, a `dispatch.single_shot` step, each item of a `dispatch.map`) and
  stamped on its `dispatch.*` bookends, turns, tool calls, `telemetry.*` and
  `budget.*` records; a resumed dispatch keeps its id. A `dispatch.map` step
  no longer writes one `dispatch.start` / `complete` pair around the whole
  step: each item writes its own. Token totals, the DISPATCHES chip, the
  records-emitted pairing and the run lifecycle key on the execution, so a
  session holding several (a map's items) no longer blends them. Stored
  findings are filed under `<execution_id>/<seq>`, and `darkmux finding list
  --dispatch <id>` is now `--execution <id>`; a finding filed before 4.0 keeps
  its address. darkmux's readers give a pre-4.0 record of an execution the id
  `legacy:<session>:<mission>` and never rewrite the file. **Migration:** a
  hook rule or external reader that counted a map step's `dispatch.start` as
  one per step now sees one per item; one that joined a step's records by
  `session_id` alone can join by `execution_id`; the runtime image's
  `--session-id` flag is `--execution-id` (the image and binary are version
  locked, so nothing to do but upgrade both).
- **`finding list --json` rows carry `execution`, not `dispatch`** (breaking:
  `--json` shapes are semver-bound). Each row's `dispatch` field is now
  `execution`, holding the `<execution_id>` half of the finding's key; the
  `--dispatch` flag is refused with a message naming `--execution`.
  **Migration:** read `.execution` where a script read `.dispatch`, and
  pass `--execution <id>` where it passed `--dispatch <id>`. The catalog's
  per-day and per-mission DISPATCHES counts are executions too, so a map
  step's items each count.
- **Every machine in a fleet upgrades together** (FLOW 2.0.0). A 4.0 reader
  upgrades a 3.x peer's records, but a 3.x reader does not know the dotted
  spellings: a 3.x hub misreads a 4.0 peer's records (its missions never
  end, its step results aren't folded). **Migration:** upgrade every
  machine in the fleet before relying on the hub's views.
- **Dotted hook globs now match the bookends too** (FLOW 2.0.0). Before 4.0
  these actions were spaced, so a dotted glob never saw them: `dispatch.*`
  now also matches `dispatch.start` / `complete` / `error` / `route`;
  `mission.*` also matches `mission.start` / `close` / `abort` / `pause` /
  `resume`; `step.*` (which matched nothing before) matches `step.start` /
  `complete` / `error` / `result` / `timing` / `seat_unresolved`; and
  `phase.*` matches `phase.start` / `complete` / `abandon` / `added` /
  `id_ambiguous` and `phase.review.begin` / `aborted` / `dispatch` /
  `failed` / `verdict`. **Migration:** a receiver behind one of these globs
  sees more records; narrow the rule (`dispatch.tool`) if it should not.
- **The viewer matches the dotted flow vocabulary only, and says when it
  meets anything else.** Every record enters the viewer through one module
  (`ui/src/lib/ingest.ts`); the spaced spellings are no longer matched
  anywhere in it. The event log's activity filter names scheduler and mission
  records by their dotted action (`step.start`, was `step start`), and a
  record whose action is neither current nor retired adds `· N unknown` to
  the event count. A record whose timestamp does not parse is kept on every
  surface: it closes its run, shows `--:--:--` for its clock, and is left
  out of any duration or rate arithmetic. **Migration:** re-pick any saved
  activity filter that named a spaced action.

- **A profile names the machine that runs it: `profile@machine`; `dispatch
  --machine` is removed** (#2916 stage 2, no alias). `darkmux dispatch
  <role> --profile host@studio` sends the dispatch to the studio's fleet
  listener (stage 1's authenticated channel, below), and the studio resolves
  `host` against its OWN registry and loads it; the sending machine needs no
  profile of that name. An address naming this machine runs here, on the
  bare name. The machine part is a `machine_id` (letters, digits, `-`,
  `_`), resolved at dispatch time against this machine's roster,
  case-insensitively. The receiver is the only judge of the profile: an
  undefined name is refused by name, never replaced by its
  `default_profile`. On a path that runs only on this machine (the lab, a
  mission step, until mission steps route), an address is refused naming
  it, never read as an undefined local name. The sender's `dispatch.route`
  record carries `profile_address`; tokens are counted once, by the machine
  that runs the model, never on the sender's records. A profile name that
  contains `@` cannot be addressed. **Migration:** `darkmux dispatch <role>
  --machine <m> [--profile <p>]` becomes `darkmux dispatch <role> --profile
  <p>@<m>`; name the profile on `<m>` that the job should run on (with no
  `--profile`, the old form resolved the role's binding on `<m>`; name that
  profile now). `darkmux mission dispatch`, which also took `--machine`,
  is removed in this release (#2954, below).

- **`remote.concurrent_cap = 0` means unbounded everywhere** (#2916 stage 2).
  The scheduler's hosted track used to clamp `0` to `1`, while the fleet
  listener read `0` as no limit. Both now read it as no limit, the darkmux
  bound convention. **Migration:** if you set `0` to mean "one at a time",
  set `1`.

- **Busy is decided per seat, and a receiver chooses refuse or queue**
  (#2916 stage 2). A worker no longer runs one submitted job at a time. A
  job on a LOCAL model holds that model for its run (LM Studio serves one
  request at a time per instance), so a second job for the same model is
  busy while a job for a different local model runs beside it; a job on a
  HOSTED endpoint runs beside others up to the receiver's own
  `remote.concurrent_cap`. Past either limit the receiver's new
  `fleet.busy_policy` answers (CONFIG 1.33, `refuse` by default, written
  visibly by `init`): `refuse` says `busy: <machine> is running other work
  on that seat (...)` at once, naming what runs; `queue` holds the job
  (first come, first served per seat; at most 4 queued per sending
  machine) and tells the sender it is waiting, with a `queued` line every
  20 seconds until it runs. A queued job passes every admission check
  again when its seat frees (the fleet token in force against the one it
  was admitted with, the sender's network identity, its allow-list entry,
  the config preflight, the scope with its profile resolved afresh), so
  `untrust` or removing the sender from the network also stops jobs
  already waiting. Rotating or removing the fleet token takes effect when
  the daemon restarts (it reads the token once), and a restart drops the
  queue anyway. A sender that closes its connection
  gives its place back and its job never runs; one that vanishes without
  closing it (a laptop that sleeps) keeps its place until TCP gives up on
  the connection. A waited-on job waits no longer than its connection
  allows (worked out from its timeout, which a container-agentic run does
  not enforce), and one queued without `--wait` at most 30 minutes (then
  it is answered busy and never runs). A reply with a status this darkmux
  does not know (a newer receiver) is reported as such, with the job
  possibly still running. Only jobs from other machines count: this machine's own
  dispatches are not seen by the listener. When a connection drops after
  the receiver may have taken the job, the sender says the job may still be
  running there and names the run to follow. The sender prints the
  receiver's words verbatim. A bad value is refused at the listener's start and reported
  Fail by `darkmux doctor` (#2947). The work-submission wire moves to
  schema `6` (a reply body is newline-delimited: `queued` lines, then the
  answer), so both machines must run the same darkmux; a mismatch is
  refused naming both versions.

- **The degeneracy detector's policy values name the action: `off`,
  `record`, `warn`, `conclude`** (#2947). `enforce` is now `conclude` (still
  the default, behavior unchanged: on repeating output the runtime closes
  the model's thought so it answers from what it has, and escalates if it
  keeps repeating; nothing is discarded) and `observe` is now `record`. New:
  `warn` measures and, on each finding, prints a warning for the dispatch,
  writes a Warn-level `dispatch.degeneracy.warning` flow record and counts
  `degeneracy_warnings` in the run envelope, without concluding anything.
  The old spellings are refused with the new word ("`enforce` was renamed
  to `conclude` in 4.0"); there is no alias and no automatic migration.
  CONFIG 1.31, FLOW 1.63.0. **Migration:** a `config.json` written by an
  earlier `darkmux init` carries `"policy": "enforce"`, so every dispatch
  refuses until you run
  `darkmux config set runtime.detection.degeneracy.policy conclude` (or
  `record` where you had `observe`); `darkmux doctor` prints the exact
  command.
- **An unrecognized value in an enum-valued setting is refused, not
  guessed** (#2947). `runtime.detection.degeneracy.policy` used to run as
  `enforce`, `fleet.mode` used to read as `standalone`, and a typo in
  `runtime.thermal.pause_at` / `resume_at` used to pass through and disarm
  the thermal governor's soft tiers. Now `darkmux dispatch`, `mission
  launch` (dry runs included), `lab run` / `lab eval`,
  `radio` and the ACP panel refuse before starting anything, naming the
  value, where it was set (env var or `config.json` key) and the valid
  values. Fleet work submission does the same for
  `fleet.identity.provider`: `machine add` refuses before writing the
  roster, and a receiving machine refuses a submission synchronously (503)
  before accepting it. `--skip-preflight` does not waive any of this.
  `darkmux doctor` reports every bad value as Fail.
  Two kinds of setting are, by design, refused by no preflight, and doctor
  says so: `fleet.mode` (no command that starts work reads it; a bad value
  only makes viewer links use the direct address, with a warning), and a
  hook rule's `match.level` / `match.category`: a bad value turns the
  hooks sink off, loudly, while the run itself continues without it.
  `darkmux config set <key>` with no value (exit 2), `config list` (at a
  terminal) and `config set --help` list every valid value with its
  meaning. The config file is still read leniently (a typo in one of these
  keys never discards the other settings); what changed is that the value
  is refused where it is used (CONFIG 1.31, above). **Migration:** run
  `darkmux doctor`; fix any Fail row it names with the fix it prints.

- **Fleet work no longer travels through Redis; a `profile@machine` address
  submits straight to the target machine, which checks who is asking** (#2916,
  stage 1). The `darkmux:work` queue could not say who wrote an entry and
  every node that could write the hub's Redis could fill it, and every
  `darkmux serve` with Redis configured ran whatever it claimed. The queue
  is retired outright: the daemon no longer consumes `darkmux:work` (no
  `darkmux-runners` consumer group, no claim loop), and nothing publishes to
  it. `darkmux dispatch <role> --profile <p>@<id>` now sends the job to that machine's
  **fleet listener** (the roster host of `<id>` on `fleet.listener.port`,
  default 8766) with the fleet token (the serve token, #881, one value on
  every machine). The receiver runs it only when the overlay network
  (`fleet.identity.provider`, `"tailscale"`: `whois` on the connection)
  names the sending node as one on its allow-list, and only on a profile in
  that entry's scope (never one that runs on the utility model, #2914; a
  `--workdir` only with `workspace`). Deny by default; no identity answer
  is a refusal. The address is no longer an advisory hint: the named
  machine runs the job or answers at once with the reason ("studio does not
  accept work from macbook-pro", "not in the allow-list scope: profile
  X", "studio is busy running <session>": one submitted job at a time).
  With `--wait` (the default) the reply carries the remote exit code and
  output instead of a synthetic line read back off the flow stream, so a
  cross-machine `--wait` no longer needs Redis at all. `--profile` now
  crosses (it names a profile on the target). There is no "any machine
  claims it" any more: a submission names its target. The job wire shape is schema v5:
  `target_machine` required, `profile` added, `attempt` and
  `published_by_orchestrator` removed. **Migration:** on every machine
  that should take work, store the fleet token if it has none (`security
  add-generic-password -U -a "$USER" -s darkmux-serve-token -w`, same value
  everywhere, plus `darkmux config set serve.token_keychain true`),
  trust each sender (`darkmux machine trust <sender> --profiles
  <profile>,... --roles <role>,...`), `darkmux config set fleet.listener.enabled true`, and
  restart `darkmux serve`. On the hub, delete the dead streams: `redis-cli
  DEL darkmux:work darkmux:work:inference` (`darkmux doctor` names any that
  remain) once every machine runs 4.0. **Mixed versions:** a 3.x daemon
  still consumes `darkmux:work` (and re-creates the stream when it
  starts), so the queue stays an open, unauthenticated way to make that
  machine run work until it is upgraded; `darkmux doctor` names any daemon
  still consuming it. A 4.0 sender reaching a 3.x machine gets "no answer"
  (3.x has no fleet listener); a 3.x `--machine` dispatch publishes to the
  queue, no 4.0 machine reads it, and it waits silently until its timeout.
  Two 4.0 builds on different wire schemas are told so by name.

- **One machine utility model, declared once with its window, never a
  task's model** (#2914; finishes #590, supersedes the open parts of #70).
  `internal.utility` in `profiles.json` now also accepts
  `{ "id": "<model>", "n_ctx": <window> }` (a bare id still reads);
  `darkmux init` ships the object form. That binding is where compaction
  and radio routing run, and it is set aside by every task/step selection
  path: a profile that still lists it puts work on its other model, a
  profile that lists only it is a loud error naming the fix, and
  `darkmux mission launch` refuses, before minting anything, a task whose
  staffing (or a `dispatch.single_shot`/`dispatch.map` step's `config.model`)
  resolves to it. Profiles hold work models only. The lab keeps benchmarking
  a candidate utility model through a profile that lists it. PROFILES
  schema 2.0 (an older binary cannot read the object form).
- **Utility jobs run lean** (#2914). Compaction and radio routing emit their
  `telemetry.tokens` usage record (`purpose: utility`) and nothing else: no
  session, no `dispatch start`/`complete` bookends, no run. The runs board
  lists work only (it listed dozens of `radio-router` runs a day); the fleet
  total still counts utility under its own chip. CLAUDE.md contract 2
  (dispatch liveness) is amended to cover work executions; a busy utility
  instance makes a routing call wait behind a compaction, by decision, and
  #2915 will show why. One model-load seat tag, `utility`, replaces the
  `compactor`/`utility` pair on `telemetry.lms` records. FLOW schema
  1.60.0.
- **`machine add` refuses a loopback address, and a roster entry's id is
  the machine's `machine_id`** (#2924, groundwork for #2916). The documented
  way to register a machine in its own roster was `machine add <me>
  --address 127.0.0.1:8765`, which put an address in the roster that no
  peer can use: other machines read the roster (the daemon serves it to
  every viewer), and a loopback address reaches whichever machine reads it.
  `machine add` now exits 2 on an address that reaches only the reading
  machine (`127.x` including short forms like `127.1`, `::1`, `0.0.0.0`,
  `::`, their v4-mapped forms, `localhost`) and names the fix;
  `--allow-loopback` keeps it for several daemons on one host (a same-host
  test fleet), and the entry records that it was intended. Whether an entry
  is THIS machine (and so gets its hardware identity recorded) is now
  decided by its id matching this machine's `machine_id`, not by a loopback
  address. `machine list`, `machine status <id>` and `machine resources
  <id>` reach this machine's own entry at the local daemon, so the roster
  address stays the name peers dial. The hub guide and the
  `darkmux-add-machine` skill register every machine, the hub included,
  under its `machine_id` at its tailnet DNS name. **Migration:** for an
  existing loopback entry, re-add it with the DNS name (`darkmux machine
  add <id> --address <tailnet-dns-name>`, which keeps its added time; if
  `darkmux doctor` also reports the entry renamed, remove it and add it
  under the machine's current name instead, as that row says). `darkmux
  doctor` lists each one.

- **The mission config schema is 4.0** (was 3.5), a major bump: a `panel`
  key is now refused, and a step's `config` is checked against its kind's
  schema, so a 3.x document that carried either can fail where it loaded
  before. The shipped configs declare `"4.0"`.
  `darkmux doctor` notes each user-tier mission config whose schema major is
  older than this darkmux's, naming the file. **Migration:** run `darkmux
  doctor`, fix what it names (the `panel` block and unknown step-config keys
  are the usual ones), and set `"schema_version": "4.0"`.
- **A run row's `session_id` is `dispatch_id`** (`darkmux run list --json`,
  `darkmux mission show --json`'s `runs`, and the daemon's `GET /runs`). It is
  the id the viewer's `#dispatch=<id>` route and `GET /flow-dispatch/:id`
  take; its value is unchanged. **Migration:** a script that read
  `.session_id` on a run row reads `.dispatch_id`.

### Removed (breaking, 4.0)

- **The persona roles and the acknowledgment gate are gone** (#3036). The
  built-in roles `fitness-coach`, `health-research`, `legal-research`,
  `trip-researcher`, `logistics-coordinator`, `voice-editor` and `lab-manager`
  no longer ship, nor do the skills only they used (`documenting`, `writing`,
  `voice-editing`). A dispatch to one of those ids fails with the ordinary
  `role not found` error. The licensed-adjacent acknowledgment gate (the
  `ACKNOWLEDGE` prompt and `<role>.ack` files) is deleted, and so are
  `dirs.ack` and `DARKMUX_ACK_DIR`: a leftover `DARKMUX_ACK_DIR` only warns,
  while a leftover `dirs.ack` key in `config.json` is refused at preflight
  (dispatch, mission launch, lab run, fleet work submission, serve) and failed
  by `darkmux doctor` until you delete it. The disclaimer's section about those
  role prompts is removed with them. **Migration:** keep a role you still want
  as your own file under `<darkmux root>/roles/`; delete `dirs.ack`.
- **`darkmux lab eval` is removed whole** (#3036). The verb with all its flags
  (`--mode`, `--workdirs`, `--prosecutor-profile`, `--defender-profile`,
  `--judge-profile`, `--cases-dir`, `--scores-out`), the `dialectic-*` and
  `pr-reviewer-agentic`/`-freeform` roles, and the `pr-review-bench` fixture are
  gone. Use `darkmux lab run <workload>` and
  `darkmux mission launch review`. The runs board no longer prints a
  `try it yourself` line naming the removed verb. `pr-reviewer` stays.
- **Stale docs and an unused plugin are deleted** (#3036). `ROADMAP.md`,
  `docs/roadmap/`, `docs/463-workspace-split-plan.md`, `docs/design/`,
  `docs/architecture/observability-unification-plan.md`, the `/topology` and
  `/viewer` redirect stubs under `docs/`, the repo-root `AGENTS.md` and
  `plugins/darkmux-bundler-edge` are removed. `darkmux init`'s AGENTS.md
  integration for your own project is unchanged.
- **`runtime.log_level` and `DARKMUX_LOG` are gone** (CONFIG 2.2). The setting
  switched on one debug line on the tool-less hosted dispatch path and nothing
  else read it. A leftover `runtime.log_level: "info"` in `config.json` (what `darkmux init`
  wrote) and a leftover `DARKMUX_LOG` warn and are ignored (#3057); any other
  `log_level` value is refused at preflight. **Upgraders:** delete it.
- **The `machine_rollup` block, its two env vars and the `machine.rollup` flow
  record are gone** (CONFIG 2.2, FLOW 2.0.0). Nothing replaced the periodic
  whole-machine heartbeat: the machine lens reads `GET /machine/resources`.
  `darkmux doctor` loses its `machine_rollup` row, and an archived
  `machine.rollup` record reads as an unknown action. A leftover `machine_rollup`
  block with `enabled: false` (what `init` wrote) warns and is ignored, and so do
  the two `DARKMUX_MACHINE_ROLLUP_*` vars (#3057); `enabled: true` is refused at
  preflight, since ignoring it would drop a feature you turned on.
  **Upgraders:** delete the block (and `runtime.log_level`).
- **`fleet.accept_work.<name>.workspace` is a receiver path grant and nothing
  more** (CONFIG 2.2). It never authorizes a fetch, a checkout or a push; git
  handoff (#755) gets its own grant, and its checkouts live outside the
  worktrees base this grant covers. The entry gains an optional `repos` list,
  reserved for that handoff and read by nothing yet, so it changes no admission
  decision.

- **Limits belong to the endpoint, and the `remote` block is gone** (CONFIG 2.3,
  #3035). "Remote" was the wrong axis: a local server on the same machine is an
  endpoint too, and what matters is whether darkmux manages it.
  `remote.max_tokens_per_step` (and its 4.0 name `remote.max_tokens_per_execution`),
  `remote.step_budget_policy` and `remote.concurrent_cap` are retired, with
  `DARKMUX_REMOTE_MAX_TOKENS_PER_STEP`, `DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION`,
  `DARKMUX_REMOTE_STEP_BUDGET_POLICY` and `DARKMUX_REMOTE_CONCURRENT_CAP`.
  A leftover `remote` block at its old defaults warns and is ignored (#3057);
  one that sets a spend cap is refused at preflight by every consuming entry
  point until you move it, and `darkmux doctor` fails it;
  `config set` refuses the old keys. A leftover `DARKMUX_REMOTE_MAX_TOKENS_PER_STEP` or
  `DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION` is refused (ignoring a spend cap would
  remove it); `DARKMUX_REMOTE_CONCURRENT_CAP` and `DARKMUX_REMOTE_STEP_BUDGET_POLICY`
  only warn.
  **Nothing is carried over: limits are off until you set them per endpoint.**
  **Migration:** move `remote.max_tokens_per_step` to
  `endpoints.<id>.limits.tokens_per_dispatch`, `remote.step_budget_policy` to
  `endpoints.<id>.limits.policy`, and `remote.concurrent_cap` to
  `endpoints.<id>.limits.concurrent_calls` in `profiles.json`, then delete the
  `remote` block from `config.json` (and the env vars from your shell rc).
  Behavior that changes with it:
  - The token cap is **per dispatch** (one role execution) and applies to any
    endpoint, managed or not, under the endpoint's `policy` (`warn` once a cap
    is set, `off`, or `wait` for its rolling `window`). Each `dispatch.map` item
    is one dispatch with its own cap; a container dispatch settles each model
    call into its cap as it lands. A `dispatch.map` step's `bucket_group` and
    `bucket_budget` are removed: a config naming either is refused, pointing at
    the endpoint's `limits.window`, which is how a whole-run budget is written now.
  - **An endpoint darkmux does not manage runs its calls one at a time** unless
    it declares `limits.concurrent_calls` (`0` is unbounded), within one darkmux
    process: two missions, radio or a fleet job at the same endpoint at once are
    not serialized together. darkmux says so once per launch when it matters,
    and never guesses a number. Different
    endpoints no longer wait on each other. On a **managed** endpoint
    `concurrent_calls` is refused at validation (preflight and doctor): the
    scheduler owns its parallelism. The same number bounds how many fleet jobs
    from other machines hold one endpoint at once.
  - A **managed** endpoint's window and per-dispatch cap are enforced too. A
    `wait` holds the dispatch and keeps its seat: between container turns it
    reuses the thermal governor's pace file, and at the start or on a
    single-shot call it is the existing polling gate.
  - `policy: wait` with no `window` is refused (a dispatch's own spend never
    expires, so nothing would free room). An inline `config.endpoint` object can
    carry `tokens_per_dispatch` and `concurrent_calls`, and a window on one is
    refused: name the endpoint under `endpoints` instead.
  - `darkmux doctor`'s `endpoints` row shows each endpoint's limits, its
    per-dispatch cap, how many of its calls run at once, and its window spend.
  - The machine card is schema 1.2: `seats.hosted.cap` is gone (there is no
    machine-wide hosted cap), and `/health`'s `fleet_busy` loses `hosted_cap`.
    `run_step_graph` and `run_bounded` lose their `remote_cap` parameter.
  - **"remote" and "hosted" are renamed out of the wire** (FLOW 2.0.0 and card 1.2
    are unreleased, so no version moved): `budget.*` payload `scope` `step` is
    `dispatch` and its `step` field is `dispatch`; `step start` `seat_class`
    `remote_endpoint` is `unmanaged_endpoint`; `step result`
    `remote_max_tokens_per_execution` is `tokens_per_dispatch`; the card's
    `seats.hosted` is `seats.unmanaged` (an older card's `hosted` still reads);
    an envelope's `remote_budgets` is `dispatch_budgets` (the old key still
    reads, never written); `machine list` says `unmanaged N`; doctor's
    `remote endpoint credentials` row is `unmanaged endpoint credentials`.
  - **The last `remote` wire names follow** (same unreleased versions, nothing
    bumped): a `telemetry.tokens` or `step result` payload's `remote` flag is
    `unmanaged`, a `dispatch.complete`'s `remote_tokens` is `unmanaged_tokens`,
    a run record seat's `remote` is `unmanaged`, `--json` models report
    `unmanaged` (not `remote`), and a role's `residency` of `remote` is
    `unmanaged`. The flag means "the endpoint is not one darkmux manages",
    decided from the endpoint's kind, so an unmanaged server on this machine
    reads as unmanaged. An archived record or envelope's old key still reads,
    and nothing writes it.
  - A flow record read off the hub carries `hub_id`, now listed in the FLOW
    history (it is never written to a flow file).

- **`darkmux machine list --deep` is retired.** The card is the default content
  of `machine list`, so there is nothing to ask for; the flag is refused, naming
  that. **Migration:** drop the flag. A script that read `--json`'s `specs`,
  `reachable` or `probe_ms` reads `card.card.specs` and `card.state` instead.

- **The daemon HTTP API is a semver contract, and its aliases are gone** (C1,
  A3, C2, C3). From this release the daemon's routes and response shapes change
  only on purpose: every JSON body is a serialized Rust type in
  `crates/darkmux-serve/src/wire.rs` with a generated TypeScript twin,
  `ui/src/types/handwritten.ts` is deleted, and
  `crates/darkmux-serve/route-table.golden` pins every route's method, path and
  response type. **Removed with no alias (each answers 404):** `GET /next`,
  `GET /mission/:id/graph`, `GET /flow-status`, `GET /worktree-summary/:session_id`.
  **Renamed:** `GET /flow-session/:id` is `GET /flow-dispatch/:id`, and
  `GET /fleet/sessions/live` is `GET /fleet/dispatches/live` with its `sessions`
  array now `dispatches` ("session" is an internal join key, contract 8).
  **Viewer links:** `#session=<id>` is `#dispatch=<id>`, `#lens=lab` is
  `#lens=runs&kind=lab`, `#lens=machine&uid=<uid>` is `#lens=machine&machine=<key>`
  and `panel=mission-status-all` is `panel=mission-status&opt.all=all`. The old
  `#lens=` spellings and any hash carrying `session=` open the "Unknown route"
  page instead of being rewritten (the session id is withheld from that page). `darkmux
  mission status` now prints `#mission=<id>` and `opt.all=all` links (it printed
  the retired `/mission/<id>/graph` and `mission-status-all` forms).
  **Response shapes changed on the wire:** `GET /machine/resources` answers
  HTTP 500 with a plain-text body when the ledger gather panics, where it
  answered 200 with an `{"error": ...}` body; `GET /fleet/roster` entries are a
  fixed set of fields (the roster file's own unknown `extras` no longer leak
  through); `GET /lab/runs` rows carry `has_reviews` (was `has_funnels`) and a
  `staffing` reduced to each seat's `name`/`model`/`k`/`n_ctx`/`max_tokens`;
  `GET /lab/run/detail` returns `reviews` (was `funnels`, the whole envelope) as
  a six-field summary per case, and `scores` as `{role, mode, profile}` (was the
  whole scores document). Every other route keeps the bytes it served; what
  changed is that its shape is now one Rust type with a generated twin, and the
  viewer's old hand-written copies were corrected to it (they had drifted:
  `/runs` and `/flow-mission|dispatch/:id` had always sent `meta`, ledger fields
  the viewer typed as numbers are nullable, a ledger state is `amber` not
  `yellow`, a `SessionBeat` carries `display_name`, `role` and `model`).
  **Migration:** none for operators; a script that
  read the old routes or fields must use the new names. `funnels.json` and
  `funnel-events.jsonl` in an old lab run directory are still read as archives
  (no writer produces them since #2310).

- **The per-config `panel` block, and the panel's per-config slash commands**
  (`/review`, `/machine-status`, `/pr-merge`, ...). The editor panel now has
  one command, `/mission`, with three verbs, and every config `darkmux mission
  launch` accepts is listable and launchable through it. `/mission list` and
  radio's catalog run the same first check a launch does, so they list
  configs exactly when a launch could start. A mission config
  carrying a `panel` key is refused by the user-file gate and by `mission
  config` validation, with a message naming `/mission launch <id>`.
  `mission config list --json` rows and `mission config show --json` lose
  their `panel` field, and the text list loses its `panel` column.
  **Migration:** delete the `panel` block from your configs; run `/review` as
  `/mission launch review`, `/pr-merge 2049` as `/mission launch pr-merge 2049`.
  A config takes text after its id only if a task reads `__panel_args__`
  (this replaces `panel.accepts_args`); text sent to a config that takes none
  is refused, not dropped. Radio's router now reads the first sentence of a
  config's `description` (else its `name`) where it read `panel.description`,
  so a config you want routable should lead with one plain sentence. Panel
  ids are the ones `mission launch` accepts (lowercase), so a config whose
  file name has an uppercase letter is not listed. **One stale user-tier
  mission config blocks every launch** (a leftover `panel` key is enough):
  `mission launch`, `/mission list` and radio all refuse with the same text
  until the file is fixed, and `darkmux doctor` names it. A value with spaces
  in the panel is written `name="two words"`.
- **Radio asks before it runs anything it chose.** The router can now pick any
  launchable config from free text, so `darkmux radio` prepares the launch's
  inputs first, prints the `darkmux mission launch <id> --param ...` command
  with every param that will run (for `review`, the `diff_file`, `workspace`
  and `head_sha` it makes from the current directory; the first two name
  temporary files, and `head_sha` is a commit hash), and asks `Run it? [y/N]` before running it. A repo with nothing to
  review is reported without asking. With no interactive terminal it prints
  the command, says it was not run and, when inputs were made from the
  current directory, that they are temporary and must be replaced with your
  own, then exits 1. An interrupt at the prompt ends it within a moment, runs nothing
  (even if a `y` follows) and removes the temporary files. A routed input
  holding a control character or an invisible formatting character (a bidi
  override, a zero-width space) is refused. The editor
  agent panel does the same for free text (no slash): the pick is shown in a
  code block in the panel's permission dialog, and only Allow runs it; Reject,
  cancel or no answer runs nothing and the panel says "not run". An explicit
  `/mission launch <id>` is your own command and is not asked again.
  **Migration:** a script that relied on radio running its pick unattended
  must run the printed command itself (for `review`, with its own
  `diff_file` and `workspace`); a panel user answers the dialog once per
  routed message.

- **Flag spellings that named the internal noun "session", or a misleading
  grain, are renamed** (A10 to A13). No aliases: each retired spelling exits 2
  naming its replacement.
  **Migration:** `dispatch --session-id` is `--name`;
  `flow note|catch|record|tier-decision --session-id`, `flow tail --session`
  and `memory correction list --session` are `--execution`, which takes the
  `exec-...` id of a role execution, the one `darkmux dispatch` now prints on
  its "execution id" line (it used to print the session id). A session id
  given to `--execution` is refused with exit 2, and a note recorded with
  `--execution` is stamped with that execution and its session; an execution
  the flow trail has no `dispatch.start` for is refused (the id encodes when
  it was minted, so only that UTC day and its neighbors are read, however old
  the id is);
  `lab eval --freeform|--agentic|--dialectic` is `--mode
  freeform|agentic|dialectic` (one choice, `strict` by default; the dialectic
  per-seat profile flags are refused with exit 2 under any other mode);
  `lab run --runs` and `lab tune --runs` are `--repeat` (`-n` is unchanged);
  `mission status --missions` is `--named`. The viewer's "try it yourself"
  `lab eval` line prints the new spelling. The `--no-wait` follow-up lines no
  longer print a `flow tail` command with an id.
- **The session no longer shows in three operator outputs.** The session is an
  internal join key; these now show the role execution or the run.
  **Migration:** `memory correction list` prints `[exec-...]` (or `[no
  execution recorded]` for a note written before executions carried an id), and
  its `--json` rows carry `execution_id` (a string, or `null`) in place of
  `session_id`; `flow tail`'s last column is the execution id, or the run id
  for a record outside any execution (`flow tail --json` still forwards the raw
  record, `session_id` included); `dispatch --no-wait` to another machine
  prints `run=<id>` in place of `session_id=<id>`, followed by `darkmux run
  list --kind dispatch` (the row id there is this value), and its "submitting
  to" line says `run=` too.
- **Mission state files are read in one spelling** (A18). A `mission.json`
  using `sprint_ids`, `closed_ts` or status `closed`, a task file using
  `sprint_id`, and a `sprints/` directory (the old name of `phases/`) are
  refused, naming the fix, instead of loading as if the field were absent.
  `darkmux doctor` fails each one in its "mission state files" row (which also
  reports the flat pre-#148 files). Flow archives still read a record's
  `sprint_id`. **Migration:** rename the key (`sprint_ids` to `phase_ids`,
  `closed_ts` to `finalized_ts`, `"closed"` to `"finalized"`, a task's
  `sprint_id` to `phase_id`) or rename `sprints/` to `phases/`.

- **`darkmux mission dispatch` and the hand-built mission verbs** (#2954).
  Missions now come only from mission configs. Removed with no alias:
  `mission dispatch`, `mission add-phase`, `mission start`,
  `mission pause`, `mission resume`, and `dispatch --phase-id`. Each now
  exits 2 with a line naming its replacement. `WorkJob.phase_id` is gone
  from the work-submission wire (WORK_JOB 7 to 8), so a v7 sender gets the
  version remedy. **Migration:** to run a role on another machine,
  `darkmux dispatch <role> "<message>" --profile <profile>@<machine>
  [--no-wait]`; to run a mission, write or edit its config and
  `darkmux mission launch <config>` (it starts the mission it creates);
  end it with `mission finalize <id>` or `mission abort <id>`. Growing a
  running mission by hand, and pausing one, have no replacement. Routing a
  mission's own steps to another machine comes back as a step-staffing
  feature, not as a verb.
- **The `paused` mission status and the `mission.pause`, `mission.resume` and
  `phase.added` flow actions** (#2954). Nothing writes them any more. A
  `mission.json` that says `"status": "paused"` still loads and reads as
  `active` (a leftover `paused_ts` is ignored), and the three actions read
  from an archive as retired. **Migration:** none; the mission board and
  `run list` no longer show a `paused` group.
- **One run noun: recorded lab runs are read through `darkmux run`, and live in
  `lab/`** (B4, B5). Contract 8 makes "run" the umbrella over mission, dispatch
  and lab runs, so a `runs/` directory holding only lab runs and a
  `lab run list|inspect|stats|compare` family beside `darkmux run list` were
  the umbrella's name on one kind. **Migration (CLI):** `darkmux lab run list`
  is `darkmux run list --kind lab`; `lab run inspect|stats|compare` are
  `darkmux run inspect|stats|compare`, with the same output and the same
  `--json` documents. The old spellings fail naming the replacement.
  `run inspect|stats|compare` read lab runs only and refuse a mission or
  dispatch run id, naming where to look. `darkmux lab run <workload>` is the
  launcher only; a workload named `list` still launches with the escape,
  `darkmux lab run -- list`. The
  workload/profile/verify table `lab run list` printed is gone with it: the
  `run list` rows carry kind, status, start, duration, tokens and id; a lab
  row's subtitle names the workload and its verify outcome (`verify pass`,
  `verify FAIL`, `verify —` for not checked). A bare run id now resolves under
  the lab dir only: a same-named directory in the cwd is no longer read (pass a
  path to read one).
  **Migration (disk):** the lab-run root default moved from
  `~/.darkmux/runs/` to `~/.darkmux/lab/`. darkmux does not move your data.
  While the old directory holds runs and the new one does not exist, `darkmux
  doctor` fails and prints the exact command, and every lab verb (`lab run`,
  `lab eval`, `lab loop`, `run list --kind lab`, `run inspect|stats|compare`)
  refuses, naming it: `mv ~/.darkmux/runs ~/.darkmux/lab` (`rmdir` the new dir
  first when it already exists and is empty, which the printed command does).
  `lab doctor` does not touch the lab dir and is not gated. `darkmux serve`
  still starts: it names the move in its startup banner and on `GET /lab/runs`
  (`pending_move`). If both hold runs, doctor warns and prints a merge that
  never overwrites. An explicit
  `DARKMUX_LAB_DIR` / `dirs.lab` is untouched.
- **The fleet page's orchestrator note** (#2983): the "Orchestrator note:"
  line under the token panel, its `history →` list, and the stock sentence
  it showed when no note existed. The panel is one line shorter; nothing
  else on the page changed size. **Migration:** the fleet page no longer shows an
  orchestrator note; `darkmux flow note --source orchestrator` is no longer
  rendered anywhere. The verb still writes the record, old note records in
  flow archives still read, and `--execution <id> --source adjudication`
  notes keep feeding coder briefs, `darkmux memory correction list`, and
  `mission debrief` unchanged.
  A non-note record tagged `--source orchestrator` (a `flow catch`, say) now
  files under its own action in the event log, not under "note".
- **`metrics.json`: the runtime no longer writes it, and nothing reads it.**
  Every count (turns, compactions, tokens, rests) is now a fold of the
  run's `trajectory.jsonl`, the one log the live tailer, `lab run stats`,
  `lab run inspect` and `lab loop` all read (the new `darkmux-trajectory`
  crate). The file was written only on a clean exit, so a killed run kept
  whichever run's copy was there before: in one measured archive, 37 of
  241 disagreed with their own trajectory, and every time the file was the
  wrong one. The checks and flags that existed only to catch that
  disagreement are gone with it: `RunChecks.tokens_reconcile`,
  `turns_match_trajectory`, `rest_matches_trajectory`, `metrics_stale`,
  `missing_required_events`, `checkpoint_parse_consistent` and
  `checkpoint_events_seen`, and the `STALE-METRICS`, `TOKENS`, `PARSE` and
  `COUNTS` flags (`RUN_STATS` 2.0.0). A lab run records no copy of the file
  (coding-task manifest v7, tool-bench v3). **Migration:** read totals from
  `darkmux lab run stats <run> --json` or the envelope's `metrics` block,
  never from `metrics.json`; an old run directory still reports its totals
  from its trajectory, and a leftover `metrics.json` in it is ignored.
- **The runtime's own totals output.** Its plain-text summary is now a
  `--- run ---` block (result, turns, compactions, tokens, rests, wall)
  read from the trajectory, and its `--json` envelope carries no `metrics`
  block: the host writes that block from the fold, so `darkmux dispatch
  --json` still has one. `metrics.this_run` and `metrics.total_messages`
  are gone: every figure in the block is this invocation's own. The
  same holds on `dispatch.complete`: for a resumed dispatch its `rest_ms`
  and `rests` are now this invocation's rests (they were the whole task's,
  seeded from the checkpoint), like its token counts. A checkpoint no
  longer carries the prompt-token and rest running totals, which nothing
  read; an older checkpoint that has them still resumes.
  **Migration:** read `metrics.prompt_tokens` (and the rest) where you
  read `metrics.this_run.*`; sum a task's rests over its runs'
  `dispatch.complete` records.
- **`dispatch.complete`'s `cumulative_prompt_tokens` /
  `cumulative_completion_tokens`** (FLOW_SCHEMA 2.0.0). Their one source
  was `metrics.json`. A usage record (`telemetry.tokens`) now omits a
  count the provider did not report rather than writing 0. **Migration:**
  a task's whole token total is the sum of its sessions' `telemetry.tokens`
  records. (`cumulative_turns`/`cumulative_compactions` went too; see
  "The flow record's leftover fields" below.)
- **`radio.router_profile`, `DARKMUX_RADIO_ROUTER_PROFILE`, and the
  `role_profiles.radio-router` binding** (#2914). The radio routing seat
  now runs on the machine's one utility model (below), so there is no
  profile to bind it to. Removed outright, no deprecation release, no
  compatibility read: `darkmux config set radio.router_profile` rejects the
  key, `config set role_profiles.radio-router` is refused with the fix, and
  `darkmux doctor` names whichever of the three is still set. CONFIG 1.28.
  **Migration:** delete `radio.router_profile` from `config.json` (and the
  `radio-router` entry from `role_profiles`, and the env var from your
  shell); a profile that existed only for the router (a 16K `radio`
  profile, typically) can be deleted. `radio.answerer_profile` and
  `role_profiles.radio-host` stay: answering the user is work. Radio and
  ACP routing now REQUIRE `internal.utility`: with no utility model
  registered, a message cannot be routed (the router returns a refusal
  naming the fix), where before it fell through to `default_profile`.
- **The compactor's window is no longer read from a profile's `models[]`**
  (#2914). It comes from `internal.utility` alone (below). A profile entry
  for the utility model is inert: `darkmux doctor` names each such profile
  with the window it declared and the binding to move it into.
- **`darkmux mission propose` and the `mission-compiler` role** (#2912).
  The verb dispatched a local utility model to turn pasted text into a
  Mission plus Phases, the pre-graph mission shape from before missions
  became task/step graphs, so even a perfect proposal could not be
  launched. Removed outright, no deprecation release, no compatibility
  read. **Migration:** write the mission config yourself (or have your
  orchestrator write it from the intent text) at
  `~/.darkmux/mission-configs/<id>.json`, then `darkmux mission launch
  <id>`; `darkmux mission config list`/`show` render the configs you can
  launch. A config's `ticket` key still sets the mission's ticket (the
  old `--ticket` flag went with the verb). A leftover
  `<DARKMUX_HOME>/roles/mission-compiler.json` (or `scribe.json`, below)
  still loads as a user role and shows in `darkmux role list`, though
  nothing dispatches it; delete it. `darkmux doctor` names any such file
  still present, and names any role whose `skills` list points at a skill
  that no longer exists (the old `mission-compiler` manifest names the
  deleted `mission-compiling` skill). The crew index skips that one link
  with a warning instead of failing, so `role list` and `role show` keep
  working.
- **`darkmux lab notebook draft` / `lab notebook list`, the `scribe`
  role, and the `DARKMUX_NOTEBOOK_DIR` / `dirs.notebook` setting**
  (#2913). Built-in notebook prose is not needed when a skill can do it
  with the orchestrator, and the data side now exists in a better form
  (`darkmux lab run stats <run> --json`). Removed outright; the
  `<root>/notebook` directory is no longer created by `init`.
  **Migration:** run `darkmux init` to install the bundled
  `darkmux-lab-notebook` skill, which drafts an entry from `lab run stats
  --json` (and the run's `manifest.json` when needed) and writes it
  wherever your own instructions say your notebook lives. Delete
  `dirs.notebook` from `config.json` (it is refused at preflight and failed by `darkmux doctor` until you do) and unset `DARKMUX_NOTEBOOK_DIR` (it only warns);
  neither is read any more, and `darkmux doctor` warns naming whichever
  is still set with the exact change to make. `darkmux config set
  dirs.notebook ...` now rejects the key. Existing entries on disk are
  untouched.
- **`darkmux mission migrate` and the pre-#148 flat mission layout.**
  Flat `<root>/missions/<id>.json` / `<root>/phases/<id>.json` files are
  not read; `darkmux doctor` FAILS naming each one still present.
  **Migration:** run `darkmux mission migrate --apply` on 3.x before
  upgrading.
- **The pre-Beat-33 `<root>/crew/{roles,missions,phases,crews,skills}`
  fallback read.** User state resolves under `<root>/<subdir>/` only;
  `darkmux doctor` now FAILS on a leftover `crew/` subdir and prints the
  move script. **Migration:** run the script `darkmux doctor` prints.
- **Dispatching with no resolvable profile no longer probes LMStudio's
  first loaded model.** With no `--profile`, no `role_profiles.<role>`
  binding and no `default_profile` (or a profile that selects no model for
  the role), the dispatch now fails with an error naming the fix, where 3.x
  printed a deprecation warning and ran against whatever was loaded.
  **Migration:** set `"default_profile"` in `profiles.json`.
- **`darkmux lab eval --k`, `--roster-profile`, `--exec-mode` and
  `--bundler`.** They configured the funnel mode deleted in #2310 P4d and
  were accepted and silently ignored since; `--k` claimed a value above 1
  was a loud error, and it was not. **Migration:** drop the flags; they
  never changed a run.
- **Doctor's "legacy compaction extras" check.** The openclaw-shape keys
  it warned about (`mode`, `maxHistoryShare`, `recentTurnsPreserve`,
  `customInstructions` under `runtime.compaction`) are now retired keys,
  refused by name like any unknown key (CONFIG 2.0, below). **Migration:**
  delete them (`custom_instructions` is the typed field).
- **Doctor's residue checks for pre-3.x removals:** the `crews` map in
  `profiles.json`, the `review{}` config block,
  `runtime.telemetry_record_every_samples`, and the "daemon predates the
  build field" verdict. Each key is now a retired key, refused by name
  (CONFIG 2.0, below). **Migration:** delete any of those keys still
  present (`darkmux doctor`'s `user file keys` rows name them).

- **A role's `escalation_posture`.** Nothing read it: the runtime treated
  `auto` and `pause` the same. A role manifest that still sets it is refused
  like any retired key. **Migration:** delete `escalation_posture` from your
  role manifests (`darkmux doctor`'s `user file keys` row names each file).

- **darkmux no longer reads the formats it retired** (#3036). Old data still
  loads and never panics; it reads as unknown.
  - **Flow archives.** A record spelled the pre-4.0 way (`dispatch start`,
    `step result`, `mission close`, `note`, `verdict: <v>`, `sprint *`, ...) or
    carrying an action darkmux retired outright (`telemetry.process`, `funnel.*`,
    `mission.run.*`, `crawl.*`, ...) reads as an unknown action, kept verbatim and
    counted by `darkmux doctor`'s unknown-action check. Nothing is rewritten on
    read: a retired `source` reads as `unknown`, a retired payload key keeps its
    old name, the pre-`sprint_id` field is not read, and a record of an execution
    that names none gets none (no `legacy:` id). A 3.x archive therefore still
    lists, but most of its records are unknown actions and the viewer shows little
    of it. The reader's `Retired` class, `darkmux_flow::legacy` and the golden
    archive pair are deleted. A stored finding record's old `dispatch` field and
    an old `compactor` seat in `telemetry.lms` also read as absent and unknown.
    **Migration:** none for new data; keep a 3.x archive only to look at it with a
    3.x build.
  - **Hook rules.** A rule written in a retired action spelling is no longer
    refused: it matches no action darkmux writes, so the hook sink loads, warns,
    and `darkmux doctor` warns `CANNOT MATCH`. **Migration:** write the dotted
    spelling (`dispatch.complete`, `phase.*`).
  - **Openclaw trajectories.** A run directory from the retired openclaw runtime
    loads and reads as zero turns, compactions and tokens. `darkmux run inspect`
    loses `--summary`, which only dumped that runtime's compaction summaries, and
    no longer prints a `tokensBefore` line.
  - **`mission.pause` and `mission.resume` are retired forever,** with the mission
    fields `paused` and `paused_ts`: the names are never reused, so an archive's
    no-op pauses cannot be mistaken for a real one. A future operator pause is a
    separate `hold` field with `mission.hold` / `mission.release` (#2996).

### The flow record's leftover fields (breaking, 4.0, FLOW 2.0.0)

- **`payload.runtime` is gone from every record.** It named the dispatch
  topology (`internal`, `direct`, `scheduler`) and nothing outside the
  viewer's run brief read it; the `--json` envelope's `metrics.runtime` for a
  hosted single-shot goes with it. **Migration:** a receiver that branched on
  it reads the record's `action` and `session_id`, or `payload.endpoint`.
- **`FlowRecord.source` is a closed set with one spelling** (`snake_case`):
  `crew_dispatch`, `scheduler`, `phase_lifecycle`, `mission_lifecycle`,
  `phase_review`, `mission_debrief`, `host_sampler`, `presence_reconciler`,
  `cmd_gate_audit`, `hook`, `host`, `detector`, `runtime`, `tokens`,
  `context`, `compaction`, `lms`, `thermal`, `battery`, `budget`, `utility`,
  and the four an operator writes. `host-sampler`, `presence-reconciler` and
  `cmd-gate-audit` were kebab-case; they are `host_sampler`,
  `presence_reconciler` and `cmd_gate_audit`. `darkmux flow
  note|catch|record|tier-decision --source` accepts `orchestrator`,
  `adjudication`, `manual` or `frontier` and refuses anything else
  (`frontier-orchestrator` is `frontier`). darkmux's readers map the retired
  spellings on read (`sprint_lifecycle`, `sprint_review`,
  `frontier-orchestrator`, the per-dispatch sampler's `process`) and read a
  source that maps nowhere as `unknown`. **Migration:** a receiver that
  filters on `source` uses the `snake_case` spellings.
- **`tier` says who acted, not where the model ran.** `local` (written on
  every record, hosted-endpoint executions included) is now `darkmux`; the
  values are `operator`, `frontier` and `darkmux`. `flow record --tier local`
  is refused by the CLI's value list. **Migration:** read `payload.endpoint`
  and `model` to learn where a call ran.
- **`host.peak_cpu_pct` / `host.peak_mem_pct` are gone** from
  `dispatch.complete`'s and the `--json` envelope's `host` block. They
  mirrored `host.cpu.peak_pct` / `host.mem.peak_pct` for one release
  (1.27.0) and that release ended long ago.
- **`FlowRecord.work_id` / `attempt` are gone.** The work queue they came
  from retired with #2916; an archive that carries them still reads.
- **`stage: "estimate"` is gone** (nothing ever wrote it); a record that
  carries it reads as `unknown`. `ship` stays: the hook sink's own records
  carry it.
- **`dispatch.complete`'s and the envelope's `cumulative_turns` /
  `cumulative_compactions` are gone.** Every count on the record is this
  invocation's own, and the whole-task count across resumes is no longer
  reported (nothing read it). **Migration:** a resumed run's `turns` counts
  only its own turns, and a hand-back resume continues the prior run's last
  turn, so do not sum `turns` across a resume: that turn is counted in both
  runs. Compaction counts do sum.
- **One unit for time in payloads: durations are `*_ms`, instants are
  `*_at_ms` (epoch milliseconds).** Renamed, with the value converted:
  `budget.wait` `wait_seconds` is `wait_ms` and its ISO `resume_at` is
  `resume_at_ms`; `utility.start` `stall_after_seconds` is `stall_after_ms`;
  `machine.rollup` `period_seconds` is `period_ms`; `dispatch.complete`'s
  `live` block `sampler_us` / `forward_us` are `sampler_ms` / `forward_ms`
  (fractional); `machine.battery_health` `total_operating_time_hours` /
  `time_at_soc_hours` are `total_operating_ms` / `time_at_soc_ms`, which
  `GET /machine/resources` serves under the same keys in its `load.battery_health`
  block. The config keys (`machine_rollup.period_seconds`, and the
  `bounds` block, which is keyed by the config knob a value came from) keep
  their names. darkmux's readers rename and convert an old record's keys on
  read.

### Typed flow payloads (4.0, FLOW 2.0.0)

- **Every flow record's payload is written from one Rust type per action, and
  the viewer reads it through that type's generated twin.** A record is built
  from a `Payload` variant, which fixes its action, and the write check refuses
  a payload that is not its action's, one that did not parse as its action's
  type, and a payload on an action that carries none (`session.end`,
  `machine.online`, `machine.offline`, `phase.*`, `step.complete`,
  `step.error`, `operator.note`, `operator.catch`, `stream.error`,
  `tier.decision`, `mission.debrief.prompt`, `phase.review.begin|aborted|dispatch|failed`).
  `mission.start`, `mission.close` and `mission.abort` keep an open JSON object
  (a mission config authors that outcome document). `ui/src/lib/flowPayloads.ts`
  is deleted; `FlowPayloads.ts` and one `<Action>Payload.ts` per type are
  generated. **On the wire, key order in a payload now follows its type's
  field order; no key is renamed and no value changes.**
- **An optional key with no value is omitted, not written as `null`,** in
  `dispatch.start`, `dispatch.complete`, `dispatch.error`, `dispatch.rest`,
  `step.result`, `telemetry.detector` and `budget.*` payloads (for example
  `reasoning_tokens`, `cached_tokens`, `stderr_excerpt`, `turn_delay_effective_ms`,
  `policy`, `tail_ratio`, and the token counts of a call that reported none).
  `telemetry.tokens` keeps `null` for "not reported", and so do the `--json`
  envelope's own keys; the exception is `dispatch --json`'s `detections[]`,
  which now omits an absent key like the flow record it shares a type with.
  **Migration:** a hook rule that matches `payload.<key>: null` keeps working:
  an expected `null` matches both a `null` value (archived records) and an
  absent key (current records). A receiver or `jq` filter that tests
  `.payload.<key> == null` still holds, since a missing key reads as `null`
  in `jq`; code that tests for the key's presence (`has("key")`, an
  `"key" in payload` check) must accept absence.
- **Archives still read.** A payload that does not parse as its action's type
  is kept as it was and never re-written. The retired review spelling `tokens`
  reads as `total_tokens` on `dispatch.complete` and `step.result`. A field
  an older version never wrote (`sampled_at_ms`, `tool_calls_so_far`, `result`,
  `turns_so_far`, `parent_model`, `reason`, `delivery_id`, `seat_class`, `source`),
  or wrote as `null`, reads as absent, never as zero. The role `compactor` reads
  as the utility seat, and a word in a closed set (a result class, a detector
  kind, a seat class) that this build does not name reads as `unknown` instead
  of dropping the record. A refused write of a flow record is now said once per
  action on stderr. A
  `dispatch.start` `bounds` block from before the newer knobs existed reads with
  the knobs it has.

### Fixed (4.0)

- **A phase stop ends only that phase's dispatch** (5.0). Abandoning a phase, or
  aborting a mission, while a run waited on its endpoint budget raised the
  process-wide interrupt flag, so a mission launch running other phases'
  dispatches had every one of them killed. The stop now lands on the one
  dispatch it names; Ctrl-C and SIGTERM still end everything.
- **A resume keeps its origin's image and has one live execution** (5.0).
  `--resume-from` with no `--image` runs on the image the original ran on; a
  different `--image` is refused as `RESUME IMAGE MISMATCH`. A resume while the
  original (or another resume) still runs is refused naming that execution, and
  one after the execution ended in success is refused as `RESUME ALREADY
  COMPLETED`; resuming again after an interrupted or failed run (any non-zero
  exit) still works. A run that stops at `max_turns` exits 0, so it counts as
  completed and cannot be resumed.
  The lock is `<out-dir>.execution.lock`, beside the out-dir like the resume
  origin record (opened without following symlinks, and only if this user owns
  it), and `doctor`'s orphan count covers it. Because a lock dies with the
  darkmux process, a resume also asks docker and is refused while the origin's
  recorded container is still running (including the container of an earlier
  resume, which is recorded on the origin too). If a phase stop's `docker kill` fails, the
  watchdog's retried kill takes over at once.
- **A hosted call the endpoint may have processed is charged, once, one way** (5.0).
  A timeout or dropped reply after the request was sent, an unreadable reply,
  and a 5xx write an `absent` usage record (the endpoint's window counts it as
  a call) and charge the per-dispatch cap what a reply with no usage is charged:
  the granted cap plus the estimated prompt. A failure before sending, a redirect (3xx), a 4xx
  (400, 401, 403) and a 429 charge nothing. `dispatch`, `dispatch.single_shot`
  and `dispatch.map` share the rule; the last two used to charge nothing on any
  error.
- **A fleet seat frees only the claim that took it** (5.0). A retried or
  repeated sender session shares one receiver session id; dropping one of two
  claims under it used to free both, so a cap of 2 could run 3. Each claim now
  holds its own ticket.
- **A peer cannot make another entry's learned name ambiguous** (5.0). A card
  stating a name another roster entry already holds as its learned name is no
  longer taken.
- **A job that names a hardware uid never falls back to the name** (5.0). A
  receiver that cannot read its own uid refuses it as misaddressed (421) with a
  reason, instead of matching on the machine name.
- **Busy and queued replies never carry an endpoint URL** (5.0). An inline
  endpoint is named "an inline endpoint" to a peer; a named one by its id.
- **`config set` (and `machine trust` / `untrust`) refuse a `config.json` written by a
  newer darkmux** instead of rewriting it with keys this binary cannot place.
- **A malformed `bounds` block no longer drops a whole `dispatch.start` record**
  (it costs the bounds, and a wrong-typed optional knob only itself), and a knob
  `value` of a type no darkmux wrote reads as unknown (`KnobValue::Unrecognized`)
  rather than as `null`, which means uncapped.
- **`meta.cut.fleet` is true only for a Redis stream that was trimmed or hit the
  read's `COUNT`**, not for a young stream whose first entry is simply newer than
  the day (it said a complete day was cut).
- **`profile list --machine <missing>` no longer prints a stray "machine card"
  line first** in a terminal; the line prints for a log or under `--verbose`.
- **A crash mid-append no longer breaks the audit chain.** Each audit line and
  its newline are one write; before an append, an incomplete last line is moved
  to `<day>.jsonl.torn-<ts>` beside the day file and the chain continues from
  the last complete line (a complete line missing only its newline is kept).
  `flow integrity-check` lists the sidecar as `torn_tails` and `darkmux doctor`
  warns naming it. Removal from the END of a day file is still not detected, and
  the audit docs now say so.
- **The hub backfill survives a restart.** The earliest unsent record time is
  kept in `<darkmux root>/state/hub-outage.json`; any process whose hub write
  fails records it, and the daemon backfills from it on start and after
  recovery, so a restart mid-outage and records written by one-shot CLI runs are
  re-sent. Delivery is at-least-once, de-duplicated on read.
- **Hook retry docs match the code.** A 5xx, 408, 429, or network failure retries
  without a cap (1s doubling to 60s); other 4xx give up after 3 and a redirect at
  once, each with `hook.failed`. The guide, config docs, and
  comments said "bounded retries" and implied `hook.failed` for a receiver that
  stays down.
- **A healthy daemon picks up a one-shot writer's outage on its next tick.** The
  daemon's hub catch-up thread (its own thread, on the presence reconciler's cadence)
  `stat`s the hub-outage watermark and, when it changed, backfills from it, so those
  records no longer wait for a daemon restart. The watermark's generation counter
  survives a clear, so a stale backfill cannot erase a newer outage.
- **A hook delivery's rejected total is updated before its `.last` status.** A
  reader that saw the status could see a total that did not yet include it.
- **The jq transform timeout cannot leak its orphan counter.** The worker thread
  owns the decrement, so a worker finishing in the timeout window no longer
  leaves the rule stuck `Busy`.

- **Selecting text on a card or row never clicks it.** A drag that selects
  text, or a double-click that selects a word, no longer drills the fleet
  machine card, a run row, an event row, or a mission step; a plain click
  still does.
- **The utility robot reads each machine's own card.** It is solid (and phosphor
  green) when the card says the utility model is resident, faded gray when it is
  not loaded, dashed gray when the card was not read or states nothing, and absent
  (its slot kept, so nothing shifts) when the card was read and registers none.
  A peer's robot no longer reads "unknown" when its card states residency, and
  reads the same from any serving machine.
- **A long mission no longer floods the fleet's Redis stream with heartbeats (#2101).** `dispatch.turn.heartbeat` (every two seconds per running execution) was most of a crawl's records, so one pass evicted every other machine's records from the capped stream many times over. The Redis sink, live and in the outage backfill, now skips it; the local day file keeps every heartbeat, and a peer's run stays live through its tool and turn records.
- **The local flow sink follows `DARKMUX_HOME` in test builds too (#2101).** The shipped binary already wrote flows under the resolved root; the test-build sink kept a scratch dir of its own that ignored it, so it was the one place the root did not scope a `dirs.*` default. It now resolves through `flows_dir()` like everything else, and a test pins that every dir default (flows, findings, mods, lab, hooks, liveness, caches, the fleet file) lands under a scratch `DARKMUX_HOME`.
- **A run's resume origin can no longer be forged by the model (#2972).** The record of a run's workspace path, mount mode and image used to sit in the out-dir the container mounts read-write, so a model could flip `workspace_read_only` and have a `:ro` run resumed read-write, or steer the resume hint. It now lives beside the out-dir (`<out-dir>.resume_origin.json`), where the container never mounts it, and the old in-out-dir file is never read. A checkpoint with no such record (every one written before 5.0) is refused for resume with a message to start the dispatch fresh. The record is written 0600 without following a planted symlink, read only when this user owns it, removed together with its out-dir, counted by `doctor` when its dir is gone, and a dispatch is refused if any mount (workspace, cache, attachments) would contain it.
- **A machine that renames itself keeps receiving work, with no edit on the machines that address it (#3028).** A roster entry now learns its peer's hardware uid and current `machine_id` from the peer's own card (the read `machine list` and the daemon already make, over the verified, pinned path), and `<profile>@<name>` resolves against an entry's id or that learned name. The work wire is 8.1: a job carries the target's optional `target_machine_uid`, and the receiver compares it to its own uid and accepts under any name, refusing a different uid as misaddressed without printing either. A sender that has not learned a uid, and any 8.0 sender, are checked by name as before. `roster identity` warns that an entry's id differs from its machine's own name and that addresses with either name work, and the fleet card is labeled with the machine's own current name. An 8.0 receiver refuses an 8.1 submission by naming both versions.
- **A daemon resumes publishing to the fleet hub on its own after an outage (#3023).** The Redis flow sink used to disable itself after three failures "for the rest of the process", so a hub restart left a long-lived `darkmux serve` silent until someone restarted it, and the other machines never saw what it did meanwhile. `darkmux serve` now probes the hub on a capped backoff (2s doubling to 60s, each probe bounded by the 500 ms connect timeout), re-enables when it answers, and re-sends its own records from the local day files (current and previous UTC day, in `ts` order, from the first record that failed) as stream entries marked `late`; readers already de-duplicate by record identity. CLI invocations keep the old behavior. `/health` carries `hub_link` (`connected`, or `unreachable` with since and reason; this machine only) and `darkmux doctor` has a `flow hub link` row.
- **A run on a machine that is not reporting reads unknown, not running (5.0).** A run recorded as running on a peer the fleet view holds as down, with no live session beat from it, has no live evidence and no terminal record can arrive, so the runs board's status chip and Status filter and the run page's pill and timing line say `unknown` (no pulse, no ticking clock) instead of `running`. A run that recorded its end is unaffected. A run quiet past the staleness window still reads "no ending recorded".
- **The machine page's hardware line is the machine's own, or "hardware not reported" (5.0).** A peer's page reads the card the fleet view just read (ahead of a presence beat's older string) and says "hardware not reported" instead of a blank header; this machine's page still reads its own probe.
- **The status line says what "last dispatch" covers (5.0).** Its tooltip names it as the newest dispatch start on any machine whose records reach this viewer, not only this machine.

- **A `dispatch.map` step whose every item failed is an errored step, and one with some failed items reads degraded.** The step used to complete regardless, so a mission where every item errored could finish Clean with exit 0. Now all items failing fails the step with the first item's error. Some items failing keeps the step `complete` (its output still reaches later steps) but marks the mission envelope `degraded`, with a warning naming the step and how many items failed. No shipped mission config uses `dispatch.map` today, so this affects your own configs.

- **The daemon's peer mission-graph proxy no longer sends the fleet token to a peer whose pin is not saved.** On first contact it verified the peer's node but never pinned it, and attached the token anyway. `fleet_get` and `fleet_post_json` now take only a `SettledTarget`, which exists only after the first-contact pin was written to the roster (or the target needed none: loopback, this machine, already pinned). The proxy pins on first contact like a work submission, and with an unwritable roster it sends nothing. The pin is written compare-and-set under the roster lock: a removed entry, an edited address, or a different node pinned meanwhile refuses instead of pinning the wrong node. `machine status <id>` and `machine resources <id>` go through the same single pin helper. A dispatch that stays on this machine under a `managed_only` boundary is now refused when its profile resolves to a hosted endpoint.
- **An interrupted lab run keeps its trajectory (#3014).** A run stopped with
  Ctrl-C or SIGTERM left its trajectory only in a temp directory, so
  `darkmux run stats` said "no trajectory events" for an hour of evidence. The
  lab harness now names the dispatch's out directory up front and copies the
  trajectory and findings into the run directory on every exit path, and the
  runtime catches SIGTERM and SIGINT and closes the trajectory with a
  `dispatch.complete` whose `result` is `interrupted` (a SIGKILLed run gets the
  same record from the host). `run stats` reports what completed, prints an
  interrupted caveat with the figures, and flags the run `INTERRUPTED`.
- **A compaction loop that never stops is now bounded (#3013).** When every
  compaction succeeds but the turn after each one re-reads exactly what the
  turn before it read, no occupancy counter grew and the run went on until
  `max_turns` (uncapped by default). After five such compactions in a row the
  run now escalates with `result: "escalation_compaction_reread_loop"`, the
  same graceful hand-off as `escalation_compaction_unproductive`. Only
  read-only tools count, and any turn that reads something else, edits, or
  runs a command ends the run of repeats.
- **A turn the runtime ended now counts toward the run's tokens and its cap.**
  A call cut by the degeneracy gate or a silent stream never receives the
  endpoint's usage, so its tokens were dropped from the totals and the
  cumulative cap never saw them, and the per-call budget line printed
  `<unknown>`. The runtime now counts what streamed past, records it as
  `completion_estimate` beside a null `usage` (an estimate, never a reported
  figure), and adds it to the cumulative cap. `run stats` and the dispatch
  envelope report `unreported_calls` and `estimated_completion_tokens`, so a
  total read beside them is known to be a floor.
- **A daemon started with `--port N` is found by every client on the machine (#3007).** `darkmux serve` now records where it actually bound (`<darkmux home>/run/daemon.json`: pid, host, port) and removes the record on clean shutdown. Doctor, the dispatch nudge, `machine list` and the viewer links resolve the daemon's address from a live record first, then env, config and the built-in 8765, and the `daemon reachable` row names which source it used. A record whose pid is gone is ignored. Before, a daemon on `--port 8766` read as "not reachable at 127.0.0.1:8765" and the `fleet token` row failed against it.
- **Doctor's `fleet token` row no longer fails for a shell without the token when the daemon has it.** `/health` now carries `fleet_token_set` (a boolean, this machine only, never the value); when the shell cannot resolve a token but the local daemon reports one, the row passes and says the token is set in the daemon's environment only.
- **Dispatching to a peer whose listener is off names the listener.** The error was `no answer from http://<host>:8766/fleet/work: ... Connection refused (os error 61)`. It now reads `the fleet listener at <url> is not accepting connections (off, or the daemon is down); nothing was sent`, the same sentence `/fleet/view` gives as the detail of its `listener_off` outcome (one classifier, `is_listener_off`).
- **A switched-off fleet listener says so.** `/health` reported `fleet_listener: null` when `fleet.listener.enabled` is false; it now reports `off` (`off (fleet.listener.enabled is false)` to this machine), and the startup banner says the listener is off instead of describing the token as if it were serving work.
- **A relayed run's id is greppable on the receiver.** The receiver's session id was `radio.solo.relay.<peer>.radio_2Esolo_2Eadhoc_...`, the sender's id with every dot escaped. A relay now ends with the sender's wire string verbatim (`radio.solo.relay.<peer>.radio.solo.adhoc....`), so the id the sender printed is a substring of the receiver's. Session ids written by an earlier 4.0 build no longer parse.
- **A dispatch running inside the daemon no longer probes for a daemon.** Relayed fleet work ran in-process under `darkmux serve --port 8766` and printed "darkmux serve isn't reachable on 127.0.0.1:8765". The nudge is now silent inside the daemon, and it suggests `brew services start darkmux` only for a Homebrew binary (otherwise `darkmux serve`). Doctor's `daemon reachable` row already follows `DARKMUX_SERVE_PORT` / `serve.port`; a `--port` flag exists only in the daemon's own process, so set one of those for doctor to find a non-default port.

- **The fleet lens reads `GET /fleet/view` for its machine list, hardware and
  liveness.** A peer with Redis off has no presence beat, and the lens drew it as
  "offline, hardware unknown" even when the daemon had read its card. Each card
  now comes from the view's row: a machine whose card was read is never shown
  offline, and one the daemon could not reach shows the typed reason on its
  subtitle line ("listener off"), never an address or a uid. A peer shows what it
  lets this machine do ("runs diff-review · radio-host here") beside its
  hardware, except on a phone. The static demo reads a committed snapshot of the
  view. Machines outside the view (an unverified source, one nobody rostered) are
  still drawn from flow and presence, and a replay draws every machine that way.
  The lens no longer calls `GET /machine/specs`.

- **`darkmux doctor`'s `crew/` merge script and its flat-mission check now
  agree.** The script moved pre-#148 flat mission files into `missions/`, where
  the `mission state files` check then refused them and told you to run a
  `mission migrate` that no longer exists. The script now keeps flat mission and
  phase files and `crew/sprints` in `<root>/archive/pre-148-missions/` (kept,
  `mv -n`, never deleted), and the check's remedy is the same move. The script
  is wrapped in `bash <<'DARKMUX_CREW_MERGE'` so it also runs when pasted into
  zsh, where an unmatched `.[!.]*` glob aborts, and doctor prints that block
  unwrapped at column zero, since word-wrapping split its long quoted paths.

- **`darkmux doctor` names why a `profiles.json` that exists does not load**,
  and all of it at once. The `profile registry` row prints the whole cause
  chain instead of "parsing JSON", and suggests `darkmux init` only when there
  is no file. The `user file keys: profiles.json` row now runs on the file
  itself, so a registry the typed load refuses still has every refused shape
  named in one run: a bare-string `internal.utility`, inline endpoint objects,
  and the retired `role` key on a model. The rows that depend on the registry
  say it did not load, with the cause, instead of "no profile registry".

### Added (4.0)

- **A fleet card shows only facts about its own machine; the console names the
  machine it runs on.** The grant line ("runs fast") is gone from the fleet
  cards: what a peer lets the serving machine do is a relationship, not a card
  fact, and the same fleet must read the same from any server. A line under the
  count says, in words, what the machine serves ("serves 3 profiles · radio").
  The machine card (schema 1.2) carries
  `serves_radio` (its `fleet.accept_work` grants `radio-host` to at least one
  peer) and `serves_profiles` (how many distinct work-class profiles of its
  registry the allow-list grants to at least one peer); neither names who, and a
  1.1 card states nothing and its serves line is empty. `/fleet/view`'s `accepts` is unchanged, for `machine list` and
  `profile list`. The console's command line now reads `<machine> $ darkmux ...`,
  naming the machine the daemon runs the command on.

- **The fleet machine card, read at a glance and the same size in every state.**
  The status dot is a lamp whose form says what the viewer knows: filled for
  proven work, a ring for proven quiet (idle), dashed for no reading (checking…,
  not streaming, online with no stream, disconnected), dim for offline; while a
  reading is on the line the lamp takes its color, as the run page's lamps do
  (one shared component). What the machine serves is a fixed line in words under
  the count, shown on a phone too. On a desktop the readout is centered on the
  tube's axis and the tube's center word is larger; on a phone the robot sits on
  the card's content edge. "not streaming" and the placeholder dash read dim,
  and the hardware line of a machine nothing has arrived from reads "hardware
  unknown (nothing received)".
  With two or more executions the count line reads "2 running · 1/2" and
  tapping the tube shows the next one; the pager row, which made that card
  taller, is gone.

- **`darkmux profile list --machine <peer>` and `--remote` list the profiles a
  peer lets this machine use.** `--machine <peer>` prints each profile the
  peer's allow-list grants this machine, with the models it runs and its
  dispatch address (`<profile>@<peer>`, the form `--profile` takes); `--remote`
  does the same for every roster peer, grouped by peer. Both read the peer's
  machine card, the one `machine list` shows. A peer whose card cannot be read
  says why and the verb exits 1: an unreadable peer is never reported as 0
  profiles. `--machine <this machine>` is the plain local list. With `--json`
  the verb prints a `peers` list whose entries are `listed` (with `profiles`) or
  `unreadable` (with `reason`); the shape is pinned in `tests/cli-json.golden`.
  The console gains a ninth panel, `profile-list`, for the same view, with a
  `--remote` toggle and a `--machine` token that offers the roster's names; a
  link such as `#lens=console&panel=profile-list&opt.machine=<peer>` opens it
  and runs it. Its `machine` option is the one panel option whose legal values
  are the roster's own names, checked by the daemon against the roster.

- **A run row carries `machine_uid` beside its display `machine`** (`/runs`, `run list --json`, additive and optional). It is the hardware uid of the machine whose records produced the row; a tracked mission or lab row reports this daemon's own uid. The viewer decides which machine a run belongs to by this uid, so a renamed machine, a `.local` alias or two machines sharing one display name no longer merge, split or misattribute runs. A record that carried no uid leaves the field absent.

- **A machine states its fleet role, and the hub hands out fleet defaults (#3022,
  CONFIG 2.1).** The machine card carries `fleet_mode` (`standalone`, `hub`,
  `peer`, from `fleet.mode`) and `hosts_fleet_redis` (whether the machine's own
  `redis.host` is loopback or itself); presence beats and `machine.telemetry`
  records carry `fleet_mode` too, so nobody has to ask a machine which one is
  the hub. The fleet card and the machine page show a HUB badge for it. The new
  `fleet.defaults` block, meaningful on the hub only, holds the first default,
  `fleet.defaults.radio.answerer_profile` (`<profile>@<machine>`; `config set`
  refuses a bare profile name). The hub serves it only in its machine card,
  never through Redis, and a card that does not declare `hub` has its defaults
  refused. Radio's answering seat resolves this machine's own setting first, then
  the hub's default, then the built-in, keeps the last copy it read under the
  darkmux home (used when the hub cannot be asked), and prints which one it used
  on every answer. `darkmux doctor` gains a `fleet hub` row (exactly one machine
  declares `hub`, and it hosts the Redis this machine points at) and a `fleet
  defaults` row (the resolved seat and its source). Machine card schema 1.0 and
  FLOW 2.0.0 are unreleased and were edited in place.

- **`run stats` counts the model calls that reported no usage** (`calls_unreported`, RunStats 2.1.0, `--json` too). A call that reports no usage adds 0 to the token figures, so a partly reported run read as a smaller run. Above zero, `completion_tokens` and `reasoning_tokens` are a lower bound, and the run's unreconciled list says so.

- **The fleet work wire is `major.minor` and grows by minors from here; the
  receiver enforces a data boundary; a check asks "would this route work"; every
  refusal carries a typed code** (`WORK_JOB_SCHEMA_VERSION` is `"8.0"`, the
  version this wire freezes at). A receiver takes the same major with a minor at
  or below its own, and refuses a newer minor, another major, or a schema that is
  not `major.minor` (a bare `"8"` included) naming both versions; unknown fields
  are still refused within a minor, so a new optional field ships as a minor and
  the sender leaves it out for a peer whose card says an older minor. New on the
  wire, all optional: the job's `boundary` (`managed_only`: the message may go
  only to a model the receiver serves itself) and `mode` (`run`, the default, or
  `check`); the reply's `refusal` (`token`, `identity`, `not_listed`,
  `role_not_allowed`, `profile_not_allowed`, `image_not_allowed`,
  `workspace_not_allowed`, `profile_undefined`, `boundary`, `busy`,
  `seat_changed`, `version`, `misaddressed`, `self`, `bad_config`,
  `bad_request`, or `unknown` for a code a newer darkmux sends), `check` and the
  `checked` status. The receiver checks `managed_only` against the profile the
  job resolves to, when it arrives and again when a queued job gets its seat, so
  a profile repointed at a hosted endpoint is never sent a private message
  because a card was stale. A `check` runs every gate a run meets (token,
  network identity, allow-list, role and profile scope, boundary, seat, version)
  and answers `checked` (with the profile, whether its model is `managed`, and
  whether the seat is `free` or the job `would_queue`) or the refusal a run
  would get, without running anything, taking a seat or a queue slot, or writing
  dispatch records. `darkmux_fleet::check_route` is its sender side. Radio's
  peer answering seat sends full grounding under `managed_only`; when the
  receiver's check says its profile is hosted, radio asks once more with the
  hosted-safe grounding and says so on stderr. **Migration:** run the same darkmux major on
  both machines; a 4.0 release candidate that still speaks `"8"` is refused
  with the version remedy. `tests/fixtures` in `crates/darkmux-fleet` pin the
  8.0 wire: a shape change without a version change fails a test.

- **Fleet awareness: one view every machine reads** (#3004). A machine's card, one
  typed `MachineCard`, travels between machines on ONE channel: its fleet listener,
  `GET /fleet/card`, the only surface that can say who is calling. It describes the
  machine truthfully: its identity and specs (the `/machine/specs` document), its
  profiles with each endpoint's kind (`managed`, `unmanaged`, `mixed` or
  `unresolved`, from the same target resolution dispatch uses) and models, its
  seats when its fleet listener runs, and its governor state (the OS thermal word,
  the battery reading and the battery gate the operator wrote; the gate's decision
  is `null` when no sampler ran). Every daemon serves `GET /fleet/view`, a
  `FleetView` with a row per roster machine and always a row for this machine,
  gathered in parallel, single-flight, cached 5 s (`cache_ttl_ms`), from each
  peer's listener over the verified peer path and never from Redis. A machine is
  visible to every fleet node that authenticates, whether or not it lets that node
  run work. `darkmux machine list` prints the view (this machine's own daemon's
  when one runs, else one gathered in process and marked `gathered_by:
  cli_process`, with this machine's seats, thermal state and battery not
  observed); `--json` prints the `FleetView`. The contracts, one line each:
  - `GET /machine/card` is retired: the daemon serves no card, and a machine reads
    its own card from its own row of `GET /fleet/view`. There is no daemon
    fallback: a peer whose listener is off is `unreachable` with reason
    `listener_off`.
  - `GET /fleet/card` authenticates (the fleet token, the sender's node, not this
    machine) and does not require an allow-list entry; it answers
    `ListenerCard { card, grant }` with `grant` one of `listed` (with the caller's
    own entry and no other), `not_listed` or `unknown`. A job still authorizes:
    `darkmux_fleet::admit` is `authenticate` then `authorize`.
  - A row carries `accepts` (`granted` with the entry, `not_listed`,
    `this_machine`, `withheld` or `unknown`) beside `card`, no longer inside it.
    `accepts` goes only to a reader on this machine (the fleet token is shared
    fleet-wide, so it does not open it); any other reader gets `withheld`. Each
    card's seat block goes to this machine or a token holder (the doctor panel's
    audience); any other reader gets no seats.
  - A row carries a resolved `machine_uid` and its `uid_source` (`card`,
    `declared` or `flow_history`), `received_at_ms` (this machine's clock) and
    `fetch_ms`. This machine is recognized by the verified node behind an entry's
    address, else by uid, else by name, so a roster alias folds into its row and
    is not dialed; `GET /fleet/roster` and the view read one roster.
  - Every roster peer is dialed: presence is display only, `liveness` is `live`,
    `no_beat` or `unknown` (`gone` and the `presence_gone` reason are removed).
  - `machine list` words each unverified-address remedy from the fleet crate's one
    `TargetFault::remedy`, the same text a refused send prints, and a 401 row says the
    peer did not accept this machine's token (missing or wrong), not that none was sent.
  - Gathering the view pins each peer's node in the roster on first contact, as a
    work submission does, before the fleet token is sent to it; a node that does
    not match its pin is never sent the token.
  - A card that cannot be read says why: `unavailable` carries `why`
    (`no_card_route`, `other_schema_major` or `unparseable`) and the peer's version
    with its source (`peer` or `presence`, the peer's own answer preferred).
    Unreachable reasons are `bad_address`, `dns_failed`, `identity_unavailable`,
    `not_on_overlay`, `pin_mismatch`, `pin_not_saved`, `listener_off`, `auth_required`,
    `refused_by_peer`, `listener_unavailable` and `bad_answer` (`connect_failed` is
    now `listener_off`).
  - Seats state facts only: `seats.local` is `[{model, held_by_peer_job}]`,
    `seats.hosted` is `{held_by_peer_jobs, cap}`, and `seats.counts_own_work` is
    `false`; `free_models` and `hosted.free` are removed, since the listener does
    not see this machine's own dispatches. `seats.busy_policy` is the wire enum
    `CardBusyPolicy` (`refuse`, `queue`, `unknown`).
  - Every enum on a card or a view, and `ReplyStatus` on the work wire, has an
    `unknown` arm: a value a newer darkmux invents reads as `unknown` and the rest
    of the card is shown, and `unknown` is never read as a known value. A card
    carries `cache_ttl_ms`, and its shape is tied to `CARD_SCHEMA_VERSION` (1.0) by
    a golden hash and committed 1.0 fixtures.
  - Cost: the serving machine caches its card 2 s and its identity lookups for
    card reads 10 s (a job never reads that cache), the per-address connection cap
    is 6 (was 3) so waited jobs cannot starve card reads, and the flow-history uid
    scan runs at most every 5 minutes.
  Both routes are in `route-table.golden`, and their types have generated twins.
  Work submission and card reads dial the listener over plain http whatever
  scheme the roster address wrote.
- **Radio and doctor check a fleet route before using it.** For a `<profile>@<machine>`
  answering seat, radio asks the receiver first (`darkmux_fleet::check_route`) and
  submits the job only when the receiver says it would run it, so a job the
  receiver would refuse is never sent. A refused or unreachable seat prints "the
  answering seat was unavailable" with the receiver's own reason, on the CLI and in
  the panel, and no longer prints the router's refusal text or the command
  catalog (a reply radio rejects keeps the old fallback). Radio's grounding gains
  a "fleet" section from the same `FleetView` `machine list` and
  `GET /fleet/view` give (machine, liveness, card outcome, loaded models, profiles
  with their endpoint kind, what this machine may run there; never a uid, node
  name, address or token), so "what is loaded on studio?" is answerable from any
  machine; a hosted seat does not get it. `darkmux doctor` gains a `fleet routes`
  row that runs the same check for `radio.answerer_profile` and every
  `role_profiles` address naming a peer: ok, or the receiver's typed refusal, and a
  warn (never a fail) when the peer cannot be asked.
- **A refused sender's `machine trust` remedy names an entry that works.** It named
  the sender's network host, so following it wrote a second allow-list key. It now
  names the receiver's roster entry for the verified node, else the machine id the
  job claims with `--node <host>`, and shows the role the job asked for.
  `WorkJob::for_peer` is removed (the receiver's version gate is the authority).

- **The machine lens battery shows when macOS is holding the charge.** A battery reading now
  carries a typed `state` (`charging`, `held`, `discharging`, `full`, `unknown`) that REPLACES the
  old `charging` boolean on the `machine.telemetry` and `machine.battery` payloads and on
  `load.now.battery` of `/machine/resources`. This is a break: readers use `state == "charging"`.
  `held` means observed: on AC, not charging, not full, current about zero. The configured limit
  percent is not readable, so none is reported. The lens marks the held level with an (i) that
  explains it. The graphic is larger and scales with its panel, and the power glyph is a line icon.

- **Radio's answering seat can run on a fleet peer.** Set `radio.answerer_profile`
  (or `role_profiles.radio-host`) to `<profile>@<machine>` and the seat's dispatch
  is submitted to that machine, which runs it on its own profile; the peer must
  trust the sender for the profile and the `radio-host` role (`darkmux machine
  trust <sender> --profiles <profile> --roles radio-host`). A peer seat is not
  read as a hosted endpoint (grounding is not withheld), its busy check is the
  peer's, and every refusal names the address. The peer builds radio's persona
  from its own `radio-host` prompt and runs one tool-less exchange (a new
  optional `single_shot` field on the fleet job, still version 8: humor,
  surface and token budget, never prompt text; only the `radio-host` role has
  it), under the smaller of the sender's `runtime.max_tokens_per_call` and its
  own. An address naming the machine you are on is a local seat, so a hosted
  profile written that way still withholds grounding. `config set` refuses a
  malformed address for either key, `doctor` no longer reports a `radio-host`
  address binding as an undefined profile, and a new `radio peer seat` check
  warns when the address names a machine that is not in the roster.

- **An optional, one-time upgrade skill for a 3.x home**,
  `docs/upgrade/darkmux-upgrade/SKILL.md`. An agent follows it to apply what
  `darkmux doctor` names: back the home up, then fix `config.json`,
  `profiles.json`, missions and the rest in order, never overwriting, keeping the
  user's own notes, and asking before any judgment call. It is not bundled and
  `darkmux init` does not install it; `darkmux doctor` links it when it finds
  retired keys, spellings or paths, and the getting-started guide describes it.

- **`darkmux mission show <id>`** and the panel's `/mission show <id>`: one
  mission in full, from one derivation. The config it was launched from and
  its declared inputs, every phase, task and step with status, tokens, turns
  and model, its runs, total tokens, and a viewer link. `--json` is a
  semver-bound shape (`MissionShow`: `id`, `status`, `description`, `config`,
  `graph` (the daemon's `/mission/:id/graph.json` value), `runs` (the `run
  list` rows for this mission), `tokens`, `link`). `mission status` stays the
  board. An input the launcher fills itself, `mission_id`, reports
  `required: false` in `config.inputs` (and is not marked required in the text
  listing), since no caller has to pass it. The link is the graph lens's
  `#mission=<id>` route.
- **`/mission list`, `/mission launch <config> [name=value ...]` and
  `/mission show <id>` in the editor panel**, replacing the per-config
  commands. Arguments after the config id map onto its declared inputs the way
  `--param` does (a `name=value` token naming a declared input is a param, the
  rest is the config's `__panel_args__` text). `/mission launch review` with
  no inputs still synthesizes the diff, workspace and `head_sha` from the
  editor's working directory, now triggered by a declared required
  `diff_file` input and skipped when you pass one.
- **Panel values have no escapes.** In `/mission launch <config> name="two words"`
  a backslash right before the closing quote is refused, naming the input,
  instead of being guessed at; use the other kind of quote around a value that
  holds one.

### Added

- **The run page's readout names the tool call running now and its file**
  (#2963). While darkmux runs a `read`, `write` or `edit`, the line under
  the scope's lamps reads the action and the file, e.g.
  `write · src/lib/tokenRate.ts`; a long path is trimmed from the left so
  the file name stays. The TOOLS icon names the same call. `write`'s icon is
  now a plain file (it replaces the whole file; the old "+" read as "adds
  lines"). FLOW 1.64.0: `dispatch.turn` carries `tool_names` and
  `tool_paths`, one entry per call that RUNS, in run order (a call the
  runtime refuses, ungranted, not a tool, or cut off mid-arguments, is not
  in them). `tool_paths` holds only the path argument, never file content.
  **One expected change:** records written before 1.64.0 carry no lists, so
  in older archives (and in the demo until it is regenerated) every call of
  a multi-call turn shows the neutral TOOLS state (the gear, no line)
  instead of the name of the call that had just finished. A runtime older
  than this release writes no plan for its calls, so its turns carry no
  lists either and read the same way.
- **Per-endpoint budgets** (#2902 step 5). An endpoint declared under
  `endpoints` in `profiles.json` (and named by id) can carry a rolling
  budget, `"limits": {"window": {"period": "1d", "tokens": 2000000}}`: the
  last 24 hours from now, counted from this machine's usage records, with
  no calendar reset (`tokens`, `calls`, or both; the period is `<n>m`,
  `<n>h` or `<n>d`; `0` is not a budget and is refused: set `policy` to
  `off` to turn one off, and under `off` the number is left alone; a 0
  edited in while a call waits is refused the same way, said once on the
  CLI, and the wait keeps the budget it had). `limits.policy` says what reaching it does: `warn`
  (the default once a budget is set: a warning on the CLI, a Warn-level
  `budget.warn` flow record, and the work keeps going), `wait` (calls to
  that endpoint pause until enough of the window has expired to have room,
  saying how long on the CLI, in a `budget.wait` record the run page shows
  and in `darkmux mission status`, then resume; the run is never stopped,
  no work is lost, and the wait extends the run's wall-clock bound), or
  `off` (nothing is counted). An optional `limits.warn_at` (e.g. `0.8`)
  warns once ahead of the limit; darkmux never picks one. Nothing is
  counted unless you set a budget: `profiles.example.json` ships the fields
  as `null`. An unregistered `policy`, or `limits` that cannot be read or
  whose `period` does not parse, refuses every `dispatch`, `mission launch`
  and `lab run` at preflight, naming the `profiles.json` path and the valid
  shape, and is a Fail in `darkmux doctor`, which also shows each budget's
  policy and the spend in its window. `darkmux mission abort` (or Ctrl-C)
  ends a wait without sending: the wait checks its mission's status on disk
  every half second, and an agentic-remote run the budget holds between
  turns is ended rather than released; either way an announced wait that
  ends this way is closed by a `budget.stop` record. The viewer reads a hosted call's wait as
  `REST budget · <endpoint>` until it resumes, on the run page and the
  fleet card, live and in playback, although the call has not started yet
  (a waiter silent a minute past its resume time has died and stops
  reading as live); a day window's wait counts down as `23h 53m`, and a
  long endpoint id is trimmed to fit the line. A misspelled
  key in `limits` (`windw`, `polcy`, `tokns`) is refused the same way,
  naming the nearest valid key. Budgets apply to
  calls darkmux sends to an endpoint it does not manage; `tokens_per_dispatch`
  and `concurrent_calls` are still shown and not enforced, and
  `remote.concurrent_cap` still applies. FLOW 1.65.0 (`endpoint_id` on usage
  records, the four `budget.*` actions: warn, wait, resume, stop).

- **`darkmux machine trust <name>` / `machine untrust <name>`** (#2916).
  Trust adds `fleet.accept_work.<name>` to THIS machine's config.json (and
  touches nothing else): the peer's node is looked up through the identity
  provider by the name the network reports (`--node`, else the host of the
  peer's roster address, else `<name>`) and its stable node id is stored,
  never typed; a machine's own node is refused, and the OS host name a node
  reports about itself never matches. `--profiles` sets the work-class
  profiles it may run here (refused if undefined or utility-only);
  `--roles` the roles it may dispatch (required, explicit, utility roles
  refused); `--images` the Docker images it may name (default: only
  darkmux's own runtime image); `--workspace true` lets its jobs mount any
  directory under this machine's worktrees base read-write. A submitted job
  never mounts this machine's shared toolchain cache. The confirmation shows
  the node's online state and owner. Untrust removes the entry.
  `--workspace true` is, in effect, letting that machine run code here
  (its jobs write into live worktrees your own git and test commands run
  in): grant it only to a machine you would give a shell.
- **Every request that carries the fleet token checks where it is going**
  (#2916): a `profile@machine` dispatch, `machine status`/`resources <id>`,
  `machine list` and the daemon's peer mission-graph proxy all go
  through one helper that resolves the roster address, requires the node
  there to be the tailnet node pinned for that entry (pinned by `machine
  add` or first contact), and connects to that verified address; only this
  machine's own daemon and loopback entries skip it. A peer that fails the
  check is shown as `unreachable` with the reason `unverified` in `machine list`. Everything a
  peer sends back is printed with control characters, bidirectional
  overrides and zero-width characters removed; a field shown in a table
  or on one line also loses newlines and tabs and is cut to its column,
  so a peer cannot print a forged row or warning. The token is never sent
  to a loopback address (this machine's own daemon does not need it; a
  loopback port can be held by another process). Machine names are
  case-insensitive everywhere and may not contain `-from-`. The listener reads
  the allow-list per request, so both take effect with no restart.
- **The fleet listener** (#2916): with `fleet.listener.enabled`,
  `darkmux serve` opens a second port bound only to the address the
  identity provider reports for this machine (never `0.0.0.0`, loopback or
  a LAN address, and not behind `tailscale serve`, which makes every peer
  arrive as loopback). One route, `POST /fleet/work`; every request on the
  port passes the token, then the network identity, then the allow-list.
  The token is checked before the provider runs. The listener serves at
  most 32 connections and 3 per peer address, allows 3 s for request
  headers, logs refusals at most 5 per peer address per minute (the rest
  counted), drops a submitted job's `phase_id`, and runs it under a relay of the
  sender's session in a standalone run. `/health` reports the listener's
  state (in full only to this machine). CONFIG 1.29 adds
  `fleet.identity{provider,bin}`, `fleet.listener{enabled,port}` and
  `fleet.accept_work`; env `DARKMUX_FLEET_LISTENER_ENABLED` /
  `DARKMUX_FLEET_LISTENER_PORT` (the identity provider and the allow-list
  have no env tier, on purpose).
- **`darkmux doctor` fleet rows** (#2916): `fleet token` (resolves or not),
  `fleet identity` (what the provider reports for this machine),
  `fleet listener` (bound or not, and where), `fleet trust` (each trusted
  machine by name with its scope, the node the provider reports for it now
  and whether it is online, and any profile in scope that cannot run here),
  and `retired work queue` when `darkmux:work` streams are still in Redis.
  No row prints a node id or the token.
- **A live channel for sub-second model state** (#2928). While an
  execution runs, its model state (generated and visible text, reasoning,
  a tool call being written, the next turn opening) reaches this machine's
  viewers every 250 ms, and every transition at once, so a short think
  burst between the 2 s heartbeats now shows as THINK instead of reading as
  generation; a state that begins and ends between two screen updates is
  still drawn for one frame. Utility jobs ride it too: a sub-second radio
  routing job lights the fleet card's utility glyph while it runs. Nothing
  is written anywhere: the samples go from the dispatch to the local
  `darkmux serve` over a unix socket and to its open viewers as SSE `live`
  events, never to a day file, Redis, the audit chain, playback or the
  runtime's trajectory, so the durable heartbeat stays at 2 s and no
  history grows. A slow or absent daemon never slows a dispatch. Local
  only: another machine's cards stay at 2 s. Lab benchmark runs don't feed
  it. The cadence is `runtime.live_sample_ms` (`0` off, clamped
  100..=1000); `darkmux doctor` shows it and names a stale socket or a
  daemon the dispatches can't reach (started with another `--port`, or
  another `DARKMUX_HOME`), and every `dispatch complete` carries the channel's own cost as
  `payload.live`. A replayed scope says on hover that it is drawn from 2 s
  heartbeats. No layout changes. FLOW schema 1.62.0, CONFIG 1.29.

- **Utility work is visible** (#2915). A utility job (compaction, radio
  routing) writes a lean `utility.start` record when it starts (the job, its
  model, the execution it serves, its own stall bound); its usage record, now
  carrying `job`, marks the end, and a failed routing call ends with
  `utility.error`. While an execution compacts, the PROMPT lamp stays lit, the
  scope reads "compacting" in a gray utility treatment and the status line
  counts "compacting · 12s", where it used to read "processing prompt" for the
  whole compaction. Each fleet card gets a utility strip at the end of its name
  row (a radio signal while routing, a squeeze while compacting, a generic
  pulse for any other job, stall color past the job's bound), and the machine
  page gets a Utility section (model, declared window, residency and
  footprint, the live job, each job's calls and tokens), replacing the
  `utility` badge on the model's residency row. Neither changes any box's
  size. FLOW schema 1.61.0.

- **Endpoints are declared once and named by id** (#2902 step 4).
  `profiles.json` gains a top-level `endpoints` map; each entry has a
  `url`, `managed` (`"lmstudio"`, or absent for an endpoint darkmux only
  sends requests to), `dialect` (`chat-completions`, the default for an
  unmanaged endpoint, or `chat-completions-max-tokens` for a server that
  only accepts `max_tokens`), `auth` (a Keychain item or env-var NAME, never
  the secret) and `limits` (`tokens_per_dispatch`, `concurrent_calls`, and a
  `window` with a `period` and `tokens`/`calls`). A profile model names one
  with `"endpoint": "<id>"`. **Limits are parsed, validated and shown by
  `darkmux doctor` but not enforced yet** (#2902 step 5); the `remote.*`
  knobs still apply. Inline `endpoint` objects keep working, and doctor's
  new `endpoints` check names the move to an id (as advice; it passes). An
  id that `endpoints` does not define is refused when used, never sent to
  LM Studio on a guess. The registry stays lenient: a value this darkmux
  does not know in `managed`, `dialect` or `limits` loads and is refused
  when used (and named by doctor); any other broken `endpoints` entry, or
  an `endpoints` value that is not an object, is quarantined (and then
  absent from the loaded registry, like a quarantined profile). A `"managed": "lmstudio"`
  endpoint that also declares a `url`, an `api_version` or another dialect
  is refused when used: its address is `lmstudio_url`.
  `profiles.example.json` (what `darkmux init` writes) uses the id form.
  PROFILES schema stays 2.0: the string form joins that unreleased major
  (a binary from before it quarantines a profile that uses it).
- **`darkmux doctor` checks the fleet roster against the fleet** (#2924).
  Two rows, shown when a roster exists. `roster addresses` flags an entry
  whose address reaches only the reading machine (an entry added with
  `--allow-loopback` is reported as intentional). `roster identity` checks
  each entry against the `machine_id` of the machine it describes, the one
  name flow records, presence beats and the viewer already use. It warns
  only on evidence: the entry's own hardware identity now goes by another
  name (both repairs offered, rename the entry or set that machine's
  `machine_id`, never the second when another machine already holds the
  name), or flow history links the name to exactly one machine that now
  goes by another name (a conservative repair: no address reuse and no
  `machine_id` change, since session-only names land in history too; the
  same conservative repair applies when a declared uid's current name is
  only known from history). A second entry for a machine that already has
  its own is a duplicate to remove. A name several machines have used, a
  name only this machine used (session names collect there), a name whose
  machine already has its own entry, or one nothing is known about (the
  normal state of a peer that is off) is a note, not a warning. The row
  says when presence could not be read and when this machine's
  `machine_id` comes from a `DARKMUX_MACHINE_ID` override, which is then
  not used to judge its own entry. Nothing in the roster or config is
  rewritten. Flow history is read only when this machine and presence
  cannot settle an entry, and only the last 120 flow files.
- **Roster entries keep fields the running binary does not know** (#2924).
  A `machine add` by an older binary used to drop a newer field (such as
  `loopback_intended`) or an operator's hand-added one; unknown entry
  fields are now written back unchanged.

- **`machine-status` is a built-in mission config** (#2918). "Which
  models are loaded on this machine right now?" was refused: the catalog
  radio's router (and the editor panel) route over had no machine command
  in it, so the question fell through to the answering seat. The read-only
  `darkmux machine status` verb now ships as a built-in mission config,
  listed like an operator's own configs, so the router routes to it and
  `/mission list` lists it; run it in the panel as
  `/mission launch machine-status`. Read-only only: `machine eject` stays
  un-advertised.

### Fixed

- **A request proxied to loopback no longer counts as this machine**
  (#2988). The daemon exempted any loopback connection from the bearer
  check, and `tailscale serve` (the documented way a hub reaches the
  tailnet) delivers every tailnet peer on loopback, so with a token set,
  remote reads were served without it. With read auth on, the gate now uses
  the same test `/health` already did: loopback AND no reverse-proxy header
  (`X-Forwarded-For`, `Forwarded`, `Tailscale-User-*` and the like).
- **Model output can no longer reach host files through symlinks** (#2869).
  Every host read or copy of a container-writable path (the out-dir,
  `.darkmux-runtime/`, the resume checkpoint, the live trajectory tailer,
  `mods.gate`'s scratch copy) now walks with no-follow at every component and
  accepts regular files only, with a size cap. A refused file is named once in
  a warning. `mods.gate` recreates a relative link only when it provably stays
  inside the checkout; other links are skipped and named. The live tailer reads
  in bounded chunks, so a huge sparse trajectory can't exhaust memory.
- **`darkmux flow status` no longer reports every `file` hook rule as URL
  REFUSED**, and doctor no longer contradicts itself on a rule that names both
  `http` and `file` (or neither). Doctor, `flow status` and the hook engine share
  one destination decision.
- **Doctor warns when a hook rule's deliveries keep giving up**, naming the last
  error in an indented, sanitized hint. Its remedy names the resolved
  `config.json` (honoring `DARKMUX_HOME`), and delivery-side flags point at
  `darkmux flow status` rather than the config.
- **The residency planner no longer evicts when nothing needs the room** (an
  unpriced or zero-sized load, or no surviving load). Latent today: no config
  sets a model-RAM budget yet (#2987).
- **The compaction window is the selected model's own** (#2902 step 3).
  With several models in a profile, a dispatch compacted at the profile's
  DEFAULT model's `n_ctx` even when capability selection picked another
  model. One resolver now returns the selected model with its own endpoint
  and window, and every path (dispatch, the container path, seat placement,
  radio's boundary and busy checks, `mission config show`, the crawl's
  provenance stamp, the lab's `coding-task` and `tool-bench` runs, whose
  profile-built compaction settings no longer carry the default model's
  window, and `tool-bench`'s scores `n_ctx`) goes through it. Single-model profiles, and every
  profile shape `profiles.example.json` and the guide ship, resolve to the
  same URL, model id, credential source and window as before (pinned by a
  table-driven test). Two edge shapes change: a step `config.endpoint`
  object with no `url` (`{}`) now runs on the managed LM Studio instead of
  posting to `lmstudio_url` without its `/v1` path, and a seat whose
  requested profile is quarantined is reported as unplaced instead of
  being placed on the default profile's model.
- **`darkmux lab run` with no `--profile` runs on the role's bound
  profile** (#2902). It used `default_profile` even when the role the
  workload dispatches as (the manifest's `role`, else the provider's own
  default) had a `role_profiles` binding, so the run's `profile=` stamp named a profile
  the operator had not bound. It now follows the same precedence
  `darkmux dispatch <role>` does; an explicit `--profile` still wins.
- **A stale local runtime image no longer shadows the one built for this
  darkmux** (#2923). A local `darkmux-runtime:latest` was used whenever it
  existed, so a weeks-old unlabeled build ran under a newer host and the
  dispatch died with `unknown flag: --session-id`. Dispatch now reads the
  image's `org.opencontainers.image.version` label (metadata only, nothing
  runs) and uses a local `:latest` only when it matches; otherwise it runs
  the version-pinned `ghcr.io/kstrat2001/darkmux-runtime:<version>`, pulling
  it if absent, and says which local image it skipped. If no matching image
  can be had, it refuses before starting a container, naming both versions
  and the rebuild command. An image with no label counts as a mismatch (a
  GHCR version tag can explain a mismatch, never vouch for a match). The
  check runs under `--skip-preflight` and the `skip_preflight` mission step
  key too, which now skip only the Docker daemon probe. The container runs
  by the checked image id, not the tag. A development build that falls back
  to the release image for its version number says so, with the command
  that builds a matching one. `darkmux doctor`'s `docker runtime` row
  reports an image dispatch would refuse as refused, not "will pull".
  **Behavior change for
  source builds:** a local runtime image must now be built with
  `docker build --build-arg DARKMUX_VERSION=<version> -f runtime/Dockerfile -t darkmux-runtime:latest .`
  (from the repo root) to be used. `--image darkmux-runtime:<any tag>` (e.g. `:4.0-rc`) is now
  treated as darkmux's own image: version checked, run directly, never
  injected. A BYO `--image` (#703) now extracts its injected runtime from
  the matching image too. `darkmux doctor`'s `runtime image freshness`
  warns on an unlabeled `:latest` (it used to pass) and lists other
  unlabeled local tags.

- **`darkmux doctor`'s `machine_id` row names the tier the value came from**
  (#2924). It printed `(from hostname)` whenever `DARKMUX_MACHINE_ID` was
  unset, so a `config.json` `machine_id` was labeled as the hostname. The
  row now reads `from DARKMUX_MACHINE_ID env`, `from config.json
  machine_id`, or `from hostname`, and the hostname hint names `darkmux
  config set machine_id`.
- **radio says the model is busy instead of queueing behind it** (#2917).
  One LM Studio instance serves one request at a time, and darkmux caps
  concurrency only within one process, so `darkmux radio` fired while a
  coder ran queued inside LM Studio, silently, until the 300s ceiling. The
  answering seat now checks the instance it would send to BEFORE sending:
  if `lms ps` reports it reading a prompt, generating a reply or computing
  embeddings, or another live darkmux process holds it loaded and in use
  (the residency-lease registry), radio answers at once that the model is
  busy and names the run when darkmux knows it (from the runs board), else
  the darkmux process holding it by pid, else says what it checked: no
  live run on it in the last day of darkmux's records, and no darkmux
  process it can verify holding it (a run live for longer than a day may
  not be named). Facts only, never a guess about whose work
  it is; a hosted seat is not checked. The check is made just before the
  send, so work that starts in between still queues the question. Same
  copy on the CLI (exit 1: no answer was given) and in the editor panel.
  The router still waits behind a compaction on the utility instance
  (#2914's decision), but after 10s both surfaces say what LM Studio
  reports: requests waiting on the utility model (`lms ps`'s `queued`,
  which excludes the request being served), busy with nothing waiting (the
  routing call itself), idle, or that darkmux cannot tell (an older `lms`
  with no queue count, the model not listed, or `lms ps` unreadable). When
  another darkmux process has the utility model loaded, it is named
  alongside that reading; it says the call may be sharing the model only
  when the queue count is unavailable. Then it keeps waiting to the
  ceiling.
- **A residency lease left by a crashed darkmux process no longer outlives
  its pid being reused** (#2917). Leases now carry their writer's process
  start time; a lease whose pid is alive but started at a different time
  (a reboot, then a reused pid) is swept like a dead one, and only a
  verified lease can make radio say a darkmux process is using a model.
  The start time is read in a way that works for a process owned by any
  user (on macOS, `sysctl`'s process record rather than `proc_pidinfo`,
  which answers nothing across users), so an orphan whose pid now belongs
  to launchd (pid 1) or a root daemon is swept too; the value and its unit
  are unchanged, so leases written by the previous build still compare.
  A lease that cannot be verified (no stamp, from an older build, or a
  start time the platform will not report) still keeps its model pinned
  but never backs a busy claim. The sweep deletes only the lease it
  judged stale: if a new process wrote its own lease for the reused pid in
  between, that lease is put back.

## [3.13.0] - 2026-09-25

### Added

- **The tok/s scope has one look per state** (#2890). One trace morphs
  between states instead of popping:
  - **GEN:** a wave whose speed follows the rate, with the rate in the tube's
    center.
  - **Thinking:** while the model reasons rather than writes visible text,
    the ring takes a rotating violet and pink shimmer, the rate's characters
    flow through the same colors, the card's rate line reads "think tok/s",
    and the run page's lit GEN lamp reads "think". Worked out from the
    heartbeat's two counters (all characters, visible characters), so it
    needs no runtime change and works on older recordings that carry both.
  - **PROMPT:** rings peel off the ring and sink toward a brain in the
    center.
  - **TOOLS:** a comet with the tool's icon in the center.
  - **REST:** a breathing ring with the countdown in the center.
  - **STALL** collapses like a CRT switching off; **no signal** shows static.
  - **Finished:** a slow echo of the run's average rate.
  - **Idle:** every online machine's card shows its tube breathing calmly,
    with "idle" in the center, until work starts; a machine that is off
    shows none.
  - The center crossfades between kinds of content, and reads the same on
    the fleet card and the run page.
- **"tool gen" instead of a false STALL** (#2889). LM Studio sends a tool
  call's name at once and its arguments only when complete; for a large edit
  that was seconds to minutes of silence, read as PROMPT or STALL. The runtime
  now keeps sending heartbeats while the call is generated, and the scope
  shows a wrench over "tool gen", with the seconds in the status line
  ("tool gen · 12 s"); when the tool runs, its own icon takes over. Each
  turn's opening heartbeat also records the request's size, which the fleet
  card's status line shows as an estimate ("processing ~36k"). A
  degenerate-output cut now closes the connection at once.
- **The gate's findings reach the run page** (#2887). The in-stream
  degeneracy gate's observations and aborts are now forwarded to the flow
  stream, so SIGNALS shows REPETITION where it used to read CLEAN, with the
  policy in force and whether the gate acted. Each dispatch start records the
  flow schema its host wrote.
- **Fleet cards page through concurrent runs** (#2881). A machine running two
  or more executions shows a `‹ 1/N ›` pager; the arrows never move and the
  card never grows.
- **pepper-grinder is a built-in workload.** `darkmux lab run pepper-grinder`
  runs the refresh-token QA review from the Genesis series against the
  published fixture (github.com/kstrat2001/pepper-grinder): clone it,
  `darkmux lab fixture register <path>`, then run.

### Changed

- **Fleet cards fill the row three across** on desktop, with the tube
  stacked between the card's header and its status, sized from the card's
  width; on a phone it sits beside the text. The tube's text and icons scale
  with it. Long machine names and hardware lines truncate with the full text
  as a tooltip.
- **Replays open on the whole recording.** RECENT ACTIVITY offers "all",
  the recording's own span, and uses it by default in a replay; a preset
  still rolls with the playhead.
- **The playback clock shows seconds** and stays on the transport's row on a
  phone; the transport controls are borderless.
- **The run page's MODEL section** leads with the scope, the metrics beside
  it in a grid.

### Removed

- **The `long-agentic` built-in workload.** Its fixture contract was one only
  a private codebase satisfied, so no one could run it; use
  `pepper-grinder`.

### Fixed

- **Tok/s on turns with a reasoning check-in** (#2886): the live rate and the
  finished average no longer read far too low; a short turn carries the last
  rate instead of showing a flat GEN (#2885); a lost connection reads "no
  signal", not STALL.
- **The fleet pager counts executions only**: a review run recorded before
  3.7 no longer shows its whole-run record as an extra execution.
- **The event pane no longer sticks at "0 events · N hidden"**: when a
  second batch of records arrived before the restored filters rendered, the
  restored picks were overwritten and never came back.
- **`darkmux lab doctor` no longer suggests `rm -rf` on a fixture's `.git`**
  (#2888); a published fixture is a git clone.
- **Mutation testing no longer fails a test-only change** in files with
  brace or quote character literals (#2892).
- **A private codebase's source paths are gone** from docs, tests and
  fixtures (#2883).

### Schema notes

- **`FLOW_SCHEMA_VERSION` 1.55.0 → 1.56.0, additive:**
  `dispatch.turn.heartbeat` gains `phase`, `tool_name` and `prompt_chars`;
  `telemetry.detector` gains `kind: "repetition"` records with `policy` /
  `acted`; `dispatch.checkpoint` gains `policy` / `would_conclude`;
  `dispatch start` gains `flow_schema`.
- **Readers:** older readers ignore the new fields.

## [3.12.0] - 2026-09-24

### Added

- **A live token-rate scope** (#2877, #2879). A small CRT oscilloscope shows
  how fast the model is generating:
  - **Fleet cards:** while a machine generates, the card shows a tube and its
    rate beside the status line.
  - **Run page:** a TOK/S tile (full width on a phone) holds the tube with the
    rate in the middle. A finished run shows its average: billed output tokens
    over generation time, not over the wall clock.
  - **State lamps:** GEN, PROMPT, TOOLS, REST (with a countdown) and STALL sit
    under the tube, grey when off, one lit. The trace takes the lit lamp's
    color.
  - **How the rate is measured:** characters generated between heartbeats,
    including reasoning and tool-call arguments, converted with the run's own
    characters per token from finished turns. It is labeled an estimate.
- **Motion on live pages** (#2878). Arriving events slide in, meters ease
  between readings, and numbers count up. The fleet total counts up only on
  new work, not when old records leave its 24-hour window.
- **Loading placeholders** (#2862). A page draws its real layout at once and
  only the values still loading shimmer, instead of a bare "loading…" line.

### Changed

- **Playback defaults to real time** (1s/s), so a replay shows motion at the
  pace a live viewer saw it. The speed button steps 1s/s, 5s/s, 30s/s, 1m/s,
  10m/s, 1h/s.
- **A replay renders what the live page showed at the same moment.** The run
  page, fleet cards and activity timeline are derived from the records up to
  the playhead, with one clock (the playhead, or now when live). The timeline
  in a replay is the live rolling window anchored at the playhead. A scrub
  never animates; playing forward does.
- **A rest is recorded when it starts**, carrying its planned length, so a
  live page can show the rest while it happens. The host's inactivity
  deadline now includes the rest's own length.

### Fixed

- **Mission run pages** (#2759): the MODEL tiles never rolled up the mission's
  inner executions on a real run (a spelling mismatch), host tiles vanished
  once they did, and each loaded model was listed twice. A mission's page now
  stays live while any of its executions or the mission itself is running,
  and lands on COMPLETE when it ends.
- **A live page judged "now" by the newest record** rather than the clock, so
  a stall only showed once another record arrived.

### Schema notes

- **`FLOW_SCHEMA_VERSION` 1.54.0 → 1.55.0, additive:**
  `dispatch.turn.heartbeat` gains `sampled_at_ms` and `generated_chars`
  (content, reasoning and tool-call arguments).
- **Session presence** beats gain an optional `mission_id`, omitted when
  absent.
- **Readers:** older readers ignore the new fields.

## [3.11.0] - 2026-09-24

### Added

- **`darkmux lab run stats`: derived metrics for recorded lab runs** (#2855,
  #2859, #2870). One run, a set, or a set against `--baseline`:
  - **Per run:** active time next to rest time, tokens per second over the
    streams that were actually billed, what both degeneracy gates did, and
    busy-only power and energy.
  - **Per set:** ranges, not means, plus **cost per successful run**: every
    run's cost divided by the runs that passed. A failed run still used the
    GPU, so this is the figure that says what a working result costs.
  - **Checks, not trust:** each figure comes with the reconciliation check
    that says whether it may be quoted. Run flags name what is off:
    `STALE-METRICS` (the run holds another run's `metrics.json`), `OVERLAP`
    (runs that overlap in time would share host energy), `UNBILLED` (an
    aborted stream reported no usage), and `UNGATED` (the run predates the
    pass rule below).
  - **Bounded read:** the flow read is bounded by the run's own time window,
    so it does not scan the whole archive.
  - **JSON:** the `--json` shape is versioned at 1.1.0.
- **Battery on the live machine lens** (#2821, #2873). A battery gauge (bolt
  while charging, plug while on AC and full) sits beside condition, max
  charge, original capacity, cycles, temperature and operating time.
  Condition matches what macOS reports. The key darkmux used to read said
  "Check Battery" on a healthy pack.
- **A readable run page** (#2863, #2864, #2871):
  - **Sections:** MODEL, SYSTEM and SIGNALS, each with one header style.
  - **Turn headers in the events pane:** a header for every turn, showing
    model time, prompt tokens in, output and thinking tokens out, and a
    context bar with the compaction threshold marked. A turn that never
    finished says so.
  - **Rest cards:** each kind of rest gets its own SYSTEM card (thermal rest,
    turn delay, battery pause, operator hold) with total time and count. A
    card shows whenever that protection was configured for the run, even
    with zero rests.
  - **Resizable events column** on desktop.

### Changed

- **A write-the-tests lab run passes only when it did the work** (#2833,
  #2867). Previously the fixture's suite was green untouched, so a run that
  did nothing passed. When a fixture declares its baseline test count, a pass
  now requires all of the following:
  - the sandbox changed;
  - more *passing* tests than the baseline (skipped and todo do not count);
  - the suite is green and the test script is unchanged;
  - any coverage threshold the workload declares is met.

  The verdict names the rule a run failed. The evidence is in
  `manifest.verify.work_gate` (run manifest `schema_version` 6).
- **Meters show how tight a resource is**, not just how full: every compact
  dial uses the same green → amber → red ramp as the big memory gauge.
- **The events list no longer floods with periodic telemetry.** When no model
  activity is in the window, the fallback shows lifecycle events, never host
  samples, heartbeats or battery health records. A window of only telemetry
  says so, with a one-tap way to show it.

### Removed

- **The lab "series" view** (#2872). It was a port of the retired
  review-bench view: for coding-task runs, every field except the run id was
  empty or wrong, and every run read RUNNING. The runs list shows lab status
  correctly, and `lab run stats --baseline` covers comparison.

### Fixed

- **A finished lab run opened a page stuck on RUNNING** (#2860, #2861). It
  now opens the shared run view, and the list reports how it ended.
- **An event row could show a different command than the one that ran.** A
  `cd` the model wrote was stripped, and a `command` key inside a written
  file's content could be shown as the command. Multi-line and chained
  commands now carry a marker the ellipsis cannot hide, and bidi and
  zero-width characters render as visible escapes.
- **Battery health was recorded every hour.** Temperature and running totals
  counted as changes. A record is now written only when cycles, capacity
  (beyond 1% of original) or condition move.
- **The event detail pane** rendered epoch `_ms` fields as durations, and
  lists of objects as `[object Object]`.

### Schema notes

- **`FLOW_SCHEMA_VERSION` 1.52.0 → 1.54.0, additive:**
  - `dispatch.turn` gains `generation_ms`, the turn's model time.
  - `dispatch start` bounds record `thermal_pacing_enabled`,
    `battery_pause_enabled` and `battery_pause_floor_pct`.
- **Readers:** older readers ignore the new fields.

## [3.10.0] - 2026-09-21

### Added

- **Per-detector policy for the degeneracy detector** (#2846, #2856). The
  repeated-output detector had exactly one behavior: detect and act. There was
  no way to run a dispatch with it measuring but not intervening, which made it
  impossible to measure what acting on a finding actually costs or buys.
  `runtime.detection.degeneracy.policy` now takes three values:

  | policy | behavior |
  |---|---|
  | `enforce` | detect and act. The shipped default, unchanged. |
  | `observe` | detect and RECORD, never act. |
  | `off` | do not measure at all. |

  `observe` keeps every other variable fixed: the check-in cadence, the per-call
  token limit, and therefore the usable prompt budget are all identical to
  `enforce`. Only whether the verdict is obeyed changes. Previously the only way
  to quiet the detector was to raise the per-call token limit until it stopped
  being reached, which also shrinks the prompt budget, because the endpoint
  requires prompt plus `max_tokens` to fit the context window. That is a second
  variable, and it made the results of any such comparison unattributable.

  A policy enum rather than a boolean because the runtime carries four detectors
  (degeneracy, tool-call cycles, repeated reasoning, consecutive tool failures)
  and on/off cannot express the state that is most useful for diagnosing one of
  them: keep measuring, stop acting. Only `degeneracy` reads a key today; the
  other three get none until they do.

  Both gates honor the policy, not just one. The checkpoint gate judges at the
  per-call limit and the stream gate judges mid-call and can end the call
  client-side; a policy that reached only the first would still let the second
  cut generation short.

  Records carry the counterfactual: `dispatch.checkpoint` gains `policy` and
  `would_conclude`, `dispatch.gate.observation` keeps `degenerate: true` on a
  suppressed finding, and `dispatch start.bounds` stamps the resolved policy
  with provenance. An `observe` run therefore reports how many turns WOULD have
  been cut while letting them run, and every run is self-describing about the
  regime it executed under.

  Set it with `darkmux config set runtime.detection.degeneracy.policy observe`,
  or per-shell with `DARKMUX_RUNTIME_DETECTION_DEGENERACY_POLICY`. An
  unrecognized value resolves to `enforce`, the armed direction, and `darkmux
  doctor` warns rather than leaving the typo silent.

- `CONFIG_SCHEMA_VERSION` 1.26 -> 1.27 (additive). An older binary ignores the
  new `runtime.detection` block into `extras` and behaves exactly as its default
  does. `FLOW_SCHEMA_VERSION` unchanged at 1.52.0; `RULES_SCHEMA_VERSION`
  unchanged at 3.0.0.

[3.13.0]: https://github.com/kstrat2001/darkmux/releases/tag/v3.13.0
[3.12.0]: https://github.com/kstrat2001/darkmux/releases/tag/v3.12.0
[3.11.0]: https://github.com/kstrat2001/darkmux/releases/tag/v3.11.0
[3.10.0]: https://github.com/kstrat2001/darkmux/releases/tag/v3.10.0

## [3.9.0] - 2026-09-20

Schema contracts: `FLOW_SCHEMA` 1.51.0 to 1.52.0, an additive minor bump for one
new detector kind. `CONFIG_SCHEMA` is unchanged at 1.26 and `RULES_SCHEMA` at
3.0.0. No breaking changes to any payload, so an older peer on the fleet stream
degrades to ignoring the new record rather than rejecting it.

### Fixed

- **The check-in no longer destroys the tool call it was watching**
  ([#2836](https://github.com/kstrat2001/darkmux/issues/2836)).
  The reasoning checkpoint sent `max_tokens` at the check-in interval, so the
  endpoint stopped generating mid-`arguments`. The truncated JSON would not
  parse and was dropped with no record, so a tool call the model had genuinely
  made simply vanished from the run. Measured on a long agentic fixture: **9
  destroyed tool calls across 64% of check-in firings, now 0**, confirmed on
  two different local primaries.

  The runtime already streams, so detecting a degenerate turn needs visibility,
  not truncation. The check-in now observes the stream and judges it in place.
  Degeneracy is measured as a tail repetition ratio over 12-word windows,
  calibrated against a real degenerate run at 1.000 and synthetic loops at
  0.013 to 0.015.

- **A discarded tool call is now recorded rather than silently dropped**
  ([#2836](https://github.com/kstrat2001/darkmux/issues/2836)).
  A new `telemetry.detector` kind, `discarded_tool_call`, carries the tool name,
  the argument length, and `cut` -- which names *who* ended the call,
  `server_length` when the endpoint stopped generating and `runtime_abort:*`
  when darkmux did. Both eras appear in the same append-only archive, so a
  reader can tell them apart without inferring it from a version number. Every
  degeneracy observation is now recorded, not only the ones that end a turn
  ([#2846](https://github.com/kstrat2001/darkmux/issues/2846)).

- **A long tool call is no longer cut by the transport**
  ([#2836](https://github.com/kstrat2001/darkmux/issues/2836)).
  The per-call ceiling rose from 10,000 to 32,000 tokens and is no longer used
  as a stand-in for watching the stream. The read timeout was raised to 900s
  with a 30s connect timeout, and a stream that goes silent is now a typed
  condition rather than an opaque error.

- **The degeneracy judge was passing verdicts over zero characters**
  ([#2836](https://github.com/kstrat2001/darkmux/issues/2836)).
  For reasoners that emit reasoning in a separate field, the carried text was
  empty after the thought closed, so the judge scored nothing and returned a
  verdict anyway. It now falls back to the other field, and what each judge
  actually looked at is recorded.

- **The fleet hero no longer claims a local engine is cloud**
  ([#2834](https://github.com/kstrat2001/darkmux/issues/2834), [#1607](https://github.com/kstrat2001/darkmux/issues/1607)).
  The local/cloud/unattributed split keyed on whether an endpoint had a URL,
  which a local inference server on 127.0.0.1 also has, so work running at zero
  marginal cost on the operator's own GPU was counted as metered. Whether an
  endpoint costs money is a property the operator knows and darkmux does not, so
  the split is withdrawn rather than patched: one figure, every token darkmux
  dispatched. The unattributable tokens are still counted in it.
  [#1521](https://github.com/kstrat2001/darkmux/issues/1521) tracks per-endpoint
  attribution with metering declared rather than guessed from a URL.

- **Token tiles no longer round hundreds away**
  ([#2845](https://github.com/kstrat2001/darkmux/issues/2845)).
  Figures in the thousands carry two decimals, so a few hundred tokens are
  visible instead of disappearing into a bare `k`.

- **The savings hero no longer leaks its figures through the loading skeleton**
  ([#2830](https://github.com/kstrat2001/darkmux/issues/2830)).

- **The release-mode gate passes again**
  ([#2828](https://github.com/kstrat2001/darkmux/issues/2828)).
  `release-verify.yml` is the only workflow that builds and tests in the release
  profile, and it had never completed successfully. Two tests asserted a
  `debug_assert!` that compiles out under `--release`; they now assert the debug
  panic and the release no-panic separately, and the invariant that holds in
  both. This is also the release build's first real coverage of that production
  path.

### Added

- **`lab tune` reports the block total**
  ([#2848](https://github.com/kstrat2001/darkmux/issues/2848)).
  A multi-run campaign asks how long the whole set took, and the report did not
  answer it -- the sum was computed to derive the mean and then dropped, and
  `mean * n` drifts from the truth by integer division. The total is the sum of
  the runs' own durations, so in-run rest stays inside it: an engine that
  throttles on the last run shows up as a larger total rather than having the
  pause subtracted away.

### Known limitation

- **The degeneracy detector's threshold is calibrated on one engine's output
  distribution** ([#2846](https://github.com/kstrat2001/darkmux/issues/2846)).
  `DEGENERATE_TAIL_RATIO` was derived from runs on a single local primary. An
  engine whose reasoning is more repetitive by nature can cross that threshold
  on healthy output and escalate a run that would have converged. The detection
  itself is sound -- it measures what it claims to measure -- but the threshold
  is not yet engine-independent, and the remedy on crossing it is blunter than
  it should be. If you run a primary other than the one darkmux was tuned
  against and see unexpected escalations, that issue is the place to say so.

### Packaging

- **The tap is `kstrat2001/tap`**
  ([#2840](https://github.com/kstrat2001/darkmux/pull/2840)), so the install
  command reads `brew install kstrat2001/tap/darkmux`.
- **A tap serving the wrong tag is now detected**
  ([#2825](https://github.com/kstrat2001/darkmux/issues/2825)).
  `scripts/verify-tap-pin.py --tag vX.Y.Z` fetches the tap's published formula
  and the tag's real tarball and compares them. v3.8.0 was tagged and released
  while the tap kept serving v3.7.1, with every other signal green. The
  auto-sync workflow was removed with the tap rename, so the tap is now synced
  by hand and this check is what proves it landed.

[3.9.0]: https://github.com/kstrat2001/darkmux/releases/tag/v3.9.0

## [3.8.0] - 2026-09-19

Schema contracts: `FLOW_SCHEMA` 1.42.0 to 1.51.0, `CONFIG_SCHEMA` 1.22 to 1.26.
Both are additive minor bumps, lenient on read. `RULES_SCHEMA` is unchanged at
3.0.0. No breaking changes to any payload, so an older peer on the fleet stream
degrades to ignoring fields it does not know rather than rejecting records.

### Added

- **Serve address block** ([#2765](https://github.com/kstrat2001/darkmux/issues/2765)).
  `serve.port` and `serve.bind` are visible fields in `config.json`, resolved as
  `env > config > default`. `darkmux doctor` prints the resolved value with its
  provenance. A non-default daemon port no longer lives only in an invocation flag.
- **Battery sampling and a run gate** ([#2705](https://github.com/kstrat2001/darkmux/issues/2705), [#2706](https://github.com/kstrat2001/darkmux/issues/2706)).
  Charge rides the host sample beside CPU and thermal. Health (cycle count,
  capacity against design, condition, temperature) is polled hourly and recorded
  on change. A dispatch can be gated on an operator-set charge floor, and
  `darkmux doctor` reports the values. Tracked internally and readable at
  `/machine/resources` under `load.battery_health`. A viewer surface follows in a
  later patch release ([#2821](https://github.com/kstrat2001/darkmux/issues/2821)).
- **Doctor reports over-readable state files** ([#2452](https://github.com/kstrat2001/darkmux/issues/2452)).
  Any darkmux state file readable by group or world is named in the report.
- **Host-source facade and scenario library** ([#2779](https://github.com/kstrat2001/darkmux/issues/2779)).
  Thermal and power inputs come through a seam that fixtures can drive, so ladder
  tiers that cannot be elicited safely on real hardware are still testable.
- **Machine rollup on the fleet lens** ([#2775](https://github.com/kstrat2001/darkmux/issues/2775)).
- **Mutation gate across `crates/`** on the PR diff ([#2499](https://github.com/kstrat2001/darkmux/issues/2499)).

### Enhanced

- **Thermal escalation ladder**, tiers 1 through 4 ([#2774](https://github.com/kstrat2001/darkmux/issues/2774)).
  Sustained thermal or power pressure now escalates through defined tiers instead
  of being reported and ignored.
- **Compaction is bounded by the compactor's own context window** ([#2808](https://github.com/kstrat2001/darkmux/issues/2808)),
  and the pre-send bound is measured against the endpoint's own token count rather
  than a character estimate ([#2792](https://github.com/kstrat2001/darkmux/issues/2792)).
  Verified on an 88 minute dispatch: 0 of 76 requests over window, 66 compactions,
  run completed.
- **Local dispatches address the darkmux-namespaced instance on the wire**
  ([#2240](https://github.com/kstrat2001/darkmux/issues/2240), [#2539](https://github.com/kstrat2001/darkmux/issues/2539), [#2575](https://github.com/kstrat2001/darkmux/issues/2575)).
  Under co-residency a bare model key could resolve to a user-loaded copy with
  unknown load config. The dispatch now names the instance it loaded.
- **Signal guards on nine dispatching verbs** ([#2463](https://github.com/kstrat2001/darkmux/issues/2463), [#2467](https://github.com/kstrat2001/darkmux/issues/2467)).
  Container children are reaped when the host takes a signal.
- **One run-status vocabulary** across CLI, viewer and records ([#2813](https://github.com/kstrat2001/darkmux/issues/2813)).
  The viewer no longer derives its own status labels from booleans.

### Fixed

273 issues closed in this release. The full list, filterable and permanent:
[closed 2026-09-07 to 2026-09-19](https://github.com/kstrat2001/darkmux/issues?q=is%3Aissue+is%3Aclosed+closed%3A2026-09-07..2026-09-19).

The areas where the fixes change day to day use:

**Seeing work that is actually running.** The runs board hid in-flight lab runs
entirely ([#2812](https://github.com/kstrat2001/darkmux/issues/2812)). A task admitted to a wave but not yet dispatched read
running instead of waiting ([#2557](https://github.com/kstrat2001/darkmux/issues/2557)). A finalized mission that abandoned every
phase read Complete ([#1564](https://github.com/kstrat2001/darkmux/issues/1564)). A running phase whose dispatch session was dead
went unflagged ([#2682](https://github.com/kstrat2001/darkmux/issues/2682)). A peer machine's mission never appeared on the CLI
board at all ([#2652](https://github.com/kstrat2001/darkmux/issues/2652)).

**Long runs finishing.** Compaction could run every turn without ever getting
below its own trigger ([#2793](https://github.com/kstrat2001/darkmux/issues/2793)), and a structured compaction that did not shrink
the middle it replaced is now refused ([#2798](https://github.com/kstrat2001/darkmux/issues/2798)). A bash tool call leaving a
process holding its pipe is bounded ([#2215](https://github.com/kstrat2001/darkmux/issues/2215)). `--timeout` now actually bounds
the container dispatch path ([#2547](https://github.com/kstrat2001/darkmux/issues/2547)).

**The fleet showing one card per machine.** The roster joins live cards by
hardware uid rather than name ([#2768](https://github.com/kstrat2001/darkmux/issues/2768)), a machine's own name titles its card
([#2806](https://github.com/kstrat2001/darkmux/issues/2806)), and a rostered but silent machine renders offline rather than
vanishing ([#1855](https://github.com/kstrat2001/darkmux/issues/1855)).

**Being told the truth about state.** `doctor` stopped reporting problems on a
correct install and stopped printing an unverified destructive remedy
([#2149](https://github.com/kstrat2001/darkmux/issues/2149), [#1715](https://github.com/kstrat2001/darkmux/issues/1715)). The presence headline is caveated and a silent stream is no
longer called live ([#2683](https://github.com/kstrat2001/darkmux/issues/2683)). Savings verdicts key on a run rather than a
recurring session id ([#2690](https://github.com/kstrat2001/darkmux/issues/2690)), and gauges stopped claiming a scope they did not
have ([#2559](https://github.com/kstrat2001/darkmux/issues/2559)).

**Nothing left behind on exit.** Dispatch children are reaped on a signal to the
host ([#2463](https://github.com/kstrat2001/darkmux/issues/2463)), orphaned grandchildren are reaped via `--init` ([#2481](https://github.com/kstrat2001/darkmux/issues/2481)), and a
signal-interrupted run archives as interrupted rather than as a failure
([#2555](https://github.com/kstrat2001/darkmux/issues/2555)).

**The viewer on a phone.** The mission graph stacks vertically in portrait and
re-fits on rotation ([#2376](https://github.com/kstrat2001/darkmux/issues/2376)), the sticky tab row covers the safe-area band
([#2760](https://github.com/kstrat2001/darkmux/issues/2760), [#2764](https://github.com/kstrat2001/darkmux/issues/2764)), and a viewer failure is visible and diagnosable rather than a
black screen ([#1709](https://github.com/kstrat2001/darkmux/issues/1709)).

The remainder is internal hardening: test isolation, CI mutation coverage, and
state-permission tightening. Those change no behavior you drive directly.

[3.8.0]: https://github.com/kstrat2001/darkmux/releases/tag/v3.8.0

## [3.7.1] - 2026-09-07

### Fixed

- **The desktop events header stops fighting the count pill** ([#2447](https://github.com/kstrat2001/darkmux/issues/2447)) —
  on every desktop screen by default, the "50 of 1100 events · 6033 hidden"
  chip overflowed the events header's right edge and squeezed "events last
  24h" into three stacked lines. #2108 had moved the chip inside the
  follow/filters button group so the phone could have a one-row toolbar;
  that group is `flex: none` and the chip is `white-space: nowrap` at
  ~250px inside a ~380px column, so it could neither shrink nor wrap. The
  chip is now placed by viewport — inside the group on a phone, a wrappable
  child of the header on desktop, where it shares line 1 with the buttons
  when it fits and drops to its own line when it doesn't. The phone layout
  is unchanged, measured identical at 390 and 320. Both failures are pinned
  in a real browser now (`tests/e2e/event-log-desktop-header.spec.js`), the
  desktop twin of the phone's existing chip-wrap spec, since a CSS-only
  layout regression is invisible to every unit test.

[3.7.1]: https://github.com/kstrat2001/darkmux/releases/tag/v3.7.1

## [3.7.0] - 2026-09-06

A review that lands as conversations, and a run that says what it cost.

This release finishes the review path 3.6.0 started. `review` is no longer a
funnel with ten bespoke step kinds: it is the generic mission path every other
pipeline runs on (plan → review → summarize → create-mods → deliver), and
its output arrives as inline PR conversations, one per finding, with a one-click
suggestion whenever a mod passed its gate. The observability half caught up in
the same batch: one host sampler per machine instead of one per dispatch, a
`records_emitted` block in every mission envelope, and a viewer that defaults to
showing model activity rather than every record the stream carries. Verified on
a full attended run of the pipeline against a real PR: 5/5 phases, 1,914 flow
records, 656 machine-scoped host samples, one sampler owner, twelve concurrent
units at peak.

### Migration — the review funnel is gone

Operators who copied the old funnel-based `review.json` into
`~/.darkmux/mission-configs/` under another name (the guide's own
`review-lean` example among them, or a hand-rolled `review-standard` /
`review-deep` / `review-mixed` variant) now hold a config naming step kinds
that no longer exist (`review.probe`, `review.judge`, …) — `darkmux doctor`
flags it; delete the copy and re-derive your variant from the new
`review.json` if you still want one. An operator with
`role_profiles.<review-probe*|review-judge|review-verify>` bound in
`config.json`, or dispatching one of those roles directly, gets a doctor
warning naming the unknown role — delete or repoint the binding.
`profiles.example.json`'s review-pipeline profile descriptions were rewritten
for the two roles `review` actually dispatches (`reviewer`, `coder`) — diff
it against your own copy if you keep one ([#2433](https://github.com/kstrat2001/darkmux/pull/2433)).

### Added

- **The mission envelope carries a `records_emitted` block** ([#2421](https://github.com/kstrat2001/darkmux/issues/2421), [#2426](https://github.com/kstrat2001/darkmux/pull/2426)) —
  `MISSION_ENVELOPE_SCHEMA` 1.2 → 1.3. At finalize, the envelope records this
  mission's own flow-stream cost: counts by `action`, total records/bytes,
  aggregate dispatch seconds (paired dispatch-bookend segments plus open
  ones credited to finalize time — `dispatch_pairs`/`open_dispatches` say
  which), wall-clock seconds, and machine-scoped host-telemetry samples
  ([#2413](https://github.com/kstrat2001/darkmux/issues/2413)) inside that window. `darkmux mission debrief <id>` renders it —
  top actions, totals, dispatch/wall time, host-sample coverage. A miss (no
  records found) still yields an honest all-zero block plus a named
  `warnings` entry, never a silent gap; the launch's own liveness wrapper
  bookend (open for the whole `launch()` call) is excluded from dispatch
  pairing so it can't inflate `dispatch_seconds` by a whole mission's wall
  time. Old envelopes (no `records_emitted` key) still deserialize.

- **Every review finding renders as an inline PR conversation** ([#2429](https://github.com/kstrat2001/darkmux/issues/2429), [#2431](https://github.com/kstrat2001/darkmux/pull/2431)) —
  a `path`+`line` finding becomes a `comments[]` entry: a one-click
  suggestion when a gate-passed mod exists, a plain claim plus its rule id
  otherwise. An unanchored finding — no line, or a line outside the PR's
  diff — rolls up into a per-rule count in the body instead of forcing text
  onto a line GitHub would reject the whole review over. The body is
  summary-only now (coverage line, one heading per rule that still has
  something to say); the old `? Candidates: <evidence>` suffix is gone.
  `darkmux-review.yml` re-checks the PR's head sha at post time and appends
  a disclaimer if it moved since the run started.

- **The create-mod seat is a hook, not a dispatch** ([#2310](https://github.com/kstrat2001/darkmux/issues/2310), [#2393](https://github.com/kstrat2001/darkmux/pull/2393)) —
  `review`'s create-mod task waits (bounded by a new `mod_wait_seconds`
  input, default `0` — don't wait) for a mod naming the finding's key,
  instead of dispatching a local coder seat: live testing found local
  models detect problems well but write applying patches badly. The new
  `darkmux-mod-create` skill (shipped, embedded) reads a finding by key from
  the hook outbox and writes the mod via `darkmux mod create --for <key>`
  from a watching frontier session. **Optional cloud-endpoint seat for the
  unattended path** ([#2395](https://github.com/kstrat2001/darkmux/pull/2395)) —
  a disabled-by-default `create-mod-dispatch` template lets a self-hosted
  runner (no orchestrator session to answer the hook) staff create-mod with
  an endpoint profile instead; `TaskConfig.excludes` (mission config schema
  3.5) lets the two templates for one seat declare each other so only one
  is ever enabled at once, with `validate` refusing an enabled conflicting
  pair.

- **Bounded, interruptible step commands** ([#2361](https://github.com/kstrat2001/darkmux/issues/2361), [#2372](https://github.com/kstrat2001/darkmux/pull/2372)) —
  `mods.gate`'s `test_command` and `procedural.shell` now run in their own
  process group under a deadline with interrupt polling (SIGKILL to the
  group on expiry or on launch cancellation), configurable via
  `runtime.step_command_timeout_seconds` / `DARKMUX_STEP_COMMAND_TIMEOUT_SECONDS`
  (default 600s) — `CONFIG_SCHEMA_VERSION` 1.19 → 1.20, written visibly by
  `init`. A step that hits the bound records why instead of hanging the
  launch; a command left holding an unfinished stdout drain has its process
  group killed rather than leaking ([#2375](https://github.com/kstrat2001/darkmux/pull/2375)).

- **Dispatch-free steps get their own concurrency track** ([#2394](https://github.com/kstrat2001/darkmux/issues/2394), [#2397](https://github.com/kstrat2001/darkmux/pull/2397)) —
  every `StepKind` now declares a `SeatClaim` (`LocalModel` /
  `RemoteEndpoint` / `NoModel` / an unresolved seat with a reason) instead of
  defaulting to a residency guess. A step claiming no model
  (`procedural.shell`, `mods.gate`, `records.gather`,
  `deliver.github_review`) now runs on its own bounded track
  (`runtime.dispatch_free_concurrency` / `DARKMUX_DISPATCH_FREE_CONCURRENCY`,
  default 8 — `CONFIG_SCHEMA_VERSION` 1.20 → 1.21) instead of serializing
  behind the remote-endpoint cap a mission launch sets to 1. Six shell waits
  that used to run one at a time now run together.

### Changed

- Machine lens trimmed: the reload suggestion renders once per model, the gauge's caption replaces a summary line that restated its numbers, only lit status lamps render, the pricing methodology and the three tile notes live behind `how this was measured`, and the thermal bar no longer repeats its severity as text. Phone height 2,566 → 1,899 px with no fact removed ([#2440](https://github.com/kstrat2001/darkmux/issues/2440), [#2442](https://github.com/kstrat2001/darkmux/pull/2442)).
- The review workflow's header and security note describe the shipped pipeline (`plan.sites` → `crawl.unit` → `crawl.summary` → create-mods → `deliver.github_review`) and what the runner actually does with the reviewed tree: an anonymous read-only clone at `head_sha`, never executed ([#2438](https://github.com/kstrat2001/darkmux/pull/2438)).
- **`review` is now the former `review-v2` pipeline** ([#2310](https://github.com/kstrat2001/darkmux/issues/2310) P4d) —
  the generic launch path (plan → review → summarize → create-mods →
  deliver), built on the shared mission building blocks. The `review-v2`
  config id is gone; `review` is the only `review` now.
- Fork-PR heads resolve via a one-shot miss-recovery fetch (an explicit
  `git fetch` argument, not a standing refspec); the mirror's own fetch
  refspec stays heads+tags only, so an ordinary fetch never re-downloads
  the whole pull-heads namespace.
- `CONFIG_SCHEMA_VERSION` 1.21 → 1.22: the `review{}` config block
  (`judge_concurrency`, `judge_fail_on_any_skip`) is removed. Old keys are
  read leniently; `darkmux doctor` warns on them.
- `mode`/`envelope_out` are accepted on `review` for CLI-surface parity
  with the retired funnel launcher but IGNORED, with a warning when
  supplied.
- **One host sampler per machine** ([#2413](https://github.com/kstrat2001/darkmux/issues/2413), [#2419](https://github.com/kstrat2001/darkmux/pull/2419)). `telemetry.process`
  (the per-dispatch, 2s-cadence CPU/mem/gpu emitter) is RETIRED — FLOW
  1.42.0 — and `machine.telemetry` becomes machine-scoped: exactly one
  process per machine (the daemon, or a live dispatch when no daemon
  runs, arbitrated by a cross-process singleton lock) emits it, at the
  operator's configured `runtime.host_sampler_interval_ms` cadence. A
  run's SYSTEM pane now joins to the machine-scoped curve by
  `machine_uid` + a time window instead of by session. Same-process
  parallel dispatches (a crawl's sibling units) no longer all believe
  they own the sampler. A run with no samples in its window (the join
  comes up empty) now renders an explicit "no host samples for this run"
  tile instead of silently omitting the CPU/RAM/GPU tiles.
- `CONFIG_SCHEMA_VERSION` 1.22 also drops `runtime.telemetry_record_every_samples`
  — the per-dispatch downsample knob it configured has nothing left to
  configure. Read leniently; `darkmux doctor` warns on it.
- **Findings and mods render by the rule, not by how they were confirmed**
  ([#2398](https://github.com/kstrat2001/darkmux/pull/2398)) — entries in a
  posted review are grouped under the rule's own title and tagged with its
  id, the way a lint rule name is the durable handle for re-running the
  check. A gate-passed patch always renders as a one-click suggestion
  regardless of whether the rule's own confirm form is a mod, a search, or
  a question; the form only decides the rendering when nothing passed.
- **The header's LIVE badge folds into the playback pill**
  ([#2412](https://github.com/kstrat2001/darkmux/issues/2412), [#2420](https://github.com/kstrat2001/darkmux/pull/2420)) —
  one transport control instead of two. The pill's own dot pulses on a
  connected live stream, holds still and turns red with a "reconnecting"
  title when the stream drops, and switches to a `▣` glyph plus the mission
  id or date in playback. The standalone badge (`#modebadge`) is gone, and
  "today" is gone from the header entirely — the fleet view's 24h window
  can run into yesterday, so it was sometimes wrong as well as redundant.
- **Event filters default to model activity, not everything** ([#2416](https://github.com/kstrat2001/darkmux/issues/2416), [#2417](https://github.com/kstrat2001/darkmux/pull/2417)) —
  a fresh session now shows reasoning / checkpoints / tool calls / turns /
  dispatch errors by default; heartbeat and the telemetry curves start
  hidden. Picks are stored under one global key — an unchecked value stays
  unchecked everywhere, including after it briefly disappears from the
  offered facets and comes back — and the Filters button / pane chip now
  count actual hidden records instead of "1 per facet with anything
  hidden."
- **The filter panel is sectioned, and the sections toggle** ([#2444](https://github.com/kstrat2001/darkmux/pull/2444)) —
  activity facets group under MODEL / DISPATCH / MISSION / MACHINE / OTHER,
  each with a tri-state header checkbox that turns its whole section on or off,
  and the search field sits at the top on its own line instead of sharing a row
  with the count. The one-off "model only" and "clear all" buttons are gone: a
  header toggle does both jobs and names which group it is doing them to.
- **The PR-review parity suite (`next-parity-live`) now runs in CI**
  ([#2422](https://github.com/kstrat2001/darkmux/pull/2422)) — it was the
  one suite the parity loop skipped, which is how a red test (broken by the
  event-filter default above) reached `main`. Contributors touching the
  viewer's live/SSE path should expect this suite to run on their PR.

### Fixed

- The phone drawer's event list no longer scrolls sideways: one unbreakable token in a row's preview was widening the row past the panel, and the list scrolls on both axes; rows now wrap anywhere and an e2e pins no horizontal overflow at 390 and 320 px. The Events toolbar is two rows: the search field, then the clock and a filter icon button (active count as a badge, 44 px targets) with the count text on the same line ([#2441](https://github.com/kstrat2001/darkmux/pull/2441)).
- A finding whose mod exists but was never gated (no `test_command`, or the gate failed) is named in the delivered review with the skip reason and a `darkmux mod show <key>` pointer instead of rendering as if nobody proposed a change; a runtime-written mod whose kit is not a unified diff records a warning saying so ([#2438](https://github.com/kstrat2001/darkmux/pull/2438)).
- Folds and scans fail loud: `records.gather` names failed steps of a kind it does not recognize and lists inputs it could not read, an undeclared `--param` gets a did-you-mean, dispatch bookend literals in the daemon go through the shared flow helpers with a tripwire, error-shaped activities (`step error`, `phase abandon`) are on by default in the event filters, and `darkmux doctor` warns about a hook rule that has never matched because its `match.action` uses the other bookend spelling ([#2437](https://github.com/kstrat2001/darkmux/pull/2437)).
- **The playback scrubber spans the open run, not the whole loaded day**
  ([#2346](https://github.com/kstrat2001/darkmux/issues/2346), [#2347](https://github.com/kstrat2001/darkmux/pull/2347)) —
  a run that ended mid-day used to leave the scrubber's range pinned at the
  day's last record; a dispatch or mission focus now bookends the range on
  the run's own start/terminal, and rewind lands on the run's start.
- **`flows`/`audit` dirs default under the resolved root**
  ([#2359](https://github.com/kstrat2001/darkmux/issues/2359), [#2363](https://github.com/kstrat2001/darkmux/pull/2363)) —
  a `DARKMUX_HOME` install no longer writes flow records or audit files
  into the real `~/.darkmux` behind the operator's back; both now derive
  from the same resolved root the findings/mods/lab/hooks dirs already use.
- **`materialize` checks the mirror it finds and serializes concurrent
  callers** ([#2399](https://github.com/kstrat2001/darkmux/issues/2399), [#2400](https://github.com/kstrat2001/darkmux/pull/2400)) —
  a corrupted mirror (non-bare, or `origin` pointing somewhere else) is
  quarantined (renamed `<mirror>.corrupt-<ts>`, never deleted) and
  re-cloned instead of fetched into blind; a per-workspace advisory lock
  serializes concurrent plan/materialize calls against one workspace.
- **The runtime binary cache is version-keyed** ([#2402](https://github.com/kstrat2001/darkmux/pull/2402)) —
  a warm `~/.darkmux/runtime/darkmux-runtime` cache (from `dispatch --image`)
  now invalidates itself on a version mismatch, so an upgrade that adds a
  new runtime flag can no longer leave the cache serving an old binary that
  exits 2 on it. **No operator action is needed on upgrade** — the first
  `dispatch --image` after it re-extracts on its own (previously this
  needed a manual `rm`). `darkmux doctor` gained a matching
  `runtime binary cache` check alongside the existing
  `runtime image freshness` one.
- **`create_finding` hands back its own key; `create_mod` refuses an
  ungrounded one** ([#2386](https://github.com/kstrat2001/darkmux/issues/2386), [#2402](https://github.com/kstrat2001/darkmux/pull/2402)) —
  the runtime now knows its own dispatch id (`--session-id` forwarded
  host-side), so `create_finding` replies with the finding's real key
  instead of a bare "Recorded.", and `create_mod` rejects a `for` key this
  dispatch never actually recorded. A launch input supplied but consumed
  by neither a placeholder nor the launcher is refused before minting, by
  name, instead of silently doing nothing.
- **Crawl/review unit dispatches no longer collide on one container name**
  ([#2360](https://github.com/kstrat2001/darkmux/issues/2360), [#2362](https://github.com/kstrat2001/darkmux/pull/2362), [#2383](https://github.com/kstrat2001/darkmux/issues/2383), [#2385](https://github.com/kstrat2001/darkmux/pull/2385)) —
  a unit's on-disk home and its dispatch session id both now carry the
  rule segment (`units/<rule>/<unit>/out`, `crawl-<mission>-<rule>-<unit>`),
  so two rules planning the same `u-0001` no longer fight over one
  directory or one `darkmux-dispatch-…` container name.
- **Mod kits from a coder seat actually apply** ([#2387](https://github.com/kstrat2001/darkmux/issues/2387), [#2388](https://github.com/kstrat2001/darkmux/pull/2388), [#2390](https://github.com/kstrat2001/darkmux/pull/2390), [#2391](https://github.com/kstrat2001/darkmux/pull/2391), [#2392](https://github.com/kstrat2001/darkmux/pull/2392), [#2401](https://github.com/kstrat2001/darkmux/pull/2401)) —
  live create-mod runs turned up several ways a model-written unified diff
  failed at the gate: a missing trailing newline read as a corrupt patch
  (now normalized before `git apply`); a kit written in container
  coordinates (`/workspace/<source>/…`) is now mapped to the checkout's
  real path at the gate, with `--recount` trusting the diff body over a
  miscounted hunk header; the gate now stamps the resolved source id onto
  the mod record so `deliver.github_review` maps the same kit before
  rendering it as a suggestion. The shared create-mod message itself now
  states the path mapping and "a diff pasted into the reply is not a mod"
  in plain terms, for models that don't resolve it silently.
- **`/flow/<date>` and the run-detail SYSTEM pane keep the records that
  matter** ([#2409](https://github.com/kstrat2001/darkmux/issues/2409), [#2410](https://github.com/kstrat2001/darkmux/pull/2410), [#2414](https://github.com/kstrat2001/darkmux/pull/2414), [#2424](https://github.com/kstrat2001/darkmux/pull/2424), [#2436](https://github.com/kstrat2001/darkmux/pull/2436)) —
  a busy day's telemetry volume could push a `dispatch.start` out of the
  route's newest-10k window entirely, blanking the fleet lens's activity
  bars; every dispatch bookend (both the dotted and legacy spaced spelling)
  is now kept unconditionally, and the Redis+local union sorts by
  timestamp so a kept bookend can't land behind newer telemetry. The
  run-detail host-sample join recognizes the spaced bookends production
  actually emits, is bounded to the run's own day range (a daemon poll
  dropped from ~1.6s to ~0.65s on a real flows dir), stamps its own scan
  cost into the response, skips the day file's schema header, and
  hard-clamps a dead run's window so it can't absorb the next run's
  samples.
- **The mission board stops declaring a phase unreachable that is about to
  run** ([#2406](https://github.com/kstrat2001/darkmux/issues/2406), [#2434](https://github.com/kstrat2001/darkmux/pull/2434)) —
  `plans_errored` now counts a `plan.sites` step's failure too, not only
  the older `crawl.plan` kind; the `"can never run"` drift rule (which
  assumed phases gate strictly in declared order) is removed outright
  rather than patched, since phase order was never actually how the
  scheduler gates — a legitimately-waiting phase was getting a
  copy-pasteable `mission abort` suggestion that would have destroyed work
  seconds from finishing. The mission board's title for a config-launched
  run now prefers the config's own declared name over its (often
  paragraph-length) description.
- **Run-detail metric tiles share one anatomy** ([#2403](https://github.com/kstrat2001/darkmux/pull/2403)) —
  label / value / sub, consistently: the context tile no longer restates
  its own value in its label, `262.144K` reads as `262k`, and every tile is
  the same height.
- **Phone drawer handle and open state** ([#2407](https://github.com/kstrat2001/darkmux/pull/2407), [#2415](https://github.com/kstrat2001/darkmux/pull/2415), [#2435](https://github.com/kstrat2001/darkmux/pull/2435)) —
  a stored drawer height the drag interaction could never itself produce
  (opening a sliver a few percent tall that read as "won't open") is now
  clamped to the drag's own range on load; the drag handle's hit target
  grew 14px → 24px → 32px; the dead band above the first section is gone.
- **Phone pass on the LIVE pill, the event count, and the drill-in strip**
  ([#2443](https://github.com/kstrat2001/darkmux/pull/2443)) — the pill's dot
  sits beside its label instead of a word away; the event-count box centers its
  text and takes the whole row; the drill-in back strip stops stacking its
  timestamp one character per line; the filter control reads as a funnel rather
  than a caret; and the filter and clock buttons stop rendering permanently lit
  on a touch screen, where `:hover` never releases.
- **The live-status pill announces reconnects under a flapping stream**
  ([#2435](https://github.com/kstrat2001/darkmux/pull/2435)) — a stream
  that drops and recovers faster than the announcement's hold window used
  to announce nothing at all even though the dot visibly changed; the hold
  now latches on cumulative down-time instead of resetting on every
  transition. The pill also starts in "reconnecting", not an optimistic
  "live", until the stream actually opens.
- **`mission.grow`, the unit dispatch message, and run-view attribution**
  ([#2374](https://github.com/kstrat2001/darkmux/pull/2374), [#2379](https://github.com/kstrat2001/darkmux/pull/2379), [#2380](https://github.com/kstrat2001/darkmux/pull/2380)) —
  `FLOW_SCHEMA_VERSION` 1.39.0 → 1.40.0: `mission.grow` gains
  `producer_step`, a documented `producer_status` vocabulary, a
  `producer_errored` reason, and `source` now means one thing (the
  producing step's id) instead of varying by arm. `MOD_SCHEMA_VERSION`
  "1" → "2" documents the stored mod shape
  (`kit_kind`/`source`/`gate`/`gate_skipped_reason`). The create-mod
  dispatch message is now frozen against a golden. A run with steps that
  never ran is reported `Degraded`, not silently clean, its rule count
  reflects distinct rules rather than tasks, its dedup only collapses a
  ref against an earlier draw (not within the same draw), and the
  terminal summary line and the envelope now classify a step's outcome
  from the same partition instead of two that could disagree.
- **Step seat classification** ([#2397](https://github.com/kstrat2001/darkmux/pull/2397)) —
  `FLOW_SCHEMA_VERSION` 1.40.0 → 1.41.0: `step start` carries
  `payload.seat_class`, and a new `"step seat unresolved"` action replaces
  a silent fail-open default.
- **Machine lens says less at rest** ([#2378](https://github.com/kstrat2001/darkmux/pull/2378), [#2381](https://github.com/kstrat2001/darkmux/pull/2381)) —
  explanatory prose in the machine lens is trimmed (measured: 112 words of
  boilerplate down to 25 in the worst case, payload unchanged), collapsed
  into one "how this was measured" disclosure per region; ages are computed
  against the daemon's own clock so a client running behind it no longer
  renders a blank "snapshot" age; the phone drawer's Machine tab stays
  host-only (no model id, context size, or residency vocabulary — that
  stays in the dedicated machine lens).
- **Phone drawer inset, record-key wrapping, and the static demo's mission
  events** ([#2373](https://github.com/kstrat2001/darkmux/pull/2373)) —
  the mission graph canvas now ends where the drawer's tabs begin instead
  of running under them; long record keys wrap instead of clipping;
  `#mission=<id>` on the static demo reads the loaded day's own records, so
  its EVENTS pane is no longer always empty.
- **CI stopped failing on every push to `main` from a stale generated TS
  binding** ([#2389](https://github.com/kstrat2001/darkmux/pull/2389)) —
  a struct field added without regenerating its TypeScript export left the
  `quality` workflow's badge step unable to commit, silently spawning an
  orphan `badges` branch on every push. A new PR-time check
  (`git diff --exit-code` on the generated bindings) fails loudly with the
  regenerate command instead.

### Removed

- The review workflow's `mode` input (validated but never forwarded) and `review.json`'s `mode` / `envelope_out` inputs, which nothing passed ([#2438](https://github.com/kstrat2001/darkmux/pull/2438)).
- The old review funnel (bundle → probe → dedup → judge → verify →
  synthesis) and its ten Tier-3 `review.*` step kinds are deleted.
  Historical run records that used them still render in the viewer.
- The review funnel's own embedded roles — `review-probe`,
  `review-probe-high`, `review-probe-mid`, `review-probe-low`,
  `review-judge`, `review-verify` — are deleted along with it ([#2418](https://github.com/kstrat2001/darkmux/issues/2418), [#2427](https://github.com/kstrat2001/darkmux/pull/2427)).
  Nothing shipped (mission configs, skills) referenced them; the shipped
  `review` config stages its work through `reviewer`/`coder` instead. An
  operator who had `role_profiles.<one of these>` bound in `config.json`,
  or was dispatching one directly, needs to remove/repoint that binding —
  `darkmux doctor` flags an unknown role id.
- `review`'s `bundler` and `pr` inputs ([#2404](https://github.com/kstrat2001/darkmux/pull/2404)).
- The `docs/guide/bundlers.html` guide page. `--bundler` survives only on
  `lab eval` / `lab review-bench`.
- `darkmux-review.yml` no longer builds `darkmux-bundler-rust`.

[3.7.0]: https://github.com/kstrat2001/darkmux/releases/tag/v3.7.0

## [3.6.0] - 2026-09-04

The crawl is a mission, and its output is records you can pick up.

This release closes a loop that opened in 3.4.0. A crawl is no longer a launcher
with a hard-wired pipeline: it is `crawl.json`, a mission config whose steps
you enable or disable, whose plan is one step per rule, whose units grow from
each plan at the phase boundary, and whose findings can grow one coder step
each in a **create-mods** phase — plan → crawl → create mods. Every hand-off
between steps is a **typed output** with a hash and a producer, and a finding
or a mod can be pulled into any dispatch's brief by key. The first real run on
the generic path shipped this release's own pre-ship test: a comment-shrink
rule over one crate, three units, fourteen findings, twelve mods and two
justified refusals, with nothing but a JSON rule and a JSON config copy on
the operator's side. The rest is what that run surfaced: units that ran one at
a time, a wave that leased one model while its dispatches loaded another, a
pulsing chip that said three different words, and a mission header rebuilt
from a UX review.

### Added

- **Findings and mods are records** ([#2288](https://github.com/kstrat2001/darkmux/pull/2288), [#2290](https://github.com/kstrat2001/darkmux/pull/2290), [#2293](https://github.com/kstrat2001/darkmux/pull/2293), closes [#2265](https://github.com/kstrat2001/darkmux/issues/2265)).
  `create_finding` (renamed from `report_finding`) writes a finding under
  `~/.darkmux/findings/<dispatch>/<seq>/`; a mod is a kit (instructions plus
  data, opaque to darkmux) with its own minted key and `for:` provenance to one
  or more findings, written by the palette-granted runtime tool `create_mod`.
  `finding list|show|sync` and `mod list|show|create` read and write the
  stores.

- **Brief refs** ([#2295](https://github.com/kstrat2001/darkmux/pull/2295), [#2296](https://github.com/kstrat2001/darkmux/pull/2296)). `dispatch --finding <key>` / `--mod <key>`
  append the stored record to the brief; the same `brief_refs` field on a
  `dispatch.internal` step config does it inside any mission graph. Mod
  attachments mount read-only into the container. The ref resolves in ONE
  place, the step kind, so the CLI and the graph cannot drift (FLOW 1.36.0).

- **`enabled` on phases, tasks and steps** ([#2299](https://github.com/kstrat2001/darkmux/pull/2299), mission config 3.1).
  A disabled step never exists in the run: pruned when the run is minted, with
  the reason in the config snapshot and `graph-report.json`, and counted by
  `mission status` ("4 of 12 steps minted, 8 left out by config"). No gray
  state, no CLI override — edit the JSON before you run and the snapshot keeps
  it.

- **Grow** ([#2307](https://github.com/kstrat2001/darkmux/pull/2307), [#2300](https://github.com/kstrat2001/darkmux/issues/2300), mission config 3.2, FLOW 1.38.0). A task
  template with `grow: {from, items, id, config}` mints one copy per item of
  an earlier step's output at the phase boundary. Grown tasks carry
  `grown_from`; a `mission.grow` record says what grew from what.

- **Crawl as a mission** ([#2298](https://github.com/kstrat2001/darkmux/pull/2298), [#2313](https://github.com/kstrat2001/darkmux/pull/2313), [#2301](https://github.com/kstrat2001/darkmux/issues/2301), epic [#2297](https://github.com/kstrat2001/darkmux/issues/2297)).
  `crawl.plan` plans one rule per task and outputs the plan; `crawl.unit` and
  `crawl.summary` run and roll up the units grown from it; `rules=` selects
  plan tasks. Every hand-off is a typed `Output<T>` envelope (kind, blake3 hash,
  producer with machine id) exported to the viewer's TypeScript types
  (FLOW 1.39.0). The literal launcher (`src/crawl_launch.rs`) is deleted.

- **The create-mods phase** ([#2302](https://github.com/kstrat2001/darkmux/pull/2302), [#2326](https://github.com/kstrat2001/darkmux/pull/2326), off by default). The summary
  names its findings; the `create-mods` phase in `crawl.json` grows one coder
  step per finding with the finding in its brief. The coder proposes a mod
  against a read-only tree; it does not edit. Integration is a later phase.

- **Mission header, rebuilt** ([#2332](https://github.com/kstrat2001/darkmux/pull/2332)). On a narrow lens: the mission name
  with the shared status chip pinned right, a dim sub-line (start time, elapsed
  or final duration, the short hash, host gpu/cpu only while running), and the
  cost row — the token total, any cloud share in parentheses, the full split in
  a tooltip — with the renderer toggle, map, and a help glyph for the legend.
  On a wide lens, one row. Layout follows the lens's own width, not the
  window's. The refresh button, the turn count and the inline legend are gone.
  Tap the name for the full id.

- Docs: the crawl, findings and mods guide ([#2292](https://github.com/kstrat2001/darkmux/pull/2292)); `mission config show`
  reports each task's `enabled`; the `read` tool numbers its output so the
  model stops counting lines ([#2283](https://github.com/kstrat2001/darkmux/pull/2283), [#2267](https://github.com/kstrat2001/darkmux/issues/2267)).

### Changed

- `darkmux mission launch crawl` is the generic launcher. Dropped inputs:
  `source`/`rule` one-shot (use a one-source spec file), `plan`/`plan_out`
  (the plan is always written under the run), `units`/`limit`, `resume`. Kept:
  `workspace`, `rules`, `max_sites_per_unit`, `max_est_tokens_per_unit`,
  `no_fetch`, `dry_run` ([#2313](https://github.com/kstrat2001/darkmux/pull/2313)).

- `mission start` on a config-launched run carries `graph` (declared vs
  minted); `mission close` promotes the last phase's last step's JSON body on
  any config (FLOW 1.37.0–1.39.0).

- The header owns liveness and the chip owns the word ([#2278](https://github.com/kstrat2001/darkmux/pull/2278), [#2279](https://github.com/kstrat2001/darkmux/pull/2279), [#2314](https://github.com/kstrat2001/darkmux/pull/2314), [#2322](https://github.com/kstrat2001/darkmux/pull/2322), [#2330](https://github.com/kstrat2001/darkmux/pull/2330)).
  One LIVE badge on every live page, meaning the record stream is connected; a
  running mission shows it, a closed one names its day, and the chip beside it
  says TODAY, never RESULT. A running unit of work says RUNNING at every scope
  — mission, phase, task, step, run, lab run — where it used to say ACTIVE,
  RUNNING or "● live" depending on where you looked. Terminal words keep their
  scope's meaning. LIVE and RUNNING are different facts and stay separate.

- One liveness beat ([#2328](https://github.com/kstrat2001/darkmux/pull/2328)). Three pulse keyframes at two cadences
  collapsed into one, defined once and used by every running indicator; the
  RUNNING pill no longer shrinks 22% on every beat.

- Six unnamed compound predicates in the viewer got names — the crawl's own
  findings, applied ([#2285](https://github.com/kstrat2001/darkmux/pull/2285)).

### Fixed

- **Sibling crawl units ran one at a time** on an already-resident model
  ([#2329](https://github.com/kstrat2001/darkmux/pull/2329), closes [#2321](https://github.com/kstrat2001/darkmux/issues/2321)). The unit step kind declared no residency, so
  the scheduler queued it as a remote job under a cap of one. It declares the
  crawler seat like every other local step now, and the units wave-pack:
  fourteen coders in one wave in about ten minutes where five had taken two
  hours.

- **A wave could lease one model while its dispatches loaded another**
  ([#2329](https://github.com/kstrat2001/darkmux/pull/2329)). The placement resolver read `default_profile` alone; the dispatch
  read `role_profiles.<role>` first. Same chain now, in the shared helper, so
  `dispatch.internal` and the coder phase are covered too.

- Sibling dispatches in one wave raced the model load: each read the model as
  absent and issued `lms load`; LMStudio admitted one and refused the rest. The
  preflight is serialized across check-and-load, and a refusal naming an
  already-resident identifier re-probes and reuses it ([#2320](https://github.com/kstrat2001/darkmux/pull/2320), [#2318](https://github.com/kstrat2001/darkmux/issues/2318)).

- A crawl unit that errored no longer fails the whole summary; it is counted
  as errored with its reason ([#2313](https://github.com/kstrat2001/darkmux/pull/2313)).

- An accepted `report_finding` over 512 chars was rejected downstream: the
  emission now rides the record whole ([#2275](https://github.com/kstrat2001/darkmux/pull/2275), [#2272](https://github.com/kstrat2001/darkmux/issues/2272)). A
  palette-named tool outside the general catalog was unreachable since #2182
  ([#2271](https://github.com/kstrat2001/darkmux/pull/2271)).

- Viewer: a config-minted step dispatched with no phase, so its run view could
  not be reached from the graph ([#2324](https://github.com/kstrat2001/darkmux/pull/2324), [#2323](https://github.com/kstrat2001/darkmux/issues/2323)); the desktop mission
  graph went blank a second after load — React Flow dropped every node's
  measured size on each controlled update ([#2327](https://github.com/kstrat2001/darkmux/pull/2327), [#2325](https://github.com/kstrat2001/darkmux/issues/2325)); task cards
  sized from content, collapsed rows keep the name readable, the task timer is
  the task's span ([#2277](https://github.com/kstrat2001/darkmux/pull/2277), [#2282](https://github.com/kstrat2001/darkmux/pull/2282), [#2104](https://github.com/kstrat2001/darkmux/issues/2104), [#2269](https://github.com/kstrat2001/darkmux/issues/2269), [#2280](https://github.com/kstrat2001/darkmux/issues/2280)).

[3.6.0]: https://github.com/kstrat2001/darkmux/releases/tag/v3.6.0

## [3.5.0] - 2026-09-02

An unattended run stops lying to you.

This release is about the run you are not watching. A **thermal governor**
pauses an in-flight dispatch at its next turn boundary instead of killing it,
and a **breaker** stops the work at critical — both riding the pause/resume
mechanism this release adds (a pace file, a turn-boundary checkpoint, and
`dispatch --resume-from`), so a machine that gets hot rests and resumes rather
than losing an hour of work. A **power-posture pre-flight** refuses to start a
mission at serious thermal state and holds a sleep assertion for the launch's
lifetime. The rest is a burn-down, most of it found by a review swarm over the
merged diff: every way a dispatch could hang past its own deadline, spin
without terminating, execute quoted markup as a tool call, or leave a container
running after it exited.

### Added

- **Pause/resume for runs** ([#2139](https://github.com/kstrat2001/darkmux/pull/2139), [#2146](https://github.com/kstrat2001/darkmux/pull/2146), closes [#2114](https://github.com/kstrat2001/darkmux/issues/2114)).
  Between turns the loop reads `<host_out>/pace.json`; while `pause: true`
  holds it rests in bounded increments, re-reading each time, emitting a
  `runtime.rest` event per increment and absorbing the rest into the soft
  inactivity clock — so a long pause never trips the inactivity detector. State
  is checkpointed at the turn boundary, and every pace write is stamped
  `written_at_ms` so a stale pause expires instead of wedging a container.

- **`dispatch --resume-from` + `crawl --param resume=`** ([#2153](https://github.com/kstrat2001/darkmux/pull/2153)).
  The runtime has honored `--resume` since #2114, but nothing could ever
  trigger it. Now a prior dispatch's out dir is verified (`checkpoint.json`
  exists and parses), copied into this dispatch's own fresh out dir — the prior
  dir is left untouched as evidence — and `resumed_from` is stamped into the
  `dispatch.start`/`dispatch.complete` payloads. A missing or invalid
  checkpoint fails with a named error; it never silently starts fresh. For a
  crawl, `resume=<mission-id>` skips units that completed and resumes units
  that have a checkpoint.

- **Thermal governor + breaker** ([#2140](https://github.com/kstrat2001/darkmux/pull/2140), closes [#2110](https://github.com/kstrat2001/darkmux/issues/2110) and [#2109](https://github.com/kstrat2001/darkmux/issues/2109)).
  A host-side state machine fed one OS thermal reading per tick from the
  existing per-dispatch sampler — no new poller. At `runtime.thermal.pause_at`
  (default `serious`) it writes the pace file so the dispatch rests at its next
  turn boundary, clearing only after the state holds at or below `resume_at`
  for `resume_hold_ms`, so a state bouncing on the threshold cannot flap. At
  `critical`, or below `min_cpu_speed_limit_pct`, the breaker stops further
  dispatch. It **never kills the container** — the unit pauses with its
  checkpoint persisted, and resume is the operator's call. On by default:
  hardware safety is not an opt-in integration.

- **Power-posture pre-flight and a sleep assertion** ([#2138](https://github.com/kstrat2001/darkmux/pull/2138), closes [#2112](https://github.com/kstrat2001/darkmux/issues/2112)).
  A no-sudo probe reads AC/battery, Low Power Mode, thermal state, the CPU
  speed cap, and any thermal-emergency forced sleep in the last 24h. `darkmux
  doctor` warns; a mission launch **refuses to start at serious or critical
  thermal state** unless `--force`. Each launch holds an
  `IOPMAssertionCreateWithName` sleep assertion for its lifetime, released on
  every exit path.

- **Machine telemetry as flow records** ([#2173](https://github.com/kstrat2001/darkmux/pull/2173), closes [#2111](https://github.com/kstrat2001/darkmux/issues/2111); [#2167](https://github.com/kstrat2001/darkmux/pull/2167), closes [#2165](https://github.com/kstrat2001/darkmux/issues/2165)).
  `machine.thermal` and `machine.telemetry` records, a `host_window` on
  `dispatch complete`, bound provenance on cap/salvage/detector records, and a
  resolved-knobs snapshot so every run is self-describing for later comparison.

- **Hooks reach the tailnet, and transform on the way out** ([#2178](https://github.com/kstrat2001/darkmux/pull/2178), closes [#2135](https://github.com/kstrat2001/darkmux/issues/2135); [#2187](https://github.com/kstrat2001/darkmux/pull/2187), closes [#2183](https://github.com/kstrat2001/darkmux/issues/2183)).
  Delivery is no longer loopback-only: a tailnet receiver is reachable under a
  signed, attributed contract (HMAC-SHA256). Records can be reshaped by a pure
  in-process `jq` transform bounded by `hooks.jq_timeout_ms` and
  `hooks.jq_max_output_bytes`, auth headers are Keychain-referenced rather than
  written into config, and a `file` transport writes deliveries to disk
  (owner-only, `0600`) for local testing without a network.

- **Crawl unit sizing** ([#2192](https://github.com/kstrat2001/darkmux/pull/2192)).
  `max_sites_per_unit` lets an operator keep per-turn context small.

- **Viewer** — a thermal alert ladder showing all four states with the current
  one highlighted, and the mission lens's private event pane folded into the
  shared mainstay column it had been silently failing on
  ([#2163](https://github.com/kstrat2001/darkmux/pull/2163)); step drill-in,
  which selects a step and scopes the events column
  ([#2191](https://github.com/kstrat2001/darkmux/pull/2191), closes [#2189](https://github.com/kstrat2001/darkmux/issues/2189));
  a pulsing RUNNING pill and no playback badge
  ([#2221](https://github.com/kstrat2001/darkmux/pull/2221)); and a mobile pass
  — inline filters, masthead order, a header that stops repeating itself
  ([#2222](https://github.com/kstrat2001/darkmux/pull/2222)).

### Fixed

**The runtime loop.**

- **A generation check-in bounds every call** ([#2176](https://github.com/kstrat2001/darkmux/pull/2176), closes [#2171](https://github.com/kstrat2001/darkmux/issues/2171)). A non-thinking model could outlast the inactivity budget inside a single call, with no proof-of-work signal to reset the clock.
- **The reasoning check-in bounds reasoning only** ([#2166](https://github.com/kstrat2001/darkmux/pull/2166), closes [#2164](https://github.com/kstrat2001/darkmux/issues/2164)) — not a fresh turn's first call.
- **Tool calls with non-tool names are never executed** ([#2182](https://github.com/kstrat2001/darkmux/pull/2182), closes [#2169](https://github.com/kstrat2001/darkmux/issues/2169)): partition, coalesce, and name the model.
- **Quoted tool-call markup is not promoted into a dispatched call** ([#2254](https://github.com/kstrat2001/darkmux/pull/2254), closes [#2230](https://github.com/kstrat2001/darkmux/issues/2230)). Text inside a fenced code block — a model *explaining* a command — could be promoted by the plain-text tool-call promoter and executed. Confirmed executable with `rm -rf /workspace` before the fix.
- **The degeneracy gate can see whitespace-free output** ([#2245](https://github.com/kstrat2001/darkmux/pull/2245), closes [#2228](https://github.com/kstrat2001/darkmux/issues/2228)). When tokenization goes blind, a character-level fallback terminates a spinning run in 0.34s where it previously spun for 5.69s and counting.
- **The stall-recovery budget pays down on a turn that actually opened** ([#2241](https://github.com/kstrat2001/darkmux/pull/2241), closes [#2229](https://github.com/kstrat2001/darkmux/issues/2229)) — the first fix made escalation unreachable across 41 turns.
- **Empty `tool_calls` is its own signal**, the stall budget is a knob (`runtime.max_stall_recoveries`), and escalations name the model and context ([#2200](https://github.com/kstrat2001/darkmux/pull/2200), closes [#2190](https://github.com/kstrat2001/darkmux/issues/2190)).

**Dispatch lifecycle.**

- **The container is reaped on every non-success exit** ([#2249](https://github.com/kstrat2001/darkmux/pull/2249), closes [#2233](https://github.com/kstrat2001/darkmux/issues/2233)). A kill guard is armed at spawn and disarmed only on a successful exit status, so an error, a panic, or a signal no longer leaves a container running with a model resident.
- **The inactivity watchdog keeps killing until the container is gone** ([#2255](https://github.com/kstrat2001/darkmux/pull/2255), closes [#2232](https://github.com/kstrat2001/darkmux/issues/2232)). A single `docker kill` was treated as success — but `docker rm -f` exits 0 on a container that does not exist, so a failed kill was indistinguishable from a successful one. Now: retry with backoff, escalate to `rm -f`, and distinguish confirmed-stopped from never-existed.
- **A shared finalize guard across all three mission launchers** ([#2141](https://github.com/kstrat2001/darkmux/pull/2141), closes [#2131](https://github.com/kstrat2001/darkmux/issues/2131)) — SIGINT, SIGTERM, and SIGHUP each write the terminal record and reap children.
- **The crew index rebuilds when the binary's builtin set changes** ([#2145](https://github.com/kstrat2001/darkmux/pull/2145), closes [#2144](https://github.com/kstrat2001/darkmux/issues/2144)), and `missions.status` accepts `Aborted` ([#2147](https://github.com/kstrat2001/darkmux/pull/2147), closes [#2142](https://github.com/kstrat2001/darkmux/issues/2142)).

**Hangs.**

- **Every Redis command path is bounded, including the dispatch teardown** ([#2244](https://github.com/kstrat2001/darkmux/pull/2244), closes [#2227](https://github.com/kstrat2001/darkmux/issues/2227)). An unreachable-but-routable Redis wedged a dispatch's teardown for 89.4s at ten production command sites.
- **`mission dispatch --wait` cannot hang past its own timeout** ([#2251](https://github.com/kstrat2001/darkmux/pull/2251), closes [#2243](https://github.com/kstrat2001/darkmux/issues/2243)). Each blocking read now carries the remaining deadline rather than the full one, and a timed-out connection is discarded instead of reused — a reused one returns the *previous* command's reply, trading a hang for a silently wrong answer.

**Crawl, config, viewer, docs.**

- A crawl unit is bounded by turns and by progress, and its dispatch names the model ([#2198](https://github.com/kstrat2001/darkmux/pull/2198), closes [#2193](https://github.com/kstrat2001/darkmux/issues/2193) and [#2188](https://github.com/kstrat2001/darkmux/issues/2188)); a unit with an unresolvable source sha is refused rather than dispatched ([#2152](https://github.com/kstrat2001/darkmux/pull/2152)).
- Two knobs that were settable in the struct but missing from the `config set` key registry — `runtime.telemetry_record_every_samples` ([#2177](https://github.com/kstrat2001/darkmux/pull/2177)) and `hooks.jq_timeout_ms` / `hooks.jq_max_output_bytes` ([#2194](https://github.com/kstrat2001/darkmux/pull/2194)). The underlying cause, two hand-maintained lists, is tracked as [#2195](https://github.com/kstrat2001/darkmux/issues/2195).
- **The step drill-in reaches the dispatch detail view** ([#2223](https://github.com/kstrat2001/darkmux/pull/2223)). The first version keyed on session-id *shape*, which made it inert on every generic `mission launch` mission while its tests stayed green; it now discriminates on evidence of dispatch.
- **The machine info panel leads with what it is** ([#2250](https://github.com/kstrat2001/darkmux/pull/2250)) — one definition table, one value edge, sampler cadence stated once at the bottom instead of three times.
  A pre-release review caught that the rework guarded the section on a JSX
  fragment, which is always truthy, so a machine whose identity was not yet
  known rendered a bare "System specs" heading with nothing under it; the
  test named for that exact shape had been passing throughout because it
  only asserted the rows, never the heading ([#2256](https://github.com/kstrat2001/darkmux/pull/2256)).
- Docs and tests use synthetic network identifiers only ([#2179](https://github.com/kstrat2001/darkmux/pull/2179)); the SIGTERM reap assertions are scoped to the test's own child ([#2148](https://github.com/kstrat2001/darkmux/pull/2148)).

### Schema

- `FLOW_SCHEMA_VERSION` 1.28.0 → **1.31.0** — `dispatch start` gains a `bounds` block carrying the resolved runtime knobs with `{value, source}` provenance (1.29.0); `dispatch.rest` gains `reason` and `state`, and `dispatch.complete` gains `paced_rest_ms`, so a governor pause and a routine inter-turn rest are no longer distinguishable only by cadence (1.30.0); and the `machine.thermal` / `machine.telemetry` actions plus `host_window` on the dispatch terminal (1.31.0). All additive — payload keys on existing records and two new action values.
- `CONFIG_SCHEMA_VERSION` 1.14 → **1.19** — `runtime.thermal.*` (1.15), `runtime.telemetry_record_every_samples` (1.16), the hook rules' signing and attribution fields (1.17), the jq bounds plus the `headers` / `file` / `transform` rule fields (1.18), and a bookkeeping bump crediting `runtime.max_stall_recoveries` and `runtime.generation_checkpoint_interval_tokens`, two caps that shipped without one (1.19). All additive and lenient-on-read.

  Note that reaching a tailnet receiver is **not** a config flag — there is no
  enable knob. The rule's URL is the decision: loopback or a genuine Tailscale
  address is accepted, anything else is refused at load and at every POST.

Both are minor bumps, so a v3.4.0 binary reading v3.5.0 records or config sees
fields it ignores rather than breaking — which matters on a mixed-version
fleet, where the hub and a peer are not always upgraded on the same day.

[3.5.0]: https://github.com/kstrat2001/darkmux/releases/tag/v3.5.0

## [3.4.0] - 2026-08-30

The crawler becomes a mission, and the machine tells you what it is doing.

Crawling a codebase against a rule set is now just the crawler role's work as
a mission: a **workspace spec** names the sources and the file filters, rules
are a template kind, and `mission launch crawl --dry-run` previews the plan.
Every finding, step, and mission record can be routed anywhere through
**hooks**, an event-agnostic flow sink that POSTs matching records to a
loopback receiver; this release's first receiver is a small local issue
tracker. Underneath, darkmux now reads the Apple Silicon host properly: a
~7 ms **host probe** (mach ticks, IOReport power and clocks, thermal state)
replaces a `top` shell-out that cost ~780 ms per sample and reported a
since-boot average as "current CPU"; the daemon keeps a ten-minute ring and
the viewer shows thermal state, power per rail, and CPU clusters in a
**Machine info** modal and a new phone **bottom sheet** with the event log.
`runtime.turn_delay_ms` rests the GPU between turns, recorded so a rested
run is never misread as a slow model.

### Added

- **Workspace spec + rules kind + crawl mission** ([#2108](https://github.com/kstrat2001/darkmux/pull/2108), [#2096](https://github.com/kstrat2001/darkmux/pull/2096), [#2098](https://github.com/kstrat2001/darkmux/pull/2098), [#2099](https://github.com/kstrat2001/darkmux/pull/2099); part of [#1959](https://github.com/kstrat2001/darkmux/issues/1959)).
  No `crawl` verb and no "corpus": the manifest is a generic workspace spec
  mission input (`sources` + `include`/`exclude`, materialized as read-only
  worktrees under `<root>/workspaces/<name>`), rules live under
  `templates/builtin/rules/` with a user tier at `~/.darkmux/rules/`, and
  the launcher (`templates/builtin/mission-configs/crawl.json`) dispatches one
  unit per (source, rule) with `record_context` on every record. Crawl
  records are the generic `mission start/close` and `step start/complete`
  vocabulary, not a private `crawl.*` one.

- **Hooks: a flow sink that POSTs matching records to a loopback receiver** ([#2097](https://github.com/kstrat2001/darkmux/pull/2097), [#2103](https://github.com/kstrat2001/darkmux/pull/2103), closes [#2093](https://github.com/kstrat2001/darkmux/issues/2093)).
  `config.hooks.rules[]` match any record (`action`, `category`, dotted
  `payload.*` predicates) and deliver it over HTTP to `127.0.0.1` only
  (userinfo refused, no redirects); an outbox and cursor per rule survive a
  receiver outage, `flow status` shows delivery state, and `flow drain`
  flushes it. Hooks observe; they never dispatch.

- **`runtime.turn_delay_ms`: rest the GPU between inference turns** ([#2097](https://github.com/kstrat2001/darkmux/pull/2097), closes [#2094](https://github.com/kstrat2001/darkmux/issues/2094)).
  A global inter-turn sleep on every local dispatch, recorded as
  `dispatch.rest` (`rest_ms`, `rests`, `turn_delay_effective_ms`) and
  counted as proof-of-work for the inactivity watchdog; `wall_ms` stays wall
  time. Never applied to agentic-remote dispatches; clamped against the
  inactivity timeout.

- **Apple Silicon host probe** ([#2108](https://github.com/kstrat2001/darkmux/pull/2108), part of [#2107](https://github.com/kstrat2001/darkmux/issues/2107) and [#1833](https://github.com/kstrat2001/darkmux/issues/1833)).
  `host_probe/` reads CPU as mach tick deltas (a true mean over the interval,
  per cluster by `hw.perflevel`), power per rail and cluster/GPU MHz from
  IOReport (loaded at runtime, degrades to null), GPU busy and memory from
  IOKit in-process, thermal state from `ProcessInfo` and the CPU speed limit
  from `IOPMCopyCPUPowerStatus`. About 7 ms per sample. The daemon samples
  every `runtime.host_sampler_interval_ms` (5 s) into a ten-minute ring
  served as `load` on `GET /machine/resources`; `dispatch complete` records
  carry `host.thermal`, `host.power`, and `host.energy_mwh` (flow schema
  1.28.0, additive). `darkmux doctor` names which sources resolved and the
  measured cost.

- **Machine info modal and the phone bottom sheet** ([#2108](https://github.com/kstrat2001/darkmux/pull/2108)).
  The masthead ⓘ opens a Machine info modal (gauges with avg/max, thermal
  pill, power per rail, CPU cluster tiles); phones get a tabbed sheet
  (Machine info | Events) anchored under the masthead with the event log's
  list, a row tap that pushes the record's detail, and follow mode that
  always shows the list. The Machine lens renders the same block plus its
  own depth.

### Fixed

- **The CPU column was never a measurement** ([#2108](https://github.com/kstrat2001/darkmux/pull/2108)).
  `top -l 1` blocked ~780 ms per sample and its first sample is a since-boot
  average, so every earlier `host.cpu.*` value was a lifetime smoothing. Gone.
- **Fleet lens: the summary row is back on the tab row** (regression against
  the 2026-08-27 screenshot), the machine lens no longer repeats the machine
  name above its own breadcrumb, hero token figures no longer collide in the
  two-column band, the viewer's tab favicon is the two-input glyph.
- **Crawl finding records carry one rule id** ([#2103](https://github.com/kstrat2001/darkmux/pull/2103)); a receiver's per-record rejection is surfaced as `hook.fired.receiver_rejected`.
- **Docs**: the home page hands the brand off from the hero to the nav bar on scroll, and the guide header is one row on phones ([#2115](https://github.com/kstrat2001/darkmux/pull/2115)).
- **Release-candidate fixes, found by the operator on real devices** ([#2134](https://github.com/kstrat2001/darkmux/pull/2134)):
  the playback transport is the instrument (no label, no record counter, the
  track takes the row; the mission title lives in Machine info's playback row,
  [#2120](https://github.com/kstrat2001/darkmux/issues/2120)); the demo speaks
  for itself (machines named Studio / Workstation / Mini, a real mission
  title, re-shot marketing screenshots, [#2121](https://github.com/kstrat2001/darkmux/issues/2121));
  gauges color by band, amber from 80% and red from 95%
  ([#2122](https://github.com/kstrat2001/darkmux/issues/2122)); the filters
  dialog is wide and grouped on desktop ([#2116](https://github.com/kstrat2001/darkmux/issues/2116));
  an active mission now counts as running on the fleet card and the runs lens
  (presence was treated as all-or-nothing, [#2123](https://github.com/kstrat2001/darkmux/issues/2123));
  the fleet timeline keys spans by (session, mission) so review missions that
  reuse step ids no longer draw one twenty-hour bar
  ([#2125](https://github.com/kstrat2001/darkmux/issues/2125)); the review
  launcher writes its terminal record and reaps its children on SIGTERM and
  SIGINT ([#2124](https://github.com/kstrat2001/darkmux/issues/2124)); a
  bundler plugin that declines a diff falls back to the built-in bundler
  instead of failing the review ([#2119](https://github.com/kstrat2001/darkmux/issues/2119));
  a flaky mission-isolation e2e is stable ([#2117](https://github.com/kstrat2001/darkmux/issues/2117)).

### Schema

- `FLOW_SCHEMA_VERSION` 1.22 → 1.28 (hook records; generic mission/step crawl payloads; `dispatch.rest`; workspace vocabulary; host thermal/power/energy). All additive.
- `CONFIG_SCHEMA_VERSION` 1.11 → 1.14 (`hooks`, `runtime.turn_delay_ms`, `runtime.host_sampler_interval_ms`).

[3.4.0]: https://github.com/kstrat2001/darkmux/releases/tag/v3.4.0

## [3.3.0] - 2026-08-28

The demo plays a real mission, and playback rides on every route.

[darkmux.com/demo](https://darkmux.com/demo) now replays a real review mission,
the crew reviewing a merged darkmux PR, with the mission graph, the runs, the
fleet, and the event log all reading from one committed day. The playback
transport sits on a sticky row with the tabs on every route, its speed is an
honest multiplier, and a daemon page for a finished dispatch gets the same
day chip, badge, and transport the demo has. Most of this release was found
by tapping through the demo on a phone: a fleet card that landed on an empty
runs lens, run rows that errored, a playback view that jumped as events
streamed in, a top chrome that was mostly text. Each of those is fixed below.

### Added

- **The demo replays a real review mission** ([#2062](https://github.com/kstrat2001/darkmux/pull/2062), closes [#2032](https://github.com/kstrat2001/darkmux/issues/2032)).
  `scripts/demo-env/import_mission.py` imports a finished mission from a
  real `~/.darkmux`, scrubs identity once at import (machine ids, host paths,
  hostnames, tailnet names, absolute timestamps; a scrub miss fails the
  import, and CI's public-leak guard fails the PR), and the static build
  captures each mission graph into `docs/demo/demo-graphs.json`. The subject
  is always darkmux's own public code, never an engagement's, so a missed
  scrub still exposes nothing foreign.

- **Sticky tabs and a playback transport on every route** ([#2080](https://github.com/kstrat2001/darkmux/pull/2080), closes [#2071](https://github.com/kstrat2001/darkmux/issues/2071)).
  The shell owns the playhead, so a run's detail page, the fleet, and the
  runs board all follow the same clock; the tabs and the transport stay
  pinned while the page scrolls. A run rewound to before it started says so
  instead of rendering a header for nothing.

- **A daemon dispatch or mission page names its day** ([#2089](https://github.com/kstrat2001/darkmux/pull/2089)).
  The chip shows the day the run started, the `▶ PLAYBACK` badge says which
  mode the page is in, and a finished dispatch gets the transport. A run
  that is still going stays a live view. When no date resolves the chip
  reads `RESULT`.

- **`init` verifies the worker model against LM Studio** ([#2054](https://github.com/kstrat2001/darkmux/pull/2054), closes [#2053](https://github.com/kstrat2001/darkmux/issues/2053)).
  The shipped default is written only when LM Studio actually has it; the
  next-steps text no longer says `docker build`.

### Changed

- **Playback speed is a real multiplier** ([#2081](https://github.com/kstrat2001/darkmux/pull/2081)).
  The transport advanced a fixed fraction of the day per tick, so every
  recording played in twelve seconds and "1×" was thousands of times real
  time. It now advances recorded time by the measured wall-clock delta times
  the speed, labeled as recorded time per second: `1h/s` (default), `10m/s`,
  `1m/s`.

- **One date chip on every build** ([#2085](https://github.com/kstrat2001/darkmux/pull/2085), [#2074](https://github.com/kstrat2001/darkmux/pull/2074), closes [#2072](https://github.com/kstrat2001/darkmux/issues/2072), [#2073](https://github.com/kstrat2001/darkmux/issues/2073)).
  Demo and daemon render the same outlined pill with the bare date (the
  `FLOW ·` prefix was noise), and the phone chrome drops from 203 to 153
  pixels: the meta line keeps the mission and the census, the idle line is
  gone from the static build, the masthead packs the chip and badge to the
  right on phones only.

- **Uniform tabs, matching transport controls** ([#2082](https://github.com/kstrat2001/darkmux/pull/2082), [#2084](https://github.com/kstrat2001/darkmux/pull/2084)).
  Tab cells share a width on every screen and fill the row on portrait
  phones; the transport marks are SVG paths in identically sized buttons,
  and play is outlined like the rest.

- **One source resolver, one day hook** ([#2087](https://github.com/kstrat2001/darkmux/pull/2087), closes [#2086](https://github.com/kstrat2001/darkmux/issues/2086)).
  `lib/source.ts` is the only place the viewer decides whether it is a
  static page or a daemon page; `hooks/useDay.ts` is the only loader for a
  day of records. The build-type branch left the lenses.

- **Guide front door and radio page** ([#2056](https://github.com/kstrat2001/darkmux/pull/2056), [#2051](https://github.com/kstrat2001/darkmux/pull/2051), [#2052](https://github.com/kstrat2001/darkmux/pull/2052)).
  The guide index and getting-started page are one captured session on a
  fresh home, with no frontier-orchestrator assumption; the radio page is
  distilled to what works, with verbatim captures; `acp --help` stops
  calling a shipped feature a spike.

### Fixed

- A fleet card tap on the demo lands on the machine's runs instead of an
  empty board ([#2064](https://github.com/kstrat2001/darkmux/pull/2064), closes [#2063](https://github.com/kstrat2001/darkmux/issues/2063)).
- Every run row on the demo opens ([#2066](https://github.com/kstrat2001/darkmux/pull/2066), closes [#2065](https://github.com/kstrat2001/darkmux/issues/2065)): mission rows
  gate on the captured graph, dispatch rows slice their session out of the
  committed day.
- The mobile playback view no longer jumps and flickers as events stream in
  ([#2069](https://github.com/kstrat2001/darkmux/pull/2069), closes [#2068](https://github.com/kstrat2001/darkmux/issues/2068)): the hero row wrapped by the digit width of one
  tile, the inspector resized on every followed record. Cumulative layout
  shift on a full replay went from 1.21 to 0.21.
- Fleet cards on the demo read their hardware from a committed snapshot
  instead of saying it was not reported ([#2070](https://github.com/kstrat2001/darkmux/pull/2070), closes [#2067](https://github.com/kstrat2001/darkmux/issues/2067)).
- Mission graph siblings no longer overlap, phase bands share a width, and
  the canvas fits its viewport ([#2059](https://github.com/kstrat2001/darkmux/pull/2059)).

## [3.2.0] - 2026-08-28

Radio becomes interactive help, and the front door stops lying.

The home page now promises three lines to a first answer: install, `init`,
`darkmux radio "do you have a brain?"`. This release is what it took to make
that promise true on a fresh Mac, plus the page itself. It was measured the
way a new user would meet it: ten questions a first-day user would type, run
against radio before and after, graded on one thing, whether the answer names
a command they can actually run. Before: 3 of 10. After: 9 of 10.

### Added

- **Radio is grounded in the full verb index** ([#2043](https://github.com/kstrat2001/darkmux/pull/2043)).
  The answering seat was handed top-level `--help` truncated at 1,600
  characters, so `serve`, `init`, `lab run inspect`, and `mission propose`
  did not exist as far as it knew, and it invented ids to fill the gap. It is
  now handed the whole command tree, walked from clap at call time: every
  runnable verb, its options, one sentence each, 64 lines. Capability is a
  lookup, not an inference. The index is the last generic section dropped
  under the bundle's hard cap, because for a help tool "how do I" grounding
  outlives "what am I working on" grounding.

- **`init` fills in the worker model** ([#2047](https://github.com/kstrat2001/darkmux/pull/2047), closes [#2038](https://github.com/kstrat2001/darkmux/issues/2038)).
  `init` wrote a registry whose worker profiles named `<your-worker-model-id>`,
  and nothing filled the blank, so every fresh install failed at its first
  dispatch with a message that blamed LM Studio. `init` now asks LM Studio
  what is downloaded and loaded, picks a loaded model if there is one (the
  operator chose it) or else the largest downloaded model under 60% of RAM,
  writes it into every placeholder slot, and says which one and where. A
  registry the operator has already edited is never touched. No `lms`, or
  nothing downloaded: the placeholder stays and the message names the fix.

- **The home page, rebuilt for the Apple Silicon push**
  ([#2035](https://github.com/kstrat2001/darkmux/pull/2035), [#2037](https://github.com/kstrat2001/darkmux/pull/2037), [#2039](https://github.com/kstrat2001/darkmux/pull/2039), [#2040](https://github.com/kstrat2001/darkmux/pull/2040), [#2041](https://github.com/kstrat2001/darkmux/pull/2041), [#2046](https://github.com/kstrat2001/darkmux/pull/2046)).
  "The AI runtime built for Apple Silicon." An origin story instead of a
  token-rent argument, a six-row comparison against the class of harness
  built around a frontier API, Get going third instead of last, every command
  on the page one tap to copy, two-sentence feature sections, and a Get going
  card that is one real session: two radio answers captured verbatim on an
  M5 Max, attributed to the model and the setting that produced them.
  Nothing on the page bounces to Substack; nothing names a competitor.

- **The event log is a collapsible mainstay on every tab**
  ([#2026](https://github.com/kstrat2001/darkmux/pull/2026), [#2025](https://github.com/kstrat2001/darkmux/pull/2025), [#2024](https://github.com/kstrat2001/darkmux/pull/2024)),
  with a filter badge that says how many events the filters are hiding and
  filters that survive a refresh. Console panel selection is in the URL.

- **A mark** ([#2020](https://github.com/kstrat2001/darkmux/pull/2020), [#2023](https://github.com/kstrat2001/darkmux/pull/2023), [#2031](https://github.com/kstrat2001/darkmux/pull/2031), [#2033](https://github.com/kstrat2001/darkmux/pull/2033)):
  the multiplexer, four channels in and one out, on the site, in the tab, and
  on the viewer's own masthead.

- **The demo world is committed** ([#2015](https://github.com/kstrat2001/darkmux/pull/2015)),
  so the screenshots the docs are shot from are reproducible, and the static
  demo renders its own data on every lens instead of a 404 page
  ([#2019](https://github.com/kstrat2001/darkmux/pull/2019), [#2021](https://github.com/kstrat2001/darkmux/pull/2021)).

### Changed

- **Radio ships at humor 50** ([#2045](https://github.com/kstrat2001/darkmux/pull/2045)).
  The default was 65, a value carried over from the author's own persona
  override, never chosen. Sampled on one question: under about 40 the model
  reads as plain, 50 is the first setting with a line in it, 100 is the full
  persona. One constant now, one test, so the number cannot drift across its
  copies again. `radio.humor` is the dial.

- **The answering seat gets a budget a reasoning model can use** ([#2044](https://github.com/kstrat2001/darkmux/pull/2044)).
  It was capped at the single-shot path's 4096 tokens, and a 35B thinking
  model spent exactly that reasoning about a one-line question and returned
  nothing. The seat now honors `runtime.max_tokens_per_call` and otherwise
  uses 16,384. An empty answer is a failure that names the budget and the
  knob, not a blank line and exit 0.

- **Tooling uses the window** ([#2016](https://github.com/kstrat2001/darkmux/pull/2016)):
  the two lens width caps are gone.

### Fixed

- **First-inference failures say the fix once and exit 1** ([#2042](https://github.com/kstrat2001/darkmux/pull/2042)).
  Probed with a fresh home for each case: no `init`, the placeholder model, a
  model not downloaded, the LM Studio server down, no `lms`. Every one printed
  the same error twice, ended on a command listing, and exited 0. One defect:
  a routing dispatch that could not run was recast as a model refusal, and
  the answering seat then failed the same way. `RouteDecision::Unavailable`
  separates "the model declined" from "the model was never reached." The
  messages are fixed at their source, so `dispatch` and `lab` get them too:
  the placeholder is named as a blank `init` left, a missing `lms` names
  `lms bootstrap` instead of guessing at RAM, a refused connection names the
  URL and `lms server start`.

- **Six guards that did not guard** ([#2027](https://github.com/kstrat2001/darkmux/pull/2027)),
  found by a five-agent QA pass briefed to falsify claims rather than walk a
  checklist: a sentinel-vocabulary parse that failed open on a comment, an
  isolation test that scanned lines instead of tags, a drift guard that passed
  vacuously when it could not see what it guarded, and three more. Every one
  was a check that passed when you planted the thing it existed to catch.

- **The event log collapses to the side** instead of blanking its pane, the
  collapse button no longer covers content, and the follow toggle shows its
  state ([#2029](https://github.com/kstrat2001/darkmux/pull/2029)).

- **A machine's label is its most recent name**, not the first one found in
  the stream ([#2030](https://github.com/kstrat2001/darkmux/pull/2030)).

### Schema

No data-shape changes. `FLOW_SCHEMA_VERSION` stays at 1.22.0 and
`CONFIG_SCHEMA_VERSION` at 1.11; the radio flow record's `decision` field
gained the value `unavailable`, which older readers pass through.

## [3.1.0] - 2026-08-27

Three fixes about darkmux telling the truth about what happened.

3.0.0 shipped a run-detail lens good enough to look at closely, and looking
closely is what found these. Each one is a place where the system recorded or
reported something other than what occurred.

### Fixed

- **Tool results are persisted, not just measured** ([#2007](https://github.com/kstrat2001/darkmux/issues/2007)).
  Every `tool.completed` record carried `result_chars` and threw the result
  away. A run's trajectory could tell you a tool returned 4,182 characters and
  not one of them. That is the same shape as the tool-args and session-record
  findings before it, and it is now the third time this project has discovered
  it holds a value, takes its length, and drops the value. The result now
  rides the record, capped at 64 KB.

  The cap **truncates rather than drops**, and it elides from the MIDDLE at a
  3:1 head:tail ratio. A tool result's two useful ends are the command that ran
  and how it finished; head-only truncation reliably discards the second one.

- **A red test is not a broken tool** ([#2008](https://github.com/kstrat2001/darkmux/issues/2008)).
  The failure-cascade detector classified any non-zero exit from `bash` as a
  tool failure. A model running a test suite under TDD, where a red suite is
  the expected result, would accumulate cascade signals and get told its tool
  "could not run" while the tool ran perfectly and reported exactly what it was
  asked to report.

  `ToolOutcome` replaces the boolean and distinguishes three states: the tool
  worked (`Ok`), the tool worked and the command it ran reported a non-zero
  exit (`Reported`), and the tool itself could not run (`Failed`, with a
  reason). Only the third feeds the cascade detector. The feedback template was
  rewritten to match: it now names what actually happened rather than asserting
  a falsehood the model can see is false.

- **A finished run stops running** ([#2011](https://github.com/kstrat2001/darkmux/issues/2011)).
  Two defects, one root: the run-detail lens had no way to learn a run had
  ended.

  Its wall clock rendered `close.ts - startTs`, the gap between two flow
  records as the *viewer* received them, while the run's own recorded `wall_ms`
  sat unread on the completion payload. The metric was never ticking; only the
  rendering was.

  And the view stayed RUNNING until a manual reload, because liveness comes
  from presence heartbeats and the presence key is deleted BEFORE
  `dispatch complete` is written. The poll that would have fetched the terminal
  record is exactly the poll that stops. A bounded grace window now holds
  polling open across that gap.

### Schema

`FLOW_SCHEMA_VERSION` **1.20.0 → 1.22.0** (two minor bumps, both additive).
`tool.completed` gained `result`, `outcome`, `exit_code`, and
`failure_reason`. Older readers ignore what they do not know; the viewer
renders pre-1.22 records the way it always did, since a record written before
the distinction existed cannot be re-interpreted after the fact.

## [3.0.0] - 2026-08-27

A major, and the reason is one rename.

The gate that decides whether a mission config may shell out on your behalf was
named for GitHub. The MECHANISM never was — `GhConfig`'s own doc already said
"GitHub never enters darkmux core … just a list of operator-chosen VERB NAMES",
and the check did nothing but compare a string a config declares against a list
you allowlisted. But the NAME is what people build on: a GitLab user was
allowlisting `mr-merge` under `gh.allowed`, and a config gating `terraform
apply` — which wants this gate exactly as much — had to declare a GitHub-shaped
field to get a check with nothing to do with GitHub.

Renaming it now cost one schema major and **zero migrations**, because no
built-in and no user document had declared the field yet. The same rename once
it has users costs a real migration. That is the whole argument for a major
release with an empirically empty blast radius.

Alongside it: the viewer stopped rendering in Courier for almost everyone, and
`doctor` stopped running off the side of the screen.

### BREAKING

- **`MissionConfig.gh_verb` is now `cmd`** — mission-config schema `2.3` → `3.0`.
  A document still declaring `gh_verb` is a loud validation **Error**, never a
  silent overflow into `extras`. That distinction is load-bearing: this gate
  fails OPEN for configs that declare nothing (correct — most configs touch
  nothing outside darkmux), so a document left on the old name would silently
  lose its gate and run the shell-out it was protecting as if you had approved
  it. Rename the field and set `schema_version` to `"3.0"`.
- **`gh.enabled` / `gh.allowed` are now `cmd.enabled` / `cmd.allowed`** — config
  schema `1.10` → `1.11`. `darkmux config set gh.enabled …` reports
  `unknown config key` with a suggestion rather than failing obscurely.
- **`DARKMUX_GH_ENABLED` / `DARKMUX_GH_ALLOWED` are now `DARKMUX_CMD_ENABLED` /
  `DARKMUX_CMD_ALLOWED`.**

`FLOW_SCHEMA_VERSION` stays at `1.20.0` — nothing on the wire changed shape, so
a peer on 2.12.0 still reads this machine's records.

### Fixed

- **The viewer rendered in Courier for anyone without JetBrains Mono installed**,
  which is nearly everyone: the CSS named that font 89 times and shipped no
  webfont for it. Measured with CDP, the alternate `ui-monospace, SFMono-Regular,
  monospace` stack resolved to Courier too — neither `ui-monospace` nor
  `SFMono-Regular` resolves in Chrome on macOS. One `--font-mono` token behind
  all 107 declarations, landing on Menlo/Consolas/Liberation Mono. No font is
  packaged; every family is already present on its platform.
- **The type was too small to read.** 115 of ~167 font declarations were 11px or
  smaller, against a 16px browser default. One `--fs-scale` knob behind 169
  declarations, as a uniform multiplier so every existing size relationship is
  preserved exactly.
- **`.machine-lens` was never centered** — `max-width` with no `margin-inline`,
  so every pixel the cap withheld pooled on one side: 16px of gutter left and
  224px right at 1440.
- **A count could be severed from its unit.** The limit-source strip is a flat
  text run, and at the wider type the browser broke *inside* a value, rendering
  `unpriced 0` on one line and `models` on the next.
- **The savings headline could overflow a phone.** Its width is data, and a fixed
  size assumes a digit count — at 320px the 9-digit total ran 31px past the
  viewport. Now fluid, so it cannot overflow at any total.
- **`darkmux doctor` ignored the width it was asked for.** `panel.rs` passes the
  client's measured width as `COLUMNS` and every other panel verb honors it;
  doctor emitted a 2031-character line at every width, overflowing the console
  panel by 533px at a 1440 viewport. Now wrapped with a hanging indent — the
  verdict banner too, which quotes the worst check's whole message and was
  therefore the single longest line it emitted.
- **The verdict banner is one line.** It had quoted the worst check's entire
  message — measured at 9,726 characters across 50 lines.
- **Identical registry findings state their explanation once.** 15 configs
  trailing the schema by one minor produced the same ~600-character paragraph
  fifteen times; it is one fact about fifteen documents, not fifteen facts.
- **The console asked for the wrong render width.** `panelCols()` divided by a
  hardcoded 7.2px per character, calibrated against the old 12px mono — at the
  new size a 1406px panel asked for 191 columns when 164 fit. It measures the
  element's own font now.

### Added

- **`DESIGN.md` covers the command gate and ACP.** ACP had no coverage at all
  despite being a shipped surface; the section names what is still spike-grade
  rather than papering over it.
- **Real screenshots in the guide**, which had zero images across ten pages, and
  a social card that is no longer two releases stale. Every capture is shot from
  a fixture fleet by `scripts/demo-env`, so a public page never carries
  hostnames, tailnet addresses or workspace paths — and so the whole set can be
  re-shot after a UI change.

## [2.12.0] - 2026-08-26

The run-detail lens, rebuilt around one question the page could not answer:
which of these numbers describe the model, and which describe darkmux around
it. Reading `model (lms)` beside TURNS / TOKENS / WALL CLOCK, there was no way
to tell — and the page was throwing away most of what it had been handed.

Also settles the work-unit vocabulary. `run`, `dispatch`, `step` and `session`
each denoted a grain and nothing said which, so every consumer picked its own
reading; `dispatch` in particular named both a top-level run kind and the
innermost unit. See `CLAUDE.md`'s contract registry entry 8 and `DESIGN.md`.

No schema bump: `FLOW_SCHEMA_VERSION` stays at `1.20.0`. Nothing on the wire
changed shape.

### Added

- **The prompt is readable.** It was held, measured, and discarded — the page
  rendered `prompt · 1430 chars` while holding the string. Now an expander,
  with the record's authoritative length so a truncated brief still reports
  its real size (#1984).
- **MODEL and SYSTEM metric panes.** Turns, tokens and context describe the
  model's work; wall clock, compactions and host load describe the system
  around it. A step that ran no model — a `procedural.shell` step — shows no
  model pane at all rather than `0 COMPACTIONS`, which asserts something that
  cannot happen there (#1984, #1996).
- **Host CPU / RAM / GPU peaks.** Already in the flow stream, fetched by the
  page and explicitly discarded. Peaks rather than latest, because the
  question asked of a finished run is whether it saturated the machine, and
  absent rather than zero when a run predates the sampler (#1996).
- **SIGNALS**, replacing `detections`. Grouped by kind with a count,
  severity-coded, and stamped with run-relative times. The severity was always
  in the payload and always thrown away, so a recovered stall rendered with
  the same warning glyph as a doom loop (#1985).
- **A clock that moves and a pulse that beats.** Elapsed time derived from the
  newest record's timestamp, so it advanced only when a record arrived and
  froze during exactly the stalls worth timing. A run silent longer than the
  watchdog's kill timeout is treated as abandoned rather than live (#1986).
- **`loaded models`**, naming the primary. The dispatch record already carried
  the resolved model; secondaries read `also loaded` rather than being guessed
  at by size or load order (#1991).
- **`#dispatch=<id>`** replaces `#session=<id>`, with `session=` kept as a
  one-release parser alias and rewritten to the canonical form on arrival.
  Every detail route is now named for the `RunKind` it opens (#1977).

### Fixed

- **Runs → Dispatch → a row now reaches the detail view.** A tracked dispatch
  mints a crew-of-one mission, so it routed to a single-node graph that showed
  less than the detail page and had no click handler (#1996).
- **A signal rendered one character per line on mobile.** A non-shrinking
  sibling starved the detail column to zero width; reported from a phone with
  1013 tests green (#1987).
- **Accessibility pass on the same lens.** The pill and the pulse could
  describe the same run as `RUNNING` and `finished` simultaneously; signal
  severity reached sighted users only; the metric pane names existed solely as
  CSS-generated content; a timestamp failed AA contrast; the pulse was a live
  region that could flap once a second (#1987).
- **A malformed detector payload is named, not mangled** — `undefined` as a
  signal heading, `[object Object]` where structured diagnostic data had been
  (#1992).
- **A malformed or clock-skewed timestamp no longer strands a finished run.**
  Either made a completed dispatch read RUNNING forever, and the first also
  erased the brief that would have explained it. A repaired timeline now says
  so instead of presenting itself as sound (#1993).
- **The detail page polls a live session** instead of fetching once, so turns,
  tokens and signals advance while a run is in flight (#1994).
- **A `StepKind` is asked for its dispatch session** rather than a consumer
  re-deriving it from kind strings with a silent fallthrough, and a conformance
  test pins each kind's answer by value (#1981).
- The runs board's status no longer reads a mission's liveness off a different
  mission's activity clock (#1981).

### Changed

- Denser layout throughout: the run brief flows as a definition grid instead of
  stacking six label/value pairs, and the metric tiles fill their row — a pane
  label spanning every grid track had prevented `auto-fit` from collapsing the
  empty ones (#1996).
- The mobile event list has a floor measured in rows rather than a fraction of
  the viewport (#1996).

[3.3.0]: https://github.com/kstrat2001/darkmux/releases/tag/v3.3.0
[3.2.0]: https://github.com/kstrat2001/darkmux/releases/tag/v3.2.0
[3.1.0]: https://github.com/kstrat2001/darkmux/releases/tag/v3.1.0
[3.0.0]: https://github.com/kstrat2001/darkmux/releases/tag/v3.0.0
[2.12.0]: https://github.com/kstrat2001/darkmux/releases/tag/v2.12.0

## [2.11.0] - 2026-08-26

### Added

- **`darkmux dispatch` envelopes now report what darkmux OBSERVED, not just
  what the model said** (#1955, #1958). The four pathology detectors wrote to
  no orchestrator-reachable channel — not stdout, not stderr, only a trajectory
  file in a temp dir — so a dispatch that tripped a cycle detector returned an
  envelope byte-indistinguishable from a clean one. The envelope now carries
  `detections` (always present, `[]` when nothing fired), `host` peaks, and a
  `checkpoints` reduction.

- **A `crawler` role and a `report_finding` tool** (#1959). The role scans a
  bounded scope for ONE named pattern and records each match structurally
  instead of narrating it in prose. The tool reads the cited line and its ±30
  surrounding lines **off disk itself** and records them alongside the model's
  rationale, so a downstream reviewer judges real source rather than the
  crawler's account of it, and "cite the line" holds by construction rather
  than by later check. Back-pressure runs on two levels: the return value tells
  the model how much budget remains, and a hard cap bounds it regardless.

  A citation that does not resolve — a missing file, a line past end-of-file, a
  quote that disagrees with the line — is REJECTED at report time with the
  actual line handed back, costs no budget, and never reaches an artifact. The
  realistic cause of a mismatched quote is a wrong line NUMBER, and silently
  recording the file's version would attach evidence the model never examined
  to a rationale describing different code.

  Findings are copied into the lab run directory beside the trajectory, and the
  envelope reports `findings: {count, path}`. That block's ABSENCE is
  meaningful: the file is created on the first successful call, so no block
  means the reporting channel was never used.

### Fixed

- **The per-turn-cap salvage no longer dispatches the tool call it truncated**
  (#1961). The #479 salvage counted well-formed tool calls but never filtered
  them. The cap lands mid-serialization, so the last call of a salvaged turn is
  routinely cut to `arguments: ""` — and all of them were dispatched. The empty
  call failing was harmless; the damage was that the unparseable argument
  string stayed in the transcript, and the model host answered the NEXT
  streaming request with HTTP 500. **A recoverable mid-turn truncation became a
  total loss of the run**, several turns later, with a 500 as the only symptom.
  Observed live: a dispatch ended at 67 seconds with no envelope at all.

- **The viewer shows what a tool call actually did again** (#1960). The React
  port kept the filter that depends on the per-activity icon map and left the
  icon map behind, and rendered no `tool_name` or arguments at all — so every
  tool call in the event log read "tool call" and nothing else. Restores the
  glyphs and the row preview (name, arguments, result size, failure marker),
  and adds a glyph for reasoning checkpoints, which the legacy map predates.

- **A live session drill-in no longer freezes** (#1960). The session query had
  no refetch interval and hardcoded itself as historical, so opening a RUNNING
  session fetched once and never again: no new events, and nothing derived from
  them could advance, while the fleet view kept moving. Liveness now comes from
  presence heartbeats.

- **`dispatch --json` no longer reports a container path the caller cannot
  open**, and the Redis startup banner no longer prints the connection address
  (#1957).

### Changed

- **`checkpoints.last_tail_ratio` is replaced by `min_tail_ratio` and
  `mean_tail_ratio`** (#1959). Reporting the LAST measurement did not merely
  fail to signal, it INVERTED: measured on two real crawls, a run that decayed
  through fourteen checkpoints to 0.193, tripped the degeneracy gate, and then
  recovered reported `0.997`, while a clean four-checkpoint run reported
  `0.976`. The degenerate run looked healthier on the field an operator reads
  first. `min` answers "did this run ever degenerate"; `mean` answers "how much
  of it was compromised", and neither substitutes for the other.

  **Breaking, for anything parsing the dispatch envelope:** `last_tail_ratio`
  is gone rather than deprecated, per the pre-1.0 no-compat-baggage policy. The
  only consumer is an orchestrator reading the envelope; nothing in the viewer
  reads it.

## [2.10.0] - 2026-08-24

### Fixed

- **The lab run root now has one resolver, and a `cargo test` no longer writes
  into your real run store** (#1882). Five call sites — `lab run`, `lab run
  list`, `lab run inspect`, `lab notebook draft`, and the review bench —
  resolved the lab root themselves instead of through
  `config_access::lab_dir()`. Two consequences, both live: test builds wrote
  real run directories into `~/.darkmux/runs` (251 had accumulated since July,
  and recent ones rendered as live `RUNNING` rows in the viewer), and
  `DARKMUX_LAB_DIR` / `config.dirs.lab` were ignored on the WRITE side while
  honored on the READ side, so runs landed in a root the reader never scanned.
  `DarkmuxPaths.runs` is now `pub(crate)`, making the bypass a compile error
  rather than a convention.

  **Behavior change, if you set `DARKMUX_LAB_DIR` or `config.dirs.lab`:** those
  verbs now write AND read under the configured root. Runs recorded before this
  release still live under `~/.darkmux/runs` and will not appear in `lab run
  list` until moved. Neither setting is written by `darkmux init` and
  `DARKMUX_LAB_DIR` is undocumented, so most installs are unaffected. The empty
  list now names the directory it actually scanned instead of always claiming
  `.darkmux/runs/`.

### Added

- **A thinking model's turn is no longer discarded when it reasons past the
  per-call bound** (#1221). This is the headline. A model that kept reasoning
  past `max_tokens_per_call` used to have its ENTIRE turn thrown away — a
  measured 43-50% of turns on the review corpus, including one 51K-character
  turn that was tracing real code and naming a real bug when it was cut.

  darkmux now CHECKPOINTS instead: at the bound it hands the model its own
  accumulated output back as an assistant prefill, so the model RESUMES rather
  than restarting, and a distinct-12-gram novelty ratio over the accumulation
  decides whether to hand it back with the think block still OPEN (keep going)
  or CLOSED (conclude from what you have). Many checkpoints in one thinking turn
  remain ONE turn, so `runtime.max_turns` keeps meaning what it says.

  The check-in is SILENT — the model is never told a boundary happened. That is
  not a stylistic choice: a model invited to wrap up wraps up, and measured on a
  real review it produced a four-point summary with zero findings where the same
  model uninterrupted found real ones.

- **`runtime.reasoning_checkpoint_interval_tokens`** — how far the model reasons
  between check-ins (`DARKMUX_RUNTIME_REASONING_CHECKPOINT_INTERVAL`, default
  1000). Deliberately separate from `runtime.max_tokens_per_call`, because the
  two want opposite values: a checkpoint interval wants to be small so a
  reasoning loop is caught early, an answer bound wants to be large so a long
  answer is not chopped. One number serving both is what the split fixed
  (#1221).

### Added

- **`darkmux mission config list` / `show <id>`** — a `role list`/`role show`
  equivalent for the mission-config registry (#1860). `list` enumerates every
  config id across the user → on-disk → embedded tiers `mission launch`
  searches, one row each with name, source tier, phase/task counts, and
  whether it's panel-advertised; a config that fails to load prints as a row
  naming the error rather than vanishing. `show <id>` renders the whole
  graph — every phase, task, and step, whether this binary can construct
  each step's kind (the identical check `mission launch` exits `4` against),
  and, per role, the profile + model it resolves to RIGHT NOW plus the
  resolution's provenance (a launch override, the `role_profiles` map, or
  `default_profile`) and whether that model is currently loaded, reusing
  `darkmux_gestalt::decide_residency` (#1274) for the residency verdict —
  the same ownership + ctx-sufficiency arbiter every real acquire path
  plans against — and `ProfileModel::require_n_ctx` for the same local-model
  gate every dispatch path applies, rather than re-deriving either. `--param
  <role>=<profile>` previews a planned override on the review route only
  (the only route `mission launch` itself applies it on); any other config
  gets the override neutered with a warning naming why, never a false
  parity claim. Read-only end to end; no new data or resolution logic, just
  a surface for what `mission launch`/`dispatch` already resolve silently.
- **Mission-graph parity goldens**: `tests/parity/mission-graph-goldens.spec.ts`
  captures frozen canvas- and timeline-mode text goldens from the standalone
  `/mission/:id/graph` page against a sanity fixture, so the graph lens's
  future port into the React viewer (#1868) has a spec to grade against
  before any of its own code changes. Dev/test infrastructure only; no
  runtime behavior changes. (#1868)
- **The mission graph is now a real lens in the React viewer** — `#mission=<id>`
  renders `MissionGraphLens` in-place (a React Flow canvas on desktop, a
  vertical timeline on phones), replacing the old redirect that navigated
  away to the standalone `/mission/:id/graph` page. Same node/edge/step
  vocabulary, the same live status/token/turn metrics fold, the same
  peer-machine-naming honesty on a 404 — now inside the same app shell as
  every other lens, with its events pane sharing `EventLogColumn` (the
  component every other lens's event log already uses) instead of a
  second, separate implementation. `reactflow` is now a real `ui/`
  dependency (bundled by Vite), matching the pinned version the standalone
  page's vendored bundle already used. (Superseded by the Removed entry
  below, landing in this same [Unreleased] window: an earlier version of
  this entry said the standalone page and its route would stay "unchanged
  in this release... until the port has had a release cycle to prove
  itself." That condition was never met — the port has shipped zero
  release cycles — and darkmux is pre-1.0 with no compat-baggage policy, so
  the retirement lands in the same cycle instead of waiting on one.) (#1868)

### Fixed

  A note on how these were found, because it is the useful part: the runtime
  suite was fully green through every one of them. What surfaced them was
  watching one real 66-call dispatch (which generated 26,181 completion tokens
  and delivered 1,116 characters, starting mid-sentence) and two review passes
  briefed to FALSIFY a named claim rather than walk a checklist.

- **A model that closes its own `</think>` no longer strands its answer**
  (#1221). The region tracker was built on a measured fact — under
  `response_format` the grammar forbids the model from emitting `</think>` — that
  turned out to be narrower than assumed: 17 of the 29 built-in roles declare no
  `output_schema`, and on those (`coder`, `code-reviewer`, `analyst`) the inline
  qwen-3.x family closes its own block freely. Everything after that delimiter
  was filed as reasoning and never delivered, and on a terminal turn the trailing
  scratch plus a dangling `</think>` shipped AS the answer.

- **The deliverable no longer depends on how the run happened to end** (#1221).
  A turn ending on `stop` and the same turn ending at a token cap produced
  different content. One rule now serves both: the answer region when the thought
  was closed (scratch is separable, so it stays out), the accumulation when it
  never closed (it is not separable, and shipping only the last slice is the
  discard-the-turn bug this feature exists to end).

- **A repeating answer is bounded, and is never deleted** (#1221). Degeneracy
  detection was disabled once the thought closed, leaving a post-conclude loop
  with no gate — measured at 337 checkpoints with no terminal reached, backstopped
  only by a SIGKILL that produces no envelope at all. Separately, a degenerate
  verdict used to DELETE the accumulation, and that verdict is wrong for whole
  classes of legitimate output (an enum-valued JSON array, a block of identical
  match arms, an ASCII table frame all score as degenerate). Repetition now stops
  the turn and escalates for handoff with everything banked attached.

- **An empty completion no longer discards the work already banked** (#1221).
  Losing five productive checkpoints because the sixth call came back blank was
  the same bug one layer down. This also covers the `finish_reason=tool_calls`
  with an empty array shape, which popped the message the fold had just written
  the whole accumulation into.

- **A response with no `usage` object no longer kills the dispatch** (#1221).
  It made every checkpoint boundary read as a context overflow, which is a hard
  error — no envelope, no metrics, no deliverable. "Cannot tell" now reads as a
  cap hit.

- **The event stream shows one turn per turn** (#1949). A single long reasoning
  turn wrote one `dispatch.turn` record per API call, so the metrics tile read
  `TURNS 1` while the stream showed 66 — and the stream is what an operator
  reads. Continuations emit `dispatch.checkpoint`, which is what they are.

- **The run card names its machine again** (#1949). The header rendered
  `(<session> on )` with a dangling "on" — a hardcoded stub, not a data gap.

- **"Model only" shows model work while it is happening** (#1945). The filter
  omitted `heartbeat`, so during a long first turn — 171 heartbeats and zero
  per-turn records — it showed an EMPTY list at the exact moment the most was
  happening. `checkpoint` joins it for the same reason.


- **A judge stage that ruled on most of a review's flags discarded all of
  it and posted "the review produced no signal."** A judge whose remote
  token budget exhausted before the whole docket was judged — 123 of 134
  flags ruled, 7 confirmed findings, 67 needs-check, all complete with
  evidence — set the same `degenerate` flag a genuinely dead judge sets,
  because the old gate treated ANY skipped call as fatal regardless of how
  much else was judged. `darkmux mission launch review` now treats a
  judge-stage skip as a coverage fact by default: the flags that WERE
  judged still render (inline comments, the summary fallback, everything),
  with a prominent banner naming the shortfall in the run's own numbers,
  posted as `mode: "partial"` — a CI check that posts and then fails,
  never a silent clean pass. The mission board and `darkmux mission
  status` now agree with what the PR comment says (a partial run reads
  `Degraded`, matching probe/verify exhaustion's existing treatment); the
  flow record's `dispatch complete` payload agrees too, flipping
  `result_class` from `"ok"` to `"partial"` so the same shortfall reaches
  the viewer, the Redis fleet stream, and the hash-chained audit sink,
  not just the posted comment. (The workflow's own CLI exit code was
  already, and remains, unaffected either way — `mission launch review`
  always exits `0`; CI-facing pass/fail has always come from the rendered
  payload's `mode` field, not the process exit status.) An operator who
  wants the old "any skip is fatal" behavior sets
  `review.judge_fail_on_any_skip` (env
  `DARKMUX_REVIEW_JUDGE_FAIL_ON_ANY_SKIP`), surfaced with provenance by
  `darkmux doctor`. Also note: a `judge-pass2`-only exhaustion (every flag
  WAS judged; some confirms were conservatively demoted to needs-check
  because their confirmation pass was skipped) now fails the CI check too
  — previously indistinguishable from a clean run, now correctly `mode:
  "partial"` like a pass-1 shortfall. That's the safe direction (a
  demotion-only run is real, postable signal with a real gap, same as a
  pass-1 shortfall), but it does change when the check goes red on a run
  where every flag was judged. (#1876, #1877)

- **The machine page's fit projection believed a number it had already
  disproved.** `potential` is the contract "the most this resident will ever
  hold", and it can be wrong: an idle MLX resident measured 28.40 GiB against
  a priced 22.88 GiB, steady to the byte across repeated samples, with the
  estimator's own arithmetic verified exact from the model's `config.json`.
  The projection summed the prices, so the fit figure was optimistic by the
  whole overage — in the one direction that makes an operator load another
  model. It now counts `max(potential, current)` per resident, and says so:
  the row carries a footnote naming the overage and what the projection now
  counts, and a warning carries both figures, because a silently-corrected
  estimate is one nobody ever fixes. (#1854)
- **The shrink hint promised savings a context reduction cannot deliver.**
  Cutting a resident's `ctx` lowers its price, but the projection floors every
  row at its measured footprint — so once the shrunken price drops under what
  the model is already holding, further cutting reclaims nothing. A fixture's
  hint promised 4.70 GB and delivered 4.12 GB. Found by running the hint and
  recomputing, not by reading it; the rounding cushion in the suggested `ctx`
  had always absorbed the difference. (#1854)
- **The margin tile printed the word "margin" twice** — `92 % margin` above a
  `MARGIN` label. A leftover from the #1821 rename, where the unit had read
  `% free` against that label and did not collide. Now a bare `%`, matching
  its two siblings where the unit is a unit and the label is the subject.
- **An unreadable figure could render as nothing at all.** The center readout
  shows `—` when neither memory source can be read; the new seven-segment
  cells had no glyph for it, so absence drew a blank hub instead of being
  visible as absence.

### Changed

- **The machine gauge no longer renders a verdict.** The `machine total
  GREEN` chip, and the fill's green/amber/red buckets at 50% and 85%, are
  gone. Both interpreted data the reader can already see, and the buckets'
  edges were thresholds darkmux invented — a machine at 84% and one at 86%
  are not different in kind. What remains is the arc, the needle, the limit,
  and the figures. The lamp row still reports server-declared *conditions*
  (pressure, over-limit, unpriced), which are facts rather than an assessment
  of whether the machine is doing well. Extends #1839's rule from `doctor` to
  the page: darkmux describes its own state; the reading is yours.
- **The gauge's color ramp is painted across the arc's sweep** — green at 0,
  amber at mid-scale, red at the limit — fixed to the dial and identical on
  every machine and every poll, with the filled band revealing its own slice.
  A band's color travel therefore also states its width. The stops are
  cosine-spaced, because a horizontal gradient interpolates along X while an
  arc advances by angle, so the mid-scale color lands on the mid-scale tick.
- **Seven-segment readouts** replace the boxed odometer digits on the center
  figure and the three pressure tiles. Boxed cells quote a mechanical counter;
  seven-segment quotes an instrument, which is what the rest of this page
  already is. Drawn as polygons rather than an embedded font, so the unlit
  segments render too — that ghosting is what anchors a narrow `1` in its
  cell. The pressure tiles carry a visually-hidden text copy of each figure,
  since the glyphs are decorative shapes.

- `LEDGER_SCHEMA_VERSION` **2.0 → 2.1** (minor, additive): `ModelRow.
  over_price_bytes` and `MachineTotals.over_price_models`. A 2.0 reader
  tolerates the payload unchanged, and a leniency test pins that a payload
  missing both keys still parses — a real path on a fleet where one machine is
  a release behind. `MachineTotals.potential_bytes` is now summed as
  `max(potential, current)` per resident: a value change inside an unchanged
  field, and the fix above.
- `FLOW_SCHEMA_VERSION` **1.19.0 → 1.20.0** (minor, additive): a new
  `"step timing"` action (#1877's final wiring step). The scheduler now
  streams one companion flow record per step, live, for every mission that
  runs through `run_step_graph`, including coder-phase, with no change to
  its own module. A pre-1.20.0 reader ignores the unknown action entirely;
  no struct/field change on any existing action. One transient
  fleet-visible effect: until every machine has upgraded past this build,
  `darkmux flow status` / `darkmux doctor` reports `schema_skew_detected`
  (the same live-stream version comparison every prior
  `FLOW_SCHEMA_VERSION` bump has produced) until the whole fleet catches
  up. Note the comparison is symmetric: `live_foreign` is any observed
  version that differs from the running binary's, in EITHER direction, so
  the warning appears on the machine that has NOT upgraded too. One
  upgraded machine writing a single 1.20.0 record to a shared Redis stream
  is enough to flip a still-on-stable peer's flow-sink health from Pass to
  Warn. The hint says to upgrade the lagging writer without naming which
  peer, so from the lagging machine's own seat the message reads as
  pointing at itself. It is a Warn, never a Fail, and it clears once the
  fleet is on one version.

### Removed

- **The legacy viewer (`crates/darkmux-serve/assets/viewer.html`, 319 KB)
  is deleted** (#1806), completing the UI transition #1800/#1804 started.
  Nothing served it after the route flip (#1800) moved `/` and
  `/play/:date` onto the React port; it survived only as the parity
  harness's extraction source and the reference the port's remaining 11
  `test.fixme`s (#1806's own list) named as blocking its removal. All
  eleven are now built and passing on the port — the filters/notes/about
  modal system and its focus trap, the machine lens's memory-ledger bars,
  a clickable affordance into a session view, the lab-run detail
  fallback-to-list, the lifecycle-drill tail, and the XSS walk's
  previously-unreachable surfaces — closing the gap #1806 measured.
  - `tests/parity/`'s legacy extraction path retires with it:
    `extract.spec.ts`, its dedicated `playwright.config.js` (served
    `viewer.html` with `darkmux-mode=live` injected), `redprove.spec.ts`,
    `verify-goldens.mjs`, and `determinism.mjs` are deleted, along with
    the `extract`/`rebaseline`/`verify`/`redprove`/`determinism` `package.json`
    scripts. **`goldens/*.txt` survives as a FROZEN spec** — what the
    legacy viewer actually rendered against a real daemon, captured once
    and now locked in — and the `next-parity*` suites keep grading the
    React port against those same files, unaffected. `record.mjs` and
    `tripwire.mjs` (now aliased as `check`) remain for capturing and
    scanning `corpus/` fixtures. Rebaselining a golden is no longer a
    regeneration script; it is a direct hand-edit of `goldens/<lens>.txt`
    in a reviewed diff, checked against real port output.
  - `crates/darkmux-serve/src/lib_tests.rs`: of the five Rust tests that
    `include_str!`'d `viewer.html`, one (`viewer_has_no_inline_event_handlers`,
    the general "no inline `on<event>=` HTML attribute" XSS guard) is
    retargeted at `next.html` — its premise holds for any served document,
    and React's synthetic event system emits no such attributes, verified
    empirically before the retarget (it is blind to an escaped-quote
    `onerror=\"…\"` inside a JS string, or a no-leading-whitespace
    `{onclick:"…"}` object literal — its practical value is guarding
    `ui/index.html` shell regressions and a stray `dangerouslySetInnerHTML`,
    not a general XSS proof). The other four
    (`viewer_has_no_raw_record_interpolations`, `live_tail_dedups_records`,
    `savings_hero_breakdown_is_classed_and_currency_free`,
    `wt_sum_panel_is_live_gated_and_escaped`) asserted on exact legacy
    source text — function names, variable names, hand-written `${...}`
    template syntax — that has no analog in a bundled, minified React app,
    and are deleted rather than retargeted. XSS/escaping coverage for the
    port lives in `tests/e2e/viewer-xss.spec.js`; live-tail dedup and the
    tokens-only savings-hero copy get their own port-shaped regression
    coverage instead (`ui/src/lib/flow.test.ts`'s `buildFlowWindow dedup
    (#794)` suite; `ui/src/lenses/fleet/FleetLens.test.tsx`'s tokens-only
    hero test). The wt-sum panel is **not ported** — `ui/src` has no
    consumer of `GET /worktree-summary/:session_id` at all (the daemon
    still routes it; nothing in the React port calls it), so there is no
    port-side behavior for a test to cover.
  - The legacy file's source is recoverable with
    `git show v2.9.0:crates/darkmux-serve/assets/viewer.html`.
- **The standalone mission-graph page is deleted** (#1868 third packet),
  completing the arc this same release's mission-graph lens (Added, above)
  started: `crates/darkmux-serve/assets/mission-graph.html` (~2,100 lines of
  hand-written `React.createElement` JS) and its vendored bundle
  (`crates/darkmux-serve/assets/vendor/` — React + ReactDOM + reactflow as
  one minified IIFE, plus the upstream MIT `LICENSE-*` files) are gone.
  Their MIT notices already travel with the artifact that reaches users
  independently of that directory: `ui/vendor-licenses/LICENSE-reactflow`
  (added alongside reactflow becoming a real `ui/` dependency) is prepended
  into `next.html` the same way react/react-dom/@tanstack/react-query's
  notices already were, verified byte-identical to the deleted copies
  before removal.
  - `GET /mission/:id/graph` is now a **308 permanent redirect** into the
    port's own `#mission=<id>` hash route (`/#mission=<id>`) rather than a
    second HTML document — every bookmark and shared link minted against
    the old path still lands on the mission's graph, now rendered inline by
    `MissionGraphLens`. `GET /mission/:id/graph.json` (the data endpoint)
    and the `/vendor/reactflow-bundle.min.{js,css}` routes: the JSON route
    is unchanged; the vendor routes are deleted along with the bundle they
    served.
  - `tests/e2e/mission-graph-*.spec.js` (8 specs) are deleted — superseded
    by the `mission-lens-*.spec.js` suite #1871 already shipped against the
    ported lens (plus one new mobile-legend spec with no legacy analog).
    The `.served/mission-graph.html` + `/vendor/*` harness wiring in
    `tests/e2e/playwright.config.js` that existed only to serve those specs
    is removed with them; the lens specs already run against the port's
    own `index-live.html`.
  - `tests/parity/mission-graph-goldens.spec.ts` (the capture suite that
    recorded `goldens/mission-graph-{canvas,timeline}.txt` from the live
    standalone page) is retired, its own config and `package.json` script
    removed — same shape as `viewer.html`'s own extraction harness
    retirement above. **The two goldens it captured survive as a frozen
    spec**, same as `viewer.html`'s: `next-parity-graph.spec.ts` (shipped in
    this same release, see Added above) keeps grading the ported lens
    against them, byte-for-byte on the DOM regions kept identical on
    purpose. Rebaselining either is now a direct hand-edit, not a
    re-capture. Recoverable with
    `git show v2.9.0:crates/darkmux-serve/assets/mission-graph.html` and
    `git show v2.9.0:tests/parity/mission-graph-goldens.spec.ts`.

## [2.9.0] - 2026-08-16

A remediation release. An audit of every user-facing surface asked one question
— does darkmux describe its own state, or does it render verdicts on yours? —
and found that in several places it did the second. It also found 323 KB of
MIT-licensed code shipping with no attribution. Those are the release.

### Fixed

- **`darkmux config get <secret>` told you a secret was not in your config
  without opening the file.** It short-circuited on the known secret keys and
  answered "darkmux never stores it in config.json". That claim was made
  without looking, and it is false in a reachable state: `config set` refuses
  to write these keys, but a hand-added one is preserved verbatim by the
  lenient whole-file writeback on every subsequent `set`. So the one command
  you would run to check whether a secret leaked into your config actively
  reassured you it had not. It now reads the file and reports presence either
  way — and when the key IS there, says to remove it and treat the value as
  exposed.
- **`darkmux config list` printed it.** A hand-added secret went straight to
  the terminal, defeating the entire point of the Keychain carve-out. Secret
  keys now render as `(redacted …)`; the KEY still shows, because its presence
  is exactly what you need to know. Fixed in the shared reader, so the radio
  answering seat's config grounding is covered by the same change.
- **`darkmux doctor` stopped adjudicating your setup.** Three strings went:
  "Safe as-is for a single machine" (conditionally true, and false for the
  reverse-proxy setup the guide recommends — the check never looked at the
  proxy), "Password-less is fine for a local/Tailnet-trusted Redis" (a verdict
  on an unverified condition, in the hint of a Warn, i.e. telling you the
  warning was safe to ignore), and a clause volunteering a compliance
  interpretation of a dropped audit write. What each check reports about
  darkmux's own configuration is unchanged.
- The `serve daemon auth` check is renamed **`serve daemon token`**. Both arms
  return Pass by design — loopback-only with no token is the ordinary
  single-machine state — but a check that named a security concern while being
  structurally incapable of any other status read, inside `● ok — every check
  passed`, as a security check that had cleared. It never checked that.
- **The always-on hub guide was wrong in two places.** It said a plain LAN
  substitutes for Tailscale; it does not — the password-less Redis posture in
  that guide depends on the network being an authentication boundary, and a
  reader following it would have ended up with an unauthenticated Redis on
  whatever network the machine joined. And it recommended enabling automatic
  login under a "harden the OS" heading, which is a security-weakening step
  (it defeats FileVault across a reboot) presented as hardening.
- The Homebrew formula's caveats claimed `flow integrity-check` surfaces "any
  post-hoc edit". It does not, by SECURITY.md's own account — tail truncation
  and whole-file deletion are undetectable. That text prints in
  `brew info darkmux`. `SECURITY.md`'s supported-versions table also still
  said `1.x (current)`.

### Legal

- **The built viewer ships the MIT notices for the code it embeds.** `next.html`
  bundles react, react-dom and @tanstack/react-query and is compiled into the
  binary, served at `GET /`, and republished on the website — and it carried
  **zero copyright notices**, because the minifier strips `@license` banners.
  MIT requires the notice to travel with copies. The notices are now prepended
  at build time from vendored license texts, and the build FAILS if that
  directory is missing rather than silently shipping unattributed code. The
  mission-graph bundle had the mirror-image gap — React's banners present, no
  React Flow notice at all — now fixed, with the prepend written into its
  rebuild recipe as a named step.

### Added

- **A peer's darkmux version now rides the presence heartbeat**, so it is
  readable over the shared Redis without that peer's HTTP daemon being
  reachable at all. Presence already carried the flow-schema version for the
  same reason; this is the other half of the same question, and it was the
  half that went missing exactly when it was most wanted — a hub that is up
  and heartbeating but that nothing can reach. A peer on an older build
  reports no version rather than failing to parse.


## [2.8.0] - 2026-08-15

The machine page's numbers were wrong, and now they are not. Per-model memory
was read from a counter (`ps rss`) that does not count MLX weights at all, so
the gauge reported **271 MiB held while three loaded models held ~25 GiB** —
understated roughly 97×, with a green verdict beside it. That single defect,
and everything it touched once the real number was visible, is almost the
whole of this release; the gauge itself was also redrawn twice along the way
as each fix changed what there was to look at.

### Breaking

- **`LEDGER_SCHEMA_VERSION` 1.1 → 2.0** (#1821). Anyone parsing
  `/machine/resources` or `darkmux machine resources` output directly is
  affected:
  - **`warnings: string[]` is replaced by `messages: {severity, text}[]`**
    (`info` / `warn` / `error`). The old field rendered every entry the same
    amber regardless of whether it was a real degradation or a note about how
    a figure was derived; severity is now explicit instead of implied by
    which field it landed in.
  - **`pool.available_bytes` changed meaning.** It used to be the truly-free
    page count (`vm_stat` "Pages free"); it now means the colloquial "how much
    is left" (free + inactive + speculative). The old meaning is preserved
    under a new name, **`pool.free_bytes`** — if your integration wants the
    old number, read that field instead.
  - **`pressure.memory_free_percent` is renamed `pressure.margin_percent`.**
    It was always a 0–100 kernel pressure reading (`kern.memorystatus_level`),
    not a byte count, and the old name read as one next to the pool's byte
    figures.
  - **New, additive fields**: `pool.used_bytes` (Activity-Monitor-style: wired
    + compressor + (active + inactive − purgeable)); `machine.other_used_bytes`
    and `machine.projected_total_bytes` (what everything *besides* darkmux is
    holding, and what the machine would total if darkmux's own
    committed-but-unmaterialized models fully load); `potential_source` per
    model (`"arch"` for a measured estimate, `"estimated"` for the size-tiered
    fallback below, omitted when a model has no potential at all) and
    `machine.estimated_models` (counted separately from `unpriced_models` —
    an estimated resident is priced and does not block a green verdict; only
    a resident with no arch facts *and* no catalog size still forces the
    machine to unknown).

  The new fields are purely additive and ignorable by an old reader.
  `warnings` and `memory_free_percent` are gone from the payload outright — a
  reader keyed on those exact names gets nothing back and needs to move to
  `messages` and `margin_percent`. `available_bytes` is the sharper case: the
  field is still present under the same name, but its **value now means
  something different** — a reader that kept using it silently gets the
  colloquial figure instead of the truly-free one it was reading before. Move
  to `free_bytes` for the old meaning.

### Added

- **An unpriceable resident gets an estimate instead of blocking the machine's
  verdict forever.** Pricing a resident needs its `config.json` (hidden layer
  count, KV heads, head dim); a GGUF download carries that architecture inside
  the binary instead of a sidecar file, so one such resident — even a small
  one — forced the *entire* machine's fit verdict to `unknown` permanently,
  regardless of how comfortably everything else fit. A fallback estimator now
  prices those residents from catalog size alone, selecting a KV-cost rate
  tiered to size (larger dense models get a higher per-token rate, since a
  flat rate under-reserved exactly the large downloads most likely to hit this
  path). The estimate is disclosed everywhere the verdict appears — a dashed
  `ESTIMATED` chip on the model row, an `info`-severity message, the CLI table
  and its `~`-prefixed POTENTIAL column, and a new `darkmux doctor` check
  naming any resident that is still genuinely unpriceable after the fallback
  (no arch facts and no catalog size), with the fix (load an MLX build of the
  same model when one exists). **Known limit, stated rather than hidden:** the
  size-tiered rate under-reserves pre-GQA multi-head models such as
  Llama-2-13B, where KV-head count equals attention-head count instead of a
  small fraction of it — no size-derived rate can catch that shape. The
  estimator is now the *second* fallback rather than the first: see the GGUF
  header reader below. (#1819, #1823)
- **A GGUF resident is priced from its own architecture, read out of the
  binary.** The estimate above is a floor, not an answer — so darkmux now
  parses the GGUF metadata header directly for the same three facts a
  `config.json` would carry. Resolution order is `config.json` → GGUF header
  → size-tiered estimate → genuinely unpriceable, and a GGUF-derived row
  reports `potential_source: "arch"` because it is a measurement like any
  other. Verified against a real 9 GB `phi-4-Q4_K_M.gguf`: 40 layers, 10 KV
  heads, head dim 128, matching the published config exactly. Only the header
  is read, never the tensor data — parsing that file costs ~7 ms. It also
  prefers the header's own `key_length` over deriving head dim from embedding
  size, which is what keeps models like gemma-4-E4B correct (its derived
  value would be 320 where the true one is 512). Every file-supplied length
  and count is bounds-checked before it can allocate or loop, and any
  malformed, truncated or ambiguous file declines to a labeled estimate
  rather than failing. **Known limits:** GGUF v1's differing wire format is
  unsupported (v2/v3 only); GGUF carries no per-layer attention-pattern
  field, so hybrid-attention models are assumed dense — an overprice, the
  same safe direction the estimator chose. (#1820, #1831)
- **The viewer's dialogs are back: filters, notes, and about.** The React
  viewer shipped without them — the event log offered a one-shot "model only"
  quick filter in place of the real checkbox-per-facet modal, a named cut
  rather than a half-build. All three dialogs now exist on a shared shell with
  managed focus: Tab cannot walk out of an open dialog, Shift+Tab wraps,
  Escape closes it, and focus returns to whatever opened it. The old viewer
  failed the first of those — 31 Tab presses from an open filter panel landed
  on the page header, underneath an opaque backdrop, so a keyboard user was
  operating controls they could not see. Session and mission drill-in routes
  land alongside them. (#1640, #1829)

### Changed

- **The machine page reads in binary GiB, so its numbers match the machine you
  bought** (#1811). A 128 GB MacBook Pro was rendering its own memory ceiling as
  `137.44 GB`, and the gauge inherited it — labeling its arc `0 · 34 · 69 · 103
  · 137` on the one screen whose whole job is telling you how much room you
  have. Every figure in the memory ledger and on the gauge face now divides by
  a power of two and is labeled `GiB`/`MiB`: the arc reads `0 · 32 · 64 · 96 ·
  128`, the pool reads `128.00 GiB`, and the ` (128 GiB)` parenthetical that
  used to patch the mismatch is gone along with it. The stage header's own RAM
  figure keeps its `GB` label for now — it was always computed in binary, so
  it now agrees numerically; only the suffix still differs.
- **The gauge is a stacked band, not a single needle over one number.** It
  used to fill from darkmux's own committed memory alone, against a scale
  ending at the machine's *whole* RAM — so a near-empty darkmux on an
  87%-full machine still read green, because the fill never accounted for
  what anything else on the machine was holding. Two intermediate redesigns
  (a color-only fix, then a pair of concentric rings) were each superseded
  once the real problem was visible: the dial now stacks darkmux's own
  memory, everything else on the machine, and darkmux's committed-but-not-yet-
  materialized growth (hatched) in one band, so the sum — the actual "will it
  fit" question this page exists to answer — is legible at a glance instead
  of requiring cross-radius mental arithmetic. The needle lands on the
  machine's real current usage; the center readout shows that same figure
  (labeled `MACHINE USED`), not darkmux's share of it; the fill color follows
  the machine's overall state, not darkmux's alone; and a legend pairs each
  band with the figure it represents. The always-on `IN USE` caption is gone
  — the caption now appears only when it has something to say (which disjunct
  put the machine in red), the same way the per-row state chip and the
  now-deleted `darkmux/utility` card were quieted.
- **The `darkmux`/utility block moved, then was deleted outright.** It first
  moved from the top of the page (config given priority it hadn't earned)
  to below the ledger, then — on a closer look — was cut entirely: it
  described what the utility tier is *responsible for*, a property of
  configuration rather than of this machine's memory, and every fact it
  carried already existed elsewhere (the model's own ledger row, or
  `darkmux doctor`). What survives is a single neutral `utility` badge on
  that resident's own ledger row. (#1818)
- **The pressure tiles' explanatory notes are behind an `(i)` popover** instead
  of always-on 8.5px text at the bottom of the page — readable on request
  instead of illegible by default, and rewritten while there: the memory-free
  reading is now named as the only figure that can trigger red, and is a
  0–100 pressure reading rather than a byte count; the compressor note now
  spells out that it is macOS's own memory compressor, not darkmux's
  compaction. The `STATE` lamp — a second, less-informed copy of the verdict
  the machine chip already carries — is deleted. (#1822)
- **The per-row `UNKNOWN`/`ESTIMATED` state chip only renders where a row's
  state actually disagrees with the machine's overall verdict**, instead of
  stamping every row with the same word regardless of whether that row was
  the reason for it. On a healthy or uniformly-unknown machine, no row
  carries it at all. (#1818, #1819)

### Fixed

- **Per-model memory now reads what a model actually occupies.** `ps rss`
  does not count MLX model memory at all — MLX places weights in
  Metal/IOAccelerator buffers, which only `phys_footprint` sees; llama.cpp's
  GGUF weights are memory-mapped as evictable file-backed pages, which only
  `rss` sees. Neither counter alone is correct for the mix of backends
  darkmux actually runs. Per-worker memory is now `max(rss, phys_footprint)`,
  with both raw figures kept in the payload so the number is checkable rather
  than trusted. Workers are also now paired to models by weight size rather
  than by projected potential (potential includes context that may not be
  materialized yet, which could rank two residents in the wrong order and
  swap their reported figures).
- **The machine's memory is decomposed into real, distinct quantities instead
  of calling three different things "free."** There was no machine-wide
  "used" figure at all, so the operator's own read of darkmux's usage stood
  in for the machine's; the truly-free-pages percentage and the kernel's own
  pressure-margin percentage — both plausibly "how much is free" — differed
  by 51 percentage points (31% vs 82%) on the same screen, for the same
  machine, at the same instant. See **Breaking** above for the field-level
  detail.
- **The fit verdict now accounts for everything else running on the machine**,
  not just darkmux's own commitment against the machine's total capacity. A
  machine with tens of GiB held by other processes could previously read
  green as though those processes did not exist; the green/amber cascade and
  the amber shrink-hint's own arithmetic now key off the projected total
  (everything else, plus darkmux's own commitment), not darkmux's commitment
  alone.
- The gauge's aria label no longer announces a fabricated "0% full" when the
  current reading is unreadable — it now reports the reading as unreadable,
  the same thing the visible dial does.
- A popover tile note no longer pushes the rows below it down the page when
  opened, and the gauge's needle no longer stops short of the band it is
  meant to point at.

## [2.7.0] - 2026-08-14

The release where **the new viewer becomes the viewer**. `/next` graduated: the
React port now serves `/` and `/play/<date>`, it runs with no daemon behind it
at all, and the machine page stopped being a wall of numbers.

### Added

- **The React viewer is now what darkmux serves.** `GET /` and
  `GET /play/<date>` render the port; `/next` becomes a permanent redirect to
  `/` rather than disappearing, so every bookmark, phone shortcut and tailnet
  link minted during the port keeps working. The gate was a number, not a
  judgement: 21 of 22 goldens recorded from the legacy viewer asserting real
  byte parity in a real browser. (#1800)
- **The viewer runs with no daemon.** It reads the static-source metas a
  daemon-less build injects (`darkmux-flow-src`, `-runs-src`, `-lab-runs-src`),
  parses the committed flow file directly, and suppresses every live poll —
  which is what makes darkmux.com/demo the real viewer rather than a fork of
  it. The demo now also DERIVES which viewer it ships from the daemon's own
  source instead of naming a file, so it can never again silently lag the flip.
  (#1801)
- **The machine page is an instrument.** It became the residency room it was
  always described as — its runs list moved to the runs lens — and gained a
  real gauge: a semicircle reading current against the limit, a tell-tale lamp
  row, odometer cells for the monotonic pressure counters, and a redline keyed
  on the server's own state rather than a threshold invented in the browser.
  Unpriceable models render with **no** committed extent rather than a
  zero-width one, and `unknown` is a designed state instead of a blank.
  (#1806, #1809)
- **The runs lens takes a machine pin** — `#lens=runs&machine=<uid>`,
  composable with the kind filter, with a clearable chip naming the machine.
  A fleet card for a remote machine now drills straight there. (#1508, #1809)

### Fixed

- **A machine's identity is its uid, not whichever name it logged under.** One
  machine carries several `machine_id`s over its life — the hostname's short
  and `.local` forms, or a rename — so every check that asked "is this machine
  me?" by matching names failed on a machine with two aliases. It classified
  itself as remote, hid its own residency ledger, advised viewing the machine
  page on the machine you were already using, and reported "hardware not
  reported" for its own CPU. Four sites, one rule. (#1809)
- **A daemon-less build no longer polls a daemon.** The static demo opened an
  SSE stream and hit `/fleet/machines/live` every five seconds indefinitely,
  showing "reconnecting" on a page with nothing to reconnect to. (#1801)
- **The machine page's runs link no longer claims a count it cannot know** — it
  read "0 runs" while its destination listed 282, because the two counted
  different things over different windows. (#1809)
- **A stale reading survives an unreachable daemon.** An errored poll discarded
  the last good snapshot, so the stale banner was unreachable code and the
  figures vanished exactly when the daemon blinked. (#1812)
- **A navigation that changed destination as data loaded.** The local fleet
  card briefly routed to the runs lens before specs resolved — a blink on
  loopback, longer over a tailnet, and silent either way. (#1809)
- **The demo's icons and manifest 404'd under its subpath**, and the machine
  page printed `hw.memsize` twice in two byte conventions (`128 GB` and
  `137.44 GB` are the same number). (#1811)

### Notes

- The end-to-end viewer suite now grades the **shipped** viewer rather than the
  legacy one it was written against — 71 passing, with 8 kept as `test.fixme`
  naming behaviors the port does not have yet rather than deleted (#1806).
  The legacy `viewer.html` deliberately stays on disk as the reference
  implementation for exactly those, and is unreachable at runtime.
- `FLOW_SCHEMA_VERSION` is unchanged at 1.19.0 — no cross-machine schema lock
  needed for this upgrade.


## [2.6.0] - 2026-08-12

The release where you can **talk to darkmux**. Two new front doors — an agent
panel inside your editor, and plain-English routing onto your own commands —
plus a rebuilt viewer and a materially harder audit chain.

### Added

- **radio — say what you want instead of memorizing verbs.** `darkmux radio
  "what PRs are open"` routes free text onto exactly ONE advertised command and
  runs it, printing the route it chose before executing so the choice is never
  silent. Two seats do the work: a small local model classifies, a larger one
  answers when nothing matches. A message that doesn't clearly map onto one
  command **refuses and lists what's available** rather than guessing — a wrong
  refusal costs you a step, a wrong route runs the wrong command. `--dry-run`
  shows the route without executing. (#1698)
- **darkmux as an ACP agent — the Zed agent panel.** Your commands appear as
  slash commands in the panel, run as real missions, and stream back into the
  thread. Includes an operator sign-off gate on steps (fail-closed by default),
  cancellation, session pruning, and an idle exit. (#1388, #1684)
- **PR-flow panel verbs.** Author `/pr-list`, `/pr-view`, `/pr-comments-list`,
  `/pr-comment-resolve`, `/pr-approve`, `/pr-merge`, `/pr-ship` as ordinary
  mission configs: a per-verb `gh` allowlist (`gh.enabled` + `gh.allowed`, both
  fail-closed), a sign-off dialog carrying real CI and review facts, and a flow
  record for every executed verb. darkmux holds no GitHub credential — every
  verb shells out to your own `gh`. (#1685)
- **The viewer, rebuilt in React, behind `/next`.** Every lens ported with a
  parity harness as its executable spec; the legacy viewer at `/` is untouched.
  Adds a drill-in level: machine detail (local and remote), lab-run detail, and
  a per-session run view. Plus a staleness marker, so an empty panel is never
  silently empty. (#1665)
- **`flow integrity-check --strict`** — exit 3 when a file could not be
  content-verified at all, kept distinct from exit 2 (a real chain break).
  "Verified" and "could not verify" are different claims, and a cron keyed on
  the exit code can now tell them apart. (#1775)
- **A second reference bundler plugin** (`darkmux-bundler-edge`) alongside the
  `--bundler` escape hatch, for reviewing languages the built-in TypeScript
  bundler doesn't read. (#1686, #1757)

### Fixed

- **The review bundler silently dropped code from its excerpts.** Changed lines
  with no enclosing function were skipped entirely, so review seats reasoned
  about a window that was missing the code under review — and reported honestly
  about what they were shown while the pipeline promoted it into claims about
  the file. Also fixes a second, hidden size cap that stopped large functions
  from ever being located. (#1751–#1756)
- **The audit chain now hashes the stored bytes** rather than a re-serialization
  of a parsed record, closing a confirmed bypass where a record carrying an
  unrecognized enum value skipped content verification entirely and its other
  fields could be rewritten while the chain still validated. (#1768, #1769)
- Mission board is recent-first by default. (#1713, #1717 for radio's grounding)
- Viewer mission/run aggregations read the whole fleet stream, not only the
  local machine. (#1705)
- Accessibility and styling passes on `/next`, including keyboard-operable
  controls and a real identity (favicon, touch icon, manifest).

### Changed — read before upgrading

Both of these degrade gracefully: nothing errors, and no action is required.

- **The audit hash format changed** (`prefix-blake3-v1`). Files written by
  2.5.x and earlier are still READ, and reported honestly as legacy — but their
  content is not re-verified, because recomputing the old format would repeat
  the lossy round trip that made the bypass possible. `--strict` (above) is how
  you make that visible to automation. (#1772)
- **The orchestrator provenance field is removed** — the `DARKMUX_ORCHESTRATOR`
  env var, the `orchestrator` config field, and the flow-record field. It was
  stamped from machine-scoped config to describe an invocation-scoped fact, so
  every record on a machine carried the same value regardless of what actually
  drove it, and nothing read it. A stale export is now a no-op; an old config
  still loads. (#1758)

### Documentation

- README rewritten as a landing page (8,716 words → 978, nothing deleted).
- A full guide page for radio, and one for the PR-flow verbs.
- **Every audit and privacy claim a user actually meets was re-checked against
  the code and corrected.** darkmux describes what its mechanisms do and names
  their known gaps; it makes no claim about anyone's compliance with any
  regulatory framework. See `SECURITY.md` for the audit chain's limits, stated
  plainly.
- The MIT disclaimer now names both copyright holders, so it covers the
  distributor as well as the author.


### Added

- **`darkmux-bundler-edge`** — the second reference `--bundler` plugin: zero-dependency Python, Edge.js template spans + differential facts (interpolations, directives, class-attribute churn) + cross-template manifests, proving the frozen `--bundler` contract (#1319) at N=2 — a second language, for a template DSL rather than a systems language (#1686).
- **PR-flow panel-verb machinery** (#1685): a per-verb `gh` allowlist (`config.gh.{enabled,allowed}`, `darkmux doctor` provenance, `MissionConfig.gh_verb`), a flow-record audit entry (`action: "gh.verb.executed"`) per executed verb, and `--param args=<value>` now delivers into a config's `reads: ["__panel_args__"]` task from a direct `darkmux mission launch <id>` the same way it already did from the ACP panel (previously CLI-only launch hard-failed `interpret` for any config using that convention). GitHub never enters darkmux core: `pr-list`/`pr-info`/`pr-approve`/`pr-merge` are operator-authored example `procedural.shell` configs documented in the new [PR-flow guide](docs/guide/pr-flow.html), not built-ins.

## [2.5.1] - 2026-08-11

Hotfix off the v2.5.0 tag. The review bundler was silently dropping code from
the excerpts it hands to review seats, so reviews were reasoning about a window
that omitted the very lines under review.

### Fixed

- **Top-level changed lines reached a seat instead of vanishing.** A changed
  line with no enclosing function was skipped entirely when building the
  excerpt, so imports, constants, type aliases and module-level statements
  never reached a reviewer. (#1751–#1756)
- Unchanged context lines are no longer bundled as though they were top-level
  code, which had the inverse effect of padding excerpts with untouched lines.

## [2.5.0] - 2026-08-06

The honesty release. Nearly every fix here is one defect wearing different
clothes: **something was lost or wrong, and nothing said so.** A dead run that
read `running` forever. An errored dispatch that reported `ok: true`. A green
review with zero findings and no explanation. A config knob that was settable,
typed, documented, and read by nothing. A fleet boot test that takes 63 seconds
when it actually runs, reporting `ok` in eight milliseconds. The tests meant to
catch these had their own version of the same problem — passing for reasons
other than the ones they named.

### Breaking

- **Mission-config schema 2.0 — the `expand` primitive is retired** (#1550).
  Its only consumer moved to explicit per-role tasks in 2.3.0, leaving a
  declared, documented, unfeedable field. Removal is the breaking part: because
  `TaskConfig` carries a `#[serde(flatten)]` overflow, a config still declaring
  `expand` would have **parsed perfectly and silently lost its fan-out** — no
  error, a graph quietly missing tasks. `validate` now emits an **Error**
  naming the field, the schema that removed it, and the migration. Lenient on
  read, loud at validate. See **Migration** below.
- **A dangling `role_profiles` binding is now an error** (#1547). It used to
  fall through to `default_profile` in silence, so a typo'd role id bound
  nothing and looked fine.
- **The serve daemon's auth gate fails closed** (#1663). A request that carries
  no peer address is treated as **remote** (token required), not as loopback.
  It previously did the opposite, which meant the entire remote gate rested on
  a single wiring call in `run()` guarded by nothing but a comment: downgrade
  it in any refactor and every peer looks like loopback, serving flow records,
  machine specs, mission state, and worktree summaries unauthenticated — with
  every test green and nothing visible to the operator, because the viewer
  keeps working either way. That same refactor now produces 401s on the first
  remote request instead of silence. **Loopback-only installs are unaffected**
  — with no token configured the gate isn't in the stack at all.

### Migration — mission configs

After upgrading, `darkmux doctor` warns once per user-tier mission config whose
`schema_version` is a 1.x major. The configs still **load and run**; the warning
is about interpretation drift, not breakage. Two cases:

1. **Your config does not declare `expand`** (the common case — it had no way to
   be fed since 2.3.0). Migration is the version line alone:

   ```bash
   # what still needs migrating
   grep -L '"schema_version": *"2' ~/.darkmux/mission-configs/*.json

   # any config that genuinely uses the retired primitive — handle these by hand
   grep -l '"expand"' ~/.darkmux/mission-configs/*.json

   # bump the rest
   for f in ~/.darkmux/mission-configs/*.json; do
     grep -q '"expand"' "$f" && { echo "skip (declares expand): $f"; continue; }
     python3 - "$f" <<'PY'
   import json, sys
   p = sys.argv[1]
   d = json.load(open(p))
   d["schema_version"] = "2.0"
   json.dump(d, open(p, "w"), indent=2)
   open(p, "a").write("\n")
   print("bumped", p)
   PY
   done

   darkmux doctor    # confirms clean
   ```

2. **Your config declares `expand`.** Replace the template task with the tasks
   it used to fan out into, written explicitly — one task per expansion, each
   naming its own `role_id`. The built-in review config's probe stage is the
   reference shape (`templates/builtin/mission-configs/review.json`), and
   `darkmux doctor` names every offending file and field until it's done.

Nothing else in `~/.darkmux/` needs touching; `config.json`, `profiles.json`,
and the flow/audit stores are unchanged by this release.

### Added

- **`Task.reads` — a run-scoped output ledger** (#1619). A task can read any
  completed task's output without a dependency edge being drawn between them,
  so cross-phase data flow stops requiring cross-phase arrows in the graph.
  The review config's judge/verify/synthesis stages moved onto it.
- **Mutation testing and coverage in CI** (#1635) — the suite could not audit
  itself. Mutation runs on the PR diff every time and sweeps the workspace
  nightly; coverage reports which lines never execute.
- **Browser fixtures generated from the wire types** (#1637), so a viewer test
  and the Rust struct it renders cannot drift apart.
- **The fleet e2e suite actually runs** (#1662). All six `e2e_*` binaries opened
  with a `redis_available()` guard whose false arm printed and **returned** — a
  silent pass — and no workflow ever installed redis. The daemon boot test that
  takes 63 seconds locally was completing in eight milliseconds on CI, which is
  this project's merge gate: the whole fleet layer was guarded by nothing while
  loudly reporting that it was guarded. They now run against a real redis on an
  ubuntu runner, and a missing redis in CI is a hard failure rather than a skip.
  Finding this required compiling darkmux on **Linux**, which had never once
  happened because every workspace-compiling job was macOS. It did not compile.
  Now it does.

### Fixed

- **Runs, missions, and phases agree on what "alive" means** (#1642, #1633,
  #1621, #1632). One liveness decision now serves all three run kinds; a dead
  lab run stops reading `running`; and the generic launcher closes its phases
  like the bespoke ones always did.
- **A benign-empty review no longer reads as retry-worthy** (#1605, #1654). A
  PR with nothing reviewable produced the same signal as a broken run, so the
  session waiting on it would re-run — and since the input is unchanged, that is
  an unbounded retry loop. The PR comment now leads with the fact (the bundler
  ran and worked as expected; re-running will produce the same result), and the
  board reads `Clean` rather than `Degenerate`. An **error**-empty run still
  reads `Degenerate` — the carve-out narrows what gets flagged, never what gets
  recorded.
- **URL userinfo is stripped from route labels** (PR #1661). A route label rides
  to public artifacts; two of its three construction paths left sanitizing to
  the caller and would have carried an endpoint credential into one. The
  chokepoint now strips it regardless of who calls.
- **`mission_id` is stamped at the producer** (#1641) rather than inferred
  downstream, and `mission abort` announces the terminal it actually writes
  (#1660) — it printed `→ Finalized` while storing `Aborted`.
- **Three dead operator surfaces became real** (#1548, #1547, #1550):
  `runtime.feedback_injection` had no reachable off switch — no accessor host
  side, and the host never forwarded it into the container, so both tiers were
  inert.
- **A user-tier mission config newer than the binary now warns** (#1648)
  instead of being read with fields silently dropped.
- **Viewer**: a peer's mission says where it ran instead of 404ing (#1466); the
  session drill-down is addressable (#1639); the mission-graph header stopped
  crediting unattributable tokens to local (#1626); a teardown stopped reading
  as a success (#1627, #1628); plus liveness, silent truncation, mobile input
  zoom, and the first keyboard tests (#1640).
- **Docs state the backend truthfully** (#316) — darkmux drives LMStudio, and
  only LMStudio. The prior claim of "LMStudio + Ollama + llama.cpp" was
  aspiration, and a fresh agent session reading it would confidently propose
  work against backends that do not exist.

### Fixed — caught by this release's own dogfood

The release gate requires verifying each feature live rather than trusting a
green suite. Running it turned up two false statements, both of which shipped
would have been this release's own theme happening to it.

- **A posted review no longer claims a runner it has no evidence of** (#1676).
  The footer's provenance clause has four cases; three are derived from the
  envelope's member records, and the fourth — *no member records at all* —
  returned the fixed string "on a self-hosted runner". Launching a review from
  a laptop shell put that sentence into a public comment. The clause is now
  omitted entirely when there is nothing to derive it from, which is what the
  function's own documentation had claimed all along. #1298 fixed the three
  evidence-derived cases after a footer falsely claimed "no cloud API" about a
  cloud review; this fourth one survived because the no-dispatch path was rare
  until #1605 made benign-empty a normal outcome that posts a comment.
- **`darkmux doctor` no longer prescribes a model load that is unnecessary and
  namespace-breaking** (#1675). The unloaded-utility-model warning said
  compaction would fail without a manual load and suggested a bare
  `lms load <id>`. Since #1616 the dispatch path loads the compactor itself, at
  its declared context, under the `darkmux:` namespace — and a bare `lms load`
  creates precisely the un-namespaced resident darkmux will not reuse and
  `machine eject` cannot reclaim, so following the advice could cause the
  problem the namespace exists to prevent. The check remains; its remedy now
  states only what is true.

## [2.4.0] - 2026-08-03

The observability release: CLI panels in the browser, a mission board that says
what a mission IS, and a batch of producer-side defects that made the data
underneath all of it quietly wrong.

### Added

- **CLI panels in the viewer** — a `console` lens rendering real `darkmux`
  command output as styled DOM, served from an allowlisted `GET /panel/:id`
  (#1569 packets B1-B3). The CLI is the single source of truth; the viewer
  renders it rather than reimplementing it, which is the twin-drift that
  #1561 already was.
- **OSC 8 terminal hyperlinks** — mission ids in `mission status` are
  clickable through to the viewer (#1569 packet A).
- **One runs lens** — the nav catches up to `/runs`, four tabs collapse to
  three (#1584).
- **Phases render as containers** in the mission graph rather than sibling
  cards (#1594).

### Changed

- **The mission board row says what a mission IS** (#1612). It led with the
  mint id (`dispatch-code-reviewer-1785589698-5d6a-0`), which on a phone ate
  two thirds of the width for the least informative thing on the line. Rows
  now carry a name (from the description, populated on every real mission and
  previously unshown), a graph-vs-single-role glyph, the id's own short
  discriminator, and an age. The id stays one click away on the row's link,
  and any row needing an id typed still prints it verbatim beneath.
- `mission status` defaults answer a question instead of enumerating the
  store (#1569), and drift suggestions survive a paste (#1582).

### Fixed

- **A dispatch could unload a model the OPERATOR owned** (#1609). The preflight
  matched residents on the bare model key, and `lms ps` reports darkmux's copy
  and a hand-loaded copy with the same key — so on the sanctioned duplicate
  path it took whichever came first, which is yours. Ownership now means "the
  identifier this profile declares", which also honors the documented
  `identifier` opt-out.
- **A namespaced `internal.utility` binding made the compactor unloadable**
  (#1615). The namespace is a load-time decoration, so a prefixed string can
  never resolve as a model key — the load failed AND the residency check never
  matched its own resident. Compaction fell through to a JIT-load at the model
  default with truncated summaries, on the path that exists to save long
  dispatches. Verified live: the compactor now loads namespaced.
- **A starved judge grant deleted a real finding, silently** (#1610). A grant
  too small to hold a ruling produced a truncated response that read as "no
  finding" — and because the call "succeeded", no degraded gate fired. Now
  denied and counted. The same floor was missing from the `dispatch.map`
  bucket the probe stage rides.
- **A newer peer's flow record read as chain corruption** (#1611). An unknown
  enum value failed the whole record, and the audit checker reported that as a
  broken chain — a false tamper alert on the compliance substrate. Records are
  now lenient on read, and a record this binary cannot content-verify is
  reported as unverifiable-pending-upgrade rather than as evidence of
  tampering. Chain linkage is still enforced across it.
- **`dispatch.map` emitted no liveness bookends** (#1607), so a production
  path doing model work was invisible while it ran. Now RAII-guarded, terminal
  on every exit path. The savings hero also counted hosted tokens as "off the
  meter"; cloud, local and unknown are now distinguished honestly.
- **A review read `0/N` for its entire run** (#1620). Phases started lazily and
  never closed, so every touched phase sat Running until the mission finalized
  and reconciled them in bulk — `0/3` on a mission whose judge was working,
  indistinguishable from one that never started. Phases now close at their
  earned outcome as the run advances.
- **The phone asked for more columns than it had** (#1613, #1614). A 390px
  screen fits ~52 columns; both ends of the negotiation floored at 60, so nine
  columns hung off the right edge. Machine-scope status also wrapped below the
  tab strip, where a global line read as the selected tab's content.
- **The mission graph's last phase read as an empty lane** (#1618) — an
  invisible container border (1.2:1 against the background), task columns
  indented by global rather than per-phase depth so later phases ran
  off-screen, and a minimap that overflowed its viewport.
- 247 lab runs were invisible because `lab_dir` had no config tier (#1585);
  the review summary fallback that never fired (#1583); `dispatch.map` session
  ids (#1524); plus seven audit fixes from a crate-by-crate sweep (#1595-#1601).

### Notes

- `FLOW_SCHEMA` stays 1.18.0. The `#[serde(other)]` catch-alls are additive
  and no record is ever written carrying one, so existing audit chains survive
  without rotation.
- The release dogfood ran a real long-agentic coder dispatch to convergence
  (`wall=798s, verify=pass`) and confirmed #1615's compactor load live.
  Compaction itself did not trigger in that run (0 compactions in 47 turns —
  the `deep` profile's threshold is 131k tokens and per-turn context stayed
  well under it), so the path downstream of the load is unexercised by this
  release's dogfood.


## [2.3.1] - 2026-07-29

**A degraded Redis can no longer take the dashboard down with it**, plus three
fixes so the viewer stops claiming things it cannot back up. Patch release; no
schema changes (FLOW `1.18.0`, CONFIG `1.5`, MISSION_CONFIG `1.3`).

### Fixed
- **An unhealthy Redis wedged the viewer.** Only the *connect* phase was
  bounded, so a peer whose TCP port accepted but which never answered a command
  blocked the read until the route's 30s timeout returned `408` — and the
  local-file fallback was never reached, because a hang is not an error. A
  response deadline now bounds the command itself. Measured on a real
  unreachable-but-accepting hub: the viewer's two-day boot fetch went from
  `0.45s + 30s/408` (hung) to `3.06s`, both `200`.
- **A recovered Redis erased history.** Redis results replaced the local file
  wholesale, but Redis is not a superset — it rides a `MAXLEN` cap and is
  missing everything written while it was unreachable. So the outage window
  vanished from the view the moment Redis came back. The two sources are now
  unioned. Measured: 1080/1080 local records served, zero dropped.
- **The viewer asserted PLAYBACK before it knew its mode**, flashing a scrubber
  on every live load that it would never use.
- **A dropped live connection was invisible and inescapable.** It now shows
  `reconnecting`, refetches automatically when the connection returns or the
  page wakes, and has a refresh control — the escape hatch for the
  home-screen app, which has no address bar and no pull-to-refresh.
- **The idle headline hid its own recency.** "last run 18h ago" was suppressed
  past one hour to avoid looking stale, which inverted: the headline then read
  "ready" with no time reference at all, indistinguishable from a fleet that
  had never dispatched.

[2.11.0]: https://github.com/kstrat2001/darkmux/releases/tag/v2.11.0
[2.10.0]: https://github.com/kstrat2001/darkmux/releases/tag/v2.10.0
[2.9.0]: https://github.com/kstrat2001/darkmux/releases/tag/v2.9.0
[2.8.0]: https://github.com/kstrat2001/darkmux/releases/tag/v2.8.0
[2.7.0]: https://github.com/kstrat2001/darkmux/releases/tag/v2.7.0
[2.6.0]: https://github.com/kstrat2001/darkmux/releases/tag/v2.6.0
[2.5.1]: https://github.com/kstrat2001/darkmux/releases/tag/v2.5.1
[2.5.0]: https://github.com/kstrat2001/darkmux/releases/tag/v2.5.0
[2.4.0]: https://github.com/kstrat2001/darkmux/releases/tag/v2.4.0
[2.3.1]: https://github.com/kstrat2001/darkmux/releases/tag/v2.3.1

## [2.3.0] - 2026-07-28

**Composable mission graphs.** Step kinds are now stateless singletons in one
registry, launchers route on what a graph *declares* rather than what it is
*named*, and every pipeline produces its own input inside the graph instead of
in a bespoke pre-launch prelude. The practical upshot: you can store review
variants and launch them by name, and a graph can be extended from either end.

No schema-version bumps (FLOW `1.18.0`, CONFIG `1.5`, MISSION_CONFIG `1.3`, all
lenient-on-read) — a 2.2 install upgrades in place, and a 2.2 peer stays
wire-compatible.

### Added
- **Named mission-config variants launch by name.** Store
  `~/.darkmux/mission-configs/review-lean.json` (fewer probe seats, different
  judge passes, different models per role) and run `darkmux mission launch
  review-lean`. Launch routes on the step kinds a config declares, not on a
  hardcoded id, and the launched document is the one that executes.
- **Per-seat probe prompts are live.** `review-probe-high.md` / `-mid.md` /
  `-low.md` now drive their own seats, each falling back to `review-probe.md`.
  Previously all three files existed and were silently ignored — editing one
  did nothing. Byte-identical by default, so this changes nothing until you
  edit one.
- **Graph composition is checked before anything runs.** A graph whose step
  kinds require a run-scoped artifact nothing supplies now fails up front,
  naming the artifact, the kinds that need it, and how one is supplied —
  instead of panicking mid-run after the mission had already minted.
- **`demo-quickstart` ships in the binary.** The first command the quickstart
  documents (`darkmux lab run demo-quickstart`) previously failed "not found"
  for anyone who installed via Homebrew or `cargo install`.

### Changed
- **Errors on the composition surface name the fix.** A coder-phase graph
  missing one of its three required steps, a review config whose probe task
  doesn't lead with its render step, or a renamed step the launcher locates by
  id — each now reports what is wrong, which id it looked for, and the template
  to copy, rather than aborting with a bare panic.
- **The review pipeline's degenerate-run message names reachable causes.** It
  previously told you to check a per-seat `selector` and a "probe expansion" —
  both of which had become unreachable, sending you after knobs that no longer
  exist.
- **`darkmux doctor`'s mission-config finding** no longer says these documents
  don't execute. They do; a finding there is a config that will fail at launch.

### Fixed
- **`mission finalize` / `mission abort` targeted the wrong worktree** for a
  coder-phase config launched under any id other than `coder-phase` with a
  custom `workdir` — the run itself was correct, but no `Task.workdir` was
  persisted, so the terminals fell back to the derived path.
- **Posted PR review comments no longer carry local absolute paths** or raw
  stderr from an external `--bundler` plugin. The full detail stays in the
  envelope, flow records, and local output.
- **A bundling failure is no longer silent** — it previously printed nothing
  and exited 0, leaving the cause only inside the emitted JSON.
- **Prompt-building steps no longer show a token meter** in the mission graph.
  They dispatch no model, so the idle `· tok` placeholder — which means "this
  step spends tokens, just not yet" — was a false signal on every probe seat.
- **A URL with inline credentials no longer leaks its userinfo** through a
  seat's recorded endpoint host.
- **`ProfileModel.capabilities` was documented as inert; it is not.** Model
  selection scores against it, so populating it changes which model a role
  dispatches to.
- **The dashboard was slow to load AND could show stale data** — the viewer is
  now served with `Cache-Control: no-cache` plus an `ETag`, so a reload
  revalidates in zero bytes instead of re-fetching ~256 KB, and can never serve
  a stale page. Previously it carried no cache metadata at all, leaving the
  browser to choose between the two.
- **41% of local tokens were counted but never classified.** Map-dispatched
  work (the review probe and verify seats) reported only a total, so the fleet
  card's `generated` / `fresh input` / `re-read` chips silently under-reported
  against their own headline. The per-call prompt/completion split now travels
  with the result. Providers that report only a total still leave the split
  absent rather than claiming a zero.
- **The 24h activity window read `11:38:41–11:38:41`** — a time-only label
  can't distinguish two instants exactly a day apart. The range now carries the
  date when the window straddles a day boundary.

[2.3.0]: https://github.com/kstrat2001/darkmux/releases/tag/v2.3.0

## [2.2.0] - 2026-07-25

### Added
- **`/runs` aggregator** (#1523) — one flat, kind-tagged read-model unioning missions + lab runs + flow into a single per-request view (the data layer for the upcoming Runs lens). Read-side union only; no new persistence.
- **Unified machine page** (#1522) — the machine tab and fleet-drill converge on one lightweight page: a live residency/RAM health region plus a runs list, honest about local-vs-remote ("not reported from here" for a machine probed elsewhere).

### Fixed
- **Remote runs render honestly** (#1518) — a run served off-fleet on a hosted endpoint (e.g. Azure) no longer shows the box's incidental local LMStudio residency as the run's model; route + model resolve from the run's endpoint.
- **Concise review comments** (#1528) — the per-finding "needs frontier verification" note now renders once on the verdict line instead of repeating on every finding.
- **Review runs stamp `mission_id`** (#1523) — the review pipeline's dispatch bookends now carry their mission id, so a review appears as exactly one row in `/runs` (no spurious untracked-ghost duplicate) with its route on the right row.

[2.2.0]: https://github.com/kstrat2001/darkmux/releases/tag/v2.2.0

## [2.1.0] - 2026-07-22

**Config-composable review + concurrency-safe residency.** The review pipeline
stops being hard-coded and becomes pure config-composed primitives — you declare
the graph, darkmux runs it — and the residency arbiter learns to evict stale
orphans and protect a concurrent command's in-use model, so heavier cross-family
crews run without a manual `machine eject`. A `darkmux dispatch` is now a
first-class run through the same engine as missions. No schema-version bumps
(FLOW `1.18.0`, CONFIG `1.5`, MISSION_CONFIG `1.3`, PROFILES `1.5`, all
lenient-on-read) — a 2.0 install upgrades cleanly.

### Added
- **Reconcile-to-need residency + a concurrency-safe lease registry** (#1487) — the residency arbiter (`darkmux-gestalt`) now evicts darkmux-owned models a dispatch's staffing doesn't need (no more monotonic growth), and protects a model a *concurrent* darkmux command is mid-dispatch on: each command writes a per-pid lease at the load chokepoint (`~/.darkmux/residency/<pid>.lease`), read by every other command's reconcile so a busy model is never yanked. `lms ps` stays residency truth; the lease is a subordinate busy-overlay. Heavy cross-family crews (a 62 GB probe + a 35B judge) now load and run without a manual `machine eject`.
- **The review pipeline is config-composed** (#1513) — the probes are now explicit one-role tasks in `review.json`, not a hard-coded three-seat template. The probe COUNT is config-driven (compose a lean one-probe review for a constrained-RAM tier), step kinds are swappable per step, and there is no "probe role" concept in the code — a probe is just a role on a task with a probe step, emergent from your composition. Staffing is the generic machine-local `role_profiles` map (the same one judge/verify already used).

### Changed (behavior — surface preserved)
- **A mission run is a first-class record** (#1503) — each `mission launch` mints a **unique run id** (matching the lab's `run_id` convention), and the old input-hash that was the id is demoted to a queryable `spec` fingerprint for grouping. Re-launching a spec now **mints a fresh run** (history preserved for comparison) instead of reopening the prior one — AI runs are non-deterministic, so input-hash-as-identity was a category error. The mission-id scheme changes from `<config>-<inputhash>` to a minted run id; existing missions on disk load unchanged.
- **`darkmux dispatch` routes through the engine as a crew of one** (#1510) — a dispatch is now a full Mission→Phase→Task(role)→Step at cardinality one, so it mints a first-class run, emits mission/step flow records (it shows up in `mission status` and the missions lens), and — the load-bearing part — participates in the #1487 lease/reconcile regime, closing a gap where a raw dispatch could be evicted by a concurrent mission. **The `--json` output contract is byte-identical.**
- **`k` (probe draw-multiplication) retired** (#1513) — one role = one task = one dispatch; recall breadth is varying the *set* of probe roles, not drawing one model `k` times. `darkmux lab eval`/`review-bench --k>1` is now a typed rejection pointing at the config, so a recall sweep can't silently produce a flat-but-mislabeled series.

### Fixed
- **Phase↔step terminality invariant** (#1504) — a phase can never be persisted `Complete` while a step it contains is still live, and a launch that fails after minting reconciles to a terminal `Error` instead of stranding an Active mission. Enforced at the single `lifecycle.rs` chokepoint, with a loud warning when the backstop actually fires.
- **The darkmux self-review workflow** (#1515) drops the deleted roster-profile/seat-pin/`k` params and staffs from the `role_profiles` map — matching the current model.

### Migration (2.0.0 → 2.1.0)
No schema bumps; a 2.0 install upgrades in place. The behavior changes to know:
- **A dispatch is now a run.** `darkmux dispatch` appears in `mission status`, the missions lens, and the flow stream, and writes a `~/.darkmux/missions/dispatch-*` record. The `--json` output is unchanged.
- **Re-launching a spec mints a fresh run.** The prior run is left on disk for analysis; there is no implicit reuse/reopen. Mission ids are now minted, not input-hashed.
- **`--k>1` is rejected.** Vary the probe-role set in the review config for recall breadth.
- **Review configs**: `review.json` now declares explicit probe tasks — a lean N-probe review is a config edit (delete/add tasks). Staffing is the `role_profiles` map; per-run overrides remain `--param review-probe-high=<profile>` (and `-mid`/`-low`/`review-judge`/`review-verify`).

## [2.0.0] - 2026-07-18

**darkmux 2.0 — the mission orchestrator.** The 1.x line grew a swap tool into a
dispatch tool into a review pipeline; 2.0 unifies all of it under one model.
Config-defined **missions** launched with `darkmux mission launch <config>` run
as a live **Task/Step dependency graph** on a real scheduler, with concurrent
local dispatch bounded by a residency arbiter that loads exactly what each seat's
staffing declares — and a React Flow **mission-graph lens** that draws the graph
light up as it runs. The PR-review funnel and the coder pipeline both became
missions; residency (the founding profile-multiplexer) became an internal
capability underneath, not a verb the operator drives. The verb surface shrank
hard, the openclaw shell-out path is gone, and review seats staff from a
machine-local role→profile map. This is a **major** release: many verbs were
renamed or retired without deprecation shims — see **Migration** below.

### Added
- **`darkmux mission launch <config>`** — configs mint mission instances; `mission propose` emits configs; the whole pipeline runs as a real **Task/Step DAG** with a dependency-graph scheduler, concurrent local dispatch, and per-mission `MissionEnvelope` finalization (#1284, #1230, #1352). Built-in configs: `review` (bundle → probe → dedup → judge → verify → synthesis) and `coder-phase` (worktree → coder → verify).
- **Mission-graph lens** — a live React Flow Phase/Task/Step diagram is now THE mission view: direct nav + standalone shell, per-seat model chips + step metrics, an events panel, a mobile vertical timeline, refresh/reconnect, a minimap toggle, live turn/tool-call metrics for agentic seats, and **phase→phase order arrows** (#1384, #1403, #1404, #1431, #1485, #1491, #1497).
- **`darkmux dispatch <role>`** as a top-level verb (promoted from `crew dispatch`) — one role, one turn, through the internal Docker-bounded runtime; `--image <tag>` runs the seat in any Linux environment you name (#1435, #703).
- **Residency arbiter (`darkmux-gestalt`)** wired as the production resource planner — grow-only additive acquisition, wave scheduling, an architecture-aware KV estimator, and a deadline-bounded `lms` load/unload adapter, all scoped to the `darkmux:*` namespace so user-loaded models are never touched (#1230 packet 1, #1274/#1276).
- **`darkmux doctor` staleness checks** — a running daemon vs. the installed binary vs. the source tree vs. the runtime image, so a stale-binary "bug" is caught structurally (#1461).
- **`darkmux-bundler-rust`** — the reference `--bundler` plugin: Rust function-boundary scanning + differential call-site facts, so the review funnel can bundle a Rust diff with real context (#1319).
- **Mission staleness / dead-dependency drift detection** in `mission status` — an Active mission stalled at zero complete phases, or a phase permanently unreachable behind an abandoned dependency, is surfaced with a reconcile suggestion (#1230 packet 5).

### Changed (breaking)
- **Review seats staff via a role→profile map** — crews dissolve; each seat (`review-probe-high`/`-mid`/`-low`, `review-judge`, `review-verify`) resolves to a profile through a machine-local `role_profiles` map in `config.json`, with a single `--param <role>=<profile>` per-run override (`Overridden` provenance). The roster-scoring resolver and seat pins are gone (#1475, #1438).
- **Verb collapse** — `dispatch` and `machine` are the new top-level families. `swap`, `status`, `recommendations`, and the old `fleet` family fold into `machine` (`machine list/add/remove/status/eject`); the `profile` family merges; `notebook` joins `lab`; `lessons` becomes `memory` (`memory lesson` / `memory correction`); `review-bench` becomes `lab eval`; `--crew` becomes `--roster-profile` (#1426, #1430, #1435, #1437, #1462, #1470).
- **Mission lifecycle** — `mission run` collapses into `mission launch`; `mission ship` retires; `mission close` → **`mission finalize`** (which now reconciles its phases honestly from step statuses); the standalone `phase` verb family retires; `MissionStatus::Closed` → **`Finalized`** (existing "closed" records read lenient) (#1439, #1463, #1468, #1498, #1406).
- **`WorkJob` schema bump** — the `deliver` and `runtime` fields retire; a version-first mismatch now errors loudly instead of mis-parsing (#1440).
- **`FLOW_SCHEMA` 1.17.0 → 1.18.0**, **`CONFIG_SCHEMA` → 1.5**, mission-config schema, all additive / lenient-on-read.

### Removed (breaking)
- **The openclaw shell-out dispatch path** — `--runtime openclaw`, the per-dispatch `--runtime-cmd`, and `crew sync` are gone. The internal runtime reads role manifests directly and is the one and only dispatch path (#1405).
- **`GET /diff/:session_id`** and the viewer's live-diff panel (#1387) — replaced by `GET /worktree-summary/:session_id`, a numbers-only endpoint (`files`/`adds`/`dels`/`base`/`path` from `git diff --numstat`, never diff content) that rides the general remote-read gate. The session view renders a "what changed" totals line + the worktree path with a copy button and an (inert-by-default) `zed://` anchor.
- **`optimize scaffold`** and **`doctor --fix`** — two dead surfaces (#1419).

### Fixed
- **The mission graph tells the truth mid-run** — parallel seats show elapsed units + a model chip + per-seat completion (no more "all-or-nothing" wall-clock), planned steps show no phantom tokens (server-side fold gates on step-started), phase status rolls up from its tasks instead of a stale persisted value, and an error/untracked-mission state keeps the nav reachable (#1488, #1493, #1472, #1496).
- **A 0-member probe stage fails loud with per-seat reasons**, routed to a degraded run, not a silent Clean (#1486).
- **Liveness guards on the utility dispatch paths** (propose, phase review, timeout narration) + metering on the `dispatch.single_shot` hosted arm (#1413, #1412).
- **The 2.0 upgrade path** — `darkmux init` prunes retired skills and refreshes its managed doc blocks; 2.0 identity swept through source-embedded strings, clap `about`, and init templates (#1467, #1449).

### Migration (1.18.x → 2.0.0)
No deprecation shims — old verbs error rather than silently mis-run. The map:

| 1.x | 2.0 |
|---|---|
| `darkmux crew dispatch <role>` | `darkmux dispatch <role>` |
| `darkmux crew sync` | *(gone — internal runtime reads role manifests directly)* |
| `darkmux swap <profile>` | *(gone — gestalt loads what each seat's staffing declares)* |
| `darkmux pr-review run` | `darkmux mission launch review` |
| `darkmux mission run` / `mission ship` | `darkmux mission launch coder-phase` |
| `darkmux mission close` | `darkmux mission finalize` |
| `darkmux notebook draft/list` | `darkmux lab notebook draft/list` |
| `darkmux review-bench` | `darkmux lab eval` |
| `darkmux lessons …` | `darkmux memory lesson …` / `memory correction …` |
| `swap` / `status` / `recommendations` / `fleet` | the `machine` family |
| `--crew <profile>` | `--roster-profile <profile>` |
| `--runtime openclaw` / `--runtime-cmd` | *(gone — internal runtime only)* |

**Review staffing config.** 2.0 review needs a `role_profiles` map in
`~/.darkmux/config.json` binding each seat to a profile — e.g.
`{"review-probe-high":"…","review-probe-mid":"…","review-probe-low":"…","review-judge":"…","review-verify":"…"}`.
`darkmux init` on 2.0 writes the block; `darkmux doctor` surfaces an unstaffed seat.

**Data.** Existing `~/.darkmux/missions/*` load unchanged (mission status
`"closed"` reads as `Finalized` via a serde alias; `closed_ts` reads as
`finalized_ts`). Flow records and profiles are lenient-on-read across the bump.

## [1.18.5] - 2026-07-12

Two fixes surfaced by a real production 37-flag funnel run on a private repo, plus a provenance gap found on the very first Azure review.

### Fixed

- **The funnel's run-level `degenerate` gate no longer over-fires on a minority remote-judge dispatch error** (#1329) — a transient dispatch failure on even ONE flag out of many (e.g. 1 of 37) was forcing the ENTIRE run degenerate, discarding every other flag's real, valid adjudication and posting "no review signal" on a run that actually completed correctly. The per-flag outcome was always handled safely (a pass-1 failure archives just that flag, a pass-2 failure demotes it to NeedsCheck — never a silent fake confirm); only the run-level gate over-reacted, and did so asymmetrically — the same failure class via `Unparsed` (garbage output surviving its retry) was already exempt and rendered fine. Fixed by folding the dispatch-error reason into the existing `usable == 0` gate `Unparsed` already relies on — a consistency fix, not new policy. A minority dispatch error is now surfaced as an `env.warnings` entry (matching the probe stage's existing precedent) rather than going fully silent on an otherwise-healthy run.
- **Funnel provenance now stamps the model an endpoint actually SERVED, not just the requested deployment name** (#1300) — an Azure deployment named e.g. `gpt-4o` can alias to a different underlying model; every downstream provenance surface (the posted footer, the audit envelope) previously inherited only the declared/requested id, discarding `SingleShotReply.model` (the response body's ground truth) entirely. `MemberRecord` gains `served_model: Option<String>`, threaded through all three seats (probe/judge/verify) and gated to remote seats only (a local LMStudio response is also OpenAI-compatible and echoes a `model` field — `lms ps` stays the only ground truth for local dispatch). The posted footer now surfaces both when they differ ("requested gpt-4o, served gpt-4o-2026-08-01"); agreement (the common case) still shows just the one name.

## [1.18.4] - 2026-07-12

Two fixes surfaced by running the funnel on a private production repo.

### Fixed

- **config.json resolves to user scope, never a project-local shadow** (#1323) — a stray project-local `.darkmux/` (created for project-tier missions/sprints/lessons) silently flipped config resolution to Project scope under `ResolveScope::Auto`, so on a self-hosted-runner checkout **every review dispatch ran with Redis *and* the tamper-evident audit log silently disabled** — a real audit-trail hole, not a telemetry gap (diagnosed via the #1311 liveness markers: `config-resolved … redis=off audit=off`). config.json is user/machine-level (redis/audit/lms/machine_id) with no legitimate per-project variant, so both `DarkmuxConfig::load_resolved` AND the `darkmux config` CLI now `ForceUser`. A conformance test guards against regression (proven to fail under `Auto`). Same shadowing class as #1012/#1016 — the config/flow-sink resolution path they missed.
- **The review footer no longer claims "darkmux dogfooding itself in public"** — the default tagline was posted verbatim on every review, including private repos, where it's both wrong and awkward. The default is now generic ("Advisory, not a merge gate."); darkmux's own public self-review opts the flourish back in via `--attribution`.

## [1.18.3] - 2026-07-12

A one-fix patch: the review funnel's confirmed findings anchor as inline comments instead of falling into the summary's general section.

### Fixed

- **Fragment anchors resolve to inline comments** (#1299, the mis-anchor half — the dedup half shipped in 1.18.1) — the funnel's prosecutor quotes the offending code in backticks, and `extract_new_side_anchor` stores that SPAN as the finding's anchor. A span is often a *sub-expression* of a changed line, not the whole line, so it matched the diff by **substring** at extraction time, but `resolve_anchor`'s **exact whole-line** lookup missed it — dumping the finding into the non-anchored "general" body section instead of posting inline. Frontier-staffed funnels (mechanism-level findings) hit this hardest. `resolve_anchor` gains a substring fallback symmetric with extraction: after the exact lookup fails, anchor to the new-side line whose whitespace-collapsed content *contains* the collapsed span — only when exactly one distinct line matches (never guess between candidates). An 8-char floor refuses short fragments; the fallback runs only after the exact path fails, so whole-line anchors (single-model reviews) are byte-identical to before. Offline replay against a real preserved review envelope: 0 inline / 7 general → **6 inline / 1 general**.

## [1.18.2] - 2026-07-11

The production-hardening patch — a ledger correctness fix plus the credential-and-hang surface the Studio's first Azure-review day surfaced.

### Fixed

- **The memory ledger prices models whose LMStudio path metadata is wrong** (#1309) — a model whose `lms ls`/`ps` reports a directory that doesn't exist on disk (e.g. devstral: reported `mistralai/devstral-small-2-2512`, real dir `mlx-community/Devstral-...`) was unpriceable, and the machine total silently undercounted. A content-scan fallback resolves the real config dir by token-subset match, with an ambiguity guard (multiple matches → stay unpriced, never guess a wrong config). Verified live: devstral went from unpriced to a correct 20.24 GB.

### Added

- **A dependency-free dispatch liveness floor** (#1311) — a heartbeat file (`<home>/liveness/<pid>.log`) plus `[darkmux-liveness]` stderr markers at each dispatch phase boundary, with NO dependency on config/Redis/audit/flow. A dispatch that hangs *before* flow-sink init (the #563 incident: an Azure review that froze 19 min with zero trace) now leaves "started, last alive at phase X" instead of a black box. Phase markers carry resolved detail (sinks, crew/seat-count/endpoint hosts, keychain item names, bundle counts, elapsed) — secrets never logged. `DARKMUX_LOG=debug` adds per-call host/model/token/wall detail.
- **`EndpointAuth.key_env`** (#1312, PROFILES_SCHEMA 1.4 → 1.5) — declare which environment variable holds an endpoint's API key (any provider). Resolution is `env(key_env) > per-dispatch cache > Keychain`; with the var set, `security` is never spawned — the standard headless-CI-secrets fix, so a self-hosted runner needn't read the macOS Keychain at all. Matches darkmux's existing `DARKMUX_REDIS_URL`/`DARKMUX_SERVE_TOKEN` env-over-Keychain pattern.

### Changed

- **All three Keychain reads are now bounded** (#1311) — the Redis-password read (during flow-sink init, the leading #563 freeze point), the serve-token read, and the endpoint auth spawn `security` with a 15s timeout: a locked/hung login keychain on a headless runner fails fast and actionable instead of a multi-minute freeze. The endpoint credential is also cached per-dispatch (it was read per hosted call — dozens of `security` spawns per review).


## [1.18.1] - 2026-07-11

The review-output patch — everything a first real production Azure review (on a private engagement repo)
surfaced about how the funnel *presents* its findings. No behavior change to what it finds; four
fixes to how it posts.

### Changed

- **Confirmed findings post a non-blocking `COMMENT` review by default, not `REQUEST_CHANGES`** (#1302) — the funnel was submitting a formal `REQUEST_CHANGES` (a real GitHub merge gate via branch rulesets) while its footer claimed "advisory," and darkmux could never clear its own block (a clean re-run posts a plain comment, which doesn't update `reviewDecision`), forcing manual dismissals that a compliance-monitored org must document. Confirmed findings now post the same non-blocking `COMMENT` class Gemini uses — inline comments intact, never a merge gate. A crew-level `request_changes: true` opts back into blocking (documented: no automated resolution path until #1260's verify-seat lifecycle exists). No workflow change — the YAML forwards the binary's review verbatim, so one binary upgrade fixes every consuming repo. `PROFILES_SCHEMA_VERSION` 1.3 → 1.4 (additive `request_changes`).
- **A configurable judge `passes` count replaces the hardcoded double-confirm** (#1266) — `passes: 1` on a judge seat runs a single pass (the frontier cost lever — a stable frontier judge needs double-confirm less than the local judge it was designed for), `passes: 2` is today's double-confirm (default), `passes: N` is unanimous consensus with early-exit. `PROFILES_SCHEMA_VERSION` 1.2 → 1.3.

### Fixed

- **The posted-review footer no longer claims "local model (no cloud API)" on a cloud review** (#1298) — the dispatch-provenance line is now derived from the run's actual seats (`env.members`): a remote crew reads "via a hosted cloud endpoint (`<models>`)", never "no cloud API"; all-local keeps the honest local claim; mixed names both. The old hardcoded claim was an audit-integrity problem on the first all-Azure review.
- **Frontier-worded duplicate findings collapse; the needs_check tier clusters instead of walling** (#1299) — the dedup was calibrated on local models and let a frontier judge's restatements through (9 "confirmed" that were 3 bugs; a 25-item needs_check wall). Collapse now keys on file + mechanism-family + overlapping-symbol + overlapping-location — conservatively (missing location never collapses; a collapse unions both locations and the absorbed finding's text, so a mistaken merge degrades to "one bullet, two framings" never a vanished bug), and the needs_check tier clusters by (file, mechanism) with the count conserved.


The frontier-staffing release: any review-funnel seat can be staffed by a hosted model, so a
machine that can't (or shouldn't) run local inference does PR review entirely off a cloud
endpoint. Underneath it, a new model-lifecycle foundation — a pure planning core, a
memory ledger, and a deadline-bounded host adapter — ships tested but not yet wired to the
live dispatch path (that cutover lands in 1.18.1).

### Added

- **Remote (frontier) staffing for any funnel seat** (#1260, #1177) — a `crews` seat whose profile carries an `endpoint` block dispatches to that hosted model instead of a local one. No new syntax: endpoint-presence is the whole signal. Remote seats **skip model cycling entirely** — nothing is loaded or unloaded, zero LM Studio contact — so an all-remote crew runs PR review with LM Studio shut down (verified end-to-end against Azure with the `lms` binary and local URL both disabled). Message assembly is byte-identical to a local seat (only the HTTP transport differs); provenance stamps the model and endpoint **host** only, never credentials.
- **The `review-verify` seat** (#1260) — an optional fourth funnel stage. When a crew declares it, each double-confirmed finding gets one frontier adjudication pass: `verified` posts without the "needs frontier verification" marker, `refuted` demotes to archived, `uncertain` keeps the marker. A crew without the seat behaves exactly as before.
- **Per-execution remote token buckets** (#1260, #1186) — `remote.max_tokens_per_execution` (default 500000, written visibly by `init`) caps each pipeline stage's hosted spend; exhaustion stops that stage's remaining remote calls with a named envelope reason (a load-bearing stage degrades the run honestly, never a silent pass). Remote tokens are accounted separately so savings surfaces never count cloud spend as "off the meter." The agentic-remote container loop is not metered in this release (#1293).
- **Memory ledger — potential vs. current** (#1286) — `darkmux model ledger [--json]`, a `/machine/memory` serve endpoint, and a mobile-first `#lens=machine` viewer lens show, per loaded model, what its config *commits* (weights + KV-cache-at-loaded-context + margin, from the model's own architecture) against what has *materialized*, color-coded green / amber ("made it by luck") / red, with a shrink hint that names the context reduction to reach green. Observability is read-only and dispatch-free by design (kernel counters + `lms` metadata only; the gather stamps its own cost) — codified in the new CLAUDE.md "the observer must not join the observed" doctrine.
- **GestaltManager model-lifecycle core** (#1274, present but not yet wired — cutover in 1.18.1): a pure planning crate (`darkmux-gestalt`) that decides load / unload / reuse / reconcile / block from abstract facts (memory pools as data, an ownership namespace, a RAM budget) and emits an inspectable plan with a typed reason on every action; a **wave scheduler** (#1285) that partitions co-resident models into parallel-or-sequential waves under a budget (which doubles as a hardware-tier emulator); an **architecture-aware estimator** (#1286) that computes true KV-cache cost per model; and an **`LmsHost` adapter** (#1276) that makes every `lms` call deadline-bounded — the unbounded-load hang becomes structurally impossible.

### Changed

- **`ProfileModel.n_ctx` is now optional** (#1282, profiles schema 1.1 → 1.2, minor) — endpoint-bearing models omit it (the provider owns the context window); a local model still requires it, enforced at resolution time with a named error and surfaced by `darkmux doctor`, never on the load path.
- **The profile registry is lenient per entry** (#1282) — one structurally-broken profile or crew entry is quarantined (with serde's exact field error) instead of failing the whole file; siblings load normally, `darkmux doctor` lists each quarantined entry, and a dispatch that names a quarantined profile hard-fails with that entry's parse error instead of silently substituting a model.

### Fixed

- **Endpoint-bearing profiles no longer break crew resolution** (#1269 → #1270, shipped in 1.17.1; the schema unblock here completes the story).

## [1.17.1] - 2026-07-10

The canary-day patch: three production findings from the Studio's first hours running the
funnel, fixed same-day.

### Fixed
- One invalid crew in `profiles.json` no longer fails the ENTIRE registry load — crew
  validation moved to resolve time (its doctrinal home); `darkmux doctor` gains a per-crew
  validation check; a genuine registry parse failure is one clear hard error instead of the
  deprecated probe fallback (#1269, #1270).
- The funnel's sequential cycler reconciles a same-model resident loaded at a different
  context (darkmux-owned: unload + reload at the required ctx; user-owned: an actionable
  error naming the instance) instead of attempting a doomed second load that LMStudio's
  guardrail refuses on 32 GB machines; explicit-alias residents reuse correctly; reuse at a
  larger ctx leaves a log breadcrumb (#1271, #1275).
- Production funnel runs now emit `dispatch.start`/terminal bookends (RAII-guaranteed on all
  exit paths, `source: "funnel"`) so a live PR review is visible as a running dispatch in the
  viewer's fleet and machine views (#1272, #1277).

[2.1.0]: https://github.com/kstrat2001/darkmux/releases/tag/v2.1.0
[2.0.0]: https://github.com/kstrat2001/darkmux/releases/tag/v2.0.0
[1.18.5]: https://github.com/kstrat2001/darkmux/releases/tag/v1.18.5
[1.18.4]: https://github.com/kstrat2001/darkmux/releases/tag/v1.18.4
[1.18.3]: https://github.com/kstrat2001/darkmux/releases/tag/v1.18.3
[1.18.2]: https://github.com/kstrat2001/darkmux/releases/tag/v1.18.2
[1.18.1]: https://github.com/kstrat2001/darkmux/releases/tag/v1.18.1
[1.18.0]: https://github.com/kstrat2001/darkmux/releases/tag/v1.18.0
[1.17.1]: https://github.com/kstrat2001/darkmux/releases/tag/v1.17.1

## [1.17.0] - 2026-07-10

The review-funnel release: PR review graduates from a single reviewer dispatch to a measured
prosecution-and-judgment pipeline, with the lab observability to watch and tune it.

### Added
- **The review funnel** (#1222 Phase B): `darkmux pr-review run` — procedural bundling
  (built-in Rust bundler with callee/sibling bodies, param-flow facts, external-symbol
  manifests; `--bundler <cmd>` plug-in contract) → strong-prior probe seats with k draws →
  mechanism-family dedup → double-confirm judge → three-tier synthesis (double-confirmed
  inline REQUEST_CHANGES comments carrying a "needs frontier verification" marker;
  needs_check as a non-blocking section; everything archived in the envelope artifact).
  Sources: local `--worktree` or `--github` + `--head-sha` (GitHub API, no checkout).
  (#1229, #1231, #1235, #1236, #1239, #1250)
- **Crews registry** (#1231): `crews` in `profiles.json` — saved seat assignments
  (`review-probe`/`review-judge`) staffed per profile/model with `k`, `max_tokens`, and
  `bundle_selector`; profiles schema 1.1.
- **Funnel review workflow** (#1232): `darkmux-review.yml` replaced with the funnel form —
  inputs `pr`/`crew`/`mode`/`k`, one `pr-review run` invocation, envelope uploaded as an
  artifact, crash-before-emit guard; Studio migration checklist in the runner docs (#1261).
- **`review-bench --funnel`** (#1238): the release-guard validation mode — corpus scoring
  unchanged, per-case funnel console line, `funnels.json` artifact.
- **Lab run observability** (#1247): funnel flow-record emission through a sink-agnostic
  emitter (production → flow stream; bench → per-run-local `funnel-events.jsonl`),
  per-case atomic envelope streaming, staffing snapshots (incl. `n_ctx`) for series
  comparison, crash-safe step bookends, host telemetry sampling during funnel runs
  (#1248, #1253, #1264). Flow schema 1.17.0.
- **The lab observer lens** (#1262): third viewer lens, machine-local — run list grouped
  by case with knob-diff provenance between runs (two-variable changes warn), live run
  detail (pipeline stages + ruling feed + host load), `#lens=lab` deep links, served from
  `darkmux serve --lab-dir <path>`.
- **Dialectic review-bench mode** (#1223, #1224): the P→D→J three-seat chain.
- **Agentic + free-form review-bench modes** (#1206, #1179); hosted single-shot dispatch
  gains `reasoning_effort` (#1204), 429/capacity-shed retry classification (#1207, #1211),
  and Google-compat fixes (index-less streaming deltas, stop-turn tool calls,
  thought_signature round-trip) (#1212, #1213, #1214).
- **Runtime**: per-call token cap override `DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL` (#1225);
  tool-call arguments recorded in trajectory + flow and surfaced in the viewer (#1220).
- **Viewer**: activity filter facet + per-activity icons (#1215, #1218), new-run
  affordances on the machine view (#1208).

### Fixed
- Funnel prompt assembly byte-matches the measured Phase A prompts (per-seat code-slice
  formats, prior in the user message, intent-free probes), enforced by golden tests
  generated from the reference implementation; role texts re-frozen on the measured
  versions (#1258, #1263).
- `--bundler`/`--k` warn when ignored with `--from-envelope`; example crew staffs three
  distinct models (#1250).
- Judge pass-2 step records open when pass-2 rulings actually begin (#1264); shakedown
  fixes for wrapped-line anchors, fence-aware quote spans, cliff recovery, watchdog
  streaming (#1226, #1228); clippy 1.97 toolchain lints (#1259).

[1.17.0]: https://github.com/kstrat2001/darkmux/releases/tag/v1.17.0

## [1.16.0] - 2026-07-05

**The production-review release** — everything the self-hosted QA pipeline needs to run agentic, cloud-backed PR review honestly: a freeform review contract that works WITH tools (a grammar-constrained `output_schema` combined with tools makes a model skip tool-calling and fabricate — verified empirically), a `pr-reviewer-agentic` role that explores the checked-out repo before concluding, a `doctor --probe` that live-verifies a remote credential actually works, and a render pipeline that can no longer present a produced-nothing review as a clean pass. Plus the live viewer stops counting paid cloud tokens as "off the meter." `FLOW_SCHEMA` / `RULES_SCHEMA` / `CONFIG_SCHEMA` unchanged.

### Added

- **`darkmux doctor --probe`** (#1177, #1191) — live-verifies each profile model's remote endpoint with ONE minimal chat completion through the exact URL/auth/POST path a real hosted dispatch uses: DNS, TLS, credential validity, deployment routing, api-version — not just Keychain presence (the free offline check, unchanged). Opt-in because each probe is a real API call; the result line shows the probe's own token cost, round-trip time, and the model the endpoint SAYS served the request — which surfaces deployment-name vs served-model drift as a one-liner. One probe per distinct (url, model, api-version, keychain) declaration; failures print the endpoint's error verbatim and exit 1.
- **Freeform review contract + `pr-reviewer-agentic` role** (#1113, #1192) — `MUST FIX`/`CONSIDER [path] \`anchor\`` marker blocks plus a `VERDICT: pass|flag` line, parsed by `pr-review render` AFTER the JSON contract (structured roles unchanged) and resolved to inline comments by the same quote-the-line machinery. The new builtin role has read/exec tools, deliberately no `output_schema` (a loader test pins that invariant), and an explore-before-concluding directive. Live-verified end-to-end: a real Azure dispatch followed the contract exactly on first exposure and all anchors resolved to correct inline lines.
- **`pr-review render --attribution <text>`** (#1192) — the posted footer's claim about where the model ran is now the workflow's to make (it knows; darkmux doesn't guess). The default footer is unchanged; the body header drops its "(local model)" suffix — locality is the footer's job.
- **Local/cloud token split in the live viewer** (#1186, #1189) — the savings hero's single "tokens off the meter" number was blind to agentic-remote dispatches and counted paid Azure tokens as off-meter. The hero now shows two co-equal numbers under a "by your fleet" frame: **local tokens** (green) and **cloud tokens** (cyan), resolved per-session via the dispatch's `endpoint` field, with a fallback so tool-less single-shot hosted dispatches (which emit no token telemetry) count from their completion records. Tokens only, never currency, on either tier. Labels renamed from "off the meter" ("what meter?" — the phrase presumed a known meter; "local" explains itself), and the same reasoning retired "off the meter" from user-facing copy (the home-page tagline now says "no API bill").

### Fixed

- **A produced-nothing review can no longer read as a green check** (#1113, #1193) — `pr-review render` emits `mode: "degraded"` (distinct from `review`/`comment`) for a missing/empty envelope, an empty reply, or a vacuous pass (zero findings, no summary, and anything but an explicit `flag` verdict). The posted comment states loudly that no automated review happened; the self-review workflow posts it and fails the run. The motivating incident: a SIGKILLed dispatch (exit 137, zero tokens) rendered identically to "clean review, no findings" on a production-deploying repo.
- **Redundant GitHub Pages deploy workflow removed** (#1190) — it raced the built-in branch-based Pages pipeline on every docs push; the loser failed with "Deployment failed, try again later" noise while the site stayed current throughout.
- **Dangling issue citations corrected** (#1187, #1188) — code comments shipped with agentic-remote dispatch cited `#92`, an unrelated merged PR; #1187 is the retroactive tracking issue and all citations now point at it. *Correction to the 1.15.0 entry below: its "(#92, #1180)" citation should read "(#1187, #1180)" — left in place as published, corrected here.*

## [1.15.0] - 2026-07-04

**Agentic-remote dispatch** — a tool-granting role (e.g. `code-reviewer`) can now be driven by a remote OpenAI-compatible endpoint (Azure OpenAI, OpenAI, …) as its "brain," running the SAME real tool-calling loop (multi-turn `tool_calls`, `bash`/`read`/`write`/`edit`/`search`) local models get via the internal container runtime — not just a single-shot chat completion. Tool-less roles (e.g. `pr-reviewer`) are unaffected; they stay on the existing light single-shot `dispatch_remote` path from 1.13. Also carries forward #1172 (deferred from 1.14.1) and a viewer host-load meter. `FLOW_SCHEMA` **1.14.0 → 1.15.0** (additive — new CPU/RAM/GPU host-load telemetry fields; an older binary tolerates the newer schema, no breaking change). `RULES_SCHEMA` / `CONFIG_SCHEMA` unchanged.

### Added

- **Agentic-remote dispatch** (#92, #1180) — the remote endpoint's auth credential is piped over the container's stdin once at spawn, immediately consumed, never written to any file or env var: a mounted secret-bearing file would be reachable by the container's `bash` tool (no `/workspace`-escape check on `bash`, unlike `read`/`write`/`edit`), so stdin closes that exposure entirely. Live-verified against a real Azure endpoint: a genuine multi-turn `tool_calls` round-trip, and confirmed no auth artifact exists on host or container at any point (including the model's own attempted `cat` of the old file path failing outright).
- **`darkmux doctor` — remote endpoint credential presence check** (#85, #91) — surfaces a profile model that declares a remote endpoint whose Keychain credential is missing or absent, before the first real dispatch bails on it. Read-only; never touches the secret value.
- **Host-load meter** (#1064, #1176) — CPU, RAM, and GPU utilization in the run view, sampled alongside existing telemetry.
- **`pr-review-bench` multi-finding parity scoring** (#1119, #1172) — corpus-wide recall/precision against a labeled corpus, not just single-anchor pass/fail.

### Fixed

- **Agentic-remote dispatches were missing the `endpoint` flow-record field** (#1181). The light single-shot `dispatch_remote` path already recorded which remote endpoint served a dispatch; the new agentic-remote container path didn't, so the viewer rendered every agentic-remote dispatch as a local LMStudio run regardless of where the model actually ran. Caught live, watching the viewer during the first real agentic-remote dispatches.
- **Compaction now always uses a local-only client, never the dispatch's remote brain.** An agentic-remote dispatch was routing its compaction requests through the SAME client as its primary loop — silently mis-billing the remote deployment (Azure ignores the request body's `model` field; the deployment is in the URL) or hard-failing the whole dispatch outright (OpenAI-style endpoints validate `model` server-side) the moment compaction fired, which is exactly the long, tool-heavy dispatch this feature exists for. Found by an independent security audit; regression-locked with a two-mock-server test, and live-verified with a real forced-compaction dispatch against Azure (confirmed via a differential test: unloading the local compactor model makes the dispatch fail with an error naming the local LMStudio URL, not Azure).

## [1.14.1] - 2026-07-03

A viewer performance hotfix — the live observability tab degraded over a long-open day (multi-second loads, laggy clicks). Released as a clean patch off `v1.14.0` (this entry lands on `main` for continuity; the tag itself was cut from the 1.14.0 line, excluding the concurrently-merged #1172 which rides the next minor). Drop-in; no `FLOW_SCHEMA` / `RULES_SCHEMA` / `CONFIG_SCHEMA` change.

### Fixed

- **The live viewer no longer degrades over a long-open day** (serve + viewer, #1173). Two independent costs, both profiled on a real daemon (160 sessions, ~4.8k records): (1) every click and the initial paint paid a ~2.5s `render()` because `liveSessionSet()` fell back to an O(sessions×records) scan (`flowLiveSessions`) when Redis presence was empty, and the fleet timeline + crew cards called it hundreds of times per render — it's now memoized per render (keyed on the data snapshot + a 2s wall-clock bucket) → ~20ms (123×); (2) the 20s SSE-backstop reconcile re-fetched and re-parsed both full day files (multi-MB) on the main thread every tick — `GET /flow/:date` now accepts an optional `?since=<ts>` and the reconcile requests only the recent tail, so the parse cost no longer grows with the day.

## [1.14.0] - 2026-07-02

Cross-day playback discoverability + a run-detail telemetry-panel overhaul. Drop-in over 1.13.1 — no `FLOW_SCHEMA` / `RULES_SCHEMA` / `CONFIG_SCHEMA` change.

### Added
- **Cross-day mission/dispatch catalog** (#691) — playback is now navigable by the *thing that ran*, not just by calendar day. Disk-backed endpoints `GET /flow-missions` (a rollup across every day file), `GET /flow-mission/:id`, and `GET /flow-session/:id` (#1166), plus a viewer catalog with a missions section and `?mission=`/`?session=` replay-by-query that stitches a mission's records across every day it touched (#1167).

### Fixed
- **Run-detail telemetry panel** overhaul (#1169): CPU and context charts now share one wall-clock time axis (they were on different scales); context is a step-area with a marker at every turn — visible even when a turn's token delta is sub-pixel — left-anchored at t=0, with a green→amber→red fullness gradient and a labeled window ceiling; CPU is shown in **cores busy** (docker's per-core % scales past 100% on many-core machines, so a 100% floor was useless); a dashed line marks the compaction-trigger level; the `model (lms)` panel populates on the dispatch's first sample instead of reading "no telemetry yet"; and long session ids in crew cards wrap instead of overflowing.

Note: cargo `1.14.0` numerically coincides with `FLOW_SCHEMA` `1.14.0` — these are independent version lines (the binary vs. the flow-record data shape), not coupled.

## [1.13.1] - 2026-07-01

A stability patch from a review-swarm audit of the recently-shipped code: five
bug fixes across the runtime, fleet queue, serve daemon, and viewer, plus two
message/comment cleanups. No schema changes (`FLOW_SCHEMA` stays `1.14.0`,
`CONFIG_SCHEMA` `1.1`), so it stays fully compatible with a v1.13.0 peer/hub.

### Fixed

- **Compaction no longer orphans a tool-result at the tail boundary** (runtime, #1158). Compaction preserved a fixed head + tail using raw indices; when the preserved tail began on a `tool` result whose parent assistant was in the summarized middle, the next model request failed with HTTP 400 — hard-failing an otherwise-productive dispatch (an opaque "LMStudio returned 400", non-deterministic so it read as flaky). Boundaries now snap off tool-call groups.
- **A dispatch panic no longer silently kills the fleet runner** (fleet, #1159). A panic (not an `Err`) in the dispatch path unwound the runner thread; the daemon kept serving and the presence heartbeat kept emitting, so it looked healthy while the machine stopped claiming work forever. The claim loop now catches the panic, releases the queue lease, and continues.
- **No more silently-lost jobs published before a runner exists** (fleet, #1160). A `--machine` dispatch to a target whose daemon had never run (or a fresh Redis) was dropped, because `XGROUP CREATE … $` parks the group cursor after the message. `publish_job` now ensures the consumer group exists before the `XADD`.
- **`/diff` no longer blocks the async runtime or over-allocates** (serve, #1161). The handler ran three `git` subprocesses inline on an async worker (executor-starvation risk) and buffered git's entire stdout before truncating to 256KB. It now offloads to the blocking pool and streams stdout under the cap.
- **The live viewer no longer leaks per-session ids on a long-lived tab** (viewer, #1162). `runtimeUids` escaped the rolling-window age-out and grew unbounded on an always-on tab (phone dashboard / hub viewer); it's now pruned alongside the window trim.
- The daemon-unreachable nudge is brew-aware ("start the daemon: `brew services start darkmux`" instead of "run `darkmux serve` in another tab"), and the serve-wrapper header comment no longer describes pre-#661 Redis behavior (#1163).

## [1.13.0] - 2026-06-30

The fleet-foundation + self-diagnosing-doctor release: declare a machine's fleet
position, set config without hand-editing JSON, and let `darkmux doctor` catch
the cross-setting traps + tell you where to open the viewer — plus a live-view
UX pass. **No `FLOW_SCHEMA` change** (stays `1.14.0`), so cross-machine flow
stays compatible with a v1.12.0 peer/hub. **`CONFIG_SCHEMA` 1.0 → 1.1** (additive
`fleet{}` block; lenient-read, so an older binary tolerates a newer config).

### Added
- **`fleet.mode` — hub | peer | standalone (#933).** A machine's declared place
  in a multi-node fleet, a `fleet{}` block in `config.json`. The operator
  declares it; `darkmux doctor` shows it with provenance. Downstream fleet
  tooling keys on it.
- **`darkmux config set/get/list` (#937).** Read/write `config.json` from the CLI
  (`darkmux config set redis.host <addr>`, `… fleet.mode peer`) — the key is
  validated against a registry (a typo is surfaced with a suggestion, never
  silently written) and the value coerced to the field's type. Secrets are
  refused with a pointer to the Keychain `security` form.
- **`darkmux doctor` L1 — cross-setting coherence + a verdict banner (#934).**
  New rules catch traps no single check sees: a stale `DARKMUX_*` env var
  shadowing an enabled `config.json` block, and a brew/cargo binary split-brain
  (a daemon serving an older schema than the CLI). Doctor now leads with an
  `● ok / needs attention / broken` verdict naming the highest-severity finding,
  not a flat list.
- **Doctor surfaces the viewer URL (#1155).** The `daemon reachable` line shows
  where to open the viewer — the loopback URL plus, when `tailscale serve` is
  proxying to the daemon, the tailnet/phone URL.
- **Live token tiles + activity-timeline presets (#1151).** The run view's
  tokens-in/out accumulate live (per-turn telemetry) instead of dashing until the
  run ends; the fleet activity timeline gains `10m/1h/4h/24h` presets with a
  now-anchored axis.

### Fixed
- **New runs surface without a manual refresh (#1151).** An SSE backstop re-pulls
  the bounded live window so a run dropped during a Redis reconnect-gap
  self-heals, instead of needing a page refresh.
- **Viewer state survives the live rebuild (#1147 / #1149).** Expanded
  `<details>` no longer snap shut, and the run view's scroll + open state
  survives the ~1/sec live update (render-once + targeted-update).
- **Mobile viewer layout (#1151).** Shortened savings-hero labels, left-aligned
  the breakdown when it wraps, and packed the LIVE badge onto the brand row so
  both machine timelines fit on a phone.

### Changed
- **darkmux self-review profile default → `diff-review` (#1150).** The
  `darkmux-review.yml` workflow now dispatches with the `diff-review` profile by
  default (was `review`).

## [1.12.0] - 2026-06-29

A build-visibility + run-observability release, plus the production-hardening
fixes surfaced by darkmux's first brew-stable production user. **No `FLOW_SCHEMA`
change** (stays `1.14.0`), so cross-machine flow stays compatible — but the
`runtime/` image **is** rebuilt this release (the empty-`tool_calls` recovery),
so a `brew upgrade` pulls a new `darkmux-runtime` image.

### Added
- **Build identity in three places (#1129).** `darkmux --version`, the lead
  `build` line of `darkmux doctor`, and a chip in the observability viewer header
  all show `<version> (<git-sha>)` — or `<version> (release)` on a Homebrew build
  — plus the `flow_schema` version. The package version alone doesn't change
  between releases, so it couldn't tell you whether a running daemon had your
  latest code; the git SHA does.
- **Run drill-down page clarity (#1125).** The per-run page now leads with a
  status pill + `run · <role>` (not "subsystem"), a run brief (runtime / model /
  workspace / mission / timing), **tokens in / out** tiles, and a done-aware
  context tile (a finished run shows peak, not a misleading "now").
- **About modal (#1132).** The header build chip opens an "about · darkmux" modal
  consolidating build / flow-schema / connection / mode / machine / hardware +
  links.
- **The dispatch prompt + runtime image in the run brief (#1127 / #1126).** The
  run page shows the dispatch's prompt (collapsed) and the container image it ran
  in — both previously absent or a dead reference.
- **`darkmux doctor` is issues-only by default (#1130).** It shows the build line
  + any warnings/failures and collapses the passing checks to a count;
  `darkmux doctor -v` prints the full list.
- **`darkmux lab review-bench` (#1119).** A reproducible PR-reviewer eval — a
  labeled diff-mix fixture + a scoring provider — so model bake-offs for the
  review role are repeatable, not one-off.

### Fixed
- **`crew dispatch` honors the profile's `n_ctx` (#1135).** The dispatch resolved
  the model id but let LMStudio JIT-load it at the **model default** (e.g. 4096),
  silently truncating large inputs (a pr-review diff overflowed → garbage review,
  no error). darkmux now loads the selected model at the profile's declared
  context before dispatching (reusing a sufficient resident load, reloading a
  too-small one), and surfaces a clear RAM-hinting error if the load fails. Also
  fixes a latent `lms load` quiet-flag bug that leaked the load spinner into a
  `--json` envelope (and into `darkmux swap --json`).
- **Compaction meter no longer double-counts (#1122).** Each compaction emits two
  flow records (a work event + a token-telemetry record); the viewer folded both
  into the compaction count, reporting 2×. The token-telemetry record is now
  canonical.
- **The runtime recovers from an empty `finish_reason=tool_calls` (#1123).** A
  model returning a wholly empty completion under a `tool_calls` finish reason
  hard-killed the dispatch; it now routes through the same intra-turn stall
  recovery (nudge + retry, bounded) as the empty-`length` case.
- **Internal-path dispatch errors carry the stderr text (#1042).** The internal
  runtime (the default) emitted only `stderr_chars`; it now carries a bounded
  stderr tail excerpt on error, like the openclaw path — so a failed dispatch is
  diagnosable from the flow stream alone.

## [1.11.2] - 2026-06-28

A bug-fix + accessibility + security patch from a board triage. No schema change
(`FLOW_SCHEMA` stays `1.14.0`) and the `runtime/` image is unchanged from
`1.11.0` — a pure `brew upgrade`, no image pull.

### Fixed
- **Live "in flight" derives from presence, not flow records (#857).** A
  hard-killed or orphaned dispatch could read "running" / "dispatch in flight"
  forever. All live-mode activity derivations (fleet card, timeline bars,
  burn-down "+N in flight") now key on presence via one `sessionRunning()` helper
  — an orphan ages out on its own (TTL); playback still uses the durable
  close-edges.
- **Truthful, de-duplicated live status line (#1103).** Dropped the "live"/"today"
  that the badges already show; "last run" now measures real wall-clock elapsed
  (it was stuck on "just now"); the backwards-looking clock range became the
  window scope ("last 24h"); machine presence is decoupled from the record count.
- **Consolidated live headline (#1105).** Dropped the "fleet" wording (wrong for a
  solo local machine) and folded the machine count into a chip glyph.
- **Dispatch-error records carry the stderr text (#1042).** The openclaw-path
  error record had `stderr_chars` (a count) but not the text, so you couldn't see
  *why* a dispatch failed; it now carries a bounded stderr tail excerpt
  (null on success).

### Accessibility
- **Keyboard navigation for the drill cards (#1090).** Fleet → machine → session
  cards were mouse/touch-only; they're now focusable (`role=button` + tabindex via
  a delegated observer), Enter/Space-activatable, with a visible focus ring.
- **Non-color status cue (#1092).** Timeline bars now carry a per-state pattern
  (diagonal/solid/vertical/cross-hatch) and the active cycle stage a dot — state
  is no longer color-only, including under `prefers-reduced-motion`.

### Security
- **`pr_labels` flag-injection guard (#1111).** A repo-declared PR label starting
  with `-` (e.g. `--config`) was passed unvalidated to `gh pr create --label` and
  parsed as a flag; labels are now validated (non-empty, no leading dash) like
  branch names already were.
- **`external pull` argument-injection guard (#1112).** A `--gh`/`--url` target
  starting with `-` was passed unvalidated to the `gh`/`curl` subprocess; targets
  are now rejected before spawn. (The SSRF hardening of `curl -L` remains tracked
  + deferred for the operator-typed threat model.)

## [1.11.1] - 2026-06-28

A focused **viewer + UX pass**, mostly mobile, plus one local-PR-reviewer
reliability fix. The dashboard reads cleaner on a phone, the status colors mean
one thing everywhere, and the chrome is icon-first instead of word-cluttered.

No schema change (`FLOW_SCHEMA` stays `1.14.0`) and the `runtime/` image is
unchanged from `1.11.0` — a pure `brew upgrade`, no image pull.

### Changed
- **Unified status-color convention (#1071).** Cards, recent-runs rows, and the
  activity timeline now share one enum: green = success/complete, yellow + pulse
  = running, orange = canceled, red = failed/killed. A watchdog kill reads as
  red, not as a disabled-gray "complete".
- **Icon-first chrome (#1067).** The filters/history/follow/back/play controls
  are now compact icons; the filter is a funnel (not a settings gear) and follow
  is a clock to read as real-time (#1098). History opens from the "today" badge,
  retiring the button that looked like a stop control.
- **Local-timezone timestamps (#1069).** Absolute times render in the browser's
  zone instead of the record's machine zone.
- **Fleet machine cards redesigned (#1095).** Uniform size, a default machine
  icon, and a tighter stat line with state on its own row.
- **Savings hero on the missions tab, full-width (#1096).** It now shows on
  missions (not just fleet) and spans the column with no dead right gutter,
  aligning with the timelines below it.
- **Default avatar on crew role cards (#565).** A person icon stands in until a
  role-specific avatar is set.
- **Dropped the redundant "Live" word from the source badge (#1065)** and
  consolidated the savings-hero green onto the `--good` token (#1083).

### Fixed
- **Mobile log pane (#1100, regression from #1089).** The event list gets room
  again instead of being squeezed to two or three visible events.
- **Mobile responsive hardening (#1089).** Fixed-width elements no longer
  overflow the viewport on phones; icon-only controls meet touch-target size
  (#1087).
- **Back button shows only when there's somewhere to go (#1072/#1074)** and is
  otherwise removed — the breadcrumb and lens tabs already cover navigation
  (#1094).
- **Empty-state placement (#1070).** The "no activity" hint drops below the crew
  cards instead of crowding beside them, and a spurious stray label is gone.
- **Accessibility:** an `aria-label` on the rewind glyph button (#1080).
- **PR reviewer no longer copies its own example (#1084).** The role prompt's
  worked-example finding was being emitted verbatim by small models as a real
  (false-positive) finding; the response grammar already enforces output shape,
  so the copyable example is gone.

## [1.11.0] - 2026-06-27

darkmux's local **PR reviewer** got materially better and self-contained. It now
reads each change against its **stated intent** (the PR title + description), so it
stops flagging the very bug a fix removes; it anchors findings by **quoting the
line** and resolving that quote to a coordinate in the harness (local models name
the construct reliably but guess line numbers badly); and the whole review-render
step now lives **in the binary** (`darkmux pr-review render`), versioned with the
role schema, instead of a copied script every repo had to keep in sync. darkmux
also reviews **its own PRs** in public, on a local model, via a self-hosted runner.

No schema change (`FLOW_SCHEMA` stays `1.14.0`) — a clean `brew upgrade`. The
`runtime/` image is rebuilt for the reasoning-content fix below, so a fleet on the
internal runtime pulls the new `darkmux-runtime` image.

### Added
- **`darkmux crew dispatch --profile <name>` (#1054).** Select a named profile
  from the machine's registry for a dispatch's model + context-window resolution;
  a name not defined on this machine falls back to `default_profile` (with a
  note). Lets a machine-agnostic caller (a CI workflow) name the profile it wants
  while each machine owns which lab-validated model that maps to.
- **Intent-aware PR review (#1053).** The `pr-reviewer` role now assesses the diff
  against the PR's stated purpose (title + description, fetched procedurally — no
  AI), flagging only where the change *fails* its intent, not the problem it's
  solving. Validated head-to-head: an 8B and a 122B both stopped false-flagging a
  correct fix once given the intent — input-shaping over raw model size.
- **Quote-the-line anchoring for review findings (#1053).** Findings carry an
  `anchor` (a verbatim quote of the line) instead of a line number; the harness
  resolves it to the exact new-side line. Mis-located inline comments go away;
  file-level findings post as general comments instead of onto a guessed line.
- **`darkmux pr-review render` (#1060).** Binary-owned generation of the GitHub
  review payload from a dispatch envelope + diff (resolve anchors → inline
  comments + summary). Replaces the per-repo `pr-review-post.py` copy, so the
  render versions *with* the role's output schema and never silently drifts; the
  workflow keeps the `gh` post, and `--emit` writes the payload for full control.
- **darkmux self-review workflow (#1047) + overridable `role`/`profile` inputs
  (#1057).** darkmux reviews its own PRs on a local model (no cloud API), on a
  self-hosted runner, posting native inline comments — `workflow_dispatch`-only
  for public-repo safety. `-f role=` / `-f profile=` override the dispatch per run.

### Fixed
- **Thinking models no longer return empty reviews (#1050).** qwen3_5-family
  models routed their whole answer to `reasoning_content`, leaving message
  `content` empty; the runtime now promotes terminal reasoning to content (guarded
  so it never disables the length-runaway stall recovery).
- **Viewer phantom "unknown" machine card (#1048).** The flow stream's
  schema-header line was bucketed as a machine in the topology view; it's now
  skipped.

## [1.10.0] - 2026-06-26

A local model can now run as an automated **PR reviewer**: a tool-less role
reviews a diff and emits a structured, cite-the-line JSON review that CI posts
back as native inline pull-request comments — and the runtime can now
grammar-constrain any role's output to a declared schema, so a small local
model cannot emit malformed JSON.

### Added
- **Tool-less `pr-reviewer` role (#1037).** Reviews a unified diff provided
  inline and emits a structured, cite-the-line JSON review (path + line +
  severity + detail + how-to-fix advice + optional one-click suggestion),
  designed for CI to post as inline PR comments. No repo, no shell, no tools —
  pure reasoning over the given diff.
- **Grammar-constrained structured output — `output_schema` on a role (#1039).**
  A role manifest can declare an `output_schema` (JSON Schema); the internal
  runtime passes it to LMStudio as `response_format: json_schema` (strict), so
  the model is grammar-constrained to emit exactly that shape — the structural
  cure for local-model JSON malformation, vs post-hoc repair. Backward-compatible:
  roles without `output_schema` behave exactly as before.
- **`pr-reviewer` findings carry `advice` + `suggestion` (#1044).** Each finding
  has `advice` (prose how-to-fix, always present) and `suggestion` (the exact
  literal replacement line for a clean one-line fix, or `null` — rendered as a
  one-click GitHub suggestion). Keeps fix-guidance on every finding while
  reserving the one-click path for fixes that actually apply cleanly.

### Fixed
- **`output_schema` nullable fields use `anyOf`, not a type union (#1040).**
  LMStudio's grammar compiler rejects `"type": ["string","null"]` (`ValueError:
  'type' must be a string`); nullable fields are now expressed as
  `anyOf: [{"type":"string"},{"type":"null"}]`. A builtin-role strict-safety
  test now guards the rule. Caught dogfooding the live `pr-reviewer` dispatch.
- **Capability-aware verification boundary for `code-reviewer` + `test-designer`
  (#1035, #400).** The post-dispatch verification rule no longer holds these
  roles to a code-mutation check they aren't expected to satisfy.

## [1.9.0] - 2026-06-23

The dispatch-to-PR loop's engagement-context cure goes from foundation to
finale: the loop can now key cautions to the code they fired on, rank what's
relevant to the dispatch, budget what it injects, and **measure** whether the
injected memory changed behavior. Plus dispatch ergonomics for substantial briefs.

### Added
- **Lessons sovereignty verbs — `darkmux lessons edit/remove/export/import/recall`
  (#1003).** Full operator curation of the engagement-context lessons store
  (`add`/`list` shipped in 1.8.0): in-place edit, delete, a self-describing JSON
  export/import roundtrip (idempotent, order-independent), and read-only recall.
- **Loop-lab engagement-context A/B — `darkmux lab loop --ab` (#1004).** Run the
  same workload twice, once with the injected lessons/cautions and once without,
  and report the verdict shift — the empirical proof of whether institutional
  memory changes loop behavior. `--inject-from-mission <id>` scopes the cautions.
- **`crew dispatch --message-from-file <path>` (#386).** Pass a substantial brief
  from a file instead of the command line. The message now flows to the runtime
  via a bind-mounted file rather than `docker run` argv, so a large brief can't
  hit ARG_MAX or show up in `ps`.
- **Proportional injected-context budget (#1011).** The coder brief's injected
  context (cautions + lessons + corrections) is budgeted as a fraction of the
  model's context window with per-authority floors, replacing three flat counts.
  Tunable via `runtime.injected_context_fraction` / `DARKMUX_INJECTED_CONTEXT_FRACTION`.

### Changed
- **Staleness-aware cautions (#1001 + #1002).** Detector firings now capture a
  BLAKE3 hash of the file they fired on; at retrieval, a caution about a file
  whose content has since changed is ranked **down** as stale. Cautions and
  lessons about a file the dispatch will touch rank **above** engagement-level
  ones (file-in-play precision).
- **Prior-sprint output is capped in the brief (#146).** Each dependent sprint's
  injected upstream output is bounded (default ~8000 chars, `DARKMUX_SPRINT_CONTEXT_MAX_CHARS`)
  so a long parent reply can't crowd a small model's window.

### Internal
- Test coverage for the fleet routing completion-matching path (#842).

## [1.8.0] - 2026-06-23

The dispatch-to-PR loop learns from its own failures, gains a closing ceremony,
and the live observability viewer stops asserting state it can't see and starts
showing what it actually observes.

> **Cross-machine schema note.** `FLOW_SCHEMA` bumped **1.13.0 → 1.14.0**: the
> dispatch lifecycle now emits a `Stage::Debrief` value (the NASA-vocabulary
> rename of the old `retrospect` stage). A single machine is unaffected. In a
> **mixed-version fleet**, upgrade every machine together — an older binary does
> not recognize the `debrief` stage value in records written by a 1.8.0 peer.

### Added
- **Engagement-context layer — the doom-loop cure (#994).** The dispatch-to-PR
  loop now closes the detect → distill → inject → don't-repeat loop. Detector
  firings capture the engagement-context files they touched (#995); the index
  derives **cautions** from the flow stream (#996); those cautions surface in
  the next coder brief so a known failure is not silently re-walked (#997); and
  a durable SQLite **lessons** store backs operator-authored conventions —
  `darkmux lessons add/list` — which inject into the brief alongside the
  auto-derived cautions (#998). Two tiers: per-repo and global.
- **Mission debrief ceremony — `darkmux mission debrief <id>` (#1000).** A
  closing read on a finished mission: sprint/mission status, the diffs and flow
  history it produced, and a distiller skill (`darkmux-mission-debrief`) that
  turns the run into reusable lessons. `mission close` now nudges toward it.

### Changed
- **NASA vocabulary, end to end (#999).** The engagement-context store and verb
  are now **lessons** (was `knowledge`); the dispatch lifecycle's closing stage
  is **`Debrief`** (was `Retrospect`), bumping `FLOW_SCHEMA` to 1.14.0 (see the
  cross-machine note above). A vestigial index table was dropped.

### Fixed
- **Viewer derives liveness from the flow stream when Redis presence is down
  (#1007).** With the presence substrate unreachable, running/ended state now
  falls back to recent flow activity instead of showing an empty fleet.
- **Per-dispatch drill-down scopes to the latest attempt (#1013).** A re-run no
  longer blends the prior attempt's subsystem trace into the current one.
- **Operator-state resolves to the user scope, not a project `Auto`-scope
  (#1012).** `lessons add` in a repo no longer silently creates a project-local
  `.darkmux/` that shadows the user's missions and lessons.
- **doctor tags eureka rules by declared runtime, not a substring match (#1010).**
  OpenClaw-only rules are suppressed without `--openclaw` by a `RuleKind::runtime()`
  classification rather than matching the string "openclaw".
- **Observability viewer shows observed state, not asserted fiction.** The
  session CPU chart is relabeled **container CPU** — tool work, not the
  inference that runs off-container in LMStudio (#814); the utility card and
  machine spec line render the model's **observed** residency
  (resident / registered-not-loaded / not-configured / not-reported) instead of
  a hardcoded "resident" (#1008); and the spec line reports RAM in GiB so a
  128 GB machine reads **128 GB**, not 137 (#1020).

## [1.7.0] - 2026-06-22

Loop-engineering tooling and correctness: a bench for measuring how a dispatch
loop behaves, and a fix for the wrong-diagnosis-stuck failure mode.

### Added
- **Loop lab — `darkmux lab loop <workload>` (#986).** A single-run
  loop-engineering bench. Run one dispatch under a chosen harness config and get
  back a verdict for how the loop behaved: `productive`, `struggled` (a loop
  detector fired and the harness bounded it), `inert-false-pass` (the model made
  no tool calls yet verify reports pass because the baseline passes regardless),
  or `failed`. Two loop-variation axes: caps (`--max-turns` / `--max-tokens` /
  `--timeout`) and compaction (`--compact-threshold-tokens` /
  `--compact-threshold-ratio` / `--compact-strategy` / `--bail-after-compactions`
  / `--context-window`); the model axis comes from `--profile` /
  `--profiles-file`. `--json` for programmatic use. The report reads the run's
  trajectory, metrics, and sandbox hashes; no new infrastructure.

### Changed
- **Prior reviewer corrections read as findings-to-verify, not directives
  (#453).** In the dispatch-to-PR loop a confident-but-wrong reviewer diagnosis
  could anchor the next coder into a no-progress loop. Corrections injected into
  a follow-up coder brief, and the code-reviewer and coder role prompts, now
  frame a prior finding as something to verify against the live workspace before
  applying: a concrete change (a renamed field, a command) gets a quick check; a
  diagnosis (a race condition, a failing test) gets reproduced first. A
  correction that does not hold is re-diagnosed, bounded by the existing
  escalation contract. The #849 carry-forward is unchanged.

### Tests
- **Coverage pass (#842).** Closed the genuine remaining gaps in the fleet
  queue-claim decode path (`parse_xreadgroup_response` protocol-shape errors,
  `extract_field` edge cases), the docker-run argv builder (compaction-strategy
  mapping, allowed-tools block-all vs allow-all, the feedback-templates guard),
  and `build_work_job` (the cross-machine WorkJob constructor, previously
  untested). Test-only; no behavior change.

## [1.6.0] - 2026-06-21

Dispatch-to-PR loop correctness, and the lab made fit for profile development.

### Added
- **Corrections persist into the next coder brief (#849).** A correction the
  reviewer records at the gate (`flow note --source adjudication`) is now
  injected into the next dispatch's brief for the same mission — a correction
  made once is carried forward, not re-derived (the doom-loop fix). Injected as
  provenance-framed context (the count + each correction surfaced at dispatch
  time), never a silent rule. Plus a codified recheck-vs-rethink escalation
  policy in the agent docs.

### Fixed
- **`lab run --profiles-file` now reaches the dispatch's model resolution
  (#984).** The flag resolved the profile for lab run's own bookkeeping, but the
  dispatch re-resolved its model from `env > default` — silently using the wrong
  model, which blocked profile development. `config_path` is now threaded end to
  end; `lab tune` / `lab characterize` inherit the fix. No behavior change off
  the lab path.

## [1.5.0] - 2026-06-21

Dispatch-to-PR loop robustness. The headline is the verifier-fabrication
backstop: when a coder's verifier command (e.g. `cargo test`) *failed to run* —
never executed — `mission ship --merge` now holds the auto-merge for human
review instead of trusting a SIGNOFF that may rest on a command that never ran.

### Added
- **Verifier-fabrication gate (#799).** `mission run` parses the dispatch
  envelope's `failed_tool_invocations` (stamped by the runtime in 1.4.x), emits
  a per-run `mission.run.verification` flow record, and prints a gate banner
  naming any verifier that failed to run. `mission ship --merge` reads the
  latest run's record back and **holds** the auto-merge (new exit code `3` — PR
  stays open, worktree intact, never torn down) when the latest run had
  failures. Soft everywhere: never auto-fails, never auto-ships, only holds for
  human review. New flow action `mission.run.verification`; `FLOW_SCHEMA` is
  unchanged (additive action, not a shape change).

### Changed
- **Single source of truth for the `docker run` argv (#847).** The four
  arg-builder helpers (volume mounts, runtime injection, cache mount, compaction
  flags) are no longer duplicated between dead helpers and an inline copy in
  `build_docker_run_argv` — the helpers are the one impl and `build_docker_run_argv`
  delegates to them. Eliminates the divergence trap behind earlier dispatch
  regressions (same bug-class as the 1.4.1 hotfix). No behavior change — the
  emitted argv is byte-identical.

## [1.4.1] - 2026-06-21

Hotfix. The internal-runtime dispatch (`darkmux crew dispatch`, `darkmux
mission run`) was broken in 1.3.x–1.4.0: it invoked `docker docker run` and
exited 125, so the local-AI dispatch-to-PR loop could not start. `--runtime
openclaw` was unaffected. `brew upgrade darkmux` restores it; no schema or
config-surface change.

### Fixed
- **Internal-runtime dispatch ran `docker docker run` (exit 125) (#975).**
  `build_docker_run_argv` returns the full command with the program name at
  `argv[0]` (`["docker", "run", "--rm", …]`), but the consumer pushed the whole
  vector as arguments to `Command::new("docker")`, duplicating the program.
  Split it (program = `argv[0]`, args = `argv[1..]`). Regressed in #848 and
  shipped silently because the tests only asserted the argv vector, never the
  constructed `Command` — the dispatch-argv coverage gap #842 flagged. Added a
  regression test that inspects the real `Command`.

## [1.4.0] - 2026-06-19

Completes the milestone-1.0 hardening pass. The `--json` machine-readable
output convention is now consistent across the read commands the frontier
orchestrator parses (the additive feature that makes this a minor), plus three
batches of correctness/safety polish from the swarm code review. No schema or
config-surface change; `brew upgrade darkmux` is a drop-in.

### Added
- **`--json` parity across the read commands (#907).** `status`, `profiles`,
  `model status`, `recommendations show`, and `role list`/`show` now accept
  `--json`, emitting machine-readable output for the frontier orchestrator
  instead of ANSI-styled text. Each serializes its existing domain shape;
  `role list --json` carries the full (untruncated) description.

### Fixed
- **Serve-daemon request-rate hardening (#925).** A per-route request timeout,
  a cap on concurrent SSE streams, and a bounded per-line read on the flow file,
  so a slow or abusive client can't exhaust the daemon.
- **Runtime nit-batch (#905).** XML tool-call promotion now fails soft per block
  (one malformed `<tool_call>` no longer drops the whole turn's recovered calls);
  the `TIMED OUT` marker only fires when the `timeout` wrapper actually ran (a
  user command exiting 124 isn't mislabeled); a failed non-JSON dispatch prints
  a summary instead of vanishing behind a bare exit code. Plus doc corrections
  (first-close-wins think-block scan; Bash isn't workspace-validated).
- **Lab / flow / profiles / hardware / crew nit-batch (#906).** Escalation
  hand-off targets are validated before the index rebuild (a clear, role-named
  error instead of an opaque deferred-FK abort that rolled back the whole
  rebuild); loaded-context sufficiency compares in `u64` (no truncation); an
  all-`.` `setupContent` key is rejected up front; `doctor` treats a TOCTOU
  file deletion as Pass, not a spurious Warn; Linux `physical_cores` counts
  physical cores (not logical); manifest reads have a 1 MiB cap; `lab register`
  warns that a fixture's `verify_command` runs on the host shell.
- **CLI / dispatch nit-batch (#907).** `mission migrate --apply` refuses to
  clobber an existing destination; `mission run`/`ship`/`abort` work for repos
  at non-ASCII / special-char paths (git C-quoted porcelain decode); docker
  image refs are validated before reaching docker; `external pull --url`
  allowlists `http(s)`; the default daemon port is single-sourced (correct for
  IPv6 / port-less addresses).

## [1.3.4] - 2026-06-19

The third milestone-1.0 safety-net cluster — fleet-substrate + correctness
fixes. No schema or config-surface change; `brew upgrade darkmux` is a drop-in.

### Fixed
- **Memory-headroom estimate tolerates more size formats (#904).** `eureka`'s
  `parse_size_gb` dropped `"18.45 GiB"`, `"18.45GB"` (no space), and comma
  sizes to `0`, undercounting the working set so the `MemoryHeadroomTight`
  warning under-fired (a tight system read as fine). It now parses binary
  (`GiB`/`MiB`/`TiB`) and no-space forms, and reports `Skipped` (naming the
  model) when a size truly can't be parsed instead of silently undercounting.
- **`notebook list` exits 0 when the dir is absent (#895).** A fresh user (or
  `notebook list && …` chaining) no longer sees a false error exit for a
  read-only "nothing to list".
- **Malformed work entries are XACKed, not leaked into the PEL forever (#903).**
  A claimed-but-unparseable fleet work entry (missing `record`, bad JSON, or a
  non-array fields slot) is now dropped from the consumer's pending-entries
  list via a new `Malformed` claim outcome, instead of being mistaken for a
  connection error and left pending indefinitely.
- **Presence reconciler closes two edge races (#902).** A failed close-edge
  write now releases its dedup claim so a peer can still record it (no lost
  `machine.offline`/`session.end` bracket), and the first tick after a
  `read_live` outage rebaselines instead of re-firing long-gone machines as
  fresh disappearances. (Also fixed a latent test-isolation flake surfaced
  along the way.)

### Changed
- **Doc-only: the fleet work-queue `schema` tag is documented as provenance,
  not a compat gate (#882).** Cross-version compatibility is enforced by serde
  shape (`deny_unknown_fields` + required-field deser), as the canonical
  `WORK_JOB_SCHEMA_VERSION` doc already states; the publish-side over-claim is
  corrected to match. No behavior change.

## [1.3.3] - 2026-06-19

A crash-path-hygiene patch — the second cluster of the milestone-1.0
safety-net drain. Four fixes that stop dispatches from corrupting operator
config or leaking resources on crash/error paths. No schema or config-surface
change; `brew upgrade darkmux` is a drop-in.

### Fixed
- **Atomic writes to `openclaw.json` (#901).** `apply_runtime` and the
  `doctor --fix` path wrote the operator's runtime config with a bare
  `fs::write` (truncate-then-stream); a crash / ENOSPC / power-loss mid-write
  could leave the operator's whole hand-authored config (`agents.list[]`,
  channel routing) empty or truncated. Both now write to a sibling temp and
  `rename(2)` onto the file, so a crash leaves the old config intact.
- **Lab-registry temp name is collision-free across threads (#898).** The
  atomic-save temp was process-unique only (`json.tmp.{pid}`); since `save()`
  is `pub(crate)`, two threads racing it could tear the temp before the rename.
  It's now process- and call-unique (`json.tmp.{pid}.{counter}`).
- **Dispatch tears down the watchdog and kills the container on a wait error
  (#889).** If `wait_with_output` itself failed, the dispatch returned without
  signaling the watchdog or killing the container — leaking a watchdog thread
  (which then fired a spurious kill) and potentially orphaning a running
  container until its deadline. The error path now stops the watchdog/sampler
  and best-effort `docker kill`s by the deterministic container name.
- **Auto dispatch workspaces are reclaimed on error/panic (#888).** A
  no-`--workdir` dispatch allocates a throwaway scratch tree in `/tmp`; it was
  never cleaned, so repeated failed dispatches accumulated trees (slow
  disk/inode exhaustion). An RAII guard now reclaims the auto-workspace on an
  error/panic exit before the container completes. An operator `--workdir` is
  never touched, and the bookkeeping dir (trajectory/metrics) is always
  retained so failed dispatches stay debuggable.

## [1.3.2] - 2026-06-19

A robustness patch — the first cluster of the milestone-1.0 safety-net drain.
Five agent-loop / runtime correctness fixes, no schema or config-surface change;
`brew upgrade darkmux` is a drop-in.

### Fixed
- **Hard-kill watchdog survives a poisoned deadline mutex (#890).** The inactivity
  deadline is shared between the trajectory tailer and the host watchdog; a panic
  in the tailer while holding the lock poisoned the mutex, and the watchdog's
  `.lock().unwrap()` then panicked on its next tick — silently disabling the
  hard kill so a stuck dispatch could hang forever. All deadline lock sites now
  recover a poisoned lock, making the safety-net thread the most panic-resilient
  consumer rather than the least.
- **Error-path metrics no longer mislabel infra failures as turn-cap hits (#884).**
  The loop-error branch hardcoded `max_turns_reached: true`, so every
  infrastructure failure looked like a turn-cap termination, corrupting the
  three-way result discrimination downstream consumers branch on. It now reports
  `false`, matching the success path's derivation.
- **Compaction reports the true summary size (#885).** `summary_chars` was read
  from a fixed `messages` index (assuming the preserved head was exactly two
  messages); the compaction functions now return the inserted summary's actual
  char count, so the observability field can't silently report an unrelated
  message's length.
- **Failure-cascade detector framing corrected (#886).** The per-`(tool, args)`-
  signature failure counter was named `consecutive_failures` and described as
  "consecutive / in a row" across the runtime, the host flow message, and the
  analyze-run skill doc — none accurate. Renamed to `failure_count` and reworded
  to the real per-signature semantics. Behavior unchanged.
- **`mission propose` JSON extraction handles malformed model output (#896).**
  `extract_json_block` now prefers a ` ```json `-tagged opener over a bare fence
  (so a bare code block before the real JSON can't capture the wrong region) and
  emits a distinct "unterminated fenced block" error on truncated output instead
  of a misleading "no block found".

(Also: #887 — the inactivity soft-warning's inability to fire mid-stream — was
confirmed working-as-intended and documented; the host hard kill covers
within-turn hangs. No behavior change.)

## [1.3.1] - 2026-06-18

A security-hardening patch. Drains the milestone-1.0 security cluster — five
fixes that close workspace-escape, traversal, and denial-of-service surfaces
across the runtime, lab, serve daemon, and crew/flow subsystems — and finishes
the daemon colorization started in 1.3.0. No schema or config-surface change;
`brew upgrade darkmux` is a drop-in.

### Fixed
- **Runtime refuses writes through a final-component symlink (#883).** A coder
  dispatch could previously be steered into writing through a symlink whose final
  path component pointed outside the mounted workspace. `resolve_write` now
  `lstat`s the final component and refuses a symlink target, closing the escape.
- **Lab validates the sandbox-seed path and stops following symlinks (#897).**
  `coding_task` now rejects seed-key paths that escape the sandbox base
  (canonicalized + `starts_with` containment on both sides) and copies seed
  directories with a no-follow walk, so a symlinked seed entry can't read or write
  outside the run sandbox.
- **Serve daemon bounds the per-day flow-file read (#900).** `/flow/:date` now
  streams the file and keeps only the newest 10,000 records in a ring buffer
  instead of loading an unbounded file into memory, removing a memory-exhaustion
  vector. (Broader request-rate limiting is tracked in #925.)
- **`crew sync` requires `--yes` to write `openclaw.json` (#893).** A bare
  `crew sync` now previews the pending changes and bails with a re-run pointer
  rather than silently mutating operator-owned `openclaw.json`; `--dry-run`
  previews without the gate. Restores the preview-then-confirm sovereignty
  contract.
- **Audit re-seed requires a schema header (#899).** `flow integrity-check` only
  re-seeds the hash chain from a single-line file when that line is the schema
  header; a non-schema single line now bails instead of silently anchoring the
  chain to arbitrary content. The "tamper-evident" phrasing across the code, docs,
  and README is scoped to the detection property the `integrity-check` verb
  actually provides.
- **Colorized the remaining daemon runtime output (#922).** The presence,
  reconciler, fleet-runner, and routing error/warning lines now render through the
  shared style module (TTY- and `NO_COLOR`-gated), completing the daemon
  colorization begun in 1.3.0 (#918).

## [1.3.0] - 2026-06-17

Hardens the serve daemon and the crew index. The headline is **serve daemon
authentication** (#881), which closes the last unauthenticated exposure when the
daemon binds beyond loopback — alongside a fix for a daemon shutdown hang and a
cluster of crew-index correctness repairs.

### Added
- **Serve daemon authentication (#881).** The flow daemon can require a bearer
  token: remote reads and `/diff` are gated while loopback stays open (the local
  viewer is unaffected), and `/health` is always exempt. The token lives in the
  macOS Keychain (`darkmux-serve-token`) or `DARKMUX_SERVE_TOKEN` — never plaintext
  config — and `fleet status --deep` forwards the shared token to peers. `darkmux
  doctor` and the startup banner report the auth posture.
- **Colorized daemon runtime output (#918).** The serve and fleet-runner runtime
  error/warning lines now render red/yellow through the shared style module
  (TTY- and `NO_COLOR`-gated), matching `doctor` and the startup banner.

### Changed
- **BREAKING (narrow): `darkmux serve` refuses a non-loopback `--bind` unless a
  token is configured (#881).** The default install is unchanged — loopback bind,
  no token, the viewer works as today. Only the previously-allowed "bind to a
  non-loopback address with no authentication" setup is now refused (it exposed
  flow records, machine specs, mission state, and live `git diff` to any reachable
  peer). Set a serve token to bind beyond loopback. No action needed for default
  or loopback users.

### Fixed
- **Serve daemon shutdown hang (#918).** The force-exit watchdog ran as a tokio
  task that was cancelled when the runtime dropped, so a wedged background thread
  (e.g. a Redis worker pointed at an unreachable endpoint) could hang the daemon
  after "clean shutdown" printed. The watchdog now runs on a dedicated OS thread
  and guarantees the process exits within the grace window.
- **Crew index self-heals across schema changes (#914).** `darkmux role list`/`show`
  and `crew list`/`show` rebuild the local index on demand, and a schema-drifted
  index (e.g. the mission/sprint timestamp columns) no longer crashes the rebuild
  or silently serves stale data. No operator action — the index auto-rebuilds.
- **Crew index correctness cluster (#894, #891, #892).** `role show` no longer
  errors when a hand-off target row is missing; drift detection catches content
  edits that don't advance mtime; manifest ids strip exactly one `.json`, and
  `load_skills` keys on the authoritative body id so a misnamed user skill
  overrides the builtin.
- **Activity lane brackets `session.end`-only sessions as ended (#856),** so an
  idle machine's bar no longer stretches to the playhead; adds the first
  viewer-lifecycle e2e regression gate.

[1.14.1]: https://github.com/kstrat2001/darkmux/releases/tag/v1.14.1
[1.14.0]: https://github.com/kstrat2001/darkmux/releases/tag/v1.14.0
[1.13.1]: https://github.com/kstrat2001/darkmux/releases/tag/v1.13.1
[1.13.0]: https://github.com/kstrat2001/darkmux/releases/tag/v1.13.0
[1.12.0]: https://github.com/kstrat2001/darkmux/releases/tag/v1.12.0
[1.11.2]: https://github.com/kstrat2001/darkmux/releases/tag/v1.11.2
[1.11.1]: https://github.com/kstrat2001/darkmux/releases/tag/v1.11.1
[1.11.0]: https://github.com/kstrat2001/darkmux/releases/tag/v1.11.0
[1.10.0]: https://github.com/kstrat2001/darkmux/releases/tag/v1.10.0
[1.9.0]: https://github.com/kstrat2001/darkmux/releases/tag/v1.9.0
[1.8.0]: https://github.com/kstrat2001/darkmux/releases/tag/v1.8.0
[1.7.0]: https://github.com/kstrat2001/darkmux/releases/tag/v1.7.0
[1.6.0]: https://github.com/kstrat2001/darkmux/releases/tag/v1.6.0
[1.5.0]: https://github.com/kstrat2001/darkmux/releases/tag/v1.5.0
[1.4.1]: https://github.com/kstrat2001/darkmux/releases/tag/v1.4.1
[1.4.0]: https://github.com/kstrat2001/darkmux/releases/tag/v1.4.0
[1.3.4]: https://github.com/kstrat2001/darkmux/releases/tag/v1.3.4
[1.3.3]: https://github.com/kstrat2001/darkmux/releases/tag/v1.3.3
[1.3.2]: https://github.com/kstrat2001/darkmux/releases/tag/v1.3.2
[1.3.1]: https://github.com/kstrat2001/darkmux/releases/tag/v1.3.1
[1.3.0]: https://github.com/kstrat2001/darkmux/releases/tag/v1.3.0

## [1.2.0] - 2026-06-15

The stability + security hardening release. A multi-agent code review swept the
whole codebase; this release lands the remediation — closing a path-traversal
write primitive, an audit-record loss gap, a config-precedence bypass, and two
runtime panics — alongside dispatch-boundary hardening and richer CLI output.

### Added
- **Colorized dispatch/lab telemetry + tabular CLI verbs (#776).** Run and lab
  telemetry render in color, and the tabular verbs align cleanly for at-a-glance
  reading.
- **`mission ship` is commit-identity-aware (#834).** It honors
  `conventions.json` `commit_author` and enforces a separation-of-duties guard.
- **Dispatch-boundary hardening.** Queue-originated `WorkJob.image` is validated
  at the queue boundary (#838) and `WorkJob.workdir` is base-restricted under
  `~/.darkmux/worktrees` (#840); the dispatch `docker run` invocation is hardened
  (#839).

### Fixed
- **Path traversal from untrusted model output (#867).** Model-supplied
  `mission.id` / `sprint.id` are validated with `fleet::validate_identifier`
  before any path construction, closing a constrained arbitrary-`.json`-write
  primitive in `mission propose`.
- **Audit-record silent loss (#877).** A dropped `AuditFileSink` write now leaves
  a durable breadcrumb in the local sink and `doctor` surfaces the dropped-write
  count, instead of a record vanishing under the best-effort `TeeSink`.
- **Config-precedence bypass (#875).** Production `DARKMUX_*` reads
  (`redis.stream`/`maxlen`, `audit.dir`/`enabled`, `default_role`, CORS origins)
  now route through `config_access`, so `config.json`-only operators get their
  settings honored.
- **Runtime panic on multibyte input (#873).** The compaction slot cap clamps to
  a char boundary before truncating, so a non-ASCII objective no longer panics
  `apply_slot_caps`.
- **Lab harness panic on non-ASCII (#869).** `detect_claim_verify_mismatch`
  builds its excerpt in a consistent index space, so a non-ASCII window around a
  matched claim phrase no longer panics after the dispatch ran.
- **`requires_fixture` honesty (#871).** The matcher is documented as literal
  `name@version` and loudly rejects semver operators that would silently never
  match.
- **Stale `prompt_tokens` (#854).** A stale token count is detected and a local
  estimate substituted for the compaction trigger, fixing a suppressed
  compaction + phantom context drop.
- **`mission ship` from inside a worktree (#844, #846).** Post-merge
  sprint-complete + teardown no longer silently drift when run from the worktree
  layout; the viewer counts `session.end` as a dispatch terminal (#856).
- **Config tier no longer leaks into tests (#811).** Test builds neutralize the
  config tier by construction, so test flow records never reach the operator's
  real Redis stream and default-assertion tests don't flake on a populated
  `config.json`.

### Documentation
- Research-grounded `ROADMAP.md` with themed post-1.0 milestones (M4 loop-depth
  lead) and verified per-theme citations (#850, #853).
- Orchestrator-first getting-started, post-1.0 framing, screenshot refresh, and
  an em-dash cleanup pass across the public docs (#858, #859, #860, #861, #862,
  #863).

[1.2.0]: https://github.com/kstrat2001/darkmux/releases/tag/v1.2.0

## [1.1.0] - 2026-06-14

The work-level observability release: missions become a first-class lens —
across the fleet, in the CLI, and on the dashboard — so you can see how a
mission progresses (sprints + the run→qa→gate→ship cycle), not just what each
machine is doing.

### Added
- **Missions lens in the viewer (#827).** A `fleet | missions` toggle adds a
  work-centric view alongside the machine-centric fleet view — "all machines as
  one" at the work level. A missions index lists every mission with sprint
  progress + cross-machine token rollup; the detail renders the durable sprint
  plan with each sprint's status and a **run → qa → gate → ship cycle strip**,
  click-through to the per-machine run (#828, #832, #833).
- **`darkmux mission status` (#829).** The global mission-control read,
  completing the `<noun> status` family (`flow status`, `model status`): every
  mission grouped by status with sprint progress, the drift that needs
  attention (a Closed mission with a non-terminal sprint; an open mission whose
  sprints are all done), and copy-pasteable, state-accurate reconcile commands.
  Read-only; `--json` for the orchestrator / CI (#830, #831).

### Fixed
- **Live-diff no longer flickers/reloads (#826).** The session-view diff panel
  was rebuilt on every live record (~1/sec during a run), destroying its DOM
  and scroll; it now paints into a stable mount, repainting only on real
  changes with scroll preserved.

### Internal
- A bundled maintainer skill, `darkmux-point-release`, standardizes this release
  ceremony (not shipped to brew installs).

[1.1.0]: https://github.com/kstrat2001/darkmux/releases/tag/v1.1.0

## [1.0.0] - 2026-06-13

darkmux 1.0 — semver stability begins. The release that closes the loop:
darkmux now runs the full local dispatch-to-PR cycle, shows the work (and the
savings) live, and was used to build itself — the observability features in
this release were shipped through `mission run`, and the savings figure on
darkmux.com is this release's own development telemetry.

### Added
- **`mission run` / `mission ship` / `mission abort` — the local dispatch-to-PR
  loop (#782).** `run` creates an isolated git worktree, dispatches the coder
  (sprint-bound, internal runtime), runs the local `code-reviewer` QA against
  the diff, and STOPS at a sign-off gate; `ship` commits, pushes, opens the PR,
  and (opt-in, green-gated) squash-merges — never auto-merge (#786, #787, #788).
- **Verbatim spec fidelity (#815).** `mission propose --ticket <ID>` stamps the
  operator's unabridged input onto the mission; every coder brief carries it
  under an authority-stamped provenance block, so exact strings and constraints
  survive the mission-compiler's summarization (#820).
- **Repo-level shipping conventions (#816).** `<repo>/.darkmux/conventions.json`
  — branch/commit-subject/PR-title templates with `{ticket}`/`{sprint}`/
  `{mission}`/`{subject}` vars, a PR body template, and PR labels. Ship pushes
  the worktree's actual branch, so mid-flight conventions edits can't drift
  (#821).
- **Per-turn token telemetry (#795).** The runtime tailer emits a
  `telemetry.tokens` flow record per model turn (FLOW_SCHEMA 1.13) — the
  dashboard's savings odometer climbs live DURING a dispatch (#800).
- **The savings hero (#783, #803).** "Tokens off the meter" headline with a
  token-class breakdown — generated / fresh input / re-read input — that
  teaches the agent-loop economics (a typical day: ~90% of input is re-read
  context). Tokens only, never currency (#791–#793, #804, #805).
- **Orchestrator notes (#807, #817, #819).** A real channel for the frontier
  orchestrator's voice: `darkmux flow note --source orchestrator` renders as
  the card's conclusion with a history modal; gate/ship print ready-to-paste
  scaffolds (session-id pre-filled) splitting upbeat dashboard notes from
  session-scoped technical adjudications; ship soft-warns when a gated sprint
  ships with a noteless trail (#808, #812, #818, #819, #822).
- **Live diff (#756).** `GET /diff/:session_id` serves the running git diff of
  a mission-run worktree (path-contained, ref-validated, size-bounded); the
  session view renders it live — watch the agent's code form in real time. The
  endpoint was built end-to-end by the local coder through `mission run`
  (#801, #802).
- **Activity-driven live headline (#789).** The viewer's headline tracks the
  live session (mission-scoped → clickable) and reads an affirmative fleet
  status when idle (#790).
- **CLI styling pass (#772–#776).** Semantic color across doctor / scan /
  model-status / profiles / dispatch telemetry, tty-gated (#777–#781).
- **Runtime image on GHCR (#759).** The `darkmux-runtime` image publishes on
  release and pulls on demand — `brew install darkmux` alone can dispatch
  (#764, #765).
- **darkmux.com refresh.** Homebrew-first install docs, copy-to-clipboard on
  all snippets, and the live savings-hero screenshot under the why-headline —
  real work, real telemetry, not a mockup (#763, #766–#770, #784/#823).

### Fixed
- **Saturated Redis streams no longer drop the live tail (#809).** Day reads
  and fleet completion-waits now read newest-first (`XREVRANGE`); at the
  `MAXLEN` cap the oldest records age out instead of the newest vanishing
  (#810).
- **Live-tail idempotency (#794).** SSE re-delivery is identity-deduped so
  cumulative readouts can't inflate and "reset" on refresh (#796).
- Activity-timeline rightmost bar no longer clips past the track (#797);
  savings hero is always visible and compact on mobile (#792, #793).

[1.0.0]: https://github.com/kstrat2001/darkmux/releases/tag/v1.0.0

## [0.9.0] - 2026-06-11

First tagged release. 0.9.0 exercises the full Homebrew + release pipeline ahead
of 1.0; it captures the work merged on `main` since the changelog was seeded.

### Added
- **`config.json` configuration subsystem (#661).** `darkmux init` writes a
  self-documenting `~/.darkmux/config.json` with the common knobs visible (not
  hidden as code-defaults). Every setting resolves with one precedence —
  `env(DARKMUX_*) > config.json > built-in default` — surfaced by `darkmux doctor`.
  Off-by-default integrations are `enabled`-gated blocks; the Redis password is the
  only carve-out (macOS Keychain, never plaintext) (#662–#679).
- **Daemon-hosted observability viewer + playback catalog (#557, #691).**
  `darkmux serve` serves the viewer at `GET /` with a live SSE tail; a rolling 24h
  live window driven by presence heartbeats; a `/flow-days` catalog with a day
  picker; first-class event search; an expandable recent-runs list and an
  unscoped-records section (#582–#584, #682, #710, #715, #723–#729, #731, #748).
- **Presence-driven live fleet view (#638).** A machine shows in the live fleet
  when it's heartbeating — records or not — and consistently across live and
  playback (#651, #653).
- **In-sandbox compile via binary injection (#703).** `crew dispatch --image <any
  Linux image>` injects darkmux's static runtime binary into that image, so the
  coder/test-designer roles can run the inner verify loop (`cargo check`/`test`,
  etc.) in-sandbox. darkmux ships no per-language images — bring the agent, you
  bring the environment (#705–#708).
- **`darkmux flow tail` verb (#695)** — follow flow records live from the CLI (#740).
- **Google Antigravity orchestrator support** with zero-config auto-detection, plus
  unified orchestrator naming (#734, #735, #738).
- **`mission_id` / `sprint_id` stamped on crew-dispatch flow records (#716).**
- **`SECURITY.md` + a `cargo-audit` CI job** (daily + dependency-gated) (#744).
- **Homebrew distribution (#618).** The `kstrat2001/homebrew-darkmux` tap is live
  with a formula auto-synced from `main`; docs lead with `brew install` (#650, #652,
  #654). (Stable bottled release lands with this tag.)
- `doctor` proactively surfaces the Docker runtime requirement (#680) and warns when
  `OPENAI_BASE_URL` would silently defeat `darkmux swap` (#5) (#681, #753).
- Capability-based model selection scaffolding: capability vectors on `ProfileModel`,
  a `select_model` scorer, and a two-value `role_family` axis (#588, #599, #592).
- Machine-level `internal.utility` model — one global utility/compactor per machine,
  loaded alongside workers on `swap`, with a `doctor` loaded-guard and a
  pre-compaction loaded-check (#593, #594, #602).
- `lab doctor` fixture-cleanliness check — flags stray run-artifact dirs left in a
  fixture source (#610).
- The viewer respects `prefers-reduced-motion` (drops the infinite live-badge pulse) (#238, #751).

### Changed
- **OpenClaw is now opt-in, not the default.** `swap` patches openclaw config only
  under an explicit `--runtime openclaw`; `crew dispatch` / `lab run` default to the
  internal Docker-bounded runtime (#606, #607).
- **The fleet executor is now the `runner`** (was `worker`) — a single overloaded
  term retired; `lab-runner` → `lab-manager` to resolve the collision (#595, #659,
  #660, #688).
- **`DARKMUX_LMSTUDIO_URL` is now the base URL** — callers append `/v1/...`
  (semantic break) (#673).
- **The profiles registry is configured as `profiles`** — `DARKMUX_PROFILES` env and
  the `--profiles-file` flag (renamed from the misleading `--config`/`DARKMUX_CONFIG`,
  then from `--profiles`) now that a real `config.json` exists (#677, #739).
- **`swap --recommended`** replaces the reserved `"recommended"` profile name (#700, #702).
- **`profiles.json` gains `schema_version` + forward-compatible extras** so an older
  binary tolerates a newer file (#694, #712).
- Viewer output-encoding hardening — record-derived fields are escaped at the
  template edge and clicks run through one delegated handler (no inline handlers);
  container-written trajectory fields are bounded at ingest (#237, #743, #749).
- `swap` treats a profile's `n_ctx` as a minimum, not an exact size (#600).
- `crew dispatch` resolves and logs the `--profile` override rather than silently
  using the registry default (#608).
- Fleet work-routing collapsed to a single `darkmux:work` stream (first-available
  claims); per-tier routing retired (#604).
- The internal runtime writes its bookkeeping (`.darkmux-runtime/`) to a mounted
  out-dir, never inside the workspace it operates on (#611).
- One canonical `RUN_ARTIFACT_DIRS` shared by the lab clone, the content hash, and
  the workspace-delta view; per-run clones are pruned clean by construction (#609).
- The frontier-orchestrator label generalized from `frontier-claude` to `frontier`,
  with richer telemetry formatting (#738).

### Removed (breaking, pre-1.0)
- `ModelRole` — `default_model` is the canonical worker (#601).
- Machine-tier across the stack: `Role.tier`, `FlowRecord.machine_tier`,
  `WorkJob.target_tier`, and the `{inference/hub/client}` taxonomy (#587, #604, #605).
- `ProfileRuntime` camelCase serde aliases — fields are snake_case only (#699, #709).
- Run-manifest keys normalized to snake_case (#698, #719).
- Dead fixture-manifest fields `hash_include` / `hash_exclude` (never consumed) (#610).

### Fixed
- `DarkmuxPaths.profiles` pointed at `profiles.yaml` instead of `profiles.json` (#585).
- Atomic line append in `LocalFileSink` — fixes concurrent-write tearing and a
  crew-dispatch flake (#667).
- Internal dispatch is bookended with a terminal record; killed runs are recognized
  as `dispatch.error` rather than reading as still-running (#717, #718, #720, #721).
- The live SSE stream re-targets the new day file on UTC date rollover (#730, #731).
- The runtime returns a non-zero exit status on `EscalationTriggered` (#737).
- Lab fixture content-hash drift from stray `coverage/` and `.darkmux-agent/` dirs —
  now excluded from the hash and pruned from per-run clones (#609).
