# darkmux-runtime

In-house container-bounded agent runtime for darkmux. Replaces the openclaw shell-out path for `darkmux crew dispatch` with a lean Alpine container that darkmux owns end-to-end.

**Default for `darkmux crew dispatch` as of v0.4.** Operators with an existing openclaw setup can opt back into the legacy path via `darkmux crew dispatch --runtime openclaw <role>`.

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

## Environment variables

| Variable | Default | Effect |
|---|---|---|
| `DARKMUX_RUNTIME_COMPACT_THRESHOLD_TOKENS` | 60000 | Prompt-token threshold above which compaction fires |
| `DARKMUX_RUNTIME_COMPACTOR_MODEL` | `darkmux:qwen3-4b-instruct-2507` | Companion model used for compaction summaries |

## Tests

```
cargo test --release
```

60 unit tests cover tool execution, path validation, compaction logic, and trajectory recording.

## What's NOT in the runtime today

- Multi-agent parallelism inside one container (single agent loop per invocation today; nothing precludes extending to multiple `--agent` flags + parallel loops sharing the workspace)
- Audit-chain wiring at the volume-mount boundary
- Distribution: the image is built locally from this Dockerfile, not pushed to a registry — `docker build -t darkmux-runtime:latest runtime/` from the darkmux repo root is the install step

## Known gaps post-flip (v0.4)

The internal runtime is the default for `darkmux crew dispatch` as of v0.4, but a few rough edges remain:

- Variance vs openclaw across a larger sample set — current data: 5-sample distribution of converged config showed median 2-3× faster than openclaw, with 8.5× wall variance, so single-run rankings are noisy
- Migration of crew role definitions (`templates/builtin/roles/*.md`) — several roles still reference openclaw's tool palette (`exec`, `update_plan`, `process`) which don't exist here; the internal runtime ignores them gracefully but the manifests would be cleaner without
- `darkmux init` integration: today's pre-flight is at dispatch time (Docker reachable + image present); a separate setup pass during `darkmux init` would catch the prerequisite earlier
