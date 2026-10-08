# Cross-system contracts

Agent reference, read when the work touches it. Moved verbatim from the root `CLAUDE.md` (#3133), which keeps a one-line summary and a pointer here.

## Cross-system contracts — alignment is mandatory (operator finding, 2026-07-10)

darkmux has contracts that span subsystems. They are binding on EVERY producer and consumer —
a new feature conforms to them or extends them through their own versioning/doc mechanism;
it never bypasses, subsets, or fences them. Two same-day production failures on cutover day
were both contract violations that unit tests structurally cannot catch (tests exercise the
subsystem, not its alignment): crews rejecting endpoint-bearing profiles (violated: profiles
mean the same thing to every consumer — #1269), and the funnel emitting a new record
vocabulary without the dispatch-liveness bookends (violated: running work is visible work —
#1272).

The contract registry (extend this list when a new cross-cutting invariant is born):

1. **Profile uniformity** — a profile means the same thing to every consumer (dispatch,
   missions, benches). A consumer may not legislate which profiles are legal; it routes on what
   the profile declares (local vs endpoint → dialect, cycling, token accounting).
2. **Dispatch liveness** — any production code path that performs a WORK execution emits
   `dispatch.start` and a terminal `dispatch.complete`/`dispatch.error` (RAII-guarded on all
   exit paths), regardless of what richer vocabulary it also emits. Liveness surfaces key on
   these bookends plus presence (#857); new vocabularies supplement, never replace.
   **Amended by #2914 (5.0): darkmux's own UTILITY jobs are exempt, and run lean.** The
   utility jobs are defined once, by `darkmux_crew::usage::utility_job` (every runtime
   compactor call, and every call by the radio routing role; `call_purpose` and the
   `UtilityJobKind` enum both read it); they run on the machine's one utility model
   (`internal.utility`). Each emits a lean `utility.start` when it starts and its
   `telemetry.tokens` usage record (`purpose: utility`, `job`) when it ends (a failed routing
   call ends with `utility.error` instead; a compaction whose calls all failed is ended by
   its execution's next record), and nothing else: no session of its own, no bookends,
   no presence, no run. They are ACCOUNTED (the fleet hero sums them under their own chip)
   and VISIBLE (#2915: the fleet card's utility strip and the "compacting" scope reading key
   on `utility.start`), but not LISTED (the runs board, the fleet card's activity and the
   status line's last dispatch key on bookends and therefore show work only, which is the
   intent). The markers are emitted at the one chokepoint each half passes through
   (`darkmux_crew::utility::run_utility_single_shot` on the host; the runtime's
   `compaction.start` trajectory event, through the tailer), never as a bookend; a
   compaction's markers carry the session of the execution it serves, a routing job's carry
   none. Radio's ANSWERING seat (`radio-host`) is work and
   keeps its bookends and its run.
3. **Lab/fleet sink boundary** — lab runs write per-run-local artifacts; the fleet flow
   stream carries engagement work only. No crossings in either direction. Conformance:
   `RedisSink::persist` and the outage backfill drop any record whose `session_id` is a
   lab run, through the one `is_lab_session` predicate (#3074).
4. **Namespace convention** — darkmux-owned state in shared systems carries the darkmux
   namespace; operations manage only the namespaced subset (see the namespace section).
   Formalized as ABSOLUTE for model lifecycle (operator, 2026-07-10, #1274): every darkmux
   load/unload/reconcile targets only `darkmux:*` instances, darkmux dispatches only TO
   `darkmux:*` instances (a user-loaded copy of the right model has unknown load config —
   the #1135 ghost — and is never reused), and measurement (budget accounting #1243,
   dispatch provenance) counts only the namespaced subset. Non-namespaced models are user
   state: visible to the planner as pool consumption only, structurally unnameable in plan
   actions (`OwnedTarget`). When user state blocks a need, darkmux surfaces a reason naming
   the blocking instance and suggests; it never touches. This supersedes the #408-derived
   preflight behavior of reusing/unloading foreign residents.
5. **Schema versioning** — flow/rules/config/profiles data shapes change only through their
   documented semver rules. Flow-archive readers are lenient-on-read; user files follow
   contract 7.
6. **Frozen model-facing text** — measured prompts/personas live in ONE artifact with golden
   tests generated from the reference implementation; assembly and request bodies are
   byte-locked (#1256). "Frozen" means one hash, not one intention.
7. **User files: unknown keys are refused** — a user file is a JSON document the operator
   writes and darkmux reads at run time: `config.json`, `profiles.json`, role / skill / crew
   manifests, mission configs, rule files, workload documents, lab fixture manifests, and a
   crawl's workspace spec (`darkmux_types::user_files::UserFileKind` is the set). None of
   them is compile-time. A key a file's schema does not know, a typo or a key an older or
   newer darkmux spelled differently, is bad config. **Loading never crashes on it**: the
   typed load survives (a `#[serde(flatten)] extras` overflow catches the key, so one typo
   never discards the rest of the file), and `darkmux doctor` still runs against a file that
   is not even JSON. **Consuming refuses**: every entry point that consumes the file refuses
   at preflight, before minting anything, and doctor reports it as Fail, one row per file.
   A preflight refuses only over a file the operation would load: the effective copy of each
   mission config and workload id (a copy another tier shadows never loads), and the one lab
   fixture the run's workload binds. Doctor fails every file, and says when one is shadowed
   or only read by a run that binds it.
   Both name the file, the key's dotted path and the closest valid key (the same shape as
   contract 9's enum refusal); a retired key names what replaced it instead of a guess.
   **A value of the wrong type, or a missing required key, is refused the same way**, naming the path, the expected
   type and what it got: one such value fails the whole typed load, which for `config.json`
   means every setting falls back to its default (Redis and audit silently off) and for a
   user role, skill or rule means the builtin of the same id silently stands in. The
   registry is the one exception: it already quarantines a mistyped profile or endpoint
   entry by name (#1282), loudly, so the gate leaves that to it.
   `darkmux config set` refuses an unknown key through the same suggester. Semantic
   validation of a known key's VALUE still lives at resolution and in doctor, never on the
   load path (#1269), and a value that fails it is refused where it is consumed, never
   replaced by a default; for enum-valued settings that rule is contract 9. Only readers of
   append-only flow archives stay lenient.

   The mechanism is one module, `darkmux-types/src/user_files.rs`: each kind's valid keys
   are its Rust type's derived JSON schema (`schemars::JsonSchema`), walked against the raw
   document, so a new field is valid the moment it exists and no key list can drift; nested
   blocks, list items, map values and enum variants are walked too, and each value is checked
   against the schema's type, integer range, enum tokens and variant tags. The `extras` overflow is
   `#[schemars(skip)]` (it catches keys, it does not validate them); a flattened map that IS
   the schema (a hook rule's `match`) stays open, and each kind's `open_objects` test pins
   that set. `_comment` is valid in any struct-shaped object, as a note for the reader; inside
   a map it is an entry and is checked as one. A missing required key is refused too. The
   structural guard is `whatever_the_gate_passes_the_typed_load_accepts`: for every shipped
   template and example, and variants inserting `_comment`, an unknown key, or a value of
   each JSON type at every level, a document the gate passes must load with serde. `closest` is the only
   "did you mean" in darkmux. The preflight chain is `config_enum::preflight` (config.json)
   inside `darkmux_profiles::preflight_with` (the registry) inside
   `darkmux_crew::user_files::preflight_with` (roles, skills, mission configs, rules), which
   every dispatch, mission launch, radio and ACP entry point calls; `darkmux_lab::user_files::
   preflight_with` adds workloads and fixtures for a lab run. Conformance: each owning
   crate's `user_files` tests (an unknown top-level key, an unknown nested key and a near-miss
   per kind, refused at every consuming scope and at no other), `every_shipped_template_file_
   has_no_unknown_keys` (everything under `templates/builtin/` plus both example files), and
   `tests/cli.rs`'s `an_unknown_key_in_a_user_file_is_refused_by_every_consuming_entry_point`.
   A crawl's workspace spec has no fixed location: the mission-launch preflight checks the
   spec a launch input names (`"workspace": "{{<input>}}"` in a step) before minting, and
   `WorkspaceSpec::load` checks it again in the plan step.
8. **Work-unit vocabulary** — the four operator-visible work nouns each denote ONE grain,
   and every surface (CLI verb, hash route, wire type, UI label, doc) uses them at that grain
   (#1974). The containment ladder is **mission > phase > task > step > role execution**:

   - **run** — the UMBRELLA, never a grain: *a top-level unit of work the operator started*.
     Exactly three kinds (the runs-board `RunKind`): `mission`, `dispatch`, `lab`. The runs board lists runs;
     drilling into one opens that kind's own view. `darkmux run list` serves the same union.
   - **dispatch** — TOP-LEVEL ONLY: the verb `darkmux dispatch <role>`, and the `RunKind` it
     produces, which means *a run consisting of exactly one role execution*. It is named for
     its CONTENT, not for the verb. Note `RunKind::Dispatch` (the runs-board type, `darkmux_serve::runs::RunKind`) is
     decided FROM a mission by `classify_mission`: a spec with `config_id == "dispatch"` is
     Dispatch, any other spec is Mission, and only a mission with no spec falls back to shape
     (a crew-of-one graph). So it is a label on a mission, not a third ontological peer of
     `Mission`.
   - **role execution** — the INNER unit: *one role, running until it stops*. Many turns, not
     one model call (`max_turns`, `turn_seq`). This is deliberately named for the ROLE, not
     the model, because the model is DERIVED, not declared: `select_model(role, profile)`
     resolves it at dispatch entry, `DispatchOpts` takes `role_id` as required and
     `profile_name` as an optional override, and an endpoint-staffed seat has no local model
     at all. Role is the stable identity across local and remote; the model is a consequence
     of the profile. (An earlier draft called this a "dispatch", which is what made the word
     mean both ends of the ladder at once; a later one proposed `model_run`, which named the
     unit after its output.)

     *Candidates rejected on collision, recorded so they are not re-proposed:* **task** and
     **job** and **activity** and **assignment** are all TAKEN at other grains — `task` is the
     parent layer; `job` is fleet work submission (`darkmux_fleet::WorkJob`/`WorkSubmission`,
     and `WORK_JOB_SCHEMA_VERSION` is a wire schema), where a job is one dispatch sent to a
     peer machine's fleet listener; `activity` is the viewer's activity lanes; `assignment` is Task-level resource
     assignment. Reusing any of them would recreate this exact defect one word over. `shift`
     and `stint` are genuinely free and were weighed for being more humanized; `execution`
     won on precision and on composing cleanly for sub-executions.
   - **phase** / **task** — the two grouping layers between a mission and its steps
     (`Mission.phase_ids` -> `Phase.task_ids` -> `Task.step_ids`). A `Task` is also where
     resource ASSIGNMENT lives (role, profile, workdir, image), which is why a step inherits
     its staffing rather than declaring it.
   - **step** — a mission-graph node. The step is the NODE; the role execution is what the
     node DID.
     **A step contains ZERO OR MORE role executions, and the cardinality is the reason this
     layer exists.** `procedural.shell`/`procedural.noop` contain zero; `dispatch.internal` and
     `dispatch.single_shot` contain one; `dispatch.map` contains one per collection item
     (with per-item error isolation — its own doc contrasts it with "a single-dispatch
     step"), and a `dispatch.unit` step contains one per draw. Do NOT
     insert a further noun between step and role execution to name the N: a 1:1 wrapper earns nothing, and
     the N already has three domain names that are not synonyms — `dispatch.map`'s **items**
     (what the work is done to), review's **seats** (which staffed model does it), and
     **draws** (one invocation, `MemberRecord.draws`). They all bottom out in one model call,
     which is what `dispatch` already means.
   - **session** — INTERNAL ONLY: a join key tying a family of flow records together,
     typed as `SessionId { kind, run }` (`darkmux-types/src/session_id.rs`): no session
     exists without its run, `wire()` is its only string form, and a pre-5.0 string is not a
     session (`SessionId::parse` refuses it). Never an operator-facing word, because it is
     also minted for mission lifecycle transitions that are not executions at all (the
     run's own session, `SessionKind::Run`). The `session_id` FIELD keeps its name on
     disk — renaming it strands every archive.

   Two consequences that new code inherits:

   - **A role execution has exactly one SPECIALIST role.** Utility invocations inside it
     are SUB-EXECUTIONS, attributed to a utility role and their OWN model, never blended into
     the primary's metrics. They run lean (contract 2's amendment): a usage record and
     `utility.start`, with no bookends, session or run, so "sub-execution" means "attributed to
     its own role and model", not "has its own bookend pair". What counts as utility has
     ONE definition, `darkmux_crew::usage::utility_job` (`call_purpose` derives from it; compaction and the radio router ,
     darkmux's own jobs, run on the machine's one utility model, #2914; the scribe and
     mission-compiler roles this entry used to list were retired in #2912/#2913), and every
     consumer that splits work from utility reads it rather than keeping its own list. Naming the unit for the role is what lets attribution compose: a sub-execution is
     attributed the way its parent is, one level in. The compactor's per-call usage record
     (`telemetry.tokens`, `call_kind: "compaction"`) conforms since #2902 step 1b: its `handle` is `compactor`
     and its `model` the compactor's. The `dispatch.compaction` and `telemetry.compaction` records still carry
     the specialist's `role_id`/`model` at record level, naming the compactor only in `payload.compactor_model`
     (#1974).
   - **A specialist change is an EXECUTION BOUNDARY.** Escalation mints a new role execution;
     it never puts a second specialist role inside this one. Any predicate that infers a "model swap" from
     residency alone is wrong: a declared utility role going resident is not a swap (#1934).

   **One wire spelling per event (operator, 2026-09-27; supersedes "the wire keeps its
   historical spelling").** 5.0 breaks flow compatibility rather than carry two conventions
   for the same event. Every action is a `darkmux_flow::FlowAction` variant, spelled
   `<scope>.<event>[.<detail>]` (lowercase, two or three dot-separated segments), and the
   wire string lives in exactly one place: `crates/darkmux-flow/src/action.rs`. Producers
   build the enum; no constructor, builder or helper takes an action as a string, and
   `FlowAction`'s public deserializer refuses an unknown action. Consumers match
   on the enum, never on a string.
   <!-- flow-action-guard:allow-start — names the old spellings to say what they now read as -->
   Nothing in the build knows a retired spelling (5.0, #3036): the pre-5.0 spellings
   (`dispatch start`, `step result`, `mission close`, `note`, `verdict: <v>`, `sprint *`,
   ...) and the actions darkmux retired with no current equivalent (`telemetry.process`,
   `funnel.*`, ...) read, through `darkmux_flow::reader` like every other record, as
   `FlowAction::Other`, an action this build does not know, kept verbatim and counted by
   `darkmux doctor`. Nothing is rewritten on read and nothing is synthesized for a record
   (no upgraded spelling, no renamed payload key, no invented execution id). Archives are
   append-only and are never rewritten.
   <!-- flow-action-guard:allow-end -->
   It cannot be written: every sink write goes through
   `FlowSinkWrite::write`, which refuses it before any sink sees the record.
   `scripts/flow-action-guard.py` (CI) fails on a flow action written by hand in production
   Rust (a literal, a format string, a prefix test, `concat!`, or JSON inside a string), and,
   in test code, the viewer, docs, skills, templates and fixtures, on any string that looks
   like an action and is not a current one (a retired spelling, which the script lists in
   `RETIRED_SPELLINGS`, or a made-up `<scope>.<event>`); recorded archives are exempt.
   A hook rule that names a retired action spelling is no special case: it matches no
   action darkmux writes, so the hook sink warns at load and `darkmux doctor` warns
   `CANNOT MATCH`. What entry 8
   fixes is the WORD used in code, docs, UI and on the wire, where `dispatch` had come to
   mean both ends of the ladder at once; the run grain now has its own bookends (below).
   Both contracts stand: contract 2 says liveness must be visible, contract 8 says which
   noun means which grain.

   Verified by enumerating every completion-endpoint (`chat/completions`) call site. Two
   host-side entry points bookend per execution and are correct: `crew::dispatch::dispatch`
   and `dispatch_local_single_shot`. Everything model-bearing routes through one of them: all
   three lab providers (`prompt`, `coding_task` and `tool_bench` under
   `crates/darkmux-lab/src/providers/`), coder-phase, and radio's answering seat. Two things
   do not bookend:

   - **Compaction is a sub-execution.** `runtime/src/compaction.rs` calls the endpoint with
     its own `compactor_model` (a 4B utility agent) inside the specialist's role execution,
     emitting no bookends. That is correct by the sub-execution clause above; the defect is
     ATTRIBUTION, and it is the `emit_telemetry` violation already named.
   - **(Historical — fixed by deletion, not migration.) The review pipeline used to bypass
     the dispatch primitive entirely.** Its bespoke launcher's seats called the raw chat
     primitive `single_shot_chat` directly, and wrapped the WHOLE multi-model crew mission
     in ONE `with_dispatch_bookends` pair whose arguments were literally plural —
     `crew.distinct_profile_names()` and `crew_model_summary(&crew)`, emitted as
     `crew={names} models={summary}` — so those seat executions had no dispatch identity at
     all. That launcher (`src/mission_launch_review.rs`) was deleted in #2310 P4d; the
     shipped `review` mission config's seats now go through the generic building blocks
     (`dispatch.unit` → `darkmux_crew::dispatch::dispatch`, bookended, for `reviewer`;
     `dispatch.internal` for the optional `coder` seat), so the bypass this bullet describes
     no longer exists. The surviving `single_shot_chat` call site is the generic Tier-1
     `dispatch.single_shot` kind (`crates/darkmux-crew/src/step_kinds/builtins.rs`, its hosted
     twin beside it), which bookends each execution through `ExecutionBookends`. It is not
     review-specific, and not used by `review.json`.

   **Every record of an execution names it: `execution_id` (5.0).** The id
   (`darkmux_types::execution_id::ExecutionId`) is minted ONCE per role execution at the
   host entries that run one: `crew::dispatch::dispatch` (which covers the hosted
   single-shot path), `dispatch_local_single_shot`, the `dispatch.single_shot` step kind, and
   each ITEM of a `dispatch.map` (a map step writes no `dispatch.*` pair of its own; the
   scheduler's `step.start`/`step.complete` cover the step). It is stamped through ONE
   builder path (`FlowRecord::for_execution`, called by `darkmux_crew::dispatch`'s
   builders), on every record whose action declares `Execution` grain (`FlowAction::grain`,
   on the action's row): the `dispatch.*` bookends, turns, tool calls, `telemetry.*` and
   `budget.*` records. `CheckedRecord::check` refuses to write one without it. The
   compactor's sub-execution records carry the PARENT's id (its usage record is `purpose:
   utility`, so a sum can split it out); a host-side utility job (radio routing) mints its own
   for its usage record and its markers carry none; a resumed dispatch continues its execution (the id
   rides in the host-only origin record beside the out-dir, `<out-dir>.resume_origin.json`, which the container never mounts);
   a specialist change mints a new one. Consumers key on it: the token sum's legacy
   fallback, the DISPATCHES chip, `records_emitted`'s pairing, both lifecycle executors'
   attempts, and the finding store (`<execution_id>/<seq>`). A record from before 5.0
   names none, and none is invented for it (#3036): the reader and the viewer's `ingest.ts`
   stamp nothing, and only the token and run counts key such a record by `(session,
   mission)` (`usage_sum::execution_key` and `executionOf`), the one place that grouping
   survives.

   **The run grain has its own bookends (5.0).** A `mission launch` and an ACP panel run
   open `run.start` on the run's own session and close it with `run.complete` or
   `run.error` on every exit path (a `BookendGuard`: a panic or an early return still
   writes `run.error`). The role executions inside it bookend as `dispatch.*`, so
   `dispatch.*` means one role execution and nothing else. #1899 prescribed the whole-run
   pair for every generic launch; that mechanism stands, only its noun changed. Which
   actions are bookends, at which grain and edge, is declared once, on the action's row in
   `action.rs` (`FlowAction::bookend`; the viewer's `bookendOf` mirrors it). The run
   lifecycle — ONE rule with two executors judged by the same corpus
   (`tests/lifecycle/cases.json`): the viewer's `ui/src/lib/lifecycle.ts` and the daemon's
   `crates/darkmux-serve/src/run_lifecycle.rs` — opens an attempt on a bookend start at
   either grain and takes its outcome from a bookend terminal. The runs board's
   representative is the run session its `run.start` opened, the fleet card collapses a
   mission onto its run-grain group, and the status line's last-dispatch counts role
   executions only. A pre-5.0 archive's whole-run pair (`dispatch.*` with `source`
   `mission`, or the retired review launcher's `review`) is no longer read as `run.*`
   (#3036): it reads as the execution bookends it was spelled as, or as an unknown action
   when spelled the old way; no file is rewritten.

   **"step" is a known-imperfect name, deliberately not being changed (operator, 2026-08-26.)**
   It implies plurality, so it reads badly for a single-step dispatch — but a task genuinely
   can hold several, so the name is defensible and the churn is not. The decision was to fix
   the SEMANTICS and commit to them first; a rename is cheap once the meaning is settled and
   expensive while it is still moving. Do not reopen this as a naming question without new
   information about the semantics.

   Conformance: every detail hash route is named for the runs-board `RunKind` it opens.

9. **Enum-valued settings** — an unregistered value in an enum-typed setting is bad config
   (#2947). It is never resolved to a fallback, in either direction. Every entry point that COULD
   consume it refuses at preflight, before minting anything, even when one particular run through
   it would not read the value (a tool-less remote dispatch and the thermal ladder, say): bad
   config is bad config, and a refusal must not depend on which code path a run happens to take.
   The refusal names the raw value, where it was
   set (env var or `config.json` key) and the valid values; `darkmux doctor` reports it as Fail;
   `darkmux config set` refuses it; and help (`config set <key>` with no value, `config list`,
   `config set --help`) lists the valid values with their meanings. `--skip-preflight` does not
   waive it: that flag skips a Docker probe, and a bad config value is not a probe result.
   A setting that no work-starting entry point reads is refused by no preflight, by design, and
   its registry entry carries a `no_scope_reason` that doctor prints instead of claiming a
   refusal: today `fleet.mode` (a bad value only makes viewer links use the direct address, with
   a warning) and a hook rule's `match.level` / `match.category` (a bad value turns the hooks sink
   off, loudly, while the run continues without it).

   Retired spellings are refused too, naming the replacement ("`enforce` was renamed to `conclude`
   in 5.0"): a rename never reads the old word as the new one. Policy values name the action
   (`off` / `record` / `warn` / the rule's own verb, e.g. `conclude`), never `enforce`/`observe`.

   The mechanism is two declarations in `darkmux-types/src/config_enum.rs`, and everything else
   derives from them with no per-setting code: a `ConfigEnum` (implemented with `config_enum!`,
   one row per value, its token and one-line meaning, plus any retired spellings, with an
   exhaustive `match` so the value list cannot drift from the Rust enum) and an entry in
   `ENUM_SETTINGS` (key, env var, shipped value, and the consuming `Scope`s: dispatch, mission
   launch, lab run, fleet submission; or a per-item entry such as `hooks.rules[].match.level`,
   checked where the list is loaded).
   Storage stays a string parsed at the accessor, so a bad value never fails the load. A new enum
   setting is those two declarations plus a one-line accessor over `config_access::resolve_enum`.
   Conformance: `config_cmd::every_enum_setting_obeys_the_rule_on_every_surface` iterates the
   registry across preflight, doctor, `config set` and help; `tests/cli.rs`'s
   `every_enum_setting_is_refused_by_every_cli_entry_point_that_consumes_it` spawns each entry
   point; and `config_enum::every_enum_in_the_config_schema_is_registered` fails on an unregistered
   enum of any visibility in darkmux-types, an unregistered `config_enum!` anywhere, or a
   hand-rolled string-literal token match at a config accessor. Its limit: a string compared
   with `==` against a literal outside those files is not seen, and stays a review question. `docs/ENVIRONMENT.md`'s value lists are drift-tested
   against the registry. Per-endpoint `profiles.json` enums (`managed`, `dialect`) share the
   `ConfigEnum` value tables but keep their own refuse-at-use path through `Lenient<T>`.

Enforcement is structural, not procedural: every contract gets a conformance test where one
is expressible (golden files, emission-sequence assertions, boundary tests), and every review
of a new subsystem asks explicitly: **which contracts does this touch, and where is its
conformance shown?** A deliberate scope cut that fences a contract (as crews-local-only did)
is itself a contract change — it gets the same failure-mode scrutiny as a feature, because to
the operator's config file, it is one.
