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

- Single static binary built with `cargo build --release` (no runtime needed beyond LMStudio + an agent runtime like OpenClaw)
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
