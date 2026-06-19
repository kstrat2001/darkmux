# Changelog

All notable user-facing changes to darkmux are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

darkmux follows semver, stable since **1.0.0**; breaking changes are called out
explicitly in each entry (pre-1.0, the no-compat-baggage policy shipped breaks
without deprecation shims). Roadmap **milestones** (`M1`/`M2`/`M3`…) are
intentionally decoupled from these version numbers, and the `RULES_SCHEMA` /
`FLOW_SCHEMA` data-shape contracts version on their own cadence (see `CLAUDE.md`).

## [Unreleased]

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
