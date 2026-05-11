# darkmux

**[darkmux.com](https://darkmux.com) · Task-class-aware multiplexer for local LLM configurations.**

Pick the right loaded model + context length + compaction profile for each task — automatically. Designed for agentic local-AI workflows on LMStudio (and Ollama / llama.cpp via the same OpenAI-compatible surface).

> **Heads up — read before running.**
> darkmux orchestrates AI tools that execute on your machine. It modifies your local config files (`~/.openclaw/openclaw.json`), sends commands to your local LMStudio server, and — in lab mode — runs AI-generated code in a working directory that is **not a security sandbox**. AI agents can behave unexpectedly. Use it on a machine where that is acceptable. Performance numbers in this README and in the accompanying articles are measured on the author's hardware (M5 Max, 128 GB) and will differ on yours. See [DISCLAIMER.md](./DISCLAIMER.md) for details. MIT licensed, no warranty, use at your own risk.

## Quick start

### Prerequisites

darkmux orchestrates LMStudio + your agent runtime. Install these once:

| Required | Why | Install |
|---|---|---|
| **[LMStudio](https://lmstudio.ai/)** | Loads/unloads models. darkmux drives it via the `lms` CLI. | macOS / Windows / Linux installer |
| **At least one model in LMStudio** | Nothing to swap to without one. | Download via the LMStudio UI; verify with `lms ls`. |
| **Rust toolchain** | To build darkmux itself. | `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh` |

| Optional | When you need it |
|---|---|
| **An agent runtime** (e.g. [OpenClaw](https://github.com/openclaw/openclaw), Aider, Cline) | Only for the `lab` subcommand (workload dispatch, characterize, notebook). `swap`/`status`/`profiles` work without one. Override with `DARKMUX_RUNTIME_CMD=<your-runtime>`. |
| **[Claude Code](https://claude.com/claude-code)** | Only for the agent-invokable skills (`/darkmux-status`, etc.). darkmux as a CLI works without it. |

darkmux is developed and tested on Apple Silicon. Linux should work; Intel Mac is untested.

### Install + bootstrap

```bash
git clone https://github.com/kstrat2001/darkmux
cd darkmux
cargo install --path .   # builds the self-contained binary, drops it on $PATH
darkmux init             # creates ~/.darkmux/profiles.json + installs agent skills
```

### Verify your setup

```bash
darkmux doctor          # 7 pre-flight checks: registry, LMStudio, models, runtime, RAM, power
```

Doctor returns exit 0 if everything's wired up, 1 if a fail-level check needs fixing. Fail/warn lines include actionable hints.

Once doctor is green, edit `~/.darkmux/profiles.json` and replace each `<your-primary-model-id>` placeholder with an actual id from `lms ls`. (Doctor will warn if profiles don't match your loaded models — that's the moment to fix them.)

### First useful commands

```bash
darkmux profiles                  # list configured profiles
darkmux status                    # what's loaded; which profile (if any) matches
darkmux swap fast                 # swap to the "fast" profile (loads model into LMStudio)
darkmux lab characterize          # one-command "QA my Mac" — dispatch a smoke workload, get a verdict
darkmux lab run quick-q           # the smoke workload directly
darkmux lab runs --limit 5        # see your recent runs
darkmux lab inspect <run-id>      # full per-run breakdown
darkmux notebook draft <run-id>   # ask the agent to author an EE-lab-style notebook entry
```

Using Claude Code? Run `darkmux init --with-claude-md ~/.claude/CLAUDE.md` to install the skills *and* teach Claude Code about darkmux at session start.

### Updating darkmux

After pulling new commits:

```bash
git pull
cargo install --path . --force
```

The `--force` flag tells cargo to replace the existing binary even when the source path or version metadata hasn't changed. Without it, cargo can silently skip the reinstall and leave you running an older binary while reporting the same `darkmux --version`. If a new feature (like `--instrument`) is missing despite a fresh `git pull`, that's the most likely cause — re-run with `--force`.

## Why this exists

Local-AI users hit a real workload-tax problem when they go agentic. A single static configuration can't be optimal across:

- **Bounded tasks** (TODO fills, single-turn reviews) — want a slim primary, no compaction overhead, fast decode
- **Long agentic tasks** (multi-file refactors, exploratory test authoring) — want big context to avoid compaction cliffs, even at the cost of bigger KV pre-allocation
- **Mid-range tasks** — want compaction-tuned middle ground

Empirical data behind this (from the work that motivated darkmux):

| Workload | Slim config (no offload, 32-64K) | Mid config (101K + 68K compactor) | Big config (262K + 120K compactor) |
|---|---|---|---|
| Bounded TODO | **60s** ✓ | 87s | 82s |
| Long agentic (n=6) | (would risk overflow) | 478s baseline | **mean 406s, fast 222s, slow 773s** |

Bigger context wins long tasks. Slim config wins bounded tasks. **No static config is optimal across both regimes** — but a router can be.

## What darkmux does

```
┌──────────────────┐
│  Agent / IDE /   │  (Claude Code, OpenClaw, Aider, Cline, Continue, custom)
│  Wrapper script  │
└────────┬─────────┘
         │ OpenAI-compatible HTTP
         ▼
┌──────────────────┐
│     darkmux      │  task classifier + LMStudio orchestrator
│   localhost:N    │
└────────┬─────────┘
         │ OpenAI-compatible HTTP
         ▼
┌──────────────────┐
│    LMStudio /    │  the actual inference
│    Ollama /      │
│   llama.cpp      │
└──────────────────┘
```

### Three layers

1. **`darkmux swap <profile>`** — bare CLI for users who classify themselves. Unloads/loads models in LMStudio according to a named profile. ~10s wall to swap.

2. **Multi-config registry** — a JSON file naming profiles (`fast`, `balanced`, `deep`) with model IDs, context lengths, compaction settings. Profiles can also encode "warm pair" (primary + companion compactor) configurations.

3. **`darkmux serve` (proxy mode)** — OpenAI-compatible HTTP frontend. Intercepts requests, classifies the task (heuristics first, optional LLM-classifier later), routes to the right loaded profile, swaps if needed.

Layers 1 and 2 ship from day one. Layer 3 is the smart-routing piece, optional.

## Why "darkmux"

- **dark** — Darkly Energized lineage (the experimental work that motivated it)
- **mux** — multiplexer (well-known engineering jargon for routing N → 1 or 1 → N)

OSS-published under personal GitHub: `github.com/kstrat2001/darkmux`. Darkly Energized is the brand context but darkmux is intentionally infrastructure (no commercial coupling).

The project is brand-aligned but doesn't claim "agents" — positioning matters because the agent-X namespace is saturated and confused. darkmux is infrastructure, not an agent.

## Design principles

1. **Compose, don't reinvent.** LMStudio already exposes load/unload via `lms`. Don't replace it; orchestrate it.
2. **Profiles are config, not code.** Named profiles in a JSON file. Add a profile by editing config, not by writing a plugin.
3. **Heuristic classification first, LLM classification later.** Free heuristics (prompt length, channel, agent role, file pattern) get most of the way without burning inference cycles.
4. **OpenAI-compatible everywhere.** Frontend, backend, and config syntax all use the established OpenAI surface so existing agents drop in.
5. **Honest about limits.** A router only beats static configs by routing correctly. We're explicit about what darkmux DOES NOT do (e.g., it doesn't make LMStudio faster; it makes the right LMStudio config available at the right time).

## Runtime — agnostic by default

The `lab` subcommand dispatches workloads against an *agent runtime* — by default this is `openclaw` (the runtime darkmux was developed against). The runtime is invoked via:

```bash
DARKMUX_RUNTIME_CMD=<command>   # default: openclaw
```

If you set `DARKMUX_RUNTIME_CMD=aider` (or `cline`, or your own wrapper), darkmux invokes that binary instead with the same `agent --message` calling convention. Anything that exposes a single-shot `<cmd> agent --message <text> --json` surface is a candidate. The `swap` / `status` / `profiles` subcommands don't depend on the runtime at all — they orchestrate LMStudio directly.

This means: **darkmux's profile-multiplexing is runtime-agnostic** today; the lab harness is *runtime-pluggable* via the env var. The empirical findings in the article series happened to be measured against OpenClaw; the routing thesis itself is independent.

## Instrumentation

`lab run --instrument` captures cross-layer telemetry alongside each dispatch — what LMStudio actually had loaded, where the gateway process sat across the run, and any anomalies (PID changes during active dispatch, loaded-model-set shifts, missing samplers). No root required.

```bash
darkmux lab run long-agentic --instrument
```

The flag adds a sidecar sampler that writes one JSON record per line to `~/.darkmux/runs/<run-id>/instruments.jsonl`. Each line has the shape:

```json
{"t": 1778466601846, "elapsed_ms": 0, "source": "meta", "payload": {...}}
```

Three sources:

- **`meta`** — sampler lifecycle events (start, cadence, version)
- **`lms`** — LMStudio's loaded-model snapshot from `lms ps --json` (identifier, context, status per model)
- **`process`** — gateway-process residency from `ps -p`: PID, port, CPU%, RSS

### Viewer

The companion viewer at [darkmux.com/viewer](https://darkmux.com/viewer/) replays a captured run as a four-block topology you can scrub through.

![darkmux viewer mid-replay — qwen3.6-35b primary processing a prompt, qwen3-4b compactor idle. Claude → OpenClaw Gateway → LMStudio backbone runs left-to-right; model nodes branch off the right edge.](docs/media/viewer-active-model.png)

Drag your `instruments.jsonl` file onto the window. The topology renders:

- **Agent client** → **OpenClaw Gateway** → **LMStudio** runs left-to-right
- Loaded models branch off the right edge as separate nodes
- Edges fire as request/response samples come in — active model gets cyan-dashed edges, idle models stay grey
- The Anomalies panel surfaces inconsistencies (gateway PID changed during active dispatch, loaded-model set shifted mid-run, samples missing) — usually leading indicators of a misconfiguration worth investigating

The viewer is a static page served from this repo's `docs/` folder. **Nothing is uploaded.** Your `instruments.jsonl` is parsed entirely in the browser. No backend, no telemetry on the telemetry.

## Status

🚧 **Pre-alpha.** v0.1 (`swap`/`status`/`profiles`) and v0.2 lab foundation (`lab workloads`/`run`/`inspect`/`compare`) implemented. Real built-in workloads, notebook, and skills bundle pending.

## Why this exists — empirical motivation

Headline findings from the experimental work that produced darkmux's reference profiles:

- **Static config tuning has a floor.** Compaction knobs (`maxHistoryShare`, `recentTurnsPreserve`, `customInstructions`, compactor n_ctx) are tightly coupled — pulling any one of them in isolation regresses the run. Tuning at the config layer eventually stops paying dividends.
- **The "compactor loaded" tax is real.** Keeping a small compactor model warm for offload availability adds ~25s per dispatch on bounded workloads, even when compaction never fires. That cost is fixed and unrelated to compactor context size.
- **Long-task wins are bimodal.** With maximum primary context, multiple dispatches of the *identical* prompt + config split into a fast cluster (single-turn, no compaction fired) and a slow cluster (multi-turn, compaction fired). 3× variance between modes is normal — driven by emergent control-flow decisions inside the model's tool-loop, not by config.
- **Both modes still beat smaller-context baselines.** A router doesn't need to predict which mode a given dispatch will land in; it just needs to pick the right configuration for the *task class*.

The case for darkmux: **once you accept that static configs leave performance on the table — and that the right configuration depends on the task class, not the model — the routing layer becomes the highest-leverage piece of infrastructure missing from the local-AI stack.**

## Status

- ✅ v0.1: profile registry, `swap`/`status`/`profiles` CLI
- ✅ v0.2 foundation: `lab` subcommands, `WorkloadProvider` trait, `prompt` + `coding-task` providers, `quick-q` smoke workload
- 🚧 v0.2 next: real built-in workloads with sandbox seeds, `lab tune`, knob introspection
- 🚧 v0.3: notebook structure + agent-invocable skills
- 🚧 v0.4: team templates (workflow → fleet of agents → profiles)
- 🚧 v0.5: proxy mode (`darkmux serve`) with heuristic task classifier
- 🚧 v0.6: plugin system for community providers and templates

## License

MIT

## Author

Kain Osterholt — [@DarklyEnergized](https://x.com/DarklyEnergized) — Darkly Energized LLC
