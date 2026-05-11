# Claude / Agent guidance for darkmux

This file is for any AI agent (Claude Code, Cursor, OpenClaw, etc.) that's helping a user work with the darkmux source tree. Read this once before doing anything.

## What darkmux is

A pre-1.0 Rust CLI that does two things for users running local LLMs (LMStudio + Ollama + llama.cpp):

1. **Profile multiplexer** — `darkmux swap <name>` switches the loaded model + context length + (optional) compaction settings to a named profile defined in `~/.darkmux/profiles.json`.
2. **Lab harness** — `darkmux lab run <workload>` dispatches a workload against an agent runtime (default: `openclaw`) and records timing + trajectory + verify outcome under `.darkmux/runs/<run-id>/`.

The CLI is the *engine*; the empirical findings in the article series at <https://substack.com/@DarklyEnergized> are what it backs. The reproducibility story is the product story — users should be able to rerun a workload and get numbers comparable to the published claims.

## darkmux's grand vision (agent-facing)

The user-facing **"What darkmux is for"** section in `README.md` is the canonical version of the project's north-star. Below is how the same five claims translate into operational doctrine for an AI agent (Claude Code, OpenClaw, Cursor, etc.) working on darkmux or driving it on behalf of an operator.

1. **Optimization, not replacement.** When the operator asks you to pick a model from `lms ls` or propose a profile, prefer *complement* over *duplicate*. A team where every model is a 35B reasoner is not a team — it's a stack of identical instruments. Read the existing profile registry first; propose additions that fill gaps (compactor, embeddings, specialized small model) rather than swapping like for like.

2. **Harness, then model.** When the operator reports slow or wrong outputs, **check the harness before the model**. Compaction config, context-window mismatches, loaded-state drift, profile-vs-loaded model — all of these have produced 5×+ wall-clock regressions in Article 2's measurements. Default action: run `darkmux doctor`, read the eureka findings, surface those *before* suggesting the operator change models.

3. **The lab + the loop.** darkmux is not just an inspection tool — it's the loop. When you have a tuning hypothesis (e.g., *"primary at 64K instead of 100K might fit this 32GB tier"*), the correct action sequence is: **baseline → single-variable change → re-measure → compare → record in notebook**. Each step has a darkmux primitive. Do NOT skip the baseline. Do NOT change two variables at once. The discipline is the point — without it, the comparison is uninterpretable.

4. **Team integrity is your responsibility.** When proposing config changes, frame them in terms of *how this affects the team's shape*, not just an isolated metric. *"Drop the compactor to free RAM"* reduces working memory; consider whether the remaining team can still handle long-agentic dispatches before recommending. The operator is depending on you to maintain team coherence as new models arrive and hardware changes — that's the principal-engineer posture the maintainer named in [#35](https://github.com/kstrat2001/darkmux/issues/35).

5. **The success criterion is recursive.** A fresh agent session, given only a clean-slate darkmux install + these docs + the bundled skills, should reach the same conclusion about *"what is darkmux for?"* as the maintainer's comment on #35. If you find yourself uncertain or having to infer from primitives, **the docs have drifted from the vision** — surface that to the operator. Doc drift is a bug, not a footnote.

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

## Where things live

```
src/
  main.rs                    CLI dispatch (clap)
  types.rs                   Profile / ProfileRegistry / ProfileModel
  profiles.rs                Registry loader + lookup
  swap.rs / lms.rs           Stack swap orchestration + lms CLI wrapper
  runtime.rs                 Runtime config patcher (e.g. openclaw.json)
  init.rs / skills.rs        `darkmux init` + skill installer
  notebook.rs                Notebook draft generator
  lab/
    paths.rs                 Workspace dir resolution (project vs user)
    run.rs                   Workload dispatch
    inspect.rs               Single-run analysis
    compare.rs               Run-vs-run diff
    list.rs                  Recent runs table
  workloads/
    types.rs                 WorkloadProvider trait + manifest types
    load.rs                  Manifest loading (user → on-disk → embedded)
    registry.rs              Provider registry
  providers/
    prompt.rs                Trivial single-prompt provider
    coding_task.rs           Sandbox + verify-command provider
templates/builtin/workloads/  Workload manifests embedded at compile time
skills/darkmux-<name>/        Agent-invokable skill wrappers
tests/cli.rs                  Integration tests (spawn the binary)
```

## Conventions to follow

- **Don't add dependencies casually.** The dep set is deliberately small (`anyhow`, `clap`, `serde`, `serde_json`, `dirs`). A 10-line inline module beats a crate for small one-off needs (see `mod pathdiff` in `src/providers/coding_task.rs`).
- **Trait providers, not feature flags.** New workload kinds go through the `WorkloadProvider` trait in `src/workloads/types.rs`, registered in `src/workloads/registry.rs::register_builtins()`. Don't bolt new behavior into the lab orchestrator.
- **Manifests are JSON.** Workload manifests, profile registries, run manifests — all JSON. The repo briefly used YAML; that switch is done. Don't reintroduce YAML.
- **Tests over prints.** Mutating-state tests (cwd, env vars) need `#[serial_test::serial]` to avoid races. Integration tests in `tests/cli.rs` use `assert_cmd` to spawn the binary.

## Versioning — rules schema

The `eureka` rules engine emits its definitions into `instruments.jsonl` so the viewer can render findings without duplicating rule data in JS. The viewer enforces compatibility on file drop. The contract is plain semver, applied to the rules **data shape** (not to darkmux itself):

| Bump | Meaning | UI behavior |
|---|---|---|
| **Patch** (`1.0.0` → `1.0.1`) | Fully backward-compatible. Bug fix in a message, threshold tweak that doesn't change semantics, typo in a `fix_hint`. | Viewer parser ignores; works unchanged. |
| **Minor** (`1.0` → `1.1`) | Additive. New rule `kind`, new optional field on `RuleDef`. Older viewers can't *evaluate* new rules but can SAFELY IGNORE them. | Viewer soft-warns; renders normally. New rules surface as "checked at pre-flight only" until the viewer gets a JS evaluator. |
| **Major** (`1.x` → `2.0`) | Breaking. Rename/retype a field, change of `RuleKind` enum encoding, new required field. Older viewers cannot trust newer-major data. | Viewer **blocks** the Anomalies panel + shows an upgrade modal with the exact `cargo install --path . --force` command. User must update CLI or downgrade viewer. |

Rule of thumb when changing the schema:

- Adding a new rule? **Minor bump.**
- Renaming or retyping a field on `RuleDef`? **Major bump.**
- Fixing a typo in `fix_hint`? **Patch bump.**

`RULES_SCHEMA_VERSION` lives in `src/eureka.rs` as a single constant. The viewer's `EXPECTED_RULES_SCHEMA_MAJOR` lives in `docs/viewer/index.html` near the top of the script block. **When you bump major, you bump both in the same PR** — the viewer-release-PR is the contract.

## Common tasks for an agent

If a user asks you to:

| Ask | Do |
|---|---|
| "add a new workload" | Drop a JSON manifest at `templates/builtin/workloads/<id>.json`. If it's a `prompt` workload, register it in `EMBEDDED_WORKLOADS` in `src/workloads/load.rs`. coding-task workloads need a sandbox seed dir and CAN'T be embedded. |
| "add a new provider" | Implement `WorkloadProvider` in `src/providers/<name>.rs`, register it in `src/workloads/registry.rs::register_builtins()`. |
| "run the smoke test" | `cargo install --path . && darkmux lab run quick-q`. Should complete in ~6-10s if a model is loaded. |
| "make the build self-contained" | Already is — `include_str!` for embedded workloads, no external assets needed at runtime. |
| "review the diff before commit" | Run the suite (`cargo test`), eyeball `git diff`, propose a commit message — but **do not commit unless explicitly asked**. |

## Things to ASK before doing

- Anything that mutates `~/.darkmux/profiles.json` — that's user state.
- Anything that calls `darkmux swap` or runs a real lab dispatch — uses real LMStudio resources.
- Anything that does `git push` or `git commit --amend` — irreversible-ish.
- Adding external runtime dependencies — has knock-on effects on install size and license surface.

## Anti-patterns — failure modes observed on fresh sessions

Live findings from cross-machine testing (M1 Max Studio fresh-Claude session, 2026-05-11). Future fresh-Claude sessions: read these BEFORE acting on a user request that touches model selection, version compatibility, or issue tracking.

- **Don't assume models — read the profile registry first.** Models live in `~/.darkmux/profiles.json` (or wherever `darkmux profiles` reports). If an agent role needs a model and one isn't declared, **ask the user**; do NOT pick a model from the LMStudio catalog at random. Older gemma family in particular ("gemma-4-e4b", "Gemma 4 26b", etc.) is known to produce looping garbage that poisons sessions across model swaps — never default-assume gemma. If the user has saved a memory about model selection, that supersedes any inference you'd make from the catalog.

- **Don't silently roll back on regression.** If a feature appears to regress on an unfamiliar OpenClaw / LMStudio version (e.g., `systemPromptOverride` doesn't behave as expected, compaction settings don't take effect), **surface the finding to the user** with the version numbers you observed. Don't quietly revert config overrides "to make things work" — loud beats quiet. The user is debugging an unfamiliar env and needs the signal; a silent rollback hides the real bug.

- **Check existing issues before filing.** Before `gh issue create`, run `gh issue list --search "<keywords>"` (include closed issues with `--state all`) and skim. Duplicates clutter the project board and dilute the eureka-detection roadmap. Default to **commenting on an existing issue** over filing a new one. If you're not sure whether something is a dupe, **ask the user**; don't file-and-hope.

- **Cross-machine version awareness.** darkmux assumes a recent OpenClaw. Before applying any agent config (especially `systemPromptOverride`, compaction settings, or sampler tweaks), check `openclaw --version` and consider whether the feature you're about to use exists on the user's installed version. If you can't verify, ask. The currently-documented minimum is captured in `doctor` (when implemented — see open issue) and in the README's Prerequisites.

- **The empirical findings in Article 2 are load-bearing, not decorative.** When choosing compaction modes, context windows, or compactor pairings, the article's data (`default` mode beats `safeguard` for local; small dedicated compactor at ~68K cuts wall-clock in half) reflects validated configurations, not arbitrary defaults. Don't deviate from a profile's settings without acknowledging the empirical reason — the operator has chosen them deliberately.

## Project posture

The **CLI binary** is infrastructure — not an inference engine, not an agent framework, not a cloud-provider router. The README is intentionally honest about what the binary does NOT do. Match that posture in any code, CLI copy, or `--help` text you write — don't oversell what `darkmux` (the executable) does.

The **agent-facing surface** is where the *guided* part of "guided optimization" lives: this `CLAUDE.md`, the bundled skills under `skills/darkmux-*`, and the doctrine in **darkmux's grand vision** above. An agent working with darkmux doesn't replace the operator; it drives the loop the operator would otherwise drive by hand. Both surfaces are part of the same project — the dual posture is deliberate, not a contradiction.

The default runtime is OpenClaw (`DARKMUX_RUNTIME_CMD=openclaw`) but the lab harness is runtime-pluggable via env var. Users running Aider, Cline, or anything with a `<cmd> agent --message` interface can point darkmux at it.

## When in doubt

Read `README.md` for the user-facing pitch, `DESIGN.md` for the implementation reasoning, `CONTRIBUTING.md` for the dev loop. If something contradicts across files, the code is the source of truth — flag the doc drift to the user.
