# Claude / agent guidance for darkmux

The brief every session loads. Rules that apply only to one part of the code live in a `CLAUDE.md` beside that code, which Claude Code loads when it works there; cross-cutting reference lives in `docs/agents/`. The index at the end says where each topic went. If something contradicts across files, the code is the source of truth: flag the drift.

## What darkmux is

A Rust CLI for people running local LLMs, and two things at once: a **mission orchestrator** (`darkmux mission launch <config>` runs a config-defined task graph; `darkmux dispatch <role> <message>` is one role, one turn) and a **lab harness** (`darkmux lab run <workload>`, recording timing, trajectory and verify outcome). Both run through the internal Docker-bounded runtime. Model residency (loading the right models at the right context under the RAM budget) is an internal capability underneath both. **The backend is LMStudio, and only LMStudio** (a `ModelBackend` abstraction is #316, deliberately not built). darkmux uses a small local utility model for its own jobs (compaction, radio routing); the frontier session stays the strategic reasoner. Full description and file map: `docs/agents/LAYOUT.md`.

## Build and test

```bash
cargo build --release    # release binary at target/release/darkmux
cargo t-review           # test ONE area — see "Testing" below; NOT the whole suite
cargo clippy             # lint
cargo fmt                # format
cargo install --path .   # install to ~/.cargo/bin/darkmux
```

The release binary is self-contained (~11 MB as of 1.18.x — embedded workloads, roles, mission configs, and the viewer (now including the mission-graph lens, React Flow bundled in like every other `ui/` dependency, #1868) all ride inside it via `include_str!`/`include_bytes!`). `cargo install --path .` produces a binary that works from any directory without the source tree.

## Verifying a change: scale it to the risk

The full workspace suite and clippy are CI's job on this public repo (free CI). Run the area you touched, read your own diff, and let CI gate. Pick the row that matches the change:

| Change | Local | Review | Re-review |
|---|---|---|---|
| Docs, changelog, version strings, comments | nothing | none | none |
| Small fix in one area, test written first and seen failing | that area's tests, read your own diff | none; CI is the gate | none |
| Viewer only (`ui/`) | look at it first (desktop and phone), then the rules in `ui/CLAUDE.md` | only when it changes what the viewer presents as true | only if the fix pass changed logic |
| Changes behavior or a contract: a schema, a wire shape, a run's lifecycle, security, or several crates | that area's tests, plus one live run if users can see it | one frontier review, briefed to disprove the change's central claim | only if the fix pass changed logic |
| Release | the release-mode result and one live dogfood run (release skill) | none beyond the PRs' own | none |

Clippy runs in CI, not locally, except at release. Every change still starts test-first: write the test, watch it fail for the predicted reason, then fix.

Area aliases (all `cargo nextest`; install with `cargo install cargo-nextest --locked`): `t-fast` (pure-logic crates), `t-flow` (flow records, sinks, audit, schema), `t-cli` (the root binary and its integration targets), `t-review` (`-p darkmux-lab -p darkmux-crew`), `t-serve` (daemon and viewer), `t-doctor`, `t-fleet`, `t-gestalt`, `t-runtime` (`runtime/`, not a workspace member), `t-all` (CI's scope; only with a stated reason). A filter is the best first move: `cargo nextest run -p darkmux-flow integrity_exit_code`. Label a PR `full-ci` for release candidates and risky changes (test isolation, state paths, test infrastructure). `plugins/darkmux-bundler-rust` is outside the workspace and its tests run only in the PR-diff mutation job. Details, measured timings and background test lanes: `docs/agents/TESTING.md`.

## Releasing

Invoke the `darkmux-point-release` skill and follow it; do not improvise the sequence. The gate (operator mandate): **no release is cut until local darkmux runs real AI dispatches showing the release's features work**, not merely that the dispatch path runs. A green test proves the pieces; only a live run proves the thing (#1135 shipped a model loaded at 4096 context because a trivial smoke fit; #975 shipped a runtime that could not start). Full text: `docs/agents/TESTING.md`.

## Cross-system contracts (binding on every producer and consumer)

A new feature conforms to these or extends them through their own versioning; it never bypasses or fences them. Unit tests cannot catch a violation, so every review of a new subsystem asks: which contracts does this touch, and where is its conformance shown? Full text, mechanisms and conformance tests: `docs/agents/CONTRACTS.md`.

1. **Profile uniformity.** A profile means the same thing to every consumer; consumers route on what it declares and never legislate which profiles are legal.
2. **Dispatch liveness.** Every production work execution emits `dispatch.start` and a terminal `dispatch.complete`/`dispatch.error` on every exit path. darkmux's own utility jobs (compaction, radio routing; `darkmux_crew::usage::utility_job`) are exempt and run lean: `utility.start` plus their usage record.
3. **Lab/fleet sink boundary.** Lab runs write per-run-local artifacts; the fleet flow stream carries engagement work only (`is_lab_session`).
4. **Namespace.** darkmux-owned state in shared systems is namespaced (`darkmux:<model-id>` in LMStudio); darkmux loads, unloads and dispatches only to that subset and never reuses a user-loaded model. Rules: `crates/darkmux-profiles/CLAUDE.md`.
5. **Schema versioning.** Flow, rules, config and profile shapes change only through their semver rules. Flow-archive readers are lenient; user files follow contract 7.
6. **Frozen model-facing text.** Measured prompts live in one artifact with golden tests; assembly and request bodies are byte-locked.
7. **User files refuse unknown keys.** Loading never crashes on one; every entry point that consumes the file refuses at preflight, naming the file, the key path and the closest valid key; `darkmux doctor` fails it. One mechanism: `darkmux-types/src/user_files.rs`.
8. **Work-unit vocabulary.** run (mission, dispatch or lab) > mission > phase > task > step > role execution, one noun per grain on every surface. Each flow action has one wire spelling, a `FlowAction` variant in `crates/darkmux-flow/src/action.rs`, never a string literal. Every execution record carries `execution_id`; the run grain has `run.*` bookends.
9. **Enum-valued settings.** An unregistered value is refused by every entry point that could consume it, never defaulted; retired spellings are refused naming the replacement. Registry: `darkmux-types/src/config_enum.rs`.

## Doctrine in one line each

Full text: `docs/agents/DOCTRINE.md`.

- **Operator sovereignty:** the operator never has to wonder where a decision came from. Surface, suggest, record; never substitute judgment silently. Never mutate user state (`~/.darkmux/profiles.json`, `config.json`) without confirmation.
- **Harness before model:** for slow or wrong output, check compaction, context windows and loaded state (`darkmux doctor`) before blaming the model.
- **The lab loop:** baseline, one variable, re-measure, compare, record. Never skip the baseline or change two things at once.
- **No blind runs:** no measurement-grade run launches until its records stream per event, host telemetry samples alongside, the knob config is snapshotted, and a live view exists. The observer must not join the observed: no model calls on an observability path.
- **Recheck vs rethink:** never accept an agent's self-recheck for invariant- or security-bearing work; escalate to a fresh-context, higher-tier review.
- **No compliance claims:** never name a regulatory framework darkmux helps satisfy; describe the mechanism, not the outcome.
- **Engagements stay out of the CLI:** no `--engagement`/`--context` flags; engagement context lives in the frontier layer (`docs/ENGAGEMENTS.md`).
- **Anti-patterns:** don't assume models (read the profile registry, ask); don't silently roll back on regression; check existing issues before filing; empirical defaults are deliberate; name the model when characterizing local-AI behavior.

## Configuration, in short

`~/.darkmux/config.json`, resolved `env(DARKMUX_*) > config.json > default` in one place (`darkmux_types::config_access`). Propose changes as `darkmux config set <key> <value>`, not env vars. Secrets (Redis password, serve token) live in the macOS Keychain, never in config. `darkmux doctor` shows each setting's resolved value and source. Never propose or accept Tailscale Funnel. Full rules: `crates/darkmux-types/CLAUDE.md`; every variable: `docs/ENVIRONMENT.md`; fleet networking: `crates/darkmux-fleet/CLAUDE.md`.

## Conventions to follow

- **Don't add dependencies casually.** The dep set is deliberately small (`anyhow`, `clap`, `serde`, `serde_json`, `dirs`). A 10-line inline module beats a crate for small one-off needs.
- **Trait providers, not feature flags.** New workload kinds go through the `WorkloadProvider` trait in `crates/darkmux-lab/src/workloads/types.rs`, registered in `register_builtins()` in `crates/darkmux-lab/src/providers/mod.rs`. Don't bolt new behavior into the lab orchestrator.
- **Manifests are JSON.** Workload manifests, profile registries, run manifests — all JSON. The repo briefly used YAML; that switch is done. Don't reintroduce YAML.
- **Tests over prints.** Mutating-state tests (cwd, env vars) need `#[serial_test::serial]` to avoid races. Integration tests in `tests/cli.rs` use `assert_cmd` to spawn the binary.

## Things to ASK before doing

- Anything that mutates `~/.darkmux/profiles.json` — that's user state.
- Anything that runs a real lab dispatch or a dispatch that loads models — uses real LMStudio resources.
- Anything that does `git push` or `git commit --amend` — irreversible-ish.
- Adding external runtime dependencies — has knock-on effects on install size and license surface.

## Where each topic lives

| Topic (former section of this file) | Now in |
|---|---|
| What darkmux is (full); Where things live; Common tasks for an agent | `docs/agents/LAYOUT.md` |
| Grand vision; Project posture; Operator sovereignty; Anti-patterns; Loop policy; No blind runs; No compliance claims; Engagements | `docs/agents/DOCTRINE.md` |
| Testing (full); Releasing (full) | `docs/agents/TESTING.md` |
| Cross-system contracts (full) | `docs/agents/CONTRACTS.md` |
| Configuration (`config.json`); Environment variables | `crates/darkmux-types/CLAUDE.md` |
| Namespace convention | `crates/darkmux-profiles/CLAUDE.md` |
| StepKind tiering | `crates/darkmux-crew/src/step_kinds/CLAUDE.md` |
| Versioning: rules schema | `crates/darkmux-eureka/CLAUDE.md` |
| Fleet networking | `crates/darkmux-fleet/CLAUDE.md` |
| Model-facing prompt construction | `templates/builtin/CLAUDE.md` (and `runtime/CLAUDE.md`) |
| Verifying a viewer change | `ui/CLAUDE.md` |

Code comments that cite "`CLAUDE.md`'s <section>" refer to these sections.

## When in doubt

Read `README.md` for the user-facing pitch, `DESIGN.md` for the implementation reasoning, `CONTRIBUTING.md` for the dev loop. If something contradicts across files, the code is the source of truth — flag the doc drift to the user.
