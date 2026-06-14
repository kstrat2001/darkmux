# darkmux-runtime

In-house container-bounded agent runtime for darkmux. Replaces the openclaw shell-out path for `darkmux crew dispatch` with a lean Alpine container that darkmux owns end-to-end.

**The default for `darkmux crew dispatch`** (it has been the default across the 1.x line). Operators with an existing openclaw setup can opt back into the legacy path via `darkmux crew dispatch --runtime openclaw <role>`.

## Why this exists

darkmux historically delegated agent execution to [openclaw](https://github.com/openclaw/openclaw). openclaw is a Discord-first agent framework — darkmux used about 20% of its surface and inherited the rest (workspace bootstrap, multi-channel routing, persona management) as overhead. That worked while darkmux was a profile multiplexer wrapping an existing runtime; it stopped fitting once the crew abstraction landed.

The concrete failure mode that triggered the in-house runtime: a persistent `--workdir` symlink inside an openclaw workspace dir survived across unrelated dispatches and produced a publish-to-public-OSS leak. The deeper architectural smell is *darkmux state lives inside another tool's directory tree.*

A container-bounded in-house runtime replaces both layers:

- darkmux owns the runtime end-to-end (no more 20%-of-openclaw)
- The container is the kernel-enforced boundary (no more soft workspace isolation that depends on code discipline)

## What's in the runtime

| Component | Role |
|---|---|
| `src/main.rs` | CLI entry; `--check` / `--version` / `run` subcommands |
| `src/loop_runner.rs` | Tool-call loop: send → parse → dispatch tools → loop until `stop` |
| `src/lmstudio.rs` | OpenAI-compatible chat-completions HTTP client (hand-rolled `ureq`; no openai SDK) |
| `src/tools/` | Tool implementations: `search`, `read`, `edit`, `write`, `bash` (+ `echo` for unit tests only) |
| `src/tools/workspace.rs` | Path validators — canonicalize + `starts_with` check against the workspace root; symlink escapes rejected |
| `src/compaction.rs` | Token-count-aware middle-replace compaction via a companion 4B model |
| `src/trajectory.rs` | Per-dispatch JSONL trajectory + final metrics, written under `<workspace>/.darkmux-runtime/` |

## Tool catalog

| Tool | Signature | Notes |
|---|---|---|
| `search` | `{ pattern, path, max_results? }` | Literal substring match; recurses on dirs; auto-skips dependency/build/hidden dirs |
| `read` | `{ path, offset, limit }` | Requires explicit offset+limit; `limit=0` reads full file from offset |
| `edit` | `{ path, edits: [{old_string, new_string, replace_all?}] }` | Batched, atomic — any edit failing aborts the whole call without modifying the file |
| `write` | `{ path, content }` | Canonical |
| `bash` | `{ command, timeout_seconds? }` | Runs with cwd=/workspace |

Tool order in the request: `[Search, Read, Edit, Write, Bash]` — search first (positional preference).

The shape converged through an empirical-evaluation arc against the canonical Article 2 long-agentic refresh-token QA workload. Lab notebook Beats 27-29 carry the full reasoning.

## Build + run

From this directory:

```
# build the image
docker build -t darkmux-runtime .

# default CMD runs the container environment check
docker run --rm darkmux-runtime

# explicit version
docker run --rm darkmux-runtime --version

# mount a host directory as the workspace
docker run --rm -v "$(pwd):/workspace" darkmux-runtime

# real dispatch (model + system prompt + user prompt)
docker run --rm -v "$(pwd):/workspace" darkmux-runtime run \
  --model "darkmux:qwen3.6-35b-a3b-turboquant-mlx" \
  --system "$(cat /path/to/coder.md)" \
  --prompt "your task here"
```

For routine use, the wrapper is `darkmux crew dispatch <role-id> -m "<message>"` (default runtime — handles role loading, workspace allocation, model probe, and Docker pre-flight).

## Dispatching into your own environment (`--image`, #703)

By default a dispatch runs in `darkmux-runtime:latest` (slim — python + node, no compiled-language toolchain). To let the agent **compile/test in-sandbox** (the inner verify loop), point it at an image that has your project's toolchain:

```
darkmux crew dispatch coder --workdir <repo> --image rust:slim -m "..."
```

darkmux does **not** ship a catalog of per-language images. Instead it **injects** its agent into the image you name: the static `darkmux-runtime` binary (musl, runs in any Linux image) is extracted once to `~/.darkmux/runtime/` and bind-mounted into your image with the entrypoint overridden. So `--image` accepts *anything* — `rust:slim`, `node:20-bookworm`, **your project's own CI image**, or whatever your `Dockerfile` builds. The model of it: darkmux brings the agent; you bring the environment (the CI-runner / devcontainer pattern).

**Image requirement:** minimal. The agent's `bash` tool prefers `bash` but falls back to `sh` when bash isn't installed, so bare-alpine / busybox images (`rust:alpine`, `*:alpine`) work too — only a POSIX `sh` is required, which every Linux image has. (Truly shell-less distroless images aren't supported.) After rebuilding the default image, `rm ~/.darkmux/runtime/darkmux-runtime` to refresh the cached binary.

**Build cache:** `~/.darkmux/cache` is bind-mounted into every dispatch at `/darkmux-cache`, with `CARGO_HOME` / `npm_config_cache` / `PIP_CACHE_DIR` redirected into it — so the inner verify loop reuses downloaded deps across dispatches. The registry/download caches are concurrency-safe; each dispatch's `target/` stays in its own workspace, so concurrent dispatches don't contend on build artifacts.

`--image` is local-dispatch only today — ignored on `--runtime openclaw` and on cross-machine `--machine` dispatch (the remote runner uses its own image; carrying the image through the fleet queue is a follow-on).

## Environment variables

The runtime reads only two env vars directly. Everything else — model, context
window, compaction tuning, turn/token bounds — arrives as explicit **CLI flags**
from the host dispatcher.

| Variable | Default | Effect |
|---|---|---|
| `DARKMUX_INACTIVITY_TIMEOUT_SECONDS` | 600 | Per-dispatch inactivity budget. The runtime-side detector soft-warns at 75%; the host watchdog hard-kills the container at 100%. Both reset on any proof-of-work signal. |
| `DARKMUX_FEEDBACK_INJECTION` | on | Toggles the struggle-detector feedback-injection channel (cycle / tool-failure / reasoning-loop nudges). Set to `0`/`off`/`false`/`no` to disable. |

**Compaction config is NOT read from the environment** (since #368/#482). The host
derives it from `profile.runtime.compaction.*` and passes it as CLI flags —
`--compact-threshold-tokens`, `--compactor-model`, `--compact-threshold-ratio`,
`--context-window`, `--compact-strategy` (the runtime requires at least one of
`--context-window` / `--compact-threshold-tokens`; there is no env fallback). Turn
and cumulative-token bounds arrive the same way via `--max-turns` / `--max-tokens`.
The operator's tuning surface is the profile JSON, not shell env.

## Tests

```
cargo test --release
```

60 unit tests cover tool execution, path validation, compaction logic, and trajectory recording.

## What's NOT in the runtime today

- Multi-agent parallelism inside one container (single agent loop per invocation today; nothing precludes extending to multiple `--agent` flags + parallel loops sharing the workspace)
- Audit-chain wiring at the volume-mount boundary

## Known gaps post-flip

The internal runtime is the default for `darkmux crew dispatch`, but a few rough edges remain:

- Variance vs openclaw across a larger sample set — current data: 5-sample distribution of converged config showed median 2-3× faster than openclaw, with 8.5× wall variance, so single-run rankings are noisy
- Migration of crew role definitions (`templates/builtin/roles/*.md`) — several roles still reference openclaw's tool palette (`exec`, `update_plan`, `process`) which don't exist here; the internal runtime ignores them gracefully but the manifests would be cleaner without
- `darkmux init` integration: today's pre-flight is at dispatch time (Docker reachable + image present); a separate setup pass during `darkmux init` would catch the prerequisite earlier
