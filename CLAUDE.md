# Claude / Agent guidance for darkmux

This file is for any AI agent (Claude Code, Cursor, OpenClaw, etc.) that's helping a user work with the darkmux source tree. Read this once before doing anything.

## What darkmux is

A pre-1.0 Rust CLI that does two things for users running local LLMs (LMStudio + Ollama + llama.cpp):

1. **Profile multiplexer** — a dispatch loads the models a named profile declares (model + context length + optional compaction settings) from `~/.darkmux/profiles.json`, under the resident budget (the multiplexer is now internal to gestalt).
2. **Lab harness** — `darkmux lab run <workload>` dispatches a workload against the internal Docker-bounded runtime and records timing + trajectory + verify outcome under `.darkmux/runs/<run-id>/`.

The CLI is the *engine*; the empirical findings in the Genesis series on Darkly Energized (<https://darklyenergized.substack.com>) are what it backs. The reproducibility story is the product story — users should be able to rerun a workload and get numbers comparable to the published claims.

## darkmux's grand vision (agent-facing)

The user-facing **"What darkmux is for"** section in `README.md` is the canonical version of the project's north-star. Below is how the same five claims translate into operational doctrine for an AI agent (Claude Code, OpenClaw, Cursor, etc.) working on darkmux or driving it on behalf of an operator.

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

The release binary is self-contained (~11 MB as of 1.18.x — embedded workloads, roles, mission configs, the viewer, and the mission-graph lens's vendored React Flow bundle all ride inside it via `include_str!`/`include_bytes!`). `cargo install --path .` produces a binary that works from any directory without the source tree.

## Releasing — dogfood the dispatch critical path first

> **Release gate (operator mandate, 2026-06-29): NO release is cut until local darkmux runs against REAL AI dispatches that show the release's FEATURES are working — not just that the dispatch path runs.** A trivial smoke proves the *path* (container ran, loop executed, `result: "stop"`); it does NOT prove the *behaviors* this release ships actually work. For each feature-bearing change in the release, run a real local-AI dispatch that EXERCISES it and observe the expected behavior live — read `lms ps`, the flow records, the envelope, the served viewer, whatever the feature touches. `cargo test` + CI + a trivial path-smoke are necessary but not sufficient; the gate is **feature behavior verified live, locally, against real dispatches.**
>
> **#1135 is exactly why.** `dispatch --profile` silently JIT-loaded the model at LMStudio's 4096 default instead of the profile's `n_ctx`, but **a trivial smoke message FITS 4096** — so `result: "stop"` looked perfectly healthy while the feature was broken and would have shipped garbage reviews. Only a dispatch that exercised the feature *and read `lms ps` to confirm the loaded context* caught it. A path-only smoke ships this class of bug.

**Before cutting ANY release, run a real dogfood dispatch as a critical-path smoke test.** A trivial `darkmux dispatch <role> "<smoke>"` (or a `mission run`) on the build you're about to tag is the floor — it proves the end-to-end dispatch path (model selection → container spawn → `docker run` → runtime loop → envelope) runs. But the floor is not the gate (see the mandate above): also exercise the actual features. `cargo test` + `cargo clippy` are necessary but NOT sufficient: they exercise the *pieces*, not the live invocation, and never the feature behavior.

This is load-bearing, not ceremony. **v1.3.x–1.4.0 shipped a completely broken internal-runtime dispatch** (`docker docker run`, exit 125 — #975): every unit test asserted the docker *argv vector*, but nothing ever constructed and ran the real `Command`, so the break sailed through four releases of green CI. One dogfood dispatch before any of those cuts would have caught it on the first try; it was finally found only when a `dispatch` happened to run for an unrelated reason.

The discipline:
- **Dogfood the version you're tagging** — `cargo install --path .` from the release commit first, then dispatch.
- **Make a runtime image available first.** The versioned GHCR image (`darkmux-runtime:<version>`) only publishes *at* release, so a pre-release dogfood can't pull it. If `runtime/` changed in this release, `docker build -t darkmux-runtime:latest runtime/` from the release commit (darkmux prefers a local `:latest` over a pull). If `runtime/` is unchanged, the prior release's image is byte-identical — `docker tag <prev-runtime-image> darkmux-runtime:latest` and remove the tag after.
- **A trivial message is the FLOOR, not the gate.** It tests the *path*: pass = the container ran and the loop executed (`result: "stop"`, or any non-125 / non-pull-miss outcome); fail = exit 125, an image-pull miss, or an immediate error before the loop. But a path-pass is necessary, not sufficient — see the next bullet.
- **Verify the FEATURES live (the actual gate, per the mandate above).** For each feature-bearing change in the release, run a real dispatch that EXERCISES it and confirm the expected behavior against ground truth — `lms ps` (model/context loaded), the flow records (fields emitted), the envelope (output shape), the served viewer (rendered correctly), `darkmux doctor` (the check fires). The trivial smoke would have passed #1135 while the feature was broken; the feature check is what catches it. Viewer-only and failure-path features that a happy dispatch can't exercise (e.g. an error-path field) get verified by their own targeted reproduction, not skipped.
- **Name the loaded model** (see Anti-patterns) — a runaway or garbage *response* is a model finding, not a dispatch-path failure; the path passed if the container ran and the loop executed.
- **Standard coverage pair (operator, 2026-06-29): a coder long-agentic dispatch + a PR review.** These two real dispatches give broad real-AI coverage across the system in one cheap pass: (1) `darkmux lab run long-agentic` on the **default** coder profile (the pepper-grinder fixture) exercises dispatch → container spawn → tool-loop → compaction → verify; (2) a PR review (`gh workflow run darkmux-review.yml` / a `dispatch pr-reviewer`) exercises the reviewer → `mission launch review` render → post path. PR review is usually already covered by the session's own per-PR QA; the coder run is the piece to add. A coder run that doesn't *converge* is a model finding, not a path failure — the gate is that the dispatch+verify machinery ran end-to-end and produced an envelope, not that the model passed the tests.
- Composes with the pre-PR dual-QA discipline: per-PR QA catches logic bugs; the pre-release dogfood catches integration / critical-path regressions only a real container run reveals. The `darkmux-point-release` skill's preconditions should include this smoke step.

## Loop policy — recheck vs rethink (escalate, don't re-ask)

When a dispatch's output needs verification, **re-asking the same agent to re-check its own work in its own context is near-worthless.** The Self-Verification Dilemma (arXiv 2602.03485) measured that the vast majority of an agent's self-rechecks are *confirmatory*, not corrective — the agent re-derives and entrenches its original answer. Correction value comes from cross-context **re-thinking** by a *different*, ideally higher-tier reviewer.

Codified policy (not orchestrator discretion):
- **Invariant-bearing or security-bearing diffs → escalate to a fresh-context / higher-tier (frontier) review.** Never sign off on the dispatching agent's own self-recheck for these. Lived at the s3 gate: a coder's 271/271 tests + clippy were all confirmatory of its own broken work; only the fresh-context frontier review caught the four regressions (same shape as #975).
- The escalation **raises the review tier; it never lowers the gate** (operator sovereignty #44). Hygiene-only diffs may stay at the local tier.
- Pairs with #799 (terminate on a verifiable mechanical check, never self-assessment) and the persisted-corrections brief injection (#849 half 1 — a correction made once is carried into the next brief, not re-derived).

## No blind runs — instrument before you measure (operator mandate, 2026-07-09)

**darkmux exists to observe local-AI work. A darkmux run that cannot be observed refutes the product.** This is the recursive success criterion applied to the project's own development: if operating darkmux means watching `tail -f` and `lms ps`, the observability claim is failing at home — and every gap felt while operating darkmux is a P0 feature request, not an inconvenience to work around.

**The rule: no measurement-grade run launches until its observability surfaces exist.** A run whose only yield is a verdict line is a wasted run — the DATA is the product. Before any multi-hour or decision-bearing run, verify:

1. **Per-event records stream to durable per-run-local files as they happen** — never end-of-run-only writes. A killed run keeps everything completed so far (per-case envelope streaming + `funnel-events.jsonl`, #1248).
2. **Host telemetry samples alongside the work** (cpu/ram/load at ~2s cadence) so "when did it slow down and what else was the machine doing" is answerable from the artifact, not reconstructed from another tool's server logs (#1247).
3. **The knob config is snapshotted into the artifact** (resolved staffing/model/k/max_tokens — `FunnelEnvelope.staffing`), so every run is self-describing for later series comparison.
4. **A live observing surface is available** (the lab view when it lands; at minimum a live-tailing event file) — the operator must be able to SEE the run, not infer it.

If a surface on this list doesn't exist for a new run type, **building it comes before the run**. Observability work precedes measurement work in priority; it is not polish.

**Origin (2026-07-09, Phase B validation day):** a full day of funnel validation ran blind — a heavy corpus run was killed after case 1 and lost its entire envelope (end-of-run-only artifact writes); a ~10–15% inference slowdown from concurrent builds was invisible until reconstructed forensically from LMStudio's own server logs; overnight runs were nearly launched whose total observable yield would have been seven console lines. Operator: *"darkmux is fully designed to observe everything and we aren't... No data, no ability to pinpoint when things got slow because of another process. Make it doctrine or this whole project won't work."*

Composes with: single-run-full-picture-first (verify a system with ONE complete instrumented run before corpus sweeps), smoke-before-long-runs, quiesced-machine for canon runs (until host sampling ships, measurement runs get no concurrent builds), and the lab-vs-fleet boundary (bench records stay per-run-local; engagement records ride the flow stream).

### The observer must not join the observed (operator lesson, 2026-07-10, #1286)

Observing local-AI work must not perturb it. The prior art is the AMD/OpenGL stats-render paradox: on-screen debug charts could only be drawn by the very graphics engine being measured, so *rendering the stats made the stats worse* — one line of provenance for a system-design requirement, not an optimization. Getting the numbers, and displaying them, has to happen OUTSIDE the measured system. Four binding constraints on every darkmux observability path (the memory ledger + `#lens=machine` are the first consumers):

1. **Observability paths contain ZERO model dispatches.** A measurement path reads kernel counters (`vm_stat`, `sysctl`, `ps`) and `lms` metadata only — zero tokens, zero Metal work. Using the LLM to observe the LLM (e.g. a utility agent summarizing stats mid-run) is the forbidden pattern; it is the modern form of rendering charts with the measured engine.
2. **The display renders off-machine by design.** The serve daemon emits JSON; chart-rendering cost lands on the CLIENT — the phone over the tailnet, another machine, any browser that isn't the measured host. Watching a canon run from the measured host's own browser is the anti-pattern (a Chrome tab is a real RAM/CPU consumer); the quiesced-machine doctrine extends to *watch measurement-grade runs off-box*.
3. **Samplers/gatherers stamp their own cost into the artifact/payload.** The gather records its own wall-clock (`gather_ms`); a host-telemetry sampler records its own CPU time alongside the samples — so "the observer was negligible" is a VERIFIABLE claim in the data, not an assumption. We already measured this failure class from the other side: concurrent cargo builds taxed judge throughput 10–15%, invisible until reconstructed forensically. The observer must be provably not that.
4. **Cadence is a recorded knob, never adaptive-silent.** The sampling interval / cache TTL is written into the payload (`cache_ttl_ms`) at its default (~2s); if someone tightens it for a debug session, the artifact says so.

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

1. **Profile uniformity** — a profile means the same thing to every consumer (swap, dispatch,
   crews, benches). A consumer may not legislate which profiles are legal; it routes on what
   the profile declares (local vs endpoint → dialect, cycling, token accounting).
2. **Dispatch liveness** — any production code path that performs model work emits
   `dispatch.start` and a terminal `dispatch.complete`/`dispatch.error` (RAII-guarded on all
   exit paths), regardless of what richer vocabulary it also emits. Liveness surfaces key on
   these bookends plus presence (#857); new vocabularies supplement, never replace.
3. **Lab/fleet sink boundary** — lab runs write per-run-local artifacts; the fleet flow
   stream carries engagement work only. No crossings in either direction.
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
   documented semver rules; consumers are lenient-on-read, loud in doctor.
6. **Frozen model-facing text** — measured prompts/personas live in ONE artifact with golden
   tests generated from the reference implementation; assembly and request bodies are
   byte-locked (#1256). "Frozen" means one hash, not one intention.
7. **Config leniency** — registries and config files are lenient-on-read; semantic validation
   lives at resolution/consumption time and in `darkmux doctor`, never on the hot load path
   (#1269).

Enforcement is structural, not procedural: every contract gets a conformance test where one
is expressible (golden files, emission-sequence assertions, boundary tests), and every review
of a new subsystem asks explicitly: **which contracts does this touch, and where is its
conformance shown?** A deliberate scope cut that fences a contract (as crews-local-only did)
is itself a contract change — it gets the same failure-mode scrutiny as a feature, because to
the operator's config file, it is one.

## Configuration (`config.json`)

darkmux's canonical config surface is **`~/.darkmux/config.json`** (#661), written by `darkmux init`. Every setting resolves with one precedence — **`env(DARKMUX_*) > config.json > built-in default`** — and that precedence lives in exactly ONE place: `darkmux_types::config_access` (the env tier is read **live per-access**, so a `set_var` in a test or a power-user export still wins). A reader never has to wonder where a setting came from; `darkmux doctor` surfaces the resolved value + source.

**The file is self-documenting by design.** `init` writes the common knobs *visible* (not hidden as code-defaults), so the operator tunes the file, not the source. Off-by-default integrations are **feature blocks gated by an `enabled` field, not by field-presence** — `init` writes the whole block with `enabled: false` and the sub-defaults populated, so the surface is discoverable and one flip from on:

```json
{
  "schema_version": "1.2",
  "machine_id": "studio",
  "orchestrator": "",
  "lms_bin": "lms",
  "lmstudio_url": "http://localhost:1234",
  "redis":   { "enabled": false, "host": "127.0.0.1", "port": 6379, "stream": "darkmux:flow", "maxlen": 10000 },
  "audit":   { "enabled": false, "dir": "~/.darkmux/audit" },
  "runtime": { "inactivity_timeout_seconds": 600, "strict_selection": false, "feedback_injection": true, "check_updates": true },
  "remote":  { "max_tokens_per_execution": 500000 },
  "fleet":   { "mode": "standalone" }
}
```

When proposing a config change to an operator, write the visible field; don't reach for an env var as the primary mechanism. The mechanism is **`darkmux config set <key> <value>`** (#937) — it validates the dotted key (a typo is surfaced with a suggestion, never silently written) and coerces the value to the field's type; `darkmux config get <key>` / `darkmux config list` read it back (`darkmux doctor` shows the fully *resolved* value with env/config/default provenance). **Secrets are NOT config** — `config set` refuses the known secret keys (Redis password, serve token) and points at the `security add-generic-password` Keychain form. **Deliberately NOT written by `init`** (because a literal would be wrong, not because they're hidden): `dirs.*` (derived from the root — `darkmux doctor` shows the resolved path) and caps like `runtime.max_turns` (absent = uncapped, a real behavior).

**Carve-outs — the ONLY things NOT plaintext config:**
- **Redis password → macOS Keychain** (item `darkmux-redis`, the same item the Homebrew wrapper populates). `config.redis` holds only non-secret bits (`enabled`/`host`/`port`/`db`/`stream`/`maxlen`); the password is read at runtime via `security find-generic-password` and never logged — every URL is wrapped in `RawRedisUrl` (redacted `Display` + `Debug`; raw bytes only via `expose_for_probe`). Non-macOS uses the full-URL env override. `redis_url()` resolves `env(DARKMUX_REDIS_URL) verbatim > config.redis.enabled + Keychain > off`.
- **Serve-daemon bearer token → macOS Keychain** (item `darkmux-serve-token`) — #881, same carve-out shape as the Redis password. `config.runtime` holds only the non-secret `daemon_auth_enabled` gate; the token is read at runtime via `security find-generic-password`, wrapped in `RawServeToken` (redacted `Display` + `Debug`; raw bytes only via `expose_for_compare`), and lives in `darkmux-flow` beside the Redis-secret machinery. `serve_token()` resolves `env(DARKMUX_SERVE_TOKEN) verbatim > daemon_auth_enabled + Keychain > off`. Auth is *active* iff a token resolves; a non-loopback `--bind` is refused without one, and remote reads + `/diff` then require `Authorization: Bearer <token>` (loopback stays open).
- **`DARKMUX_HOME`** — the bootstrap pointer that *locates* the config root (`<root>/config.json`); it can't live inside the config it finds, so it stays an env var.

**Schema is minor-bump + lenient on read** (all-`Option` + `#[serde(flatten)] extras` overflow): an older binary tolerates a newer config, and a partial/hand-edited/malformed config never bricks the CLI — loud validation belongs to `darkmux doctor`, not the hot load path. `CONFIG_SCHEMA_VERSION` lives in `darkmux-types/src/config.rs`.

**Don't confuse `config.json` with the profiles registry.** `~/.darkmux/profiles.json` (the swap profiles) is a SEPARATE file, overridden by `--profiles-file` / `DARKMUX_PROFILES` — **renamed in #661 from the misleading `--config` / `DARKMUX_CONFIG`** (those names are retired, not reused, because a real `config.json` now exists).

## Environment variables

Every `DARKMUX_*` var below is the **top tier** of `env > config.json > built-in default` — it wins live, and each maps to a `config.json` field (mapping after the table). Use env for per-shell/CI/test overrides; use `config.json` for durable operator config. Flow records carry per-record provenance fields auto-populated from these at write time. `darkmux doctor` surfaces what each resolves to.

| Variable | Default | Effect |
|---|---|---|
| `DARKMUX_MACHINE_ID` | hostname | Logical fleet name **stamped at record-write time** on every new flow record. Operator-named (`studio`, `mini-1`) reads better in the topology view than DNS-style hostnames. Pre-1.4.0 records lack the field (which the viewer renders as `unknown`). |
| `DARKMUX_ORCHESTRATOR` | unset → field omitted | Frontier orchestrator driving this session (e.g. `claude-code`, `antigravity`, `cursor`), **stamped at record-write time**. **Operator-explicit by design** — there's no reliable way to auto-detect the frontier model from inside darkmux. Doctor warns when unset. |
| `DARKMUX_FLEET_MODE` | `standalone` | The machine's declared fleet position — `standalone` (single machine), `hub` (the always-on coordinator; supervises its own Redis per #936), or `peer` (points at a hub). Operator-**declared** under `config.fleet.mode`; detection is only a doctor cross-check (declared ≠ observed), never the source of truth. `darkmux doctor` shows it with provenance and flags an unrecognized value (treated as `standalone`). Downstream fleet features key on it (#933). |
| `DARKMUX_FLOWS_DIR` | `~/.darkmux/flows` | Where the per-day JSONL files live (LocalFileSink — casual write target). |
| `DARKMUX_AUDIT_DIR` | unset → AuditFileSink off | When set, flow records ALSO write to a hash-chained tamper-evident per-day JSONL under this directory (AuditFileSink, #163). **POSIX-only** (Linux/macOS — Windows is unsupported; the env var is recognized but the sink is skipped). Cross-process safe via `flock(2)`. `darkmux flow integrity-check` walks the chain and **exits with status 2 on any chain break** so cron/CI can flag tampering. `darkmux doctor` rolls up the same result. Compliance-strength substrate (ISO 27001, AI Act, HIPAA-as-covered-entity). |
| `DARKMUX_REDIS_URL` | unset → Redis off | When set, flow records also XADD to the Redis stream (coordination substrate; not the audit substrate). Combined with `DARKMUX_AUDIT_DIR` produces the canonical compliant composition: `TeeSink([LocalFile, Audit, Redis])`. See [#162](https://github.com/kstrat2001/darkmux/issues/162) Phase 3. |
| `DARKMUX_REDIS_STREAM` | `darkmux:flow` | Override the Redis stream name. |
| `DARKMUX_REDIS_MAXLEN` | `10000` | Approximate retention cap for the Redis stream (`XADD MAXLEN ~ N`); `0` for unbounded. |
| `DARKMUX_INACTIVITY_TIMEOUT_SECONDS` | `600` | Per-dispatch inactivity budget. The host-side watchdog **hard-kills** the container at 100% if no proof-of-work signal lands. The runtime-side detector fires a **soft warning** at 75% (model-facing nudge to wrap up gracefully or escalate via `BLOCKED:`); both reset on the same proof-of-work signals (any tool.completed, any compaction). A productive dispatch never sees either; a stuck one gets the soft chance before the hard kill. Pathological tool patterns are caught by their dedicated detectors (cycle, cascade, edit-drift, reasoning loops) — the deadline trusts activity; the detectors catch struggle. (#457 + #464 + #466; renamed from `DARKMUX_RUNTIME_DEADLINE_SECONDS`) |
| `DARKMUX_MODEL_LOAD_TIMEOUT_SECONDS` | `600` | Bounded model-load/unload phase for gestalt host-port calls (#1276). The `LmsHost` adapter spawns `lms load`/`lms unload`, polls the child, and **hard-kills it at expiry**, returning a typed timeout naming the phase — a wrong model id (or a load waiting on anything interactive) can no longer hang a dispatch until the workflow's outer kill. Resolved into the mandatory `Deadline` every `ModelHost` mutation takes; distinct from the *inactivity* budget above (that one meters dispatch proof-of-work; this one meters a single host load/unload call). |
| `DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION` | `500000` | (#1260/#1177) Per-EXECUTION remote token allowance for endpoint-staffed crew seats, where an execution = one pipeline stage. **Metered paths (1.18.0 scope):** the review funnel's remote seats (probe pass, each judge pass, the verify pass) AND the tool-less single-shot remote `dispatch` path. **NOT yet metered:** the AGENTIC-remote container path (#1187 — a tool-granting role on an endpoint profile, multi-call container loop); that follow-up is tracked separately. Only REMOTE (hosted-endpoint) calls draw from it — local seats are unmetered by it. When a stage exhausts its bucket, that stage's remaining remote calls stop with the reason named in the envelope: a load-bearing JUDGE stage exhausting is an honest DEGRADED run; a VERIFY stage exhausting degrades only the STAGE (findings already verified still post; skipped confirms keep the manual-verification marker; a loud warning names it); probe exhaustion is a reduced-coverage warning and the run continues. A remote-JUDGE dispatch FAILURE (surviving bounded retries) is likewise a degraded run. Setting it to `0` refuses all remote calls with a typed error (a hard opt-out). Tokens only, never currency. |
| `DARKMUX_SERVE_TOKEN` | unset → loopback-only | The serve daemon's bearer token (a **secret** — env override; else the macOS Keychain item `darkmux-serve-token`, gated by `runtime.daemon_auth_enabled`). When a token is set the daemon may bind non-loopback and **remote reads + `/diff` require** `Authorization: Bearer <token>` (loopback stays open so the local viewer keeps working). When **unset**, a non-loopback `--bind` is **refused** (a loopback daemon needs no token). `machine list --deep` sends this token to peers — use the **same shared token** on every machine in the fleet. (#881) |

**env → `config.json` field** (the override-tier var → its durable config home):

| Env var | `config.json` field |
|---|---|
| `DARKMUX_MACHINE_ID` | `machine_id` |
| `DARKMUX_ORCHESTRATOR` | `orchestrator` |
| `DARKMUX_FLEET_MODE` | `fleet.mode` |
| `DARKMUX_LMS_BIN` / `DARKMUX_LMSTUDIO_URL` | `lms_bin` / `lmstudio_url` (base URL; callers append `/v1/...`) |
| `DARKMUX_FLOWS_DIR` / `DARKMUX_NOTEBOOK_DIR` / `DARKMUX_CREW_DIR` / … | `dirs.flows` / `dirs.notebook` / `dirs.crew` / … |
| `DARKMUX_AUDIT_DIR` | `audit.dir` (gated by `audit.enabled`) |
| `DARKMUX_REDIS_URL` (verbatim, password inline) | `redis.{enabled,host,port,db}` + Keychain password (assembled) |
| `DARKMUX_REDIS_STREAM` / `DARKMUX_REDIS_MAXLEN` | `redis.stream` / `redis.maxlen` |
| `DARKMUX_INACTIVITY_TIMEOUT_SECONDS` | `runtime.inactivity_timeout_seconds` |
| `DARKMUX_MODEL_LOAD_TIMEOUT_SECONDS` (#1276 — bounded host load/unload phase) | `runtime.model_load_timeout_seconds` |
| `DARKMUX_RUNTIME_MAX_TURNS` / `DARKMUX_RUNTIME_MAX_TOKENS` | `runtime.max_turns` / `runtime.max_tokens` |
| `DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL` (#1221 — per-CALL completion cap, reasoning + content of one turn; unset = the runtime's built-in 10000, which truncates productive reasoning on thinking-family models) | `runtime.max_tokens_per_call` |
| `DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION` | `remote.max_tokens_per_execution` (#1260) |
| `DARKMUX_STRICT_SELECTION` / `DARKMUX_CHECK_UPDATES` | `runtime.strict_selection` / `runtime.check_updates` |
| `DARKMUX_FEEDBACK_INJECTION` | `runtime.feedback_injection` exists, but is read **directly in the runtime container** (`runtime/src/feedback.rs`), NOT through `config_access` — so it does NOT yet honor the `config.json` tier (the runtime crate can't depend on `config_access`; wiring it needs a flag-plumb, deliberately deferred in #661). |
| `DARKMUX_DEFAULT_ROLE` / `DARKMUX_DAEMON_CORS_ORIGINS` | `runtime.default_role` / `runtime.daemon_cors_origins` |
| `DARKMUX_SERVE_TOKEN` (verbatim, the secret) | — (a **secret**; macOS Keychain item `darkmux-serve-token`, whose read is gated by `runtime.daemon_auth_enabled`). Only the non-secret gate lives in `config.json`; the token never does — same carve-out as the Redis password. |
| `DARKMUX_HOME` (bootstrap pointer) | — (locates the config root; can't live in config) |
| `DARKMUX_PROFILES` (profiles registry, **renamed from `DARKMUX_CONFIG`**) | — (a separate file, not `config.json`) |

The internal runtime is the only dispatch path and needs no external binary. (Historical: a `DARKMUX_RUNTIME_CMD` env var, then a per-dispatch `--runtime-cmd <path>` flag, once let operators point `dispatch` / `lab run` at an external openclaw/Aider/Cline shell-out. That whole path — the flag, the openclaw runtime, and `darkmux crew sync` — was removed on the 2.0 track; see [#1405](https://github.com/kstrat2001/darkmux/issues/1405).)

When working on darkmux from a Claude Code (or other frontier) session, export `DARKMUX_ORCHESTRATOR=<harness-name>` in the shell so flow records carry orchestrator provenance. This is part of the cultivation discipline tracked in [#130](https://github.com/kstrat2001/darkmux/issues/130).

## Where things live

```
src/
  main.rs                    CLI dispatch (clap)
  types.rs                   Profile / ProfileRegistry / ProfileModel
  profiles.rs                Registry loader + lookup
  swap.rs / lms.rs           Stack swap orchestration + lms CLI wrapper
  init.rs / skills.rs        `darkmux init` + skill installer
  notebook.rs                Notebook draft generator
  lab/
    paths.rs                 Workspace dir resolution (project vs user) + default_registry_path
    run.rs                   Workload dispatch + source-sandbox resolution + manifest fixture-enrichment
    inspect.rs               Single-run analysis
    compare.rs               Run-vs-run diff
    list.rs                  Recent runs table
    cow_clone.rs             Per-run copy-on-write sandbox clone (#487 phase 1)
    sandbox_hash.rs          BLAKE3 content hashing (baseline_hash / final_hash)
    fixture.rs               .fixture.json manifest type + validation (#487 phase 2)
    registry.rs              Fixture registry load/save/find_satisfying (lab-registry.json)
    fixture_cli.rs           lab register/unregister/fixtures verbs (#487 phase 4)
    doctor.rs                lab doctor — offline fixture integrity check
  workloads/
    types.rs                 WorkloadProvider trait + manifest types (incl. requires_fixture)
    load.rs                  Manifest loading (user → on-disk → embedded)
    registry.rs              Provider registry
  providers/
    prompt.rs                Trivial single-prompt provider
    coding_task.rs           Sandbox + verify-command provider (writes final_hash)
  crew/
    types.rs                 Role + Skill + Capability schema; capabilities() derivation; is_specialist()
    select.rs                select_model(role, profile) — dispatch model selection (phase-1 stub, E14)
    dispatch_internal.rs     Internal-runtime dispatch path (typed-config consumer)
    dispatch.rs              Runtime-neutral dispatch plumbing (ack gate, identity augmentation, flow-record builders, fleet routing)
runtime/                      Internal-runtime crate (built into darkmux-runtime Docker image)
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
  roles/                      Crew role library (manifest + .md) embedded at compile time
  skills/                     Skill library embedded at compile time (work-shape descriptors with keyword routing; renamed from `capabilities/` in refactor 0 — see #448)
  workloads/                  Workload manifests embedded at compile time
  lab-fixtures/               Built-in lab fixtures (e.g. demo-tiny-py) registered via scripts/lab-init.sh
  recommendations/            Tier-aware recommendation registry
  AUTONOMOUS_DISPATCH_PREAMBLE.md  Injected ahead of specialist-role dispatches (#427)
scripts/lab-init.sh           Standalone fixture-registry bootstrapper (NOT a CLI verb; #487 phase 5)
skills/darkmux-<name>/        Agent-invokable skill wrappers
tests/cli.rs                  Integration tests (spawn the binary)
```

## Conventions to follow

- **Don't add dependencies casually.** The dep set is deliberately small (`anyhow`, `clap`, `serde`, `serde_json`, `dirs`). A 10-line inline module beats a crate for small one-off needs (see `mod pathdiff` in `src/providers/coding_task.rs`).
- **Trait providers, not feature flags.** New workload kinds go through the `WorkloadProvider` trait in `src/workloads/types.rs`, registered in `src/workloads/registry.rs::register_builtins()`. Don't bolt new behavior into the lab orchestrator.
- **Manifests are JSON.** Workload manifests, profile registries, run manifests — all JSON. The repo briefly used YAML; that switch is done. Don't reintroduce YAML.
- **Tests over prints.** Mutating-state tests (cwd, env vars) need `#[serial_test::serial]` to avoid races. Integration tests in `tests/cli.rs` use `assert_cmd` to spawn the binary.

## StepKind tiering — physical enforcement (#1352)

Mission work runs as `Task`/`Step` graphs (`darkmux-crew`'s `scheduler`), and a `Step`'s `kind` field resolves to a registered Rust implementation of the `StepKind` trait. #1230's redesign arc grew nine of these in one pass — much faster than this codebase's own precedent for an extension point (`WorkloadProvider` stayed at three implementations across a long history) — which is exactly the "hard-wire every use case" failure mode this project exists to fight at the model-orchestration layer, recurring at the code-extension layer instead. #1352 stopped that drift with a real decision procedure, enforced PHYSICALLY (a directory a fresh session can read, not a rule that only lives in a paragraph and gets skipped under time pressure):

**The test:** a new `StepKind` is justified only when the CONTROL FLOW itself is genuinely new — not when only the DATA differs (that's config), and not when only the internal ALGORITHM differs while the outer procedure shape stays the same (that's a pluggable strategy inside an existing generic kind, not a new type).

**The three physical locations, and what each one means:**

```
crates/darkmux-crew/src/step_kinds/
    builtins.rs   — Tier 1: generic, config-driven, no new control flow.
                    dispatch.internal, dispatch.single_shot,
                    procedural.shell, procedural.noop. THE DEFAULT — check
                    here first, always, before writing new code.
    patterns/     — Tier 2: a genuinely new, reusable control-flow SHAPE,
                    with the domain-specific ALGORITHM plugged in as a
                    caller-supplied strategy (deliberately NO runtime
                    name-keyed strategy registry yet; dedup.rs's module
                    doc names the upgrade path for when a second strategy
                    needs runtime selection). multi_pass_confirm.rs (the
                    pass-1 → conditional confirmation passes → demote-on-
                    disagreement shape, generalized from the PR-review
                    judge; pass count + confirm rule are parameterized,
                    the demotion rule is currently fixed — a known,
                    documented narrowing of #1352's spec, widen when a
                    consumer needs a different demotion). dedup.rs (the
                    "scan for the first survivor a candidate collapses
                    into, per a pluggable match/merge strategy" procedure,
                    generalized from the PR-review dedup stage). Neither
                    submodule depends on any mission's own types, which is
                    what keeps a Tier 2 pattern actually reusable rather
                    than one mission's code with extra ceremony.
    types.rs      — the StepKind trait itself.
    registry.rs   — StepKindRegistry.
```

Tier 3 — genuinely bespoke, single-purpose kinds — **never lives in `darkmux-crew` at all.** It stays physically co-located with the mission module that owns it: the PR-review pipeline's bundle/probe/dedup/judge/verify/synthesis kinds live in `crates/darkmux-lab/src/lab/review.rs`; `mission run`'s worktree/coder/verify kinds live in `src/mission_run.rs`. This is reserved for when a second plausible use case genuinely isn't visible yet — revisit if one shows up, same as any other "not yet, but named" call.

**The physical location IS the enforceable test.** Is this in `step_kinds/builtins.rs`? Config it. Is it in `step_kinds/patterns/`? Reuse it, plug in your own strategy. Is it inside a mission's own module? It's bespoke on purpose — don't look here for shared infrastructure. A fresh agent session asking "where does my new Step behavior go" answers the question by reading the directory, not by re-deriving the decision procedure from a comment that may have drifted.

Two audited findings worth knowing before proposing a collapse yourself. First: the PR-review pipeline's probe/verify kinds LOOK like `dispatch.single_shot` wearing bespoke wrapping, but audited honestly they are NOT a clean Tier 1 collapse. Each is a whole per-item LOOP (probe's bundle × k-draw loop, verify's per-confirmed-flag loop) with cross-step shared state (a remote-token bucket shared across sibling probe steps, `MemberRecord` accumulation into a shared handle) that `dispatch.single_shot`'s one-call-per-`Step` shape doesn't have and can't gain without a real behavior/envelope change. Second: `mission run`'s coder kind wraps the SAME `crew::dispatch::dispatch` primitive Tier 1's `dispatch.internal` wraps, a genuine follow-up candidate, but its CLI printing, its own `mission.coder` flow-record vocabulary, and its `result_slot` readback mechanism are real differences a collapse would have to resolve first. Both are documented in place (code comments citing #1352) rather than forced. The general rule: a collapse that changes observable behavior isn't a tiering fix, it's a feature change wearing a tiering fix's clothes.

## Versioning — rules schema

The `eureka` rules engine versions its emitted definitions (`RuleDef`s) with plain semver applied to the rules **data shape** (not to darkmux itself). `RULES_SCHEMA_VERSION` lives in `crates/darkmux-eureka/src/lib.rs` as a single constant.

**Scope today: engine-internal + `darkmux doctor`.** The RuleDefs are consumed in-process and surfaced by `darkmux doctor`. There is **no viewer consumer yet**: the `instruments.jsonl` sidecar was retired (#557), the flow-stream transport that would carry RuleDefs to the viewer is unbuilt (#657), and the viewer-side rules validation is unbuilt (#12). So there is currently **no viewer-blocking behavior and no `EXPECTED_RULES_SCHEMA_MAJOR` constant** (the old `docs/viewer/index.html` is a redirect stub — it does not hold viewer code). The semver discipline below governs the data shape for when that transport lands.

| Bump | Meaning |
|---|---|
| **Patch** (`1.0.0` → `1.0.1`) | Fully backward-compatible — a message fix, a threshold tweak that doesn't change semantics, a typo in a `fix_hint`. |
| **Minor** (`1.0` → `1.1`) | Additive — a new rule `kind`, a new optional field on `RuleDef`. A future consumer can SAFELY IGNORE what it can't yet evaluate. |
| **Major** (`1.x` → `2.0`) | Breaking — rename/retype a field, change the `RuleKind` enum encoding, a new required field. |

Rule of thumb when changing the schema:

- Adding a new rule? **Minor bump.**
- Renaming or retyping a field on `RuleDef`? **Major bump.**
- Fixing a typo in `fix_hint`? **Patch bump.**

When the viewer consumer lands (#657 transport + #12 viewer rules validation), this section is where the major-bump UI contract (block stale data, prompt to update) gets defined and the viewer-side version gate gets added in the same PR. Until then there is nothing on the viewer side to bump.

## Common tasks for an agent

If a user asks you to:

| Ask | Do |
|---|---|
| "add a new workload" | Drop a JSON manifest at `templates/builtin/workloads/<id>.json`. If it's a `prompt` workload, register it in `EMBEDDED_WORKLOADS` in `src/workloads/load.rs`. coding-task workloads need a sandbox seed dir and CAN'T be embedded. |
| "add a new provider" | Implement `WorkloadProvider` in `src/providers/<name>.rs`, register it in `src/workloads/registry.rs::register_builtins()`. |
| "add a lab fixture" | Create a dir with a `.fixture.json` manifest (`name` required; `satisfies`, `verify_command`, `required_files` optional), then `darkmux lab register <path>`. A workload binds to it via `requires_fixture: "<name>@<version>"`. Built-ins live under `templates/builtin/lab-fixtures/` and register via `scripts/lab-init.sh`. |
| "check fixtures are healthy" | `darkmux lab doctor` — offline check that registered paths exist, manifests load, required files are present, and content hashes haven't drifted. |
| "run the smoke test" | `cargo install --path . && darkmux lab run quick-q`. Should complete in ~6-10s if a model is loaded. |
| "list notebook entries" | `darkmux lab notebook list` (optionally `--machine <id>` to filter). Enumerates `.md` files, parses headers. (#1426 — the notebook family folded into `lab`.) |
| "draft a notebook entry" | `darkmux lab notebook draft <run-id>` (optionally `--machine <id>` to override). |
| "make the build self-contained" | Already is — `include_str!` for embedded workloads, no external assets needed at runtime. |
| "review the diff before commit" | Run the suite (`cargo test`), eyeball `git diff`, propose a commit message — but **do not commit unless explicitly asked**. |
| "check the mission board / housekeeping" | `darkmux mission status` (#829) — the global mission-control read: every mission grouped by status with phase progress + the drift that needs attention (a Closed mission with a non-terminal phase; an open mission whose phases are all done) + copy-pasteable reconcile commands. READ-ONLY — surfaces + suggests, never mutates; the operator/you run the suggested `phase complete`/`phase abandon`/`mission close`. `--json` for programmatic consumption. **Run it as session-start housekeeping** (and before opening PRs / wrapping a work arc) so mission↔phase drift gets caught structurally rather than by memory — and so gh/jira stay reconciled off the same cue. The CLI twin of the viewer's missions lens (#827). |
| "leave an orchestrator note on the dashboard" | `darkmux flow note --text "<note>" --source orchestrator` (#807) — the savings hero renders the latest tagged note verbatim as its "Orchestrator note:" conclusion (procedural template is the fallback), and `history →` lists the window's notes. **Voice (operator-specified): 1–2 upbeat, plain-language lines — what the crew got done + keep-going energy. No jargon, no file paths, no verdict prose. This is encouragement infrastructure, not a changelog.** Emit one after a mission ships or a work arc wraps. TOKENS-ONLY discipline applies (no currency). Technical gate reasoning goes to the SEPARATE audit-trail channel instead: `darkmux flow note --session-id <sid> --text "<verdict · what you overrode · why>" --source adjudication` (#817) — session-scoped, never rendered on the hero card. |

## Things to ASK before doing

- Anything that mutates `~/.darkmux/profiles.json` — that's user state.
- Anything that runs a real lab dispatch or a dispatch that loads models — uses real LMStudio resources.
- Anything that does `git push` or `git commit --amend` — irreversible-ish.
- Adding external runtime dependencies — has knock-on effects on install size and license surface.

## Anti-patterns — failure modes observed on fresh sessions

Live findings from cross-machine testing (M1 Max Studio fresh-Claude session, 2026-05-11). Future fresh-Claude sessions: read these BEFORE acting on a user request that touches model selection, version compatibility, or issue tracking.

- **Don't assume models — read the profile registry first.** Models live in `~/.darkmux/profiles.json` (or wherever `darkmux profile list` reports). If an agent role needs a model and one isn't declared, **ask the user**; do NOT pick a model from the LMStudio catalog at random. Older gemma family in particular ("gemma-4-e4b", "Gemma 4 26b", etc.) is known to produce looping garbage that poisons sessions across model swaps — never default-assume gemma. If the user has saved a memory about model selection, that supersedes any inference you'd make from the catalog.

- **Don't silently roll back on regression.** If a feature appears to regress on an unfamiliar LMStudio version, **surface the finding to the user** with the version numbers you observed. Don't quietly revert config overrides "to make things work" — loud beats quiet. The user is debugging an unfamiliar env and needs the signal; a silent rollback hides the real bug.

- **Check existing issues before filing.** Before `gh issue create`, run `gh issue list --search "<keywords>"` (include closed issues with `--state all`) and skim. Duplicates clutter the project board and dilute the eureka-detection roadmap. Default to **commenting on an existing issue** over filing a new one. If you're not sure whether something is a dupe, **ask the user**; don't file-and-hope.

- **Empirical defaults are load-bearing, not decorative.** When choosing compaction modes, context windows, or compactor pairings, the shipped profile defaults (`default` mode beats `safeguard` for local; small dedicated compactor at ~68K cuts wall-clock substantially) reflect measured configurations, not arbitrary picks. Don't deviate from a profile's settings without acknowledging the empirical reason — the operator has chosen them deliberately.

- **Name the model-on-test when characterizing local-AI behavior.** darkmux uses a bake-off methodology to validate model hires per hardware tier — a documented head-to-head comparison with criteria written before the runs (tracked in the recommendation registry, [#159](https://github.com/kstrat2001/darkmux/issues/159)). But what's actually loaded in LMStudio at any moment may differ from the registry's pick — operators swap for reasons (debugging, A/B comparison, evaluating a new candidate, defensive escalation, or simply not having swapped back after a focused test). When you (the orchestrator) characterize behavior from a dispatch — *"the local layer's response was X"* — **know which model produced it**. `darkmux doctor` shows the active profile; `lms ps` shows the loaded models. If the loaded model differs from the recommended hire and the analysis is making generalizable claims about *the local layer*, name the model explicitly. Silent misattribution (analyzing dispatch outputs as if from the recommended model when they're actually from a reserve / candidate) inherits class-wide errors into every downstream claim. The architectural fixes are tracked as [#159](https://github.com/kstrat2001/darkmux/issues/159) (recommendation registry per hardware tier) and [#160](https://github.com/kstrat2001/darkmux/issues/160) (per-role `agent.model` pinning); this anti-pattern is the awareness layer until both ship. *Not restriction — operators have preferences and models evolve.* Just awareness, surfaced.

## Operator sovereignty (architectural principle)

The operator is the agent of intent. The system surfaces, suggests, records, and supports — but does not substitute its judgment for the operator's at any decision point. Every default is overridable; every automatic action is auditable; every suggestion is explainable.

Compressed to one rule: **the operator never has to wonder where a decision came from.**

This is the principle that ties the anti-patterns above to darkmux's grand vision. Anti-patterns are *don'ts*; the grand vision is the *why*; operator sovereignty is the *architectural principle* every new design decision should test against. When designing any new surface — CLI, config file, agent doctrine, file layout, data model — ask: *"does this leave the operator in the loop, with provenance and override?"* If yes, the design fits. If no, it doesn't — even when it would be more "efficient" or "smart."

Exemplars across darkmux's current surface:

- **Anti-patterns** — every rule is operator-sided (don't assume, don't silent-rollback, check before filing)
- **Preference fallthrough with provenance** — operator's intent at each layer; system never silently substitutes; unknown keys surfaced as typo warnings
- **Allocator 80/20** — algorithm proposes; operator stays in the 20% of decisions that matter; override is always available; allocator emits reasoning + alternatives + confidence for orchestrator audit
- **Confidence threshold per expertise** — operator self-rates per capability; system adjusts how often it asks vs decides
- **Role + Crew (not Team)** — composition is operator's call per mission; no fixed membership
- **JSON source-of-truth + SQLite derived index** — operator hand-edits any source file; system rebuilds derived state on demand; deleting the index is recoverable
- **Don't mutate user state without confirmation** — `~/.darkmux/profiles.json`, anything operator-owned. Read + propose; never write silently.
- **Namespace everything darkmux brings up in shared state** — LMStudio loaded models, anything else darkmux writes into a system other systems also use. Convention: LMStudio identifiers under `darkmux:<model-id>` (e.g. `darkmux:qwen3.6-35b-a3b`). Then darkmux's own state-mutating operations only touch the namespaced subset — user state is off-limits by construction, not by careful coding. The namespace is the contract.
- **Keyword vocabulary hybrid** — ship a starter; operator augments; system logs misses but never auto-mutates the vocabulary
- **Operator-tunable preferences are numeric scales, not hidden enums** — discoverable via example values; supports continuous tuning; UI-ready

The principle is recursive. It applies to documentation surface (this CLAUDE.md, READMEs), to CLI verbs, to data shapes on disk, to the architecture of future features. When a design decision feels like it should be made automatically by the system, that's the moment to surface it back to the operator instead.

Tracked as #44.

## Namespace convention (darkmux state in shared systems)

When darkmux maintains state in a system other consumers also use — LMStudio loaded instances, anything operator-managed — **darkmux-owned entries are namespaced** so they can be recognized at a glance and so darkmux's own state-mutating operations can scope themselves to only the namespaced subset. User state is then off-limits by construction, not by careful coding.

### Current namespaces

| System | Form | Example |
|---|---|---|
| LMStudio loaded identifier (visible in `lms ps`) | `darkmux:<model-id>` | `darkmux:qwen3.6-35b-a3b` |

(A previous namespace, `darkmux/<role>` for openclaw agent ids, was retired along with the openclaw shell-out path in #1405.)

### Why this matters

Without the namespace, darkmux's operations have to fall back on heuristics or persistent state files to know "did I bring this up, or did the user?" Heuristics are fragile (the user might happen to use the same naming convention); state files go stale (user force-quits, LMStudio restarts, manual unloads). The namespace IS the state — durable, visible, self-describing. If `lms ps` shows `darkmux:qwen3.6-35b-a3b`, that's a darkmux load and `darkmux machine eject` can unload it. If it shows `qwen3.6-35b-a3b` with no prefix, that's user state and darkmux leaves it alone.

### Transparency at dispatch time

When darkmux loads a model under `darkmux:<id>`, the underlying LMStudio model key is unchanged — `lms ps` shows `identifier=darkmux:foo, modelKey=foo`. Dispatchers calling LMStudio's chat-completion API with the bare model id `foo` still resolve via the `modelKey` match. **The namespace is invisible at dispatch time** — only visible to darkmux and operators inspecting `lms ps`. Existing dispatcher configs continue to work without migration.

### Conventions for new code

When writing a new feature that mutates LMStudio state on the operator's behalf:

1. **Generate the namespaced form** at the point of write. See `swap::namespaced_identifier`.
2. **Filter on the namespace** at the point of read/cleanup. See `swap::is_darkmux_owned`.
3. **Pass-through explicit overrides** — if the operator sets an explicit identifier in their profile, don't override it. The namespace is the *default*; the operator can opt out.

### Operator-facing commands

- `darkmux machine status` — list `lms ps` results grouped by ownership (darkmux-managed vs user state). Read-only.
- `darkmux machine eject [--dry-run]` — unload everything in the `darkmux:` namespace; never touches user state. Use to release darkmux's RAM footprint without disturbing other tools.
- `darkmux dispatch <role-id> <text>` — dispatch a single turn to the named role. Looks up the role manifest + `.md` system prompt, then runs the role through the **internal runtime** (per-dispatch `darkmux-runtime` Docker container, mounted workspace tempdir, in-house Rust agent loop with streamed flow records). Pass `--image <tag>` (#703) to dispatch into a specific environment: the default `darkmux-runtime:latest` is slim (python + node), but naming ANY Linux image (e.g. `rust:slim`, the operator's own CI image) makes darkmux **inject** its static runtime binary into that image (bind-mount + entrypoint override) so the coder runs in that environment and can `cargo check`/`test` in-sandbox — the inner verify loop. darkmux ships NO per-language images (it brings the agent; you bring the environment). The image needs `bash` + coreutils (debian/ubuntu-family work as-is; bare-alpine needs them added). **For Rust in-sandbox lint** (`cargo clippy`), name an image that includes the clippy component — `rust:latest` ships it; bare `rust:slim` may not, and a missing clippy slips lint to the frontier gate. The coder role makes one bounded `rustup component add clippy` attempt when cargo is present but clippy isn't (the single exception to its no-toolchain-setup rule), but the reliable fix is the operator's image choice — BYO-environment, so bring clippy if you want in-sandbox lint. Local dispatch only today (ignored on cross-machine `--machine`).

(A previous entry here, `darkmux crew sync` — reconciling an openclaw agent registry with the crew role manifests — was removed along with the openclaw shell-out path in #1405; the internal runtime reads role manifests directly, so there is no registry left to sync.)

Tracked alongside operator sovereignty (#44) and issues [#52](https://github.com/kstrat2001/darkmux/issues/52) (LMStudio namespace), [#55](https://github.com/kstrat2001/darkmux/issues/55) (full pre-flight checklist — partial coverage in `dispatch` today), and the `qa-review` migration that brought these verbs into the dispatch path.

## Model-facing prompt construction (AI-convention defaults + term provenance)

Local-AI models under clean dispatch context have no harness history. They can't ground darkmux-internal vocabulary by induction. Every model-facing prompt — role `.md` files, skill descriptions, the autonomous-dispatch preamble, workload prompts under `templates/builtin/workloads/`, feedback-injection templates, runtime-telemetry message wording — defaults to **AI-convention terminology** the model already recognizes from its training. When a darkmux-specific term is genuinely needed, **provide provenance** so the model can ground it.

### Convention defaults

- "the user" (not "the operator", "the human user") — the universal message-role term; "operator" is darkmux-internal vocabulary
- "system message" / "system prompt" — canonical for the system-role text
- "tool calls" / "function calls" — canonical for agent loops
- "the assistant" / "your previous turn" — self-referential canonical
- XML structure (`<example>`, `<context>`, `<instructions>`) over ad-hoc section headers when content is hierarchically structured — Anthropic-trained models recognize the convention; other major-family models parse it cleanly
- Markdown inline code (`` `cmd` ``) and triple-fenced code blocks for commands

### Provenance options for darkmux-specific terms

When a darkmux term genuinely must appear in a model-facing prompt (e.g., a verb name the model invokes, or a structural identifier present in the workload), attach provenance via one of:

1. **Tag/marker block** at first use: `<darkmux-term name="role">a stance + tool palette + system prompt for one dispatch</darkmux-term>` — the model parses the XML structure and binds the term to the definition
2. **Supplied conceptual definition** before first use, framed as inline context the model can bind to subsequent uses
3. **Self-identifying prefix** (e.g., `[darkmux-runtime]`) when speaking AS the runtime — the bracketed prefix is the provenance

### Audit surface

When reviewing a model-facing change, ask: *"what does this read as to a fresh-context model with no darkmux history?"* If a term doesn't ground in AI-convention OR have inline provenance, fix one or the other before shipping.

Applies to: role `.md` files, skill manifest `description` fields, the autonomous-dispatch preamble, workload prompts in `templates/builtin/workloads/`, feedback-injection templates (`runtime/src/feedback.rs`), future per-role feedback templates, runtime-telemetry message wording (e.g., `STALL_NUDGE_MESSAGE` in `runtime/src/loop_runner.rs`).

### Origin

Surfaced 2026-05-28 during PR #454/#455 iteration. Auditing the coder role prompt revealed darkmux-internal terms (*"the frontier"*, *"the operator"*, *"brief"*) that a clean-context model couldn't ground. Pairs with operator sovereignty (above) — the operator owns the dispatch intent; the role prompt is how that intent is communicated to the model; the communication has to land.

## Engagements (operator-defined dreamscapes)

An engagement is operator-defined, never system-defined. The system doesn't enumerate engagements, doesn't impose a directory shape, doesn't have an `engagement` config file format. The operator decides what's an engagement and how much to describe it.

An engagement can be:

- *"It's just a repo at `~/my-project`"* — one-line; the orchestrator uses the path
- *"I'm planning a 10-day Japan trip with a food focus"* — fuller context; the orchestrator may capture it in a `dreamscape.md` with tilts and constraints
- *"Our wedding site is at knot.com/our-wedding"* — engagement lives at a URL; not a local dir; the orchestrator notes the URL and maps planning sub-tasks to missions
- *"It's a Lovable.dev app I'm prototyping"* — hosted SaaS; the orchestrator references the workspace URL
- *"My personal training goal is sub-5-minute mile"* — life goal; the orchestrator captures the aspiration as missions
- *"I'm running a substack about local AI"* — long-form writing engagement; the orchestrator helps with drafts, editorial calendar, cross-post threading
- *"I'm authoring a book on systems engineering"* — multi-month writing project; the orchestrator scaffolds chapters and tracks research threads
- *"It's classified work I can't describe"* — the orchestrator respects opacity; engagement is named but content is operator-private
- Unwritten entirely — operator carries it in their head; the orchestrator works from conversation

If the operator is unsure what their engagement *is*, the orchestrator can offer a few of the above as starting shapes — picking a medium is itself one of the bridging moves the orchestrator is here to help with.

**The orchestrator's bridging role.** When working on a mission within an engagement:

- Read (or ask for) the engagement context — whatever form it takes
- Capture it durably as an `.md` if the operator wants — location is operator's call (engagement repo root, `de-lab`, a private notes file, etc.)
- Translate the soft free-form context into the structured concepts darkmux supports in code (Mission, Phase, role tilts, preferences) — proposing this translation when it'd help the operator move forward is the orchestrator's by-design job, not a thing to withhold
- Don't pry for structure the operator didn't volunteer — offer a suggestion once, let it land or get redirected, then drop it

Engagements should not be well-defined. They are open-ended dreamscapes where ideas are meant to flourish. darkmux supports the engagements it can support (local dirs, local code work) and stays out of the way for the rest (SaaS, hosted, conceptual, classified). The Rust-level data model in the schema PR (#45) names Role, Crew, Mission, Phase — concepts the system CAN model uniformly. Engagement isn't in that schema by design; it's the layer above where operator judgment lives.

This is operator sovereignty (above) applied at the project-shape level: the operator decides what their projects look like; the system doesn't impose a schema.

Tracked as #49.

### Engagement never enters CLI arg surface

Concrete doctrine that follows from the above: **engagement context lives in the frontier orchestrator layer (CLAUDE.md files, skills, conversation). It never becomes a `--engagement <hint>`-style CLI arg on any `darkmux` verb.**

Three reasons the rule is load-bearing:

- **CLI args quantize.** A `--engagement <hint>` field forces the operator to compress a dreamscape into a single string-token. *"wife time"* as a token is worse than *"this is my marriage time, not a work trip — focus on relaxation, no aggressive sightseeing"* threaded through the actual intent text. The frontier carries that nuance natively; the CLI surface cannot.
- **Utility agents are the wrong layer for engagement interpretation.** A 4B mission-compiler asked to *"interpret the operator's relationship to this engagement"* is the capability mismatch the utility-vs-specialist split (role-families, defined below) exists to prevent. Engagement nuance interpretation is judgment-bearing work that belongs to the frontier — never to a utility agent and never to a CLI arg the utility agent will read.
- **The frontier already handles it.** *"Plan our Japan trip — focus on relaxation, no aggressive sightseeing, this is for my marriage"* reads richer than `--engagement "wife time"` + `"plan Japan trip"` because the nuance threads through prose, not into a separate enum. A frontier-orchestrator-driven workflow gets engagement-shaping for free; a bare CLI invocation gets it by the operator putting context in the input text itself.

For new CLI verbs that would benefit from "context-aware" output: the operator carries that context into the verb's primary input. No separate `--engagement`, no `--context`, no `--vibe`. If the operator has no frontier orchestrator and wants context-shaping, they write the context into the input prose where the utility agent reads it as part of its bounded structuring job.

### Why the line matters at scale — the lost-in-translation problem

The mechanical reasons above (quantization, capability mismatch, etc.) are downstream of a deeper principle. **The pattern is older than AI:** in any organization, when admin staff translate vision → tasks, the vision quietly dies in the translation. The admin role IS narrower — that's why an admin layer can absorb volume — but applying that layer to vision-bearing work is the antipattern. Same dynamic in the AI stack: darkmux's *utility* layer is the AI analog of the org-world admin layer; pushing engagement-bearing work into it produces the same lost-in-translation failure mode.

What makes the line load-bearing:

- **Engagement is where the *why* lives.** The frontier orchestrator can hold engagements because it can sit in operator context, hold contradictions, and carry nuance across turns. A 4B utility agent can't hold contradictions — it'll resolve them. That resolution is where vision gets lost. A `--engagement "wife time"` flag forces the utility agent to do that resolution before it has the context to do it well.
- **The utility AI is the basic planning layer, not the strategic layer.** Capacity-matched to its actual job (bounded inputs, structured outputs, throughput). Asking it to ALSO carry *"what does this mean for the operator's broader life / org / book / engagement"* loads it past its capacity. Even when it produces something, that something is the small-picture compression of the big picture.
- **The cost scales with org size.** A solo operator can correct utility output in the next turn — the loop is tight enough that drift gets caught. An organization where the admin layer is making decisions BEFORE the operator/frontier sees them is the scenario where *big dreams get eaten alive by small bugs written by admin staff who don't have capacity yet to hold the big picture vision.* darkmux's utility layer can have exactly that pathology if its scope leaks into engagement territory; the line drawn here is what prevents it.

The frontier orchestrator's role in this layering is named **vision guard** — the layer that protects the operator's engagement-level intent from being compressed before it has been translated into structure the utility layer can handle. The cultivation discipline (how operators *shape* their frontier to actually hold their vision — CLAUDE.md files, skills, memory, conversation history) is the next-order concern; tracked separately as [#130](https://github.com/kstrat2001/darkmux/issues/130).

Surfaced 2026-05-14: Sprint 3 of #113 originally added `--engagement` to `darkmux mission propose`; operator caught it pre-merge as a doctrine violation against #49. Removed in the same PR, and the rule made explicit here so future verbs don't re-introduce it. The lost-in-translation framing came from the same exchange — codified here because the *why* is harder to reconstruct from the rule alone, and future verbs that look context-shaped will tempt the same drift.

## Project posture

**darkmux is an AI-first local-AI orchestrator.** It uses local-AI internally to manage your local-AI workflows. The CLI binary embeds dispatch logic to call into LMStudio-loaded utility agents for structuring, planning, and routine bounded reasoning tasks (compaction, phase estimation, mission proposal, notebook draft). The frontier-AI orchestrator (your Claude Code, Cursor, or OpenClaw session) remains the strategic reasoner; darkmux operates the local tier as a self-contained capability.

The recursive shape is the point: **darkmux uses local-AI to manage your local-AI.** Operators running darkmux are running local-AI dispatches whose orchestration is itself done by local-AI. That's the AI-first move — not "AI bolted on," but AI as the obvious built-in capability of a tool whose reason for existing is local-AI orchestration. Earlier framings of darkmux as *"infrastructure, not an agent framework"* were honest at the time (one-thing-only swap tool, saturated agent-X namespace) but are now aspirational. The current posture matches what the binary does.

### Role families

Two role families compose to make this work, and the distinction matters when picking models or proposing additions to a profile:

- **Utility agents** — small model (4B-class), bounded I/O, high throughput, structured output. Compactor, scribe, task estimator, mission-compiler. Each capability is asymmetric to its compute cost — one small model can fill several utility roles. darkmux dispatches utility agents internally for its own operations; the operator rarely invokes them directly. Defined by: bounded inputs + structured outputs + low per-call failure cost + throughput matters + bounded reasoning rather than strategy.
- **Specialist agents** — larger model (35B-class+), judgment-dependent, lower throughput, free-form output. Coder, code-reviewer, analyst. Operator's call: which specialist for which phase, with what tilt. darkmux makes them addressable via `dispatch <role>` but doesn't substitute its judgment for the operator's.

CLI primitives stay small and composable; the AI-built-in verbs (`mission propose`, `phase estimate`, `notebook draft`) compose those primitives with utility-agent dispatches so the operator gets structured output without authoring JSON by hand. Both surfaces are part of the same project — the dual posture (small primitives + AI-built-in verbs) is deliberate.

`darkmux dispatch` and `darkmux lab run` both use the internal Docker-bounded runtime — the only dispatch path (#1405 removed the legacy openclaw shell-out alternative).

## When in doubt

Read `README.md` for the user-facing pitch, `DESIGN.md` for the implementation reasoning, `CONTRIBUTING.md` for the dev loop. If something contradicts across files, the code is the source of truth — flag the doc drift to the user.
