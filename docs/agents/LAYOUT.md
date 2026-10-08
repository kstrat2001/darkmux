# What darkmux is, where things live, common tasks

Agent reference, read when the work touches it. Moved verbatim from the root `CLAUDE.md` (#3133), which keeps a one-line summary and a pointer here.

## What darkmux is

A Rust CLI that is two things for users running local LLMs:

1. **Mission orchestrator**: config-defined missions launched with `darkmux mission launch <config>` that run as a live task graph. A crew of local-AI roles works the phases through the internal Docker-bounded runtime (any seat can instead be staffed by a hosted cloud endpoint), every dispatch gated on operator sign-off, each run finalizing into a typed envelope. `darkmux dispatch <role> <message>` is the task-grain entry point (one role, one turn). This is the 2.0 headline.
2. **Lab harness**: `darkmux lab run <workload>` dispatches a workload against the same internal runtime and records timing + trajectory + verify outcome under `.darkmux/lab/<run-id>/`.

**Backend, stated honestly (#316):** darkmux drives **LMStudio** today, and only LMStudio. The residency arbiter, the `darkmux:` namespace convention, the empirical profile defaults, and every `lms`-shell-out in `darkmux-gestalt` are LMStudio-shaped. An earlier version of this line claimed "LMStudio + Ollama + llama.cpp"; that was aspiration, not capability, and a fresh agent session reading it would confidently propose work against backends that don't exist. A `ModelBackend` abstraction is tracked as #316 and is deliberately NOT being built — revisit when a real second backend has a real user. Until then: don't deepen LMStudio coupling gratuitously in new code, but don't pretend the abstraction is there either.

Managing model residency (the founding *profile multiplexer*: loading the right models at the right context under the RAM budget) is now an internal capability underneath both, not a verb the operator drives. The `swap` verb retired on the 2.0 track (#1426); gestalt loads what each dispatch's staffing declares.

The CLI is the *engine*; the empirical findings in the Genesis series on Darkly Energized (<https://darklyenergized.substack.com>) are what it backs. The reproducibility story is the product story: users should be able to rerun a workload and get numbers comparable to the published claims.

## Where things live

The workspace is a thin `src/` command layer over a set of `crates/` library members (the monolithic `src/` of the 0.x era split out; `swap.rs`, `src/crew/`, `src/lab/`, `src/workloads/`, `src/providers/` no longer exist at that path). The internal runtime lives in a separate `runtime/` crate that is NOT a workspace member (it needs its own `cargo clippy --manifest-path runtime/Cargo.toml`).

```
src/                          CLI command layer (clap)
  main.rs                     Entry point
  cli.rs                      The clap Command enum (the top-level verb surface)
  (dispatch is a top-level verb; the per-command modules:)
  mission_launch.rs           `mission launch <config>`: mint + drive a mission instance from a config (the `review` config's dedicated launcher — bundle→probe→dedup→judge→verify→synthesis — was deleted in #2310 P4d; `review` now runs through this generic launcher, same as any other config)
  acp_panel.rs                The ACP panel's ONE generic `/mission list|launch|show` command and the ephemeral runner for model-free configs; `prepare_launch` derives diff/head_sha/workspace params for a no-argument `/mission launch review` from the cwd's own git state
  mission_show.rs             `mission show <id>` (and the panel's `/mission show`): one mission in full, from one derivation
  mission_config_cli.rs       `mission config list|show`: the launchable configs and their declared inputs
  radio.rs / radio_cli.rs / radio_busy.rs  `darkmux radio`: free text routed onto one launchable config on the utility model, confirmed before it runs; the busy check answers from facts
  cli_json.rs                 `--json` output contract: one named type per verb (`cli_outputs!`), pinned by `tests/cli-json.golden`
  coder_phase.rs              coder-phase pipeline StepKinds (worktree/coder/verify): Tier-3 bespoke, launch-owned (`mission run` retired #1426 ship-4)
  mission_status.rs           `mission status`: the read-only mission board
  run_list.rs                 `run list`: the cross-kind union (mission, dispatch, lab), the CLI twin of `GET /runs`
  run_records.rs              `run inspect|stats|compare`: read a lab run's recorded artifacts; refuses mission and dispatch runs, naming where to look
  lab_cli.rs                  `lab` family — kind-family shape (#1465): `run <workload>` (the launcher only; recorded runs are read through `darkmux run`) · `workload list` · `fixture {list·register·unregister}` · `loop`/`characterize`/`tune`/`doctor`
  phase_review.rs             Code-review output rendering (`phase_review_output_at`) for the coder-phase QA gate; the `phase` verb family retired (#1463)
  mod_cli.rs                  `mod` family (create/list/show over the write-once mod store)
  role_cli.rs                 `role` family (list/show from the SQLite index)
  finding_cli.rs              `finding` family (#2265): `list`/`show` read the write-once finding store; `sync` replays the flow stream into it (the store's second producer, after the dispatch tailer)
  fleet_cli.rs                `machine list/add/remove` roster (the retired `fleet` family folded into `machine`, #1426)
  flow_cli.rs                 `flow` family (note, status, integrity-check, tail)
  config_cmd.rs               `config` get/set/list
  init.rs / skills.rs         `darkmux init` (idempotent setup + bundled-skill refresh) + skill installer
  conventions.rs              Shared CLI helpers
crates/
  darkmux-types/              Profile / ProfileRegistry / config / flow record schemas + config_access
  darkmux-trajectory/         Trajectory events (the one definition the runtime writes) + the one fold every host reader counts turns, tokens and rests from; a leaf crate the runtime also depends on
  darkmux-profiles/           Registry loader + lookup
  darkmux-gestalt/            Residency arbiter (ResourceProbe/pools; loads what each dispatch's staffing declares)
  darkmux-crew/               Roles, dispatch core, the Task/Step scheduler + step_kinds/ (builtins/patterns), lessons
    src/rules.rs                 The general rule-file template kind (#1959) — promoted out of the crawl module; `resolve_default` reads `templates/builtin/rules/*.json` + a user-tier override dir
    src/workspace_spec/          The generic "named sources + include/exclude + edges" mission input (#1959) — promoted out of the crawl module's retired `CorpusManifest`; `mod.rs` (WorkspaceSpec/SourceSpec/EdgeSpec, load/validate), `glob.rs` (the one filter-language matcher), `materialize.rs` (git resolution + file walk, producing a `Materialized` any mission can plan from)
    src/step_kinds/mods_gate.rs           `mods.gate` (#2310 P4c-2b) — the create-mods confirmation gate: apply a mod's kit onto a scratch copy of the source checkout, run the declared `test_command` against that patched copy, record the outcome onto every mod naming the finding
    src/step_kinds/records_gather.rs      `records.gather` (#2310 P4c-2b) — gathers a mission's finding + mod records, plus a diff and scope summary, into the typed shape `deliver.github_review` reads; mission-agnostic, not review-specific
    src/step_kinds/deliver_github_review.rs  `deliver.github_review` (#2310 P4b) — findings + mods + a diff → a GitHub review payload (`{event, body, comments}`); pure render, no model
  darkmux-lab/                Lab harness (lab/, providers/, workloads/) + the review envelope (lab/review.rs — data types + outcome mapping only; the executable pipeline these types used to describe was deleted in #2310 P4d, see darkmux-crew's step_kinds above and crawl/ below for what runs `review` now)
    src/crawl/                    The agentic bug crawler's mechanical planning half (#1959) — `plan.rs` (Materialized + [Rule] -> a token-estimated work-unit Plan; `manifest.rs`/`sources.rs` retired, superseded by `darkmux-crew`'s `workspace_spec`)
    src/crawl/plan_sites_step.rs  `plan.sites` (#2310 P4c) — the generic diff/tree plan step; the `review` config's `plan-<rule>` tasks use this with `source: "diff"` to plan over a diff's hunks rather than a whole-tree walk
    src/crawl/unit_step.rs        `dispatch.unit` + `dispatch.summary` (#2301) — the crawl's dispatch half as step kinds; `review`'s `unit-<rule>` tasks grow one `dispatch.unit` dispatch per planned site with `role_id: "reviewer"`
  darkmux-fleet/              Roster + cross-machine routing
  darkmux-flow/               Flow sinks (LocalFile/Audit/Redis/Tee) + Keychain-secret machinery
  darkmux-serve/              HTTP daemon + the bundled viewer (assets/next.html, built from ui/src)
  darkmux-doctor/             `darkmux doctor` checks
  darkmux-eureka/             Rules engine (RULES_SCHEMA_VERSION)
  darkmux-hardware/ darkmux-heuristics/  Apple-Silicon tier detection + heuristics providers
runtime/                      Internal-runtime crate (built into the darkmux-runtime Docker image; NOT a workspace member)
  src/loop_runner.rs          Agent loop; budget caps; inactivity deadline; detector + recovery wiring
  src/compaction.rs           Narrative + structured-slot compaction; JSON repair; escalation
  src/feedback.rs             Feedback-injection channel + default per-signal templates
  src/cycle_detector.rs       Repeated-tool-call detection (#418)
  src/reasoning_loop.rs       Repeated-reasoning detection (#461)
  src/failure_rate.rs         Consecutive-tool-failure detection (#419)
  src/plain_text_tool_calls.rs  Plain-text → structured tool-call promoter (#406)
  src/json_repair.rs          Truncated-JSON repair for compactor output (#401)
  src/trajectory.rs           Trajectory JSONL event writers (the analyze-run skill documents the shapes)
templates/builtin/
  roles/                      Role library (manifest + .md) embedded at compile time
  mission-configs/            Built-in mission configs (coder-phase, review, …) embedded at compile time
  skills/                     Skill library embedded at compile time (work-shape descriptors with keyword routing; renamed from `capabilities/` in refactor 0, see #448)
  workloads/                  Workload manifests embedded at compile time
  lab-fixtures/               Built-in lab fixtures (e.g. demo-tiny-py) registered via scripts/lab-init.sh
  AUTONOMOUS_DISPATCH_PREAMBLE.md  Injected ahead of specialist-role dispatches (#427)
scripts/lab-init.sh           Standalone fixture-registry bootstrapper (NOT a CLI verb; #487 phase 5)
skills/darkmux-<name>/        Agent-invokable skill wrappers
tests/cli.rs                  Integration tests (spawn the binary)
```

## Common tasks for an agent

If a user asks you to:

| Ask | Do |
|---|---|
| "add a new workload" | Drop a JSON manifest at `templates/builtin/workloads/<id>.json`. If it's a `prompt` workload, register it in `EMBEDDED_WORKLOADS` in `crates/darkmux-lab/src/workloads/load.rs`. coding-task workloads need a sandbox seed dir and CAN'T be embedded. |
| "add a new provider" | Implement `WorkloadProvider` in `crates/darkmux-lab/src/providers/<name>.rs`, register it in `register_builtins()` in `crates/darkmux-lab/src/providers/mod.rs`. |
| "add a lab fixture" | Create a dir with a `.fixture.json` manifest (`name` required; `satisfies`, `verify_command`, `required_files` optional), then `darkmux lab fixture register <path>`. A workload binds to it via `requires_fixture: "<name>@<version>"`. Built-ins live under `templates/builtin/lab-fixtures/` and register via `scripts/lab-init.sh`. |
| "check fixtures are healthy" | `darkmux lab doctor` — offline check that registered paths exist, manifests load, required files are present, and content hashes haven't drifted. |
| "run the smoke test" | `cargo install --path . && darkmux lab run quick-q`. Should complete in ~6-10s if a model is loaded. |
| "draft a notebook entry" | Invoke the bundled `darkmux-lab-notebook` skill (installed by `darkmux init`): it reads `darkmux run stats <run-id> --json` (and the run's `manifest.json` when needed) and drafts the entry, observation first, with the verify outcome stated as recorded, then writes it wherever the operator's own instructions say. The `lab notebook draft`/`list` verbs and the `scribe` role were removed in 5.0 (#2913). |
| "make the build self-contained" | Already is — `include_str!` for embedded workloads, no external assets needed at runtime. |
| "review the diff before commit" | Run the AREA you touched (`cargo t-review`, `cargo t-flow`, … — see "Testing — run the area, not the world"; `t-all` only for a cross-cutting change or a release), eyeball `git diff`, propose a commit message — but **do not commit unless explicitly asked**. |
| "check the mission board / housekeeping" | `darkmux mission status` (#829) — the global mission-control read: every mission grouped by status with phase progress + the drift that needs attention (an open mission whose phases are all done; a stalled Active mission; a phase permanently blocked by an earlier abandoned one) + copy-pasteable reconcile commands. READ-ONLY — surfaces + suggests, never mutates; the operator/you run the suggested `mission finalize`/`mission abort` (#1463 — those two whole-mission terminals reconcile phases now, so a "Finalized mission with a non-terminal phase" is no longer a reachable drift). `--json` for programmatic consumption. **Run it as session-start housekeeping** (and before opening PRs / wrapping a work arc) so mission↔phase drift gets caught structurally rather than by memory — and so gh/jira stay reconciled off the same cue. The CLI twin of the viewer's missions lens (#827). |
| "record my adjudication of a dispatch" | `darkmux flow note --execution <id> --text "<verdict · what you overrode · why>" --source adjudication` (#817, #849) — the execution-scoped audit trail for gate reasoning. Later coder briefs in the same mission carry these as `<prior-adjudication-corrections>`, `darkmux memory correction list` lists them, and `mission debrief` reviews them. Nothing renders notes on the dashboard (#2983). |
