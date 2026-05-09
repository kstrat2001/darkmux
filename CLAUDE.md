# Claude / Agent guidance for darkmux

This file is for any AI agent (Claude Code, Cursor, OpenClaw, etc.) that's helping a user work with the darkmux source tree. Read this once before doing anything.

## What darkmux is

A pre-1.0 Rust CLI that does two things for users running local LLMs (LMStudio + Ollama + llama.cpp):

1. **Profile multiplexer** — `darkmux swap <name>` switches the loaded model + context length + (optional) compaction settings to a named profile defined in `~/.darkmux/profiles.json`.
2. **Lab harness** — `darkmux lab run <workload>` dispatches a workload against an agent runtime (default: `openclaw`) and records timing + trajectory + verify outcome under `.darkmux/runs/<run-id>/`.

The CLI is the *engine*; the empirical findings in the article series at <https://substack.com/@DarklyEnergized> are what it backs. The reproducibility story is the product story — users should be able to rerun a workload and get numbers comparable to the published claims.

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

## Project posture

darkmux is positioned as **infrastructure**, not as an "agent." The README is intentionally honest about what darkmux does NOT do (not an inference engine, not an agent framework, not a cloud-provider router). Match that posture in any docs or copy you write — don't oversell.

The default runtime is OpenClaw (`DARKMUX_RUNTIME_CMD=openclaw`) but the lab harness is runtime-pluggable via env var. Users running Aider, Cline, or anything with a `<cmd> agent --message` interface can point darkmux at it.

## When in doubt

Read `README.md` for the user-facing pitch, `DESIGN.md` for the implementation reasoning, `CONTRIBUTING.md` for the dev loop. If something contradicts across files, the code is the source of truth — flag the doc drift to the user.
