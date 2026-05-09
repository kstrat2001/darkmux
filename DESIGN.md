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

## What darkmux is NOT

- Not a model-swap optimization (LMStudio handles the actual load — we orchestrate)
- Not an inference framework (vLLM/SGLang have that covered)
- Not an agent framework (LangChain/AutoGen have that covered)
- Not a prompt router across providers (LiteLLM has that covered, and it's cloud-oriented)
- Not multi-tenant (single-user local-AI scope)

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
