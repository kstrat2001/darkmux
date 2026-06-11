# Changelog

All notable user-facing changes to darkmux are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

darkmux is **pre-1.0**. Per the project's no-compat-baggage policy, breaking
changes ship cleanly while the surface stabilizes (no deprecation shims). Roadmap
**milestones** (`M1`/`M2`/`M3`…) are intentionally decoupled from these version
numbers, and the `RULES_SCHEMA` / `FLOW_SCHEMA` data-shape contracts version on
their own cadence (see `CLAUDE.md`). Semver stability begins at 1.0.

## [Unreleased]

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
