# Antigravity / Agent guidance for darkmux

This file is for any AI agent (Antigravity, Claude Code, Cursor, etc.) that's helping a user work with the darkmux source tree. Read this once before doing anything.

## What darkmux is

A pre-1.0 Rust CLI that does two things for users running local LLMs (LMStudio + Ollama + llama.cpp):

1. **Profile multiplexer** — `darkmux swap <name>` switches the loaded model + context length + (optional) compaction settings to a named profile defined in `~/.darkmux/profiles.json`.
2. **Lab harness** — `darkmux lab run <workload>` dispatches a workload against an agent runtime (default: the internal Docker-bounded runtime; pass `--runtime openclaw` to opt into the openclaw shell-out path) and records timing + trajectory + verify outcome under `.darkmux/runs/<run-id>/`.

The CLI is the *engine*; the empirical findings in the Genesis series on Darkly Energized (<https://darklyenergized.substack.com>) are what it backs. The reproducibility story is the product story — users should be able to rerun a workload and get numbers comparable to the published claims.

## darkmux's grand vision (agent-facing)

The user-facing **"What darkmux is for"** section in `README.md` is the canonical version of the project's north-star. Below is how the same five claims translate into operational doctrine for an AI agent (Antigravity, Claude Code, etc.) working on darkmux or driving it on behalf of an operator.

1. **Optimization, not replacement.** When the operator asks you to pick a model from `lms ls` or propose a profile, prefer *complement* over *duplicate*. A team where every model is a 35B reasoner is not a team — it's a stack of identical instruments. The same logic applies *within* each role family (see **Project posture → Role families** below): a profile with three different 35B specialists and no 4B utility agent is missing its compactor, scribe, and estimator; conversely, a profile of nothing but utility agents has no specialist to do the actual judgment-dependent work. Read the existing profile registry first; propose additions that fill gaps in the right family (utility: compactor / scribe / estimator / mission-compiler; specialist: coder / reviewer / analyst) rather than swapping like for like.

2. **Harness, then model.** When the operator reports slow or wrong outputs, **check the harness before the model**. Compaction config, context-window mismatches, loaded-state drift, profile-vs-loaded model — all of these can produce large wall-clock regressions that look like model problems but are actually harness problems. Default action: run `darkmux doctor`, read the eureka findings, surface those *before* suggesting the operator change models.

3. **The lab + the loop.** darkmux is not just an inspection tool — it's the loop. When you have a tuning hypothesis (e.g., *"primary at 64K instead of 100K might fit this 32GB tier"*), the correct action sequence is: **baseline → single-variable change → re-measure → compare → record in notebook**. Each step has a darkmux primitive. Do NOT skip the baseline. Do NOT change two variables at once. The discipline is the point — without it, the comparison is uninterpretable.

4. **Team integrity is your responsibility.** When proposing config changes, frame them in terms of *how this affects the team's shape*, not just an isolated metric. *"Drop the compactor to free RAM"* reduces working memory; consider whether the remaining team can still handle long-agentic dispatches before recommending. The operator is depending on you to maintain team coherence as new models arrive and hardware changes.

5. **The success criterion is recursive.** A fresh agent session, given only a clean-slate darkmux install + these docs + the bundled skills, should reach the same conclusion about *"what is darkmux for?"* as the rest of these doctrine entries name. If you find yourself uncertain or having to infer from primitives, **the docs have drifted from the vision** — surface that to the operator. Doc drift is a bug, not a footnote.

These claims compose with the existing **Anti-patterns** section below: anti-patterns are *what not to do*; the vision is *what to do instead*. If a request would violate both at once (e.g., *"silently roll back the compactor without telling me"*), the vision wins — surface the conflict and let the operator decide.

## Build and test

```bash
cargo build --release    # release binary at target/release/darkmux
cargo test               # unit + integration suite
cargo clippy             # lint
cargo fmt                # format
cargo install --path .   # install to ~/.cargo/bin/darkmux
```

The release binary is self-contained (~1.1 MB). Built-in workloads under `templates/builtin/workloads/*.json` are embedded at compile time via `include_str!` — `cargo install --path .` produces a binary that works from any directory without the source tree.

## Configuration (`config.json`)

darkmux's canonical config surface is **`~/.darkmux/config.json`**, written by `darkmux init`. Every setting resolves with one precedence — **`env(DARKMUX_*) > config.json > built-in default`** — and that precedence lives in exactly ONE place: `darkmux_types::config_access` (the env tier is read **live per-access**, so a `set_var` in a test or a power-user export still wins). A reader never has to wonder where a setting came from; `darkmux doctor` surfaces the resolved value + source.

**The file is self-documenting by design.** `init` writes the common knobs *visible* (not hidden as code-defaults), so the operator tunes the file, not the source. Off-by-default integrations are **feature blocks gated by an `enabled` field, not by field-presence** — `init` writes the whole block with `enabled: false` and the sub-defaults populated, so the surface is discoverable and one flip from on:

```json
{
  "schema_version": "1.0",
  "machine_id": "studio",
  "orchestrator": "",
  "lms_bin": "lms",
  "lmstudio_url": "http://localhost:1234",
  "redis":   { "enabled": false, "host": "127.0.0.1", "port": 6379, "stream": "darkmux:flow", "maxlen": 10000 },
  "audit":   { "enabled": false, "dir": "~/.darkmux/audit" },
  "runtime": { "inactivity_timeout_seconds": 600, "strict_selection": false, "feedback_injection": true, "check_updates": true }
}
```

When proposing a config change to an operator, write the visible field; don't reach for an env var as the primary mechanism. **Deliberately NOT written by `init`** (because a literal would be wrong, not because they're hidden): `dirs.*` (derived from the root — `darkmux doctor` shows the resolved path) and caps like `runtime.max_turns` (absent = uncapped, a real behavior).

**Carve-outs — the ONLY things NOT plaintext config:**
- **Redis password → macOS Keychain** (item `darkmux-redis`, the same item the Homebrew wrapper populates). `config.redis` holds only non-secret bits (`enabled`/`host`/`port`/`db`/`stream`/`maxlen`); the password is read at runtime via `security find-generic-password` and never logged — every URL is wrapped in `RawRedisUrl` (redacted `Display` + `Debug`; raw bytes only via `expose_for_probe`). Non-macOS uses the full-URL env override. `redis_url()` resolves `env(DARKMUX_REDIS_URL) verbatim > config.redis.enabled + Keychain > off`.
- **`DARKMUX_HOME`** — the bootstrap pointer that *locates* the config root (`<root>/config.json`); it can't live inside the config it finds, so it stays an env var.

**Schema is minor-bump + lenient on read** (all-`Option` + `#[serde(flatten)] extras` overflow): an older binary tolerates a newer config, and a partial/hand-edited/malformed config never bricks the CLI — loud validation belongs to `darkmux doctor`, not the hot load path. `CONFIG_SCHEMA_VERSION` lives in `darkmux-types/src/config.rs`.

**Don't confuse `config.json` with the profiles registry.** `~/.darkmux/profiles.json` (the swap profiles) is a SEPARATE file, overridden by `--profiles-file` / `DARKMUX_PROFILES` — **renamed in #661 from the misleading `--config` / `DARKMUX_CONFIG`** (those names are retired, not reused, because a real `config.json` now exists).

## Environment variables

Every `DARKMUX_*` var below is the **top tier** of `env > config.json > built-in default` — it wins live, and each maps to a `config.json` field (mapping after the table). Use env for per-shell/CI/test overrides; use `config.json` for durable operator config. Flow records carry per-record provenance fields auto-populated from these at write time. `darkmux doctor` surfaces what each resolves to.

| Variable | Default | Effect |
|---|---|---|
| `DARKMUX_MACHINE_ID` | hostname | Logical fleet name **stamped at record-write time** on every new flow record. Operator-named (`studio`, `mini-1`) reads better in the topology view than DNS-style hostnames. Pre-1.4.0 records lack the field (which the viewer renders as `unknown`). |
| `DARKMUX_ORCHESTRATOR` | unset → field omitted | Frontier orchestrator driving this session (e.g. `claude-code`, `antigravity`, `cursor`), **stamped at record-write time**. **Operator-explicit by design** — there's no reliable way to auto-detect the frontier model from inside darkmux. Doctor warns when unset. |
| `DARKMUX_FLOWS_DIR` | `~/.darkmux/flows` | Where the per-day JSONL files live (LocalFileSink — casual write target). |
| `DARKMUX_AUDIT_DIR` | unset → AuditFileSink off | When set, flow records ALSO write to a hash-chained tamper-evident per-day JSONL under this directory (AuditFileSink, #163). **POSIX-only** (Linux/macOS — Windows is unsupported; the env var is recognized but the sink is skipped). Cross-process safe via `flock(2)`. `darkmux flow integrity-check` walks the chain and **exits with status 2 on any chain break** so cron/CI can flag tampering. `darkmux doctor` rolls up the same result. Compliance-strength substrate (ISO 27001, AI Act, HIPAA-as-covered-entity). |
| `DARKMUX_REDIS_URL` | unset → Redis off | When set, flow records also XADD to the Redis stream (coordination substrate; not the audit substrate). Combined with `DARKMUX_AUDIT_DIR` produces the canonical compliant composition: `TeeSink([LocalFile, Audit, Redis])`. See [#162](https://github.com/kstrat2001/darkmux/issues/162) Phase 3. |
| `DARKMUX_REDIS_STREAM` | `darkmux:flow` | Override the Redis stream name. |
| `DARKMUX_REDIS_MAXLEN` | `10000` | Approximate retention cap for the Redis stream (`XADD MAXLEN ~ N`); `0` for unbounded. |
| `DARKMUX_INACTIVITY_TIMEOUT_SECONDS` | `600` | Per-dispatch inactivity budget. The host-side watchdog **hard-kills** the container at 100% if no proof-of-work signal lands. The runtime-side detector fires a **soft warning** at 75% (model-facing nudge to wrap up gracefully or escalate via `BLOCKED:`); both reset on the same proof-of-work signals (any tool.completed, any compaction). A productive dispatch never sees either; a stuck one gets the soft chance before the hard kill. Pathological tool patterns are caught by their dedicated detectors (cycle, cascade, edit-drift, reasoning loops) — the deadline trusts activity; the detectors catch struggle. (#457 + #464 + #466; renamed from `DARKMUX_RUNTIME_DEADLINE_SECONDS`) |

**env → `config.json` field** (the override-tier var → its durable config home):

| Env var | `config.json` field |
|---|---|
| `DARKMUX_MACHINE_ID` | `machine_id` |
| `DARKMUX_ORCHESTRATOR` | `orchestrator` |
| `DARKMUX_LMS_BIN` / `DARKMUX_LMSTUDIO_URL` | `lms_bin` / `lmstudio_url` (base URL; callers append `/v1/...`) |
| `DARKMUX_FLOWS_DIR` / `DARKMUX_NOTEBOOK_DIR` / `DARKMUX_CREW_DIR` / … | `dirs.flows` / `dirs.notebook` / `dirs.crew` / … |
| `DARKMUX_AUDIT_DIR` | `audit.dir` (gated by `audit.enabled`) |
| `DARKMUX_REDIS_URL` (verbatim, password inline) | `redis.{enabled,host,port,db}` + Keychain password (assembled) |
| `DARKMUX_REDIS_STREAM` / `DARKMUX_REDIS_MAXLEN` | `redis.stream` / `redis.maxlen` |
| `DARKMUX_INACTIVITY_TIMEOUT_SECONDS` | `runtime.inactivity_timeout_seconds` |
| `DARKMUX_RUNTIME_MAX_TURNS` / `DARKMUX_RUNTIME_MAX_TOKENS` | `runtime.max_turns` / `runtime.max_tokens` |
| `DARKMUX_STRICT_SELECTION` / `DARKMUX_CHECK_UPDATES` | `runtime.strict_selection` / `runtime.check_updates` |
| `DARKMUX_FEEDBACK_INJECTION` | `runtime.feedback_injection` exists, but is read **directly in the runtime container** (`runtime/src/feedback.rs`), NOT through `config_access` — so it does NOT yet honor the `config.json` tier (the runtime crate can't depend on `config_access`; wiring it needs a flag-plumb, deliberately deferred in #661). |
| `DARKMUX_DEFAULT_ROLE` / `DARKMUX_DAEMON_CORS_ORIGINS` | `runtime.default_role` / `runtime.daemon_cors_origins` |
| `DARKMUX_HOME` (bootstrap pointer) | — (locates the config root; can't live in config) |
| `DARKMUX_PROFILES` (profiles registry, **renamed from `DARKMUX_CONFIG`**) | — (a separate file, not `config.json`) |

When working on darkmux from an Antigravity (or other frontier) session, export `DARKMUX_ORCHESTRATOR=<harness-name>` in the shell so flow records carry orchestrator provenance.

## Where things live

Refer to the "Where things live" section in `CLAUDE.md` for the directory and file maps, as they remain the authoritative layout map for the workspace.

## Conventions to follow

- **Don't add dependencies casually.** The dep set is deliberately small (`anyhow`, `clap`, `serde`, `serde_json`, `dirs`). A 10-line inline module beats a crate for small one-off needs.
- **Trait providers, not feature flags.** New workload kinds go through the `WorkloadProvider` trait in `src/workloads/types.rs`, registered in `src/workloads/registry.rs::register_builtins()`. Don't bolt new behavior into the lab orchestrator.
- **Manifests are JSON.** Workload manifests, profile registries, run manifests — all JSON. The repo briefly used YAML; that switch is done. Don't reintroduce YAML.
- **Tests over prints.** Mutating-state tests (cwd, env vars) need `#[serial_test::serial]` to avoid races. Integration tests in `tests/cli.rs` use `assert_cmd` to spawn the binary.

## Common tasks for an agent

Refer to the "Common tasks for an agent" section in `CLAUDE.md` for CLI command mappings (such as adding workloads, providers, lab fixtures, and running smoke tests).

## Things to ASK before doing

- Anything that mutates `~/.darkmux/profiles.json` — that's user state.
- Anything that calls `darkmux swap` or runs a real lab dispatch — uses real LMStudio resources.
- Anything that does `git push` or `git commit --amend` — irreversible-ish.
- Adding external runtime dependencies — has knock-on effects on install size and license surface.

## Anti-patterns

- **Don't assume models — read the profile registry first.** Models live in `~/.darkmux/profiles.json`. If an agent role needs a model and one isn't declared, **ask the user**; do NOT pick a model from the LMStudio catalog at random.
- **Don't silently roll back on regression.** If a feature appears to regress on an unfamiliar OpenClaw / LMStudio version, **surface the finding to the user** with the version numbers you observed. Don't quietly revert config overrides "to make things work".
- **Check existing issues before filing.** Before creating new issues, use search and comment on existing ones where possible.
- **Cross-machine version awareness.** darkmux assumes a recent OpenClaw. Check versions and prerequisites before applying new agent configs.

## Operator sovereignty (architectural principle)

The operator is the agent of intent. The system surfaces, suggests, records, and supports — but does not substitute its judgment for the operator's at any decision point. Every default is overridable; every automatic action is auditable; every suggestion is explainable.

Compressed to one rule: **the operator never has to wonder where a decision came from.**

## Namespace convention (darkmux state in shared systems)

- LMStudio loaded identifier (visible in `lms ps`): `darkmux:<model-id>` (e.g. `darkmux:qwen3.6-35b-a3b`)
- OpenClaw agent ids (`agents.list[].id`): `darkmux/<role>` (e.g. `darkmux/coder`)
- OpenClaw channel routing: `darkmux/<key>` (e.g. `darkmux/<channel-id>`)

## Model-facing prompt construction (AI-convention defaults + term provenance)

Every model-facing prompt defaults to **AI-convention terminology** the model already recognizes from its training. When a darkmux-specific term is genuinely needed, **provide provenance** so the model can ground it.
* Gemini and Anthropic-trained models recognize standard XML structures (`<example>`, `<context>`, `<instructions>`) and markdown syntax cleanly.

## Engagements (operator-defined dreamscapes)

An engagement is operator-defined, never system-defined. The system doesn't impose a directory shape or config format.
* **The orchestrator's bridging role:** Read the engagement context, translate soft free-form context into structured concepts (Mission, Sprint, role tilts, preferences), and don't pry for structure the operator didn't volunteer.
* **Engagement never enters CLI arg surface:** Engagement context lives in the frontier orchestrator layer (`AGENTS.md` files, skills, conversation). It never becomes a `--engagement <hint>`-style CLI arg on any `darkmux` verb.

## Project posture

**darkmux is an AI-first local-AI orchestrator.** It uses local-AI internally to manage your local-AI workflows. The CLI binary embeds dispatch logic to call into LMStudio-loaded utility agents for structuring, planning, and routine bounded reasoning tasks. The frontier-AI orchestrator (your Antigravity, Claude Code, or Cursor session) remains the strategic reasoner.

### Role families

- **Utility agents** — small model (4B-class), bounded I/O, high throughput, structured output (compactor, scribe, estimator, mission-compiler).
- **Specialist agents** — larger model (35B-class+), judgment-dependent, lower throughput, free-form output (coder, code-reviewer, analyst).

<!-- darkmux:integration:agents:start -->

# darkmux

This project uses [darkmux](https://github.com/kstrat2001/darkmux) to multiplex local LLM stacks. Three reference profiles are available: `fast`, `balanced`, and `deep`.

## When to swap stacks

- **`fast`** — single-turn tasks (audits, TODO fills, short Q&A). Slim primary, no compactor.
- **`balanced`** — mid-range tasks. Tuned compaction with a small companion compactor.
- **`deep`** — long agentic tasks (multi-file refactors, exploratory test authoring). Maximum primary context for fewer compactions.

## Available skills

- `/darkmux-status` — what's currently loaded
- `/darkmux-list-stacks` — see all available profiles
- `/darkmux-swap-stack <name>` — switch to a profile
- `/darkmux-list-workloads` / `/darkmux-lab-run` — execute lab workloads
- `/darkmux-list-runs` / `/darkmux-analyze-run` / `/darkmux-compare-runs` — inspect run history

## Dispatch policy

Before starting a long agentic task that may grow context past ~30K tokens, consider swapping to `deep`. Before doing a single-turn audit or short review, consider swapping to `fast` to skip the compactor's idle KV-cache cost. Use `/darkmux-status` to confirm before making the change — swapping is idempotent so a status-matched call is a no-op.

<!-- darkmux:integration:agents:end -->
