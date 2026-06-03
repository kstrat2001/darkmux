# Changelog

All notable user-facing changes to darkmux are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

darkmux is **pre-1.0**. Per the project's no-compat-baggage policy, breaking
changes ship cleanly while the surface stabilizes (no deprecation shims). Roadmap
**milestones** (`M1`/`M2`/`M3`…) are intentionally decoupled from these version
numbers, and the `RULES_SCHEMA` / `FLOW_SCHEMA` data-shape contracts version on
their own cadence (see `CLAUDE.md`). Semver stability begins at 1.0.

This file is seeded from merged-PR history; the first tagged release will be the
first entry promoted out of `[Unreleased]`.

## [Unreleased]

### Added
- Capability-based model selection (scaffolding): model capability vectors on
  `ProfileModel`, a `select_model` capability scorer, and per-role `role_family`
  validated as a two-value axis (#588, #599, #592).
- Machine-level `internal.utility` model — one global utility/compactor model per
  machine, loaded alongside workers on `swap`, with a `doctor` loaded-guard and a
  pre-compaction loaded-check (#593, #594, #602).
- `lab doctor` fixture-cleanliness check — flags stray run-artifact dirs
  (`.darkmux-runtime`, `coverage`, …) left in a fixture source (#610).
- Daemon-hosted observability viewer — `darkmux serve` serves the viewer at `GET /`
  with a live SSE tail and a fleet activity timeline (#583, #584).
- `architecture/CONCEPTS.md` — operator-facing source of truth (#579).

### Changed
- **OpenClaw is now opt-in, not the default.** `swap` patches openclaw config only
  under an explicit `--runtime openclaw`; `crew dispatch` / `lab run` default to the
  internal Docker-bounded runtime (#606, #607).
- The internal runtime writes its bookkeeping (`.darkmux-runtime/`) to a mounted
  out-dir, never inside the workspace it operates on — so a `crew dispatch --workdir`
  directory or a lab fixture no longer collects droppings (#611).
- One canonical `RUN_ARTIFACT_DIRS` shared by the lab clone, the content hash, and
  the workspace-delta view; per-run clones are pruned clean by construction (#609).
- Fleet work-routing collapsed to a single `darkmux:work` stream (first-available
  claims); per-tier routing retired (#604).
- `swap` treats a profile's `n_ctx` as a minimum, not an exact size (#600).
- `crew dispatch` resolves and logs the `--profile` override rather than silently
  using the registry default (#608).

### Removed
- `ModelRole` — `default_model` is the canonical worker (#601).
- Machine-tier across the stack: `Role.tier`, `FlowRecord.machine_tier`,
  `WorkJob.target_tier`, and the `{inference/hub/client}` taxonomy (#587, #604, #605).
- Dead fixture-manifest fields `hash_include` / `hash_exclude` (never consumed) (#610).

### Fixed
- `DarkmuxPaths.profiles` pointed at `profiles.yaml` instead of `profiles.json` (#585).
- Lab fixture content-hash drift caused by stray `coverage/` and `.darkmux-agent/`
  dirs — now excluded from the hash and pruned from per-run clones (#609).
