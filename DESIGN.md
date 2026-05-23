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
