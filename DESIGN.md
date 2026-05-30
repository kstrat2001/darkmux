# darkmux — design notes

## v0.1 scope: `darkmux swap <profile>`

The smallest useful thing. CLI that:

1. Reads `~/.darkmux/profiles.json` (or `--config <path>`)
2. Looks up the named profile
3. Calls `lms unload --all` (or selectively unloads only mismatched models)
4. Calls `lms load <model> --context-length <N> --identifier <id>` for each model in profile
5. (Optional) Patches caller's runtime config (e.g., openclaw.json fields under `runtime:`)
6. (Optional) Runs `post_swap` hooks (e.g., gateway restart)

That's it. No proxy, no classifier, no daemon. ~200 lines of code.

## What the v0.1 user gets

- `darkmux swap fast` — single-turn config in 5-15s
- `darkmux swap deep` — long-agentic config in 5-15s
- `darkmux status` — what's currently loaded, which profile (if any) it matches
- `darkmux profiles` — list available profiles

Replaces the manual sequence:
```
lms unload <model-1>
lms load <model-1> --context-length N1 --identifier <id-1>
lms unload <model-2>
lms load <model-2> --context-length N2 --identifier <id-2>
# patch your agent runtime's config (if it has one)
# restart the runtime so it picks up the new config
```

with:
```
darkmux swap <profile>
```

## Implementation sketch (Rust)

- Single static binary built with `cargo build --release`. `swap` / `status` / `profiles` need only LMStudio; `crew dispatch` / `lab run` default to the in-house internal runtime (a per-dispatch `darkmux-runtime` Docker container) and can opt into an external agent runtime like OpenClaw / Aider / Cline via `--runtime`. No external agent runtime is required to get started.
- Profile / workload registries: `serde_json` over plain JSON files
- `lms` CLI invocation: `std::process::Command`
- Built-in workloads embedded into the binary via `include_str!` so `cargo install --path .` works from any directory without the source tree present
- Config patching for runtime configs (e.g. `openclaw.json`): targeted edits via `serde_json::Value` to preserve user-modified fields rather than full deserialize / reserialize round-trips
- Dep surface kept deliberately small (see `Cargo.toml`) — `anyhow`, `clap`, `serde`, `serde_json`, `dirs`. A 10-line inline module beats a crate for small one-off needs.

## v0.2 — `darkmux serve` (proxy mode)

OpenAI-compatible HTTP server on `localhost:11435` (or configurable). Routes by:

1. **Explicit profile hint** in request metadata (`X-Darkmux-Profile: deep`)
2. **Heuristic classifier** on prompt characteristics
3. **Default profile** when nothing matches

Calls `darkmux swap` internally if the loaded profile doesn't match. Forwards request to underlying LMStudio (or Ollama).

## v0.3 — observability hooks

Telemetry:
- Per-request: classified profile, swap-needed, swap-duration, dispatch-duration
- Per-profile: invocation count, fast/slow mode rate, p50/p95 wall clock

This is what would let users find the predictive correlate for fast vs slow modes. Make darkmux the place where that data lives.

## v0.4 — multi-machine substrate ("many machines become one")

Single-operator multi-machine is the design target. Operator owns a couple of Macs on a tailnet they control; darkmux makes them function as one development environment without becoming team tooling.

**Architecture** (shipped as of v0.4):

- **Coordination substrate**: Redis Streams via `RedisSink` (opt-in via `DARKMUX_REDIS_URL`). Two stream classes:
  - `darkmux:work:<tier>` — per-tier work queue. Publishers `XADD`; workers `XREADGROUP` + `XACK` via consumer group. First-claimant-wins is the allocation algorithm.
  - `darkmux:flow` — fleet-wide event log. Every machine's `TeeSink` includes a `RedisSink` leg; `XADD` per record. Read by the daemon's `/flow/<date>` endpoint for the decentralized topology UI.
- **Audit substrate**: `AuditFileSink` (opt-in via `DARKMUX_AUDIT_DIR`). BLAKE3-chained, `flock(2)`-serialized, per-machine per-day. `darkmux flow integrity-check` walks the chain, exits 2 on break. Composes with the casual `LocalFileSink` via `TeeSink`.
- **Provenance fields** (FlowRecord schema 1.8.0): `machine_id`, `machine_tier`, `orchestrator`, `work_id`, `attempt`. All operator-asserted (env-stamped); no authenticated identity.
- **Tier-aware dispatch routing**: roles declare `tier` in their JSON manifest. `darkmux crew dispatch <role>` (no `--machine`) auto-routes to a fleet peer when `role.tier != local DARKMUX_MACHINE_TIER`. Bails loud with hint when no peer matches. Emits a `dispatch route` flow record so the topology UI + audit chain capture *why* work went where.
- **Per-machine introspection**: `GET /machine/specs` returns version, machine_id/tier, RAM total/free, CPU brand, OS, loaded models from `lms ps`, redacted Redis URL. Consumed by `darkmux fleet status --deep` (HTTP fan-out across reachable peers).
- **Daemon resilience**: SSE Redis tail at `GET /flow/<date>/stream` is bounded — connect wedges bounded by `REDIS_CONNECT_TIMEOUT` (500ms × 2 wall-clock), persistent failures exit cleanly via a synthetic `stream.error` record after `MAX_CONSECUTIVE_XREAD_FAILURES`, and the producer→consumer channel is capped at `SSE_MPSC_CAPACITY` (256 records) with drop-newest semantics. A misbehaving viewer tab can't OOM the daemon.
- **CORS posture** (#225 → #273 → #288): default `null` (file://) only — bundled viewer from disk works; arbitrary localhost dev-server origins are denied. Operator opts in to specific origins via `DARKMUX_DAEMON_CORS_ORIGINS` (exact-match, normalized lowercase + no trailing slash). Literal `*` rejected with stderr hint.

**Out of scope (today; may revisit)**:

- Multi-tenant authn/authz (see "Not multi-tenant" above)
- Cross-machine mission/sprint state replication (per-machine FS today; tracked as a future architectural pivot, [#280](https://github.com/kstrat2001/darkmux/issues/280))
- Mission priority + cross-fleet pause/resume ([#282](https://github.com/kstrat2001/darkmux/issues/282))
- Elastic-hub failover (any peer can be promoted to hub) — would close the SPOF of a fixed-hub deployment

## What darkmux is NOT

- Not a model-swap optimization (LMStudio handles the actual load — we orchestrate)
- Not an inference framework (vLLM/SGLang have that covered)
- Not an agent framework (LangChain/AutoGen have that covered)
- Not a prompt router across providers (LiteLLM has that covered, and it's cloud-oriented)
- Not *designed* for multi-tenant deployment. **darkmux is single-operator, multi-machine.** A hobbyist or individual engineer's "few Macs joined over a mesh VPN" is the natural deployment shape. Trust boundary is the operator-controlled tailnet, not enforcement in darkmux's code: `DARKMUX_REDIS_URL` carries no auth beyond what the underlying mesh + Redis ACLs already provide; `DARKMUX_ORCHESTRATOR` and `DARKMUX_MACHINE_ID` are operator-asserted provenance, not authenticated identity; cross-machine state on the shared substrate assumes all participants are the same operator. Fork-friendly if multi-tenant matters to you — the substrate is a reasonable starting point and the missing pieces (auth, ACLs, fairness across distrusting users) are well-trodden territory in other systems.

## Relationship to openclaw

**darkmux works two ways — pick whichever matches your setup, switchable per-dispatch:**

- **Standalone** (default): with just Docker + LMStudio, darkmux runs dispatches through its built-in internal runtime — an in-house Rust agent loop in a per-dispatch container. No external runtime to install or configure. This is what `darkmux crew dispatch` and `darkmux lab run` use out of the box.
- **With your existing openclaw**: if openclaw is already in your stack, darkmux dispatches through it via `--runtime openclaw`. The agent runs as a host process under openclaw's normal session/agent model — no translation layer, no "darkmux mode" inside openclaw. Pre-flight sync (`darkmux crew sync`) keeps openclaw's `agents.list[]` aligned with the darkmux role manifests; otherwise the integration is transparent.

**darkmux is not a replacement for openclaw.** The standalone path exists for fresh operators who shouldn't need to install a second tool to get started. The openclaw path exists so operators with openclaw already wired in keep their workflow — including any existing sessions, channel routing, custom agents, and the openclaw-specific tools (`update_plan`, `process`) that darkmux's internal runtime doesn't ship. Both paths are first-class; the choice is per-dispatch, not a one-time install decision.

The two runtimes overlap on the basic shape — model + system prompt + tools + chat loop → final reply + trajectory. They diverge on the surrounding concerns:

| Aspect | Internal runtime | OpenClaw |
|---|---|---|
| Install footprint | Docker image (~150 MB) | openclaw binary + `~/.openclaw/openclaw.json` |
| Workspace isolation | per-dispatch container (kernel-enforced) | host process + symlink fences |
| Session model | per-dispatch tempdir; cross-dispatch state is file-mediated (sprint-as-contract) | persistent sessions at `~/.openclaw/agents/<id>/sessions/` |
| Agent registry | role manifests under `templates/builtin/roles/` (re-read every dispatch) | `agents.list[]` in `openclaw.json` (synced via `darkmux crew sync`) |
| Tool surface | `read`, `edit`, `write`, `search`, `bash` | broader (adds `update_plan`, `process`, background lifecycle) |
| Reach for it when | new install; out-of-box dispatching; sprint-as-contract workflows | already openclaw-wired; want session persistence; need `update_plan` / `process` |

The internal runtime has stricter isolation and a tighter feature surface scoped to darkmux's specific workflow needs. Openclaw has the broader feature surface and the mature ecosystem an existing operator may already depend on.

### Scope of the internal runtime: workflow-fit, not feature-parity

When deciding what to add to the internal runtime, the filter is **workflow-fit**, not feature-parity with openclaw. darkmux is shaped by three load-bearing decisions:

- **Mission-as-contract.** A sprint is a bounded unit of work with explicit inputs (prior sprint outputs, scope file), explicit outputs (typed text file persisted to disk), and explicit verify criteria. Cross-sprint memory is file-mediated by design — the frontier orchestrator sees what state moves between sprints. Hidden session-state that survives across dispatches breaks this contract.

- **Admin/specialist split.** Admin agents (4B-class: compactor, scribe, estimator, mission-compiler) handle bounded structured work at high throughput. Specialist agents (35B+: coder, code-reviewer, analyst) handle judgment-dependent work at lower throughput. Features that push specialists toward admin work (mid-dispatch planning, todo tracking, autonomous replanning) collapse the layering that makes the split valuable — and turn judgment-bearing work into hidden admin work.

- **Operator sovereignty + frontier-as-strategic-layer.** The frontier orchestrator (Claude Code) holds the strategic context; admin agents structure under that context; specialists execute within it. Features that move strategic choices *down* into admin or specialist dispatches — opaque session state, automated replanning, scoped planning verbs — quietly relocate decision authority into tiers that lack the context to make them well.

The filter for any proposed internal-runtime feature: **does this reinforce mission-as-contract, the admin/specialist split, and frontier-as-strategic-layer — or does it blur them?** Features that reinforce land cleanly even when they're small. Features that blur produce "works technically but feels wrong" outcomes that surface as bugs months later.

Openclaw's broader surface is a strength for openclaw's own use cases. When operators need a feature openclaw has and the internal runtime doesn't, the answer is usually `--runtime openclaw`, not "let's add it to the internal runtime." Both paths stay viable on purpose.

### Schema isolation: each runtime owns its own config

The internal runtime and openclaw dispatch are separate runtime paths with separate config schemas. **Neither runtime translates the other's config shape.** This separation enforces operator-sovereignty at the schema level: every field an operator sees in a darkmux profile maps to a darkmux-typed schema entry that the internal runtime consumes — no decorative fields that look tunable but have no effect.

The codebase distinguishes three code-path categories with distinct rules for what they may read:

**1. Internal-runtime path** (`src/crew/dispatch_internal.rs`, `runtime/src/`): reads only darkmux-native typed fields from `profile.runtime.*` — the schema defined in `src/types.rs::RuntimeCompactionConfig` and siblings. darkmux owns these field names, their semantics, and their evolution. The untyped `extras: BTreeMap` field on `RuntimeCompactionConfig` exists for legacy back-compat parse only; nothing in the internal-runtime path reads from it. The clean break was enforced consumer-side in #369 with explicit "must not auto-populate" tests `from_profile_derives_typed_threshold_ratio` and `from_profile_ignores_openclaw_maxhistoryshare_extras` in `src/crew/dispatch_internal.rs`.

**2. OC dispatch path** (`--runtime openclaw`, `src/crew/dispatch.rs`): shells out to `openclaw agent darkmux/<role-id>`. Openclaw reads its own `~/.openclaw/openclaw.json`. darkmux does not forward profile fields into openclaw's config, and openclaw never sees the darkmux profile. Operators using openclaw configure it through openclaw's own surfaces (`openclaw.json` editing, `openclaw agent` CLI flags, openclaw's own documentation). No schema bridging in either direction.

**3. OC helper tooling** (`darkmux crew sync`, OC config patcher in `src/runtime.rs`, eureka OC config diagnostics): legitimate openclaw-aware code that operates ON `~/.openclaw/openclaw.json` directly. Knows the openclaw schema because that's its job — these are *helper verbs for openclaw users*, not part of the internal-runtime path. The doctrine permits these freely; they're clearly labeled as OC tooling rather than embedded in dispatch config plumbing. Openclaw-schema *emission* that the engine doesn't itself consume — the `agents.list[]` scaffold snippets — lives one step further out, as the standalone `integrations/openclaw/oc-scaffold.sh` script (NOT a CLI verb; #538), so the binary carries no openclaw-schema knowledge it doesn't use. The `darkmux-agent-roles` crate remains only for `darkmux doctor`'s role-gap awareness.

**Profile generation discipline.** Heuristics in `src/heuristics.rs` write only darkmux-typed fields. Existing operator profiles with openclaw-shape `extras` keys (legacy `mode`, `maxHistoryShare`, `recentTurnsPreserve`, `customInstructions`) continue to load via back-compat parse, but `darkmux doctor` flags them as inactive with a migration hint to the typed equivalent (where one exists) or a removal suggestion (where it doesn't).

**Doctor scoping.** `darkmux doctor` defaults to internal-runtime-only output. Operators who use `--runtime openclaw` opt into OC-specific checks via `--include-openclaw` (covers OC binary discovery, OC version validation, OC agents.list drift, OC config role definitions). Internal-runtime-only operators get a clean doctor report without OC noise.

**Maintenance risk this prevents.** When darkmux's profile schema is purely darkmux-typed, an upstream openclaw schema change (e.g., openclaw v2026.8 redefining `maxHistoryShare` semantics) has zero impact on darkmux — because no darkmux code path consumes openclaw-shape fields on either runtime path. The maintenance dependency only exists in OC helper tooling, where it's explicit and scoped to verbs that exist to serve openclaw users.

**Implementation status** (updated 2026-05-29). The [Independence mission (#380)](https://github.com/kstrat2001/darkmux/issues/380) has landed the generator + doctor cleanup, so all three rules are now enforced end-to-end:

- **Internal-runtime consumer path (Rule 1)** — enforced; the runtime reads only typed `RuntimeCompactionConfig` fields, guarded by the `from_profile_derives_typed_threshold_ratio` / `from_profile_ignores_openclaw_maxhistoryshare_extras` tests named above.
- **OC dispatch / OC helper tooling (Rules 2 + 3)** — structurally enforced; there is no schema-bridging code on either path.
- **Profile generation discipline** — `src/heuristics.rs` now writes only typed darkmux fields. The dead-letter `mode` / `maxHistoryShare` / `recentTurnsPreserve` writes were dropped and `customInstructions` migrated to the typed `custom_instructions` field ([#391](https://github.com/kstrat2001/darkmux/issues/391)); generator tests assert those openclaw-shape keys are *absent* from generated profiles.
- **`darkmux doctor` legacy-extras warning** — shipped ([#394](https://github.com/kstrat2001/darkmux/issues/394)): `check_legacy_compaction_extras()` warns on any profile still carrying openclaw-shape keys, with a migration hint to the typed field (or a removal suggestion where no typed equivalent exists). The last back-compat reader — `sprint_cli`'s `extras["maxHistoryShare"]` fallback — was removed in [#397](https://github.com/kstrat2001/darkmux/issues/397), so the only remaining `extras` read in the codebase is that doctor warning itself.
- **`darkmux doctor --include-openclaw` flag** — shipped ([#395](https://github.com/kstrat2001/darkmux/issues/395)): OC-specific checks (binary discovery, version validation, `agents.list` drift, role definitions) and openclaw-mentioning eureka findings are gated behind the flag, so default doctor output is internal-runtime-only.

Legacy operator profiles with openclaw-shape `extras` keys still *load* via back-compat parse — they're just inert and surfaced by the doctor warning above. The doctrine is now fully realized in code; contributors extending the profile schema should keep new fields typed and darkmux-native.

## Lab reproducibility: fixtures + content hashing

The lab harness only earns the word "measurement" if a run is reproducible. The fixture cluster (#487) closes the two gaps that made earlier `coding-task` numbers untrustworthy: runs mutating their own inputs, and no way to prove two runs started — or ended — in the same place.

- **Per-run COW isolation.** Each run operates on a copy-on-write clone of the source fixture, never the source. The clone is cheap on COW filesystems (`clonefile` on APFS, `--reflink` on btrfs/xfs/zfs) and falls back to a deep copy elsewhere. The provider trait is unchanged — providers see a sandbox path; they don't know it's a clone. This eliminated the cross-run baseline drift observed in Beat 55.
- **Content hashing as proof, not policy.** `baseline_hash` (source state at clone time) and `final_hash` (post-dispatch sandbox state) are BLAKE3 over a deterministic walk that excludes derived dirs (`.git`, `node_modules`, `target`, `__pycache__`, `.coverage`, `.darkmux-runtime`). Determinism is the point: same content + same layout → same hash, independent of mtimes or inode order. Equal `final_hash` across two runs is the strongest reproducibility signal the lab can emit. Hashing is best-effort — a failure logs and records `null` rather than aborting the dispatch.
- **Registry, not embedded sandboxes.** A fixture is an operator-owned directory with a `.fixture.json` manifest; the registry (`lab-registry.json`) is a name→path lookup plus integrity metadata. `lab register`/`unregister` never move or delete the directory (operator sovereignty — `unregister` drops the *entry*, full stop). Workloads bind to fixtures abstractly via `requires_fixture: "<name>@<version>"`, resolved against each fixture's `satisfies` declaration. This replaced the brittle `DARKMUX_SANDBOX_<ID>` env-var binding: the registry is now the single persistent fixture binding, and `lab doctor` makes drift detectable offline before a dispatch is wasted on it.

## Compaction: tiers, structured slots, and graceful degradation

Compaction is the harness lever with the largest measured wall-clock impact (Articles 1–2), so it gets the most defensive engineering. Two strategies coexist behind one config knob (`profile.runtime.compaction.strategy`):

- **Narrative** (default) — prose summary, replaces the middle of the conversation with a synthetic `user`-role message. The Article-2-era shape.
- **Structured-slot** (tier-2, #352) — the compactor is called in JSON mode and emits a typed `StructuredCompactionOutput` (objective, current-truth, completed-decisions, errors-to-preserve, next-actions, verify-criteria), rendered as labeled markdown into a synthetic `system`-role message. Per-slot character caps bound each field. The default compactor prompt (`DEFAULT_COMPACTION_INSTRUCTIONS`, the empirically-won "V4 / reality-discipline" prompt) frames every slot as *show, don't tell* to suppress the hallucination-class regressions earlier prompt versions produced.

The design bet behind structured-slot is that **a small model fills labeled slots more reliably than it writes good prose**, and that typed output degrades more gracefully. Three degradation layers make that real, in order:

1. **Lexical JSON repair** (#401 layer 1) — a truncated compactor response (runaway escapes, an unterminated string, unbalanced brackets) is walked byte-by-byte and closed off, producing a parseable (if lossy) value rather than a dispatch bail.
2. **Schema patch** (#401 layer 2) — if required fields are still missing after parse, safe defaults are inserted and `compaction_metadata.truncation_patched` is set so downstream analysis can flag the run.
3. **Escalation bound** (#377) — `reserve.bail_after_compactions` caps how many times one dispatch may compact; past the bound the runtime emits an `EscalationTriggered` terminal for frontier handoff rather than looping forever.

Two model-shape accommodations round it out: thinking-mode models route JSON to `reasoning_content`, so `extract_compactor_content()` falls back there when `content` is empty (#376/#378); and the JSON-mode request uses LMStudio's `json_schema` response format (decode-time shape enforcement), not OpenAI's looser `json_object` (#375). The dispatch budget (turns/tokens used vs caps) is folded into the structured output's metadata and rendered into the synthesized message so the model sees its remaining runway (#439) — framed as a *floor not a ceiling*. The compaction config schema follows the schema-isolation doctrine above: every field is darkmux-typed; `custom_instructions` is a typed field appended to the base prompt, not an `extras` passthrough.

## Runtime resilience: struggle detection + feedback injection

A local model in an agent loop fails in characteristic ways — re-reading the same file, re-reasoning the same dead end, hammering a tool that keeps erroring, emitting reasoning until it hits the token cap with nothing to show. The internal runtime carries a family of cheap, edge-triggered detectors for these, plus the recovery and budget machinery to act on them. Three design commitments shape the family:

- **Observability before intervention.** Each detector (cycle, reasoning-loop, tool-failure cascade, cadence-drift) writes a trajectory event and, by default, nothing else changes — the MVP is *visible struggle*, not auto-bail. `MAX_TURNS` and the inactivity deadline catch genuinely-stuck dispatches *late*; the detectors exist to surface the struggle *early*, for the operator and (via feedback injection) for the model.
- **Recover, don't discard.** When a turn hits the per-call token cap (10000) but emitted well-formed tool calls, those calls are salvaged rather than treated as a failed turn (#479). A `finish_reason=length` turn with no content and no tool calls (pure runaway reasoning) is dropped, nudged, and retried within a small budget before escalating (#414). Tool calls the model wrote as plain text — bracket, harmony, or darkmux's XML extension — are promoted back to structured calls instead of being lost (#406). Each recovery is itself a trajectory event so bail/recovery rates stay visible.
- **Feedback injection is the model-facing half.** Detectors and recovery paths queue synthetic `[darkmux-runtime]`-prefixed `system` messages that are drained into the next turn's prompt — telemetry the model can act on, not just telemetry the operator reads after the fact. The bracketed prefix is the term-provenance contract (see the model-facing-prompt doctrine in `CLAUDE.md`); per-signal wording is overridable per role via the manifest's `feedback_templates`, and the whole channel is disable-able with `DARKMUX_FEEDBACK_INJECTION=0`. The deadline and budget caps (`--max-turns` / `--max-tokens`, opt-in; `DARKMUX_INACTIVITY_TIMEOUT_SECONDS` with a 75% soft warning before the host's 100% hard kill) are the coarse backstops underneath the fine-grained detectors.

The unifying principle is operator-sovereignty applied to the runtime: every detector is observable in the trajectory, every nudge is attributable to a named signal, every bound is operator-tunable, and nothing silently changes the dispatch without leaving a record of why.

## Composability

Designed to live BELOW agent frameworks and ABOVE inference engines:

```
[ agent framework: OpenClaw, Aider, Cline, Continue, custom ]
                    |
                    v
               [ darkmux ]
                    |
                    v
[ inference engine: LMStudio, Ollama, llama.cpp ]
```

Drop in via OpenAI-compatible endpoint. No changes to agent framework. No changes to inference engine. Just a smarter routing layer between them.
