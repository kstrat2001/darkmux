# Antigravity / Agent guidance for darkmux

This file is for any AI agent (Antigravity, Claude Code, Cursor, etc.) that's helping a user work with the darkmux source tree. Read this once before doing anything.

## What darkmux is

A Rust CLI (v2.x) that is two things for users running local LLMs:

1. **Mission orchestrator**: config-defined missions launched with `darkmux mission launch <config>` that run as a live task graph. A crew of local-AI roles works the phases through the internal Docker-bounded runtime (any seat can instead be staffed by a hosted cloud endpoint), every dispatch gated on operator sign-off, each run finalizing into a typed envelope. `darkmux dispatch <role> <message>` is the task-grain entry point (one role, one turn). This is the 2.0 headline.
2. **Lab harness**: `darkmux lab run <workload>` dispatches a workload against the same internal runtime and records timing + trajectory + verify outcome under `.darkmux/runs/<run-id>/`.

Managing model residency (the founding *profile multiplexer*: loading the right models at the right context under the RAM budget) is now an internal capability underneath both, not a verb the operator drives. The `swap` verb retired on the 2.0 track (#1426); gestalt loads what each dispatch's staffing declares.

**Backend, stated honestly (#316):** darkmux drives **LMStudio** today, and only LMStudio. The residency arbiter, the `darkmux:` namespace convention, the empirical profile defaults, and every `lms`-shell-out in `darkmux-gestalt` are LMStudio-shaped. This line previously read "LMStudio + Ollama + llama.cpp"; that was aspiration, not capability, and a fresh agent session reading it would confidently propose work against backends that do not exist. A `ModelBackend` abstraction is tracked as #316 and is deliberately NOT being built. Do not deepen LMStudio coupling gratuitously in new code, but do not pretend the abstraction is there either.

The CLI is the *engine*; the empirical findings in the Genesis series on Darkly Energized (<https://darklyenergized.substack.com>) are what it backs. The reproducibility story is the product story: users should be able to rerun a workload and get numbers comparable to the published claims.

## darkmux's grand vision (agent-facing)

The user-facing **"What darkmux is for"** section in `README.md` is the canonical version of the project's north-star. Below is how the same five claims translate into operational doctrine for an AI agent (Antigravity, Claude Code, etc.) working on darkmux or driving it on behalf of an operator.

1. **Optimization, not replacement.** When the operator asks you to pick a model from `lms ls` or propose a profile, prefer *complement* over *duplicate*. A team where every model is a 35B reasoner is not a team; it's a stack of identical instruments. The same logic applies *within* each role family (see **Project posture → Role families** below): a profile with three different 35B specialists and no 4B utility agent is missing its compactor, scribe, and estimator; conversely, a profile of nothing but utility agents has no specialist to do the actual judgment-dependent work. Read the existing profile registry first; propose additions that fill gaps in the right family (utility: compactor / scribe / estimator / mission-compiler; specialist: coder / reviewer / analyst) rather than swapping like for like.

2. **Harness, then model.** When the operator reports slow or wrong outputs, **check the harness before the model**. Compaction config, context-window mismatches, loaded-state drift, profile-vs-loaded model: all of these can produce large wall-clock regressions that look like model problems but are actually harness problems. Default action: run `darkmux doctor`, read the eureka findings, surface those *before* suggesting the operator change models.

3. **The lab + the loop.** darkmux is not just an inspection tool; it's the loop. When you have a tuning hypothesis (e.g., *"primary at 64K instead of 100K might fit this 32GB tier"*), the correct action sequence is: **baseline → single-variable change → re-measure → compare → record in notebook**. Each step has a darkmux primitive. Do NOT skip the baseline. Do NOT change two variables at once. The discipline is the point: without it, the comparison is uninterpretable.

4. **Team integrity is your responsibility.** When proposing config changes, frame them in terms of *how this affects the team's shape*, not just an isolated metric. *"Drop the compactor to free RAM"* reduces working memory; consider whether the remaining team can still handle long-agentic dispatches before recommending. The operator is depending on you to maintain team coherence as new models arrive and hardware changes.

5. **The success criterion is recursive.** A fresh agent session, given only a clean-slate darkmux install + these docs + the bundled skills, should reach the same conclusion about *"what is darkmux for?"* as the rest of these doctrine entries name. If you find yourself uncertain or having to infer from primitives, **the docs have drifted from the vision**; surface that to the operator. Doc drift is a bug, not a footnote.

These claims compose with the existing **Anti-patterns** section below: anti-patterns are *what not to do*; the vision is *what to do instead*. If a request would violate both at once (e.g., *"silently roll back the compactor without telling me"*), the vision wins: surface the conflict and let the operator decide.

## Build and test

```bash
cargo build --release    # release binary at target/release/darkmux
cargo t-review           # test ONE area — see "Testing" below; NOT the whole suite
cargo clippy             # lint
cargo fmt                # format
cargo install --path .   # install to ~/.cargo/bin/darkmux
```

The release binary is self-contained (~11 MB as of 2.5.0). Built-in workloads, roles, mission configs, skills, the viewer, and the mission-graph lens's vendored React Flow bundle all ride inside it via `include_str!`/`include_bytes!`; `cargo install --path .` produces a binary that works from any directory without the source tree.


## Testing — run the area, not the world

**The full workspace suite is CI's job, not yours.** CI runs it on every PR for
free. Locally, run the area you touched — committed cargo aliases name them,
and each takes seconds:

`cargo t-flow` · `t-review` · `t-serve` · `t-doctor` · `t-fleet` · `t-gestalt` ·
`t-cli` · `t-fast` · `t-runtime` (the runtime crate is NOT a workspace member,
so `t-all` does not reach it) · `t-all` (the merge gate — CI already runs it).

These wrap `cargo nextest` (`cargo install cargo-nextest --locked`). The reason
is NOT speed — on one area nextest and `cargo test` are equivalent — it is
`.config/nextest.toml`'s per-test `terminate-after`, which turns a HANG into a
loud failure instead of a wedged run.

Reach for `t-all` only for a reason you can state: a change crossing crate
boundaries no single area covers, or a release. "To be safe" is not a reason.

Three habits to avoid, all of which feel diligent: running the world when one
area covers it; re-running a green suite to feel sure (if you doubt a result,
write a test that can FAIL for that reason); and full-suite-before-every-commit.

Background lanes: `scripts/test-lane.sh review t-review` gives a run its own
`CARGO_TARGET_DIR` so it doesn't contend with foreground builds. ~13 GB per
lane; keep two or three, not one per area.

Faster tests are not more trustworthy tests — a false-green gate (#1716) just
returns its wrong answer sooner.

## Configuration (`config.json`)

darkmux's canonical config surface is **`~/.darkmux/config.json`**, written by `darkmux init`. Every setting resolves with one precedence, **`env(DARKMUX_*) > config.json > built-in default`**, and that precedence lives in exactly ONE place: `darkmux_types::config_access` (the env tier is read **live per-access**, so a `set_var` in a test or a power-user export still wins). A reader never has to wonder where a setting came from; `darkmux doctor` surfaces the resolved value + source.

**The file is self-documenting by design.** `init` writes the common knobs *visible* (not hidden as code-defaults), so the operator tunes the file, not the source. Off-by-default integrations are **feature blocks gated by an `enabled` field, not by field-presence**: `init` writes the whole block with `enabled: false` and the sub-defaults populated, so the surface is discoverable and one flip from on:

```json
{
  "schema_version": "1.0",
  "machine_id": "studio",
  "lms_bin": "lms",
  "lmstudio_url": "http://localhost:1234",
  "redis":   { "enabled": false, "host": "127.0.0.1", "port": 6379, "stream": "darkmux:flow", "maxlen": 10000 },
  "audit":   { "enabled": false, "dir": "~/.darkmux/audit" },
  "runtime": { "inactivity_timeout_seconds": 600, "strict_selection": false, "feedback_injection": true, "check_updates": true }
}
```

When proposing a config change to an operator, write the visible field; don't reach for an env var as the primary mechanism. **Deliberately NOT written by `init`** (because a literal would be wrong, not because they're hidden): `dirs.*` (derived from the root; `darkmux doctor` shows the resolved path) and caps like `runtime.max_turns` (absent = uncapped, a real behavior).

**Carve-outs (the ONLY things NOT plaintext config):**
- **Redis password → macOS Keychain** (item `darkmux-redis`, the same item the Homebrew wrapper populates). `config.redis` holds only non-secret bits (`enabled`/`host`/`port`/`db`/`stream`/`maxlen`); the password is read at runtime via `security find-generic-password` and never logged; every URL is wrapped in `RawRedisUrl` (redacted `Display` + `Debug`; raw bytes only via `expose_for_probe`). Non-macOS uses the full-URL env override. `redis_url()` resolves `env(DARKMUX_REDIS_URL) verbatim > config.redis.enabled + Keychain > off`.
- **`DARKMUX_HOME`**: the bootstrap pointer that *locates* the config root (`<root>/config.json`); it can't live inside the config it finds, so it stays an env var.

**Schema is minor-bump + lenient on read** (all-`Option` + `#[serde(flatten)] extras` overflow): an older binary tolerates a newer config, and a partial/hand-edited/malformed config never bricks the CLI; loud validation belongs to `darkmux doctor`, not the hot load path. `CONFIG_SCHEMA_VERSION` lives in `darkmux-types/src/config.rs`.

**Don't confuse `config.json` with the profiles registry.** `~/.darkmux/profiles.json` (the swap profiles) is a SEPARATE file, overridden by `--profiles-file` / `DARKMUX_PROFILES`, **renamed in #661 from the misleading `--config` / `DARKMUX_CONFIG`** (those names are retired, not reused, because a real `config.json` now exists).

## Environment variables

Every `DARKMUX_*` var is the top tier of **`env > config.json > built-in
default`**, resolved in one place (`darkmux_types::config_access`), with the
env tier read live per access.

**The full table — every variable, its default, its effect, and the
`config.json` field it maps to — is in [`docs/ENVIRONMENT.md`](docs/ENVIRONMENT.md).**
Read it when you need to know what a knob means. To find out what a setting
resolves to *right now*, run `darkmux doctor`, which prints the resolved value
with its provenance — that is the better answer to that question, and it cannot
go stale.

Two rules worth carrying without looking anything up:

- **Secrets are never `config.json`.** The Redis password and the serve token
  live in the macOS Keychain, read at runtime, wrapped so `Debug`/`Display`
  redact them. `darkmux config set` refuses those keys outright.
- **When proposing a setting change to an operator, write the visible
  `config.json` field** via `darkmux config set <key> <value>` — do not reach
  for an env var as the primary mechanism. Env is for per-shell, CI, and test
  overrides.


## Where things live

Refer to the "Where things live" section in `CLAUDE.md` for the directory and file maps, as they remain the authoritative layout map for the workspace.

## Conventions to follow

- **Don't add dependencies casually.** The dep set is deliberately small (`anyhow`, `clap`, `serde`, `serde_json`, `dirs`). A 10-line inline module beats a crate for small one-off needs.
- **Trait providers, not feature flags.** New workload kinds go through the `WorkloadProvider` trait in `src/workloads/types.rs`, registered in `src/workloads/registry.rs::register_builtins()`. Don't bolt new behavior into the lab orchestrator.
- **Manifests are JSON.** Workload manifests, profile registries, run manifests: all JSON. The repo briefly used YAML; that switch is done. Don't reintroduce YAML.
- **Tests over prints.** Mutating-state tests (cwd, env vars) need `#[serial_test::serial]` to avoid races. Integration tests in `tests/cli.rs` use `assert_cmd` to spawn the binary.

## Common tasks for an agent

Refer to the "Common tasks for an agent" section in `CLAUDE.md` for CLI command mappings (such as adding workloads, providers, lab fixtures, and running smoke tests).

## Things to ASK before doing

- Anything that mutates `~/.darkmux/profiles.json`: that's user state.
- Anything that runs a real lab dispatch or a dispatch that loads models: uses real LMStudio resources.
- Anything that does `git push` or `git commit --amend`: irreversible-ish.
- Adding external runtime dependencies: knock-on effects on install size and license surface.

## Anti-patterns

- **Don't assume models; read the profile registry first.** Models live in `~/.darkmux/profiles.json`. If an agent role needs a model and one isn't declared, **ask the user**; do NOT pick a model from the LMStudio catalog at random.
- **Don't silently roll back on regression.** If a feature appears to regress on an unfamiliar LMStudio version, **surface the finding to the user** with the version numbers you observed. Don't quietly revert config overrides "to make things work".
- **Check existing issues before filing.** Before creating new issues, use search and comment on existing ones where possible.

## Operator sovereignty (architectural principle)

The operator is the agent of intent. The system surfaces, suggests, records, and supports, but does not substitute its judgment for the operator's at any decision point. Every default is overridable; every automatic action is auditable; every suggestion is explainable.

Compressed to one rule: **the operator never has to wonder where a decision came from.**

## Namespace convention (darkmux state in shared systems)

- LMStudio loaded identifier (visible in `lms ps`): `darkmux:<model-id>` (e.g. `darkmux:qwen3.6-35b-a3b`)

## Model-facing prompt construction (AI-convention defaults + term provenance)

Every model-facing prompt defaults to **AI-convention terminology** the model already recognizes from its training. When a darkmux-specific term is genuinely needed, **provide provenance** so the model can ground it.
* Gemini and Anthropic-trained models recognize standard XML structures (`<example>`, `<context>`, `<instructions>`) and markdown syntax cleanly.

## Engagements (operator-defined dreamscapes)

An engagement is operator-defined, never system-defined. The system doesn't impose a directory shape or config format.
* **The orchestrator's bridging role:** Read the engagement context, translate soft free-form context into structured concepts (Mission, Phase, role tilts, preferences), and don't pry for structure the operator didn't volunteer.
* **Engagement never enters CLI arg surface:** Engagement context lives in the frontier orchestrator layer (`AGENTS.md` files, skills, conversation). It never becomes a `--engagement <hint>`-style CLI arg on any `darkmux` verb.

## Project posture

**darkmux is an AI-first local-AI orchestrator.** It uses local-AI internally to manage your local-AI workflows. The CLI binary embeds dispatch logic to call into LMStudio-loaded utility agents for structuring, planning, and routine bounded reasoning tasks. The frontier-AI orchestrator (your Antigravity, Claude Code, or Cursor session) remains the strategic reasoner.

### Role families

- **Utility agents**: small model (4B-class), bounded I/O, high throughput, structured output (compactor, scribe, estimator, mission-compiler).
- **Specialist agents**: larger model (35B-class+), judgment-dependent, lower throughput, free-form output (coder, code-reviewer, analyst).

<!-- darkmux:integration:agents:start -->

# darkmux

This project uses [darkmux](https://github.com/kstrat2001/darkmux), a mission orchestrator and lab for local AI. You dispatch roles and launch missions to a crew of local-AI seats; each seat runs local (your own models, off the meter) or cloud (a hosted endpoint when a role needs frontier weights). darkmux keeps the right models resident at the right context under your RAM budget — you don't manage residency by hand.

## Available skills

- `/darkmux-status` — what's currently loaded
- `/darkmux-list-stacks` — see all available profiles
- `/darkmux-list-workloads` / `/darkmux-lab-run` — execute lab workloads
- `/darkmux-list-runs` / `/darkmux-analyze-run` / `/darkmux-compare-runs` — inspect run history

## Dispatch policy

Launch a config-defined mission with `darkmux mission launch <config>` and watch it run as a live task graph, gated on your sign-off; each run finalizes into a typed envelope. For a single turn, `darkmux dispatch <role> "<text>"` sends work to one seat. Before relying on a config, measure it with `darkmux lab run <workload>` (wall clock, compaction events, verify outcome) so your choices rest on numbers, not guesses.

<!-- darkmux:integration:agents:end -->
