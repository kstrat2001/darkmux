# One interface over many engines: design and refactor plan

**Status:** stages 0 to 3 done (section 8); stage 4's default engine designed (section 9) · **Tracks:** #316 (engine abstraction), #3198 and #3136 (CRAP) · **Measured at:** `main` 3126be198 (sections 2 to 6), d1a971568 (section 8)

Every concrete claim cites the code it comes from (`path:line`) or the measurement it was taken from. Nothing here ships today; where this doc says "today", it describes the current binary.

## 1. Why

darkmux ran local models through LM Studio and only LM Studio (`CLAUDE.md`, "What darkmux is", until stage 3.7: "a `ModelBackend` abstraction is #316, deliberately not built"). Stage 3 put a facade in front of it; LM Studio is still its only backend. Not every machine or user runs LM Studio: a Linux machine serves `mlx_lm.server`, a Mac may prefer Splash or lithos, and the hosted seats speak Claude's Messages dialect. Those already run as `url` endpoints (`endpoints.<id>.url` in `profiles.json`), but each one is a special case beside the LM Studio path, and the features built on LM Studio go dark on them.

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
| plain OpenAI-compatible server (Splash, lithos, `mlx_lm.server`, vLLM) | request, usage, and `GET /v1/models` as the models it would answer to, never as what it holds (built in 4.1: `engine::OpenAiCompatible`) |
| Messages (Claude) | request, caching, usage; no load, no inventory |

**Required contract for every endpoint:** an OpenAI-style chat API (or Messages), `GET /v1/models`, and usage in each response. **Optional capabilities** are declared per backend; where one is absent, darkmux says so plainly instead of reading `loaded:false` or passing a doctor row vacuously. Endpoints in `profiles.json` become instances of a backend, so a profile means the same thing whatever serves it (contract 1).

### 3.1 Capabilities: each engine declares them, the core derives its policy

Engines differ by design, and that is the point of the facade, not a problem to flatten (operator, 2026-10-16: "these engines were always going to have their different policies … design the core to use all of them, and take advantage of special features in the more mature ones"). So no darkmux surface asks "is this LM Studio?". Each engine declares what it can do, along a few axes, and the core reads the declaration and picks its behavior. A mature engine declares more and gets more; a bare one still works, with the missing parts said plainly.

| Axis | Values | Core behavior it drives |
|---|---|---|
| **Residency** | reported, with context (LM Studio `lms ps`) · served list is the resident list · not reported | headroom rows, the machine card's `loaded`, dispatch's resolution probe; "not reported" is said, never shown as empty |
| **Loading** | explicit, at a chosen context (LM Studio) · on request, one model at a time (`mlx_lm.server`) · fixed at start · none (hosted) | the residency preflight: load at the profile's context · send the profile's model and disclose that the engine will switch to it · refuse a model the engine does not serve · skip |
| **Unload** | an API · none | eject and cleanup, within question 3 (only what darkmux loaded) |
| **Ownership mark** | a tag on the loaded instance (LM Studio's `darkmux:` identifier) · none | how question 3 knows what darkmux loaded: the tag, or darkmux's own record |
| **Catalog facts** | per field: size, context limit, tool training | `init`'s sizing against the RAM budget; a missing fact is unknown, never 0 or "fits" |
| **Stream extras** | prompt-processing progress (#3210), cached-token counts, a queue position | the scope's PROMPT percentage, cache reporting; read by a recognizer per engine, normalized into one neutral fact |
| **Request extras** | prompt caching (Messages), a progress opt-in (llama.cpp) | sent only where the engine declares them |

What each engine declares, and how sure it is:

| Engine | Residency | Loading | Unload | Mark | Source |
|---|---|---|---|---|---|
| LM Studio | reported, with context and queue | explicit at a context; also loads a bare key on request at its 4096 default, which the `darkmux:` identifier on the wire avoids (#2240) | API | tag | today's code |
| `mlx_lm.server` | not reported | on request, one at a time | none | none | its source at 0.32.1 (4.1); prefill progress measured on 0.31.3 (#3210) |
| vLLM | served list is resident (unverified) | fixed at start (unverified) | none (unverified) | none | to check before its declaration is written |
| Messages (Claude) | none: no residency concept | none | none | none | the API; prompt caching shipped (#3162, #3173) |

**Built in 4.1:** `Engine::capabilities()` returns `engine::Capabilities` (residency, loading, unload, ownership mark, catalog facts). Its enums carry only the values a built engine declares (residency `Reported`/`NotReported`, loading `Explicit`/`Unknown`); "on request", "fixed" and the stream and request extras join with the engine that declares them. Like a graphics driver's caps, a declaration is only as good as its check: `engine::assert_declaration_holds` runs in each engine's tests against its fake and fails when a declared capability's method says `NotReported`, or an undeclared one answers. The ownership mark and tool-training facts have no method to check against and are declared, not checked.

One known gap: 4.1's catalog rows carry size 0 for "unknown", because `ModelMeta.size_bytes` is not optional; it becomes optional before `init` sizes anything against it (4.4).

`OpenAiCompatible` (4.1) starts as the most conservative declaration: residency not reported, no loading or unload darkmux drives, no mark. A finer declaration per engine (an `mlx_lm.server` that loads on request, a vLLM whose list is its residents) is a variant of it, chosen by what `init` detects or by the endpoint's config, and each one is verified against the engine before it is written down (section 9).

## 4. Open questions (operator decisions, not to be settled in code)

1. **Does darkmux start and stop engines itself** (for example `mlx_lm.server` at the right model and context), or only adopt servers the operator started? **Decided (operator, 2026-10-16): only servers the operator started.** darkmux never starts or stops an engine process. Some engines run in the background on their own (LM Studio), so darkmux finds them already running and adopts them; for one that is not running, darkmux says so and names the command to start it, rather than starting it. Within an adopted engine that has a load lifecycle (LM Studio), darkmux still loads and unloads its own models as today.
2. **One memory budget per machine across backends.** Today the budget knows only LM Studio's models. **Decided (operator, 2026-10-16): memory is budgeted per machine.** There is one budget for the machine's memory, whatever engine holds a model, so a model one engine serves counts against the headroom another engine loads into. Measured the same day: with Muse Glimmer 30B served by `mlx_lm.server`, `darkmux machine resources` folded its 33 GB into "other tenants" with no model row, and an LM Studio load would have been planned without it.
3. **Contract 4 generalizes** from "`darkmux:<model>` in LM Studio" to "darkmux manages only what it started, on any backend". **Decided (operator, 2026-10-16): darkmux unloads only what it loaded, on every engine.** On LM Studio that stays the `darkmux:` name. An engine with a load and unload API but no way to tag a model is covered by a record darkmux keeps of each model it loaded there; it unloads only those, never a model the operator loaded. If the engine restarts and the record goes stale, a stale entry fails safe: darkmux reports it and unloads nothing it cannot match. An engine with no unload API (`mlx_lm.server`) has nothing darkmux may unload, since stopping the server is ruled out by question 1. (Corrected in 4.1: `mlx_lm.server` does not only serve the model it was started with. It loads whichever model a request names and drops the one it held; see question 5 and section 3.1.)
4. **Versioning.** Each stage below is behavior-preserving or additive and can ship in a minor. The doctrine line in `CLAUDE.md` is replaced when stage 3 lands, and the decision is recorded. **Recorded (stage 3.7):** stage 3 ships in a minor, since no wire, `--json`, config or profile shape changed; the `CLAUDE.md` line now reads "one engine facade, LM Studio its only backend today", and `docs/agents/LAYOUT.md` and `DESIGN.md` say the same.
5. **Dispatch to an engine that switches models on request.** Found in 4.1, reading `mlx_lm/server.py` at mlx-lm 0.32.1: `mlx_lm.server` holds one model, loads whichever model a request names (`ModelProvider._load` drops the old one first), and its `GET /v1/models` lists every MLX model in the Hugging Face cache plus the one it was started with, never which one it holds. **Direction (operator, 2026-10-16): no single policy; each engine declares its capabilities and the core derives its behavior from them (section 3.1).** For this engine that means "loading: on request, one at a time": dispatch sends the model the profile names and says that the engine will switch to it. The exact wording and where it is said are 4.3's to propose. **Built in 4.3b:** said once per dispatch, on stderr, where the route splits (`split_route`): "darkmux dispatch: `mlx` loads the model a request names; this dispatch sends `<model>`, which may replace the model it holds." `dispatch.start` already records the endpoint and the model sent. **Data boundary (operator, 2026-10-17):** the default engine reached on loopback is local like LM Studio (`identity.md`, radio's full grounding, a `managed_only` fleet job); on another host it is hosted (`brain_leaves_this_machine`).

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

| Stage | What | Done when | Status |
|---|---|---|---|
| 0 | CRAP ratchet in CI; tests for the 0%-coverage path functions | ratchet live; `cmd_model_eject`, `cmd_profile` and the two doctor checks pinned | done: #3232 |
| 1 | `run_dispatch`: boundary tests, mutation-proved, then split by responsibility | `run_dispatch` under complexity 20; every moved branch pinned; one live dispatch identical | done: #3233, #3234, #3235 (132 → 3) |
| 2 | `dispatch_internal::dispatch` and the telemetry sampler, same method | both under 20; live dispatch identical | done: #3237, #3238 (`dispatch` 78 → 6, `run_telemetry_sampler` 37 → 3) |
| 3 | the facade with LM Studio as its only backend; no behavior change | no `lms::` call outside the LM Studio backend; existing tests and a live run unchanged | done: #3241, #3244 to #3246, #3248 to #3250 (section 8.4); live coder dispatch on `deep` 2026-10-16 |
| 4 | plain OpenAI-compatible and Messages backends; capabilities reported or plainly absent; a default engine per machine, which `init` detects and records | unmanaged endpoints stop being special cases; #3209 to #3211 fall out; a machine with no LM Studio (a Linux box serving `mlx_lm.server`) runs `init` and dispatch with no hand-edited config | in progress: 4.1 (the OpenAI-compatible engine type) and 4.2 (the `default_engine` setting, read and checked; `engine::local()` follows it from 4.3, together with the surfaces that handle an engine without a load lifecycle), #3209 (a loopback endpoint reached from the container), 4.3a (`engine::lmstudio()`: a managed model's lifecycle asks LM Studio by name, a view of the machine asks `engine::local()`), 4.3b (`engine::local()` follows `default_engine`; an endpointless model is on it from the loader, `ProfileRegistry::apply_default_engine`; doctor checks it answers; a dispatch to an engine that loads on request says it may switch). Next: 4.4 init, 4.5 machine card and serve (`machine status --json` and serve still say `lms_unreachable`), 4.6 Messages |
| 5 | one memory budget per machine across backends (open question 2, decided) | a model served by any local engine is a row in `machine resources` and counts in the headroom checks and load planning; nothing an engine holds reads as "other tenants" | |

Numbering note: the titles of #3237 and #3238 say "stage 2" and "stage 3"; both are this table's stage 2. Stage 3 is the facade below.

## 8. Stage 3 plan: one facade, LM Studio its only backend

Measured at d1a971568. The inventory is a scan of tracked non-test `.rs` files (`rust_source`'s test reading, so `#[cfg(test)]` modules and `*_tests.rs` are out), each hit mapped to its enclosing function, complexity by RCA as `scripts/complexity-ratchet.py` reads it. A plain grep finds 72 `lms::` lines; 22 are comments and 1 is a test file, which leaves **46 code references in 15 files**, plus 3 module imports (`src/main.rs:117` re-exports `lms` for the binary, `darkmux-doctor/src/lib.rs:33`, `dispatch_internal.rs:13883`) and 4 sites that reach LM Studio without the `lms::` prefix (`LmsHost::new()` twice, `config_access::lms_bin()` spawns twice).

### 8.1 Inventory, by the question each site asks

| Question | Sites | Where (function, complexity) | Facade method |
|---|---:|---|---|
| What is loaded (`lms ps`, `lms::list_loaded`) | 20 | doctor 7 (`check_utility_model_binding` 3, `check_unreachable_darkmux_residents` 6, `check_models_loaded` 5, `check_profile_loaded_match` 9, `check_ram_headroom` 3, `check_ram_headroom_load_projection` 13, `ai_headroom_bytes` 1); serve 2 (`machine_status_handler` 3, `gather_specs` 3); radio_busy 2 (`busy_report_live` 1, `utility_facts_live` 2); ownership 2 (`eject_all_managed` 2); crew 2 (sampler `tick` 3, `ensure_model_loaded_at_ctx_from` 1); `cmd_machine_status` 12; `mission_config_cli::show` 11; `init::fill_worker_model` 6; lab `envelope_check` 3; eureka `collect` 1 | `loaded()` |
| What is loaded, ids only, by its own unbounded spawn | 1 | `probe_loaded_model_list` 8 (`dispatch_internal.rs:14155`: `.output()`, no bound, no text fallback) | `resident_ids()`, moved verbatim |
| What could run here (`lms ls`, `lms::list_available`) | 6 | `cmd_scan` **25**, `cmd_profile` 13, `init::fill_worker_model` 6, `init::fill_utility_model` 7, eureka `collect` 1, `check_ram_headroom_load_projection` 13 | `catalog()` |
| Model facts type (`lms::ModelMeta`) | 11 | init 6 (`choose_worker_model`, `choose_utility_model`, `plan_utility_binding`, `worker_facts`), main 3, eureka 1 (a struct field), heuristics 1 (import) | type moves to the facade, old path re-exported until the last PR |
| Unload, and "was it already gone" | 2 + 2 | `ensure_model_loaded_at_ctx_from` 1, `eject_all_managed` 2; `unload_stale_for_reload` 4, `eject_each` 5 (`lms::is_not_resident`) | `unload(id)`; `is_not_resident` already reads the typed `HostError`, so it moves as is |
| Load at a context | 2 | `LmsHost::new()` in `load_at_ctx_bounded` 1 and `lms_host_factory` 1; the free `lms::load_with_identifier` has **no caller** | `host() -> Box<dyn ModelHost>` |
| Ledger inputs (`ps` and `ls` together) | 1 | `model_ledger::gather` 1 → `gather_with_bin` 4, its own parser (`model_ledger.rs:1716`) | `ledger()` (shipped in 3.6; #316's 2026-08-15 phase 1 sketched it as `gather(&dyn Engine)`) |
| Engine version | 1 | lab `scores::detect` 1 (`lms version` spawn, `scores.rs:181`) | `version()` |
| Which binary, from where | 1 + 1 | `model_ledger::gather`'s `lms::lms_bin()`; doctor `check_lms_binary` (`lib.rs:6373`, `lms_bin_with_source`) | `binary()` |
| Pin a child's cwd (`lms::pin_cwd`) | 3 | `probe_loaded_model_list`, `scores::detect` twice (one is `sw_vers`, not LM Studio at all) | none: a process helper, moves to a neutral home |

### 8.2 Facade shape: a new facade above `ModelHost`, not a wider `ModelHost`

`ModelHost` exists as #316's comment said (`darkmux-gestalt/src/ports.rs:91`, four methods; `LmsHost` at `gestalt_host/lms_host.rs:119`, `MockHost` at `darkmux-gestalt/src/mock.rs:94`), but it is the planner's lifecycle port, and widening it is the wrong move:

- **Its facts are planner-minimal.** `ResidentFact` is identifier, model key, context; `CatalogFact` is model key and size. The 20 "what is loaded" sites read `LoadedModel` (status, size, `queued`), and the catalog sites read `ModelMeta` (architecture, maximum context, tool use, type). Widening the facts drags presentation fields into a crate whose only dependency is `darkmux-types`.
- **It has 11 implementations**, 10 of them test doubles in crew, lab and gestalt. Every new method lands on all of them.
- **A Messages backend (stage 4) has no lifecycle.** A facade whose `host()` can be absent declares that (section 3's "plainly absent"); a widened `ModelHost` would answer with four `Unsupported` stubs.

The code also shows the duplication the facade absorbs: `lms ps` is parsed four times (`lms::model_from_json` :216, `LmsHost`'s `resident_fact_from_json` :242, the ledger's `resident_from_ps_json` :1716, `probe_loaded_model_list`), `lms ls` three times, and unload twice (`lms::unload` and `LmsHost::unload`, one shared classifier since #3083). Their bounds and fallbacks differ, so stage 3 moves each **verbatim** behind the facade; merging them changes behavior and is a separate, labeled fix after stage 3.

**Where:** a trait `Engine` and its LM Studio backend in `crates/darkmux-profiles/src/engine/`. Every crate holding a site (crew, doctor, eureka, heuristics, lab, serve, the root binary) already depends on `darkmux-profiles`, which depends only on `darkmux-gestalt` and `darkmux-types`: no new edge, no cycle. The backend owns `lms.rs` and `LmsHost`; `lms_host.rs` and `ownership.rs` stay where they are.

### 8.3 Hotspots

Only one site-holding function is over 20: **`src/main.rs::cmd_scan` (25, CRAP 42.7 in `scripts/crap-baseline.json`)**, with no test naming it. Its site is one `list_available()?` before the branching, so the swap needs no split, but it needs a pin first: a CLI test running `darkmux profile scan` against a fake `lms` (`DARKMUX_LMS_BIN`). `mission_launch::launch` (101) and the stage 1 and 2 functions hold no site. Everything else is at most 13.

Boundary tests already present (read from test references, not from coverage): the doctor checks (`darkmux-doctor/src/lms_checks_tests.rs`, stage 0), `cmd_profile` (stage 0), `init::fill_*` (`src/init_tests.rs`), lab `envelope_check` (`run_tests.rs`), `cmd_machine_status` and `mission config show` (`tests/cli.rs` with a fake `lms`), the dispatch residency path (its host effects are injected closures; `crew/tests/dispatch_boundary.rs`), `eject_each` (closures), `gather_with_bin` (a stub binary). **Not pinned:** `cmd_scan`, `busy_report_live`, `utility_facts_live`, eureka `collect`, `scores::detect`, `machine_status_handler`.

### 8.4 Migration order

Each PR is behavior-preserving, moves one question, and is verified by the existing tests of the functions it touches plus the fake-`lms` seam (`DARKMUX_LMS_BIN`); no live run until 3.7.

| PR | Moves | Verified by |
|---|---|---|
| **3.1** (done: #3241) | `Engine` trait and the LM Studio backend, delegating to today's functions; the 7 doctor "what is loaded" sites | `lms_checks_tests.rs` and the doctor's inline tests, unchanged |
| **3.2** (done: #3244) | 11 of the other 13 "what is loaded" sites (`ownership`'s 2 move with unload in 3.4) | `tests/cli.rs`, `init_tests`, `run_tests`, serve `lib_tests`; pinned first: `busy_report_live`, `utility_facts_live`, eureka `collect`, serve `machine_status_handler` and `gather_specs`, and `mission config show` (its listing was unread, found by mutation) |
| **3.3** (done: #3245) | "what could run here" (all 6 `list_available` sites) and `ModelMeta`, whose old path stays a re-export until 3.7 | `tests/cli.rs` (`profile scan`, `profile draft`), `init_tests`, doctor `lms_checks_tests`; pinned first: `cmd_scan`'s whole output and eureka `collect`'s catalog |
| **3.4** (done: #3246) | unload, `is_not_resident` (its old path stays a re-export until 3.7), and `eject_all_managed`'s list | `eject_each` and residency closure tests; `cmd_model_eject` (stage 0); pinned first: the eject's post-unload re-check and dispatch's wrong-context reload (both found by mutation) |
| **3.5** (done: #3248) | load: `LmsHost::new()` → `engine.host()`; `None` is an `expect` until stage 4, so `run_bounded`'s factory type is unchanged | the gestalt and concurrent-dispatch suites (their doubles are untouched), the dispatch boundary's load tests; pinned first: the wave track's production factory (found by mutation) |
| **3.6** (done: #3249) | the bespoke spawns into the backend verbatim (`resident_ids`, `ledger`, `version`); `pin_cwd` to `darkmux_profiles::process` (its old path a re-export until 3.7) | `pin_cwd_conformance.rs` and `lms_bin_conformance.rs`, their path lists updated in the same PR; pinned first: dispatch's #408 cross-check, `machine resources`, `/machine/resources`, doctor's resident pricing and the lab fingerprint's engine version (all five found by mutation) |
| **3.7** (done: #3250) | `lms` becomes private to the backend, so a call to the `lms` module from another crate is a compile error (`LmsHost`, `gather_with_bin` and `config_access::lms_bin()` stay public, so a bypass through them is still review's to catch); drop `main.rs`'s re-export and the dead `load_with_identifier`; doctor's `check_lms_binary` asks `binary()`; replace the `CLAUDE.md` doctrine line (open question 4) | the full suite with `full-ci`; one live dispatch, with the operator's go-ahead |

**First PR (3.1):** add `darkmux-profiles/src/engine/{mod.rs,lmstudio.rs}` with `loaded()`, `catalog()`, `unload()`, `host()` delegating one-to-one to `lms::*` and `LmsHost`, and route `darkmux-doctor`'s seven `list_loaded` calls through it. It proves the shape on the best-pinned consumer, and the doctor tests stay byte-identical.

### 8.5 What stage 3 does not decide

| Open question (section 4) | Stage 3 |
|---|---|
| 1. darkmux starts engines | decided after stage 3: never; it adopts servers the operator started |
| 2. one memory budget | untouched: the ledger reads through the facade, and the budget still knows only LM Studio (decided after stage 3: one budget per machine, stage 5) |
| 3. contract 4 generalizes (decided after stage 3: unload only what darkmux loaded, by name on LM Studio and by darkmux's own record elsewhere) | **touched, kept neutral.** The facade takes identifiers verbatim and has no ownership method; `ownership::namespaced_identifier` and `is_darkmux_owned` stay the LM Studio backend's mechanism, and `ModelHost::unload` keeps its `OwnedTarget` claim check. The two free-unload paths (dispatch reload, eject) keep their string identifiers |
| 4. versioning | **cannot be avoided:** 3.7 replaces the "LMStudio, and only LMStudio" doctrine line and records the decision. No wire, `--json`, config or profile shape changes (contracts 1, 5, 7): `LoadedModel` and `ModelMeta` keep their fields, and `lms_bin` stays the one config key |

## 9. Stage 4 design: the default engine, and how `init` picks it

Today every machine's engine is LM Studio by construction: `engine::local()` returns `LmStudio` unconditionally (`crates/darkmux-profiles/src/engine/mod.rs`), and `init` plans the worker and utility models from `engine::local().catalog()` (`src/init.rs:262`, `:605`). With no `lms`, `init` stops at "install LM Studio" (`src/init.rs:267`). A Linux machine serving `mlx_lm.server` therefore has no way to say "this is my engine" except a hand-written `url` endpoint, and the LM Studio-shaped parts of `init` and dispatch stay dark.

### 9.1 The setting

One config key, **`default_engine`**: the engine `engine::local()` returns on this machine.

- **Values.** `lmstudio`, or the id of an endpoint in `profiles.json` (`endpoints.<id>`) whose server darkmux should treat as this machine's engine. `lmstudio` reuses the existing value spelling of `ManagedBackend` (`crates/darkmux-types/src/endpoint.rs:59`), so one name means one engine everywhere.
- **Unset means `lmstudio`.** Every existing install keeps today's behavior without a config change (contract 5: additive).
- **Resolved in the one place:** `env(DARKMUX_DEFAULT_ENGINE) > config.json > lmstudio`, through `darkmux_types::config_access`, with its source shown by `darkmux doctor` like every other setting.
- **Refused, never defaulted, when wrong** (contract 9): a value that is neither `lmstudio` nor the id of an OpenAI-compatible endpoint is refused at preflight by every entry point that would consume it, naming where it was set, the value and the closest valid one, and `darkmux doctor` fails it. A managed endpoint (that is LM Studio itself) and a Messages endpoint (a hosted API) are refused too. Its values are an open set the registry defines, so it is not a `config_enum.rs` setting (those are closed token lists): `darkmux-types/src/default_engine.rs` checks it against `profiles.json`, and the profile preflight (`darkmux_profiles::preflight_with`) runs that check beside the endpoint checks.
- **Changed through** `darkmux config set default_engine <value>`, never by darkmux on its own (operator sovereignty).

### 9.2 What `init` does

`init` detects which engines are **answering**, not which OS it runs on. "Linux, so MLX" is a guess: a Linux machine may run vLLM, a Mac may run `mlx_lm.server`, and an installed server may simply not be running. A probe is cheap and settles it.

1. **LM Studio:** `lms` resolves (`config_access::lms_bin()`) and `lms ps` answers.
2. **An OpenAI-compatible server:** `GET /v1/models` answers at each endpoint already in `profiles.json`, at the address passed as `darkmux init --engine-url <url>`, and at the well-known local defaults (`mlx_lm.server`'s default port, 8080, confirmed on mlx-lm 0.32.1).
3. **Decide:**
   - **exactly one answers:** suggest it;
   - **several answer:** suggest LM Studio, list the others with what each serves, and let the operator choose;
   - **none answer:** say so, naming every address probed and the command that starts each engine; write nothing.
4. **Record:** a non-LM Studio choice is written as an endpoint in `profiles.json` (if it is not one already) and as `default_engine` in `config.json`, and named in `init`'s report like the files it already writes. `--dry-run` prints the same plan and writes nothing (`src/init.rs:34`). `init` does not prompt today; when several engines answer it asks, and with no terminal to ask on it takes LM Studio and says which others it saw.
5. **Plan the models from what that engine can answer.** On LM Studio, unchanged: pick from the catalog, sized to the RAM budget. On a plain OpenAI-compatible engine, the only choice is among the models it would answer to (`GET /v1/models`, which on `mlx_lm.server` is its whole Hugging Face cache, not what it holds); `init` cannot pick a context to load at, and says so instead of guessing. darkmux does not start `mlx_lm.server` itself (open question 1, decided: it adopts only servers the operator started). Which model a dispatch may name there is open question 5.

`init` records the choice once, rather than dispatch re-detecting on every run: an engine that happens to be down one morning must not silently move the machine to another one. A down default engine is an error that names it, with the command to start it.

### 9.3 What else reads it

| Surface | With `default_engine` set to a non-LM Studio engine |
|---|---|
| `engine::local()` | returns that endpoint's backend (a managed model's load, residency and eject ask `engine::lmstudio()` by name, so they stay on LM Studio); `host()` is `None` (no load lifecycle), so residency and eject say "this engine has no load lifecycle" instead of calling `lms` |
| `darkmux doctor` | a row for the setting and its source, a reachability row for that engine, and the LM Studio rows marked not applicable rather than failing or passing vacuously (section 3) |
| dispatch | a profile on the local engine sends to that endpoint; the residency preflight is skipped plainly, as it is for `url` endpoints today |
| machine card, serve | report the engine by name, and say plainly that it does not report what it has loaded (`engine::NotReported`); `GET /v1/models` is not a residency list (open question 5) |

### 9.4 Not decided here

- **Several local engines at once** (LM Studio and `mlx_lm.server` both serving on one machine, profiles on each). `default_engine` names one; profiles that name an endpoint explicitly already route to it. A per-profile engine choice beyond that waits for a need.
- **One memory budget across them** is decided (open question 2): one budget per machine, built in stage 5.
- **Starting the engine** is settled: darkmux never does (open question 1). When `init` finds no engine answering, it names the command that starts one.

