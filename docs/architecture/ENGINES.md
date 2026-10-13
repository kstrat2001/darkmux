# One interface over many engines: design and refactor plan

**Status:** proposed (2026-10-13), not built · **Tracks:** #316 (engine abstraction), #3198 and #3136 (CRAP) · **Measured at:** `main` 3126be198

Every concrete claim cites the code it comes from (`path:line`) or the measurement it was taken from. Nothing here ships today; where this doc says "today", it describes the current binary.

## 1. Why

darkmux runs local models through LM Studio and only LM Studio (`CLAUDE.md`, "What darkmux is": "a `ModelBackend` abstraction is #316, deliberately not built"). Not every machine or user runs LM Studio: a Linux machine serves `mlx_lm.server`, a Mac may prefer Splash or lithos, and the hosted seats speak Claude's Messages dialect. Those already run as `url` endpoints (`endpoints.<id>.url` in `profiles.json`), but each one is a special case beside the LM Studio path, and the features built on LM Studio go dark on them.

The goal is a real mux: one interface the rest of darkmux calls, with LM Studio as one backend among several.

## 2. What LM Studio does for darkmux today

Inference is already engine-neutral: requests are OpenAI-style HTTP (or the Messages dialect) for every endpoint. What is LM Studio-only is everything around the request. All of it goes through one module, `crates/darkmux-profiles/src/lms.rs` (`list_loaded` :99, `list_available` :312, `unload` :375, `is_not_resident` :384, `load_with_identifier` :407).

| Capability | Today (LM Studio) | Callers | On a `url` endpoint today |
|---|---|---|---|
| Load a model at a declared context | `load_with_identifier` | dispatch residency, gestalt host | nothing: whatever the server loaded runs, at whatever context |
| Unload and evict under a RAM budget | `unload`, `is_not_resident`, `ownership.rs` | dispatch, residency | nothing |
| Own a namespaced instance (contract 4) | `darkmux:<model-id>` | everywhere that loads | n/a; `limits.concurrent_calls` gates calls |
| Report what is loaded, at what context | `list_loaded` (25 call sites: doctor 8, serve 3, dispatch 3, radio busy 2, sampler, lab, init, main) | cards, doctor, busy checks, sampler | blind: cards read `loaded:false`, doctor rows pass vacuously |
| List models available to choose from | `list_available` | `init`, profile scan, eureka, doctor | nothing: `init` cannot plan |
| Model facts (architecture, size, maximum context) | model metadata | RAM estimates, resident pricing, `n_ctx` checks | nothing |
| Live progress (heartbeats, prompt processing) | the stream | viewer scope, PROMPT state | partial: #3211, #3210 |

The first three belong to an engine darkmux **manages**; the rest are **observation** any engine could report.

## 3. Target design

**One facade.** The rest of darkmux asks one interface for a *seat* (a model on an endpoint on a machine) and asks the seat questions, never an engine directly:

- make this seat ready at this context;
- send it a request (dialect chosen by the backend);
- what is loaded, at what context; is it busy;
- what models could run here; model facts;
- progress of the current request.

**Backends**, one per kind of server, each declaring which questions it can really answer:

| Backend | Answers |
|---|---|
| LM Studio | all of them: today's behavior, wrapped, not rewritten |
| plain OpenAI-compatible server (Splash, lithos, `mlx_lm.server`, vLLM) | request, `GET /v1/models`, usage |
| Messages (Claude) | request, caching, usage; no load, no inventory |
| managed server (later) | darkmux starts and stops the engine itself (open question 1) |

**Required contract for every endpoint:** an OpenAI-style chat API (or Messages), `GET /v1/models`, and usage in each response. **Optional capabilities** are declared per backend; where one is absent, darkmux says so plainly instead of reading `loaded:false` or passing a doctor row vacuously. Endpoints in `profiles.json` become instances of a backend, so a profile means the same thing whatever serves it (contract 1).

## 4. Open questions (operator decisions, not to be settled in code)

1. **Does darkmux start and stop engines itself** (for example `mlx_lm.server` at the right model and context), or only adopt servers the operator started?
2. **One memory budget per machine across backends.** Today the budget knows only LM Studio's models.
3. **Contract 4 generalizes** from "`darkmux:<model>` in LM Studio" to "darkmux manages only what it started, on any backend".
4. **Versioning.** Each stage below is behavior-preserving or additive and can ship in a minor. The doctrine line in `CLAUDE.md` is replaced when stage 3 lands, and the decision is recorded.

## 5. Method: decomplicate before changing features

Measured from CI's coverage run at 3126be198 (`scripts/crap-report.py`, the `crap-report` artifact of quality run 38407658711): **504 of 6,351 functions are over CRAP 10** (220 over 15, 64 over 30, 17 over 100), 328 of them on complexity alone and 176 for lack of tests. That is unchanged since #3198 (505, 2026-10-10). Changing features inside these functions invites a regression nobody sees, so the facade waits until its path is decomplicated.

**Per hotspot, in this order:**

1. **Pin behavior at the boundary** (characterization tests): what callers pass in and what others observe. For the runtime that is the request bodies a mock endpoint receives, the trajectory events, the envelope and the exit code. The harness exists: `runtime/tests/compactor_endpoint_flags.rs` and `runtime/tests/compactor_disclosure.rs` drive the real `darkmux-runtime` binary against mock servers; contract 6 goldens byte-lock model-facing text. Tests on a function's internals come later: written now, they would freeze the shape the refactor exists to change.
2. **Prove the tests can fail.** Line coverage says a line ran, not that anything checks it. Mutation-check the target function on CI's Linux runners (never locally); every branch the refactor will move must be pinned by a test that goes red when the branch is removed.
3. **Refactor in small behavior-preserving steps:** extract one responsibility at a time, green after each, as #3141 did for the agent loop (`run_with_sleeper` 151 → 2).
4. **Unit-test the new small pieces;** they are cheap once split.
5. **The ratchet keeps the gain:** extend `scripts/complexity-ratchet.py` with a CRAP ceiling (#3198 step 1), so no function rises above its recorded CRAP.
6. **One live dispatch after each stage:** tests prove the pieces; only a run proves the whole.

Functions at 0% coverage on the path are the exception: small enough to test directly first.

## 6. Hotspots on the facade's path

| Function | CRAP | Complexity | Coverage | Role in the facade |
|---|---:|---:|---:|---|
| `runtime/src/main.rs::run_dispatch` (:146-1183, 1,037 lines) | 2298 | 132 | 50% | the container's whole run: argument parsing, client setup, the loop; where per-backend request behavior will live |
| `crates/darkmux-crew/src/dispatch_internal.rs::run_telemetry_sampler` | 202 | 37 | 51% | calls LM Studio directly; becomes "observe this seat" |
| `crates/darkmux-crew/src/dispatch_internal.rs::dispatch` | 166 | 78 | 76% | seat resolution and residency: the facade's front door |
| `src/mission_launch.rs::launch` | 167 | 101 | 81% | profile and utility staffing |
| `src/main.rs::cmd_model_eject` | 156 | 12 | 0% | model management, untested |
| `crates/darkmux-doctor/src/lib.rs::check_ram_headroom_load_projection` | 95 | 13 | 21% | becomes a capability-driven check |
| `src/main.rs::cmd_profile` | 93 | 13 | 22% | inventory (`list_available`) |
| `src/main.rs::cmd_scan` | 43 | 25 | 70% | inventory |
| `crates/darkmux-doctor/src/lib.rs::check_profile_loaded_match` | 40 | 9 | 28% | capability-driven check |

Off the path (left to the ratchet as files are touched, except cheap 0%-coverage tests): `TrajectoryEvent::seq` (756), `presence_reconciler::spawn_reconciler_thread` (553), `serve::xread_block_once` (247).

## 7. Stages

| Stage | What | Done when |
|---|---|---|
| 0 | CRAP ratchet in CI; tests for the 0%-coverage path functions | ratchet live; `cmd_model_eject`, `cmd_profile` and the two doctor checks pinned |
| 1 | `run_dispatch`: boundary tests, mutation-proved, then split by responsibility | `run_dispatch` under complexity 20; every moved branch pinned; one live dispatch identical |
| 2 | `dispatch_internal::dispatch` and the telemetry sampler, same method | both under 20; live dispatch identical |
| 3 | the facade with LM Studio as its only backend; no behavior change | no `lms::` call outside the LM Studio backend; existing tests and a live run unchanged |
| 4 | plain OpenAI-compatible and Messages backends; capabilities reported or plainly absent | unmanaged endpoints stop being special cases; #3209 to #3211 fall out |
| 5 | one memory budget per machine across backends | (open question 2) |
| 6 | managed servers for engines without a manager | (open question 1) |
