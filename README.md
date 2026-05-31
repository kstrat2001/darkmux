# darkmux

**[darkmux.com](https://darkmux.com) · Turn your frontier AI assistant into an engagement-aware orchestrator that distributes AI work across your machines.**

darkmux is the substrate layer — profiles per role, missions per engagement, audit per record, coordination across the fleet. Your frontier holds the engagement context; darkmux gives it the local-AI fleet to operate on.

Built for operators who need to see what their AI fleet did, when, and why.

- 🔒 **Tamper-evident audit trail** — every dispatch, decision, and review captured in a hash-chained per-machine log. Any post-hoc edit to the chain is detectable via `darkmux flow integrity-check`.
- 🤝 **Engagement-aware coordination** — sessions running on different machines share a flow stream, so two Claude Code sessions on two laptops compose into one fleet view rather than two siloed runs.
- 🎯 **Methodology-driven role specialization** — per-role models selected through documented bake-off methodology; evaluation criteria recorded before the comparison runs.
- 🔧 **Operator sovereignty by design** — defaults are overridable, writes are auditable, suggestions are explainable.
- 📊 **Reproducible benchmarks** — `darkmux lab run <workload>` captures timing + trajectory + verify outcome, so empirical claims in the article series can be re-run by anyone with the binary.

**AI-first local-AI orchestrator** — darkmux uses local-AI internally to manage your local-AI workflows. Task-class-aware profile multiplexing, utility-agent dispatch verbs (like `mission propose`), and a mission/sprint lifecycle, on top of LMStudio. Developed on Apple Silicon. **Assumes a frontier orchestrator** (Claude Code) — the engagement work happens in the frontier session.

> **Heads up — read before running.**
> darkmux orchestrates AI tools that execute on your machine. It modifies your local config files (`~/.openclaw/openclaw.json`), sends commands to your local LMStudio server, and — in lab mode — runs AI-generated code in a working directory that is **not a security sandbox**. AI agents can behave unexpectedly. Use it on a machine where that is acceptable. Performance numbers in this README and in the accompanying articles are measured on the author's hardware (M5 Max, 128 GB) and will differ on yours. See [DISCLAIMER.md](./DISCLAIMER.md) for details. MIT licensed, no warranty, use at your own risk.

## What darkmux is for

Local AI is good at things frontier models aren't — fast iteration, repeatability across hardware, an offline foundation. darkmux exists to make frontier models *better* by giving them complementary teammates running locally. **Diversity-as-synergy, not replacement.**

**The bet.** Faster and better LLM results require solving the *harness* problem, and go hand-in-hand with the *modeling* problem. The [Genesis series](https://darklyenergized.substack.com) on Darkly Energized — three Substack posts — walked from casual comparison through controlled experimentation to the architectural reveal that produced this tool. The same model on the same machine ranged from 25 minutes to 5 minutes depending on which compaction settings the operator chose. darkmux is the engine that captures what the series found.

**The Genesis series.**

- **[Darkmux Genesis I](https://darklyenergized.substack.com/p/can-a-35b-local-model-write-your)** — *Can a 35B Local Model Write Your Unit Tests?* Sweep many models on the same workload, pick a few that work locally.
- **[Darkmux Genesis II](https://darklyenergized.substack.com/p/part-2-charting-the-wake)** — *Charting the Wake.* Tune the knobs scientifically; get the most out of the right model. Empirical finding: configuration drift around the model matters more than the model.
- **[Darkmux Genesis III](https://darklyenergized.substack.com/p/darkmux-genesis-iii-hybrid-by-design)** — *Hybrid by Design.* The architectural reveal — what the operator-orchestrator-darkmux-local-stack continuum actually looks like once the protocol survives the orchestrator deciding where work goes. Closes the Genesis arc.

Posts after the Genesis series will appear on the same Substack but stand alone — the trilogy is the *why this exists* story; subsequent work is the *what's next*.

**The long view.** Once a local stack is optimized, the next step is collaboration — frontier, local, and specialized models on the same task. The optimizing agent (Claude, by default) is part of the team being optimized; it maintains the team's integrity as new models arrive and hardware changes. Optimization is not the destination — it's how the team stays sharp.

**Measurable goal.** A fresh frontier-model context, after a clean-slate `cargo install --path .`, should reach this same conclusion about darkmux's purpose without external prompting. If it doesn't, the docs (this README + `CLAUDE.md`) have drifted from the vision and that's the next thing to fix. See [#35](https://github.com/kstrat2001/darkmux/issues/35) for the maintainer's long-form answer.

> **Results will vary based on your frontier configuration.** The frontier models you use as the orchestrator need proper guidance to make the most out of darkmux. This README and the [user guide](https://darkmux.com/guide/) are a starting reference, not doctrine to enforce. Contradictory statements between this guide, your project's `CLAUDE.md`, and other frontier configs will cause more harm than good. Configure to your own strategy and goals; treat what's here as inspiration, not commandments. See [#112](https://github.com/kstrat2001/darkmux/issues/112) for the architectural reasoning.

## Who darkmux is for

Hobbyists building local-AI workflows on their own Macs. Individual engineers who want a serious agent stack running across the machines they already own. A few Macs over a tailnet (Tailscale, ZeroTier, WireGuard — your call) is the natural deployment shape; one operator who trusts every machine in their own fleet.

Not *designed* as team tooling or a multi-tenant platform. The technical surface (no auth on `DARKMUX_REDIS_URL` beyond what your mesh VPN already provides, operator-asserted provenance fields, cross-machine state on a shared substrate) assumes everyone reachable on the substrate is you. If team scope is interesting to you, the substrate is a reasonable starting point — fork it, layer in auth where you need it, and the project's design will likely benefit from the lessons. Bigger orgs have their own infrastructure for the multi-tenant problem and darkmux stays focused on the one-operator-many-Macs case; that's not a fence, it's a focus.

## Two ways to run darkmux

Pick whichever matches your setup — switchable per dispatch, not a one-time install decision:

- **Standalone** (default): with just Docker + LMStudio, darkmux dispatches through its built-in internal runtime. No external agent runtime to install or configure. The out-of-box path for `darkmux crew dispatch`, `darkmux lab run`, and the mission/sprint lifecycle.
- **With your existing openclaw**: if openclaw is already in your stack, `darkmux crew dispatch --runtime openclaw` (or `darkmux lab run --runtime openclaw`) routes through it. Your existing sessions, channel routing, custom agents, and openclaw-specific tools (`update_plan`, `process`) keep working as-is. `darkmux crew sync` aligns openclaw's `agents.list[]` with darkmux's role manifests so the two stay in step.

**darkmux is not a replacement for openclaw.** The standalone path exists so fresh operators don't need to install a second tool to get started. The openclaw path exists so operators with openclaw already wired in keep their workflow without translation. Both are first-class; the choice is per-dispatch.

See [DESIGN.md → "Relationship to openclaw"](DESIGN.md#relationship-to-openclaw) for the side-by-side comparison (install footprint, isolation model, session model, tool surface) and the scope filter for what gets added to each path.

## Many machines become one

If you have more than one Mac, darkmux makes them work as a single development environment. Operator names a role; darkmux figures out which machine runs it. Open the topology viewer from any node — you see the whole fleet. Open the fleet status from any node — you see specs, RAM, loaded models per machine.

Concretely, the capabilities the multi-machine substrate ships today:

- **Tier-aware dispatch routing.** Declare each machine's tier (`inference` / `hub` / `client`) once in `DARKMUX_MACHINE_TIER`. Tag each role's manifest with the tier it belongs on. `darkmux crew dispatch coder` from a hub-tier machine auto-routes to an inference-tier peer; consumer-group claim decides which one (#247).
- **Fleet status with specs.** `darkmux fleet status --deep` fans out across every reachable peer's `/machine/specs` endpoint — RAM-free, loaded models, OS, darkmux version, redacted Redis URL — in one table (#275).
- **Decentralized flow UI.** The topology viewer at `docs/topology/index.html` aggregates events from every machine writing to the shared `darkmux:flow` Redis stream. Open it on any peer running the daemon and see the fleet, not just the host machine (#270).
- **`/darkmux-add-machine` skill.** Walkthrough for joining a new Mac to an existing fleet — env vars, roster setup, smoke test. Run `darkmux init` to install all skills locally (#176).

Deployment shape that this assumes: a couple of Macs on a tailnet you control (Tailscale, ZeroTier, WireGuard — your call), with Redis running on the always-on member (typically `hub` tier). Redis is optional; without it, single-machine usage works fine and `LocalFileSink` captures provenance on disk per-machine.

If your hub machine drops off the network, the substrate degrades gracefully — flow writes fall back to `LocalFileSink`, dispatch bails loud with operator-actionable hints, `darkmux doctor` surfaces the degraded state, and the SSE Redis tail exits cleanly after a bounded number of failures rather than leaking spawned tasks. The verification discipline that matters here is "make sure your hub is hardened for the absences you plan" — `pmset` config + Tailscale "Run at login" + auto-login user. macOS defaults assume "laptop closed = sleep"; they're wrong for a 24/7 hub.

## Quick start

### Prerequisites

**Out of the box, darkmux works with LMStudio + Docker.** Nothing else is required for the full dispatch + lab path. Other agent runtimes are opt-in.

| Required | Why | Install |
|---|---|---|
| **[LMStudio](https://lmstudio.ai/)** | Loads/unloads models. darkmux drives it via the `lms` CLI. | macOS / Windows / Linux installer |
| **At least one model in LMStudio** | Nothing to swap to without one. | Download via the LMStudio UI; verify with `lms ls`. |
| **[Docker](https://www.docker.com/products/docker-desktop)** | Hosts darkmux's internal Rust runtime — the default for `darkmux crew dispatch` and `darkmux lab run`. Each dispatch runs in a per-invocation `darkmux-runtime` container with kernel-enforced workspace isolation. Build the image once: `docker build -t darkmux-runtime:latest runtime/`. | Docker Desktop or equivalent daemon |
| **Rust toolchain** | To build darkmux itself. | `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh` |

| Optional | When you'd want it |
|---|---|
| **[OpenClaw](https://github.com/openclaw/openclaw)** (or Aider / Cline) | If you're already running openclaw and want darkmux to dispatch through it instead of (or alongside) the internal runtime — pass `--runtime openclaw` per dispatch. `darkmux crew sync` aligns openclaw's agent registry with darkmux's role manifests. See [the dual-mode framing](#two-ways-to-run-darkmux) above. `swap`/`status`/`profiles` work without any external runtime. Override the openclaw binary path per dispatch with `--runtime-cmd <path>`.<br>**Version:** no hard OpenClaw version is required — darkmux only writes to `openclaw.json` on the opt-in OC path (`crew sync`, `--runtime openclaw`, or `swap` when openclaw is already present). darkmux is developed and tested against OpenClaw **2026.5.4**; much older OpenClaw (pre-`2026.3.x`) had a `systemPromptOverride` regression in the config darkmux writes there. `darkmux doctor --include-openclaw` warns (non-blocking) if yours predates the tested version — upgrade via your openclaw checkout (`git pull` + openclaw's own build steps) if it bites. |
| **[Claude Code](https://claude.com/claude-code)** | Only for the agent-invokable skills (`/darkmux-status`, etc.). darkmux as a CLI works without it. |

darkmux is developed and tested on Apple Silicon. Linux should work; Intel Mac is untested.

### Install + bootstrap

One copy-pasteable block — works from a fresh machine with LMStudio + Docker installed:

```bash
# 1. Install Rust toolchain (skip if `cargo --version` already works)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"   # so this shell sees the new cargo immediately

# 2. Clone + build darkmux
git clone https://github.com/kstrat2001/darkmux
cd darkmux
cargo install --path .      # builds the self-contained binary, drops it on $PATH

# 3. Build the internal-runtime container image (one-time, ~50 MB)
docker build -t darkmux-runtime:latest runtime/

# 4. Bootstrap config + agent skills
darkmux init                # creates ~/.darkmux/profiles.json + installs agent skills
                            # (skills include /darkmux-bootstrap — a guided
                            # first-time setup workflow you run in your
                            # Claude Code session after install)
```

If `cargo` is already on your PATH, skip Step 1. The `source "$HOME/.cargo/env"` line is the one most often missed by first-time-Rust users — without it, a fresh `cargo install` fails with `command not found: cargo` in the same shell that just ran the rustup installer.

### Verify your setup

```bash
darkmux doctor          # pre-flight checks: registry, LMStudio, models, runtime, RAM, power,
                        # flow substrate, audit integrity, model-pin drift, recommendation drift, …
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
darkmux optimize                 # guided "optimize for my workload" wizard (Phase 1 scaffold)
darkmux lab inspect <run-id>      # full per-run breakdown
darkmux notebook draft <run-id>   # ask the active role to author a lab-style notebook entry
darkmux mission propose --from-stdin   # AI-built-in: vague intent → structured Mission + Sprint JSONs
```

Using Claude Code? Run `darkmux init --with-claude-md ~/.claude/CLAUDE.md` to install the skills *and* teach Claude Code about darkmux at session start. Then run **`/darkmux-bootstrap`** in your Claude Code session — it walks through detecting your hardware tier, downloading the recommended models, registering profiles, and validating the end state. (The recommendation registry's picks are surfaced via `darkmux recommendations show`; the underlying methodology is bake-off — head-to-head comparison with evaluation criteria recorded before the runs.) Operator-sovereign: the skill reads + proposes; you run the commands.

### Updating darkmux

After pulling new commits:

```bash
git pull
cargo install --path . --force
```

The `--force` flag tells cargo to replace the existing binary even when the source path or version metadata hasn't changed. Without it, cargo can silently skip the reinstall and leave you running an older binary while reporting the same `darkmux --version`. If a new feature (like `--instrument`) is missing despite a fresh `git pull`, that's the most likely cause — re-run with `--force`.

## Why this exists

The long-form answer is the [Genesis series](https://darklyenergized.substack.com) on Darkly Energized — three Substack posts that walk the genesis story end-to-end. The README is the short version.

**AI-first because today you'd be crazy not to.** Pre-AI, integrating a new source or structuring an unstructured intent meant writing a bespoke parser; tools for local-AI orchestration meant operators hand-authoring JSON for missions, sprints, and profiles. With AI in the loop — specifically a small, fast, dependable *utility agent* loaded locally — that authoring tax mostly evaporates. darkmux dispatches utility agents internally for compaction, sprint estimation, and (per [#113](https://github.com/kstrat2001/darkmux/issues/113)) mission proposal, so the operator gets structured output from vague intent without leaving the local tier. The frontier orchestrator stays on strategy; the utility agent absorbs the routine.

The other half of the answer is the original one: local-AI users hit a real workload-tax problem when they go agentic. A single static configuration can't be optimal across:

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

darkmux is a CLI binary, not an HTTP proxy. Your frontier session (Claude Code) invokes `darkmux` verbs directly to operate four substrates:

1. **Profile multiplexing.** `darkmux swap <profile>` unloads + loads models in LMStudio according to a named profile in `~/.darkmux/profiles.json`. `darkmux swap recommended` resolves the active hardware tier to the bake-off-validated profile + pre-flight-checks the required models are downloaded. `~10s` wall to swap.

2. **Crew + mission + sprint lifecycle.** `darkmux crew dispatch <role>` invokes a per-role-pinned agent (coder, code-reviewer, scribe, …) via the in-house container-bounded runtime by default (or openclaw via `--runtime openclaw`). `darkmux mission propose` + `darkmux sprint estimate` are utility-AI verbs that turn vague intent into structured Mission + Sprint JSONs without the operator authoring them by hand. Each dispatch emits a flow record carrying provenance: `machine_id`, `orchestrator`, role, model, mission, sprint.

3. **Flow substrate.** Every dispatch, decision, and review is recorded as a structured JSONL event. `LocalFileSink` (always-on) writes to `~/.darkmux/flows/`. `AuditFileSink` (opt-in via `DARKMUX_AUDIT_DIR`) adds a BLAKE3 hash chain for tamper-evidence. `RedisSink` (opt-in via `DARKMUX_REDIS_URL`) adds a cross-machine coordination stream. `darkmux flow status` introspects the substrate; `darkmux flow integrity-check` walks the audit chain.

4. **Observability daemon.** `darkmux serve` is a local HTTP daemon (default bind `127.0.0.1:8765`) that serves flow records + mission/sprint state + the new `/flow-status` endpoint to the `/flow` + `/lab` viewers. Endpoints: `/health`, `/flow/<date>(.jsonl)`, `/flow/<date>/stream` (SSE tail), `/model/status`, `/missions`, `/sprints`, `/flow-status`. Foreground process — run in a separate terminal tab. `darkmux doctor` includes a `daemon: reachable` check; dispatches print a one-line stderr nudge when the daemon isn't reachable.

Both `crew dispatch` and `lab run` use the internal Docker-bounded runtime by default; pass `--runtime openclaw` to opt into the openclaw shell-out path. Override the openclaw binary path per dispatch with `--runtime-cmd <path>` (e.g. for Aider, Cline, or any tool exposing the `<cmd> agent --message` surface). The frontier session (Claude Code) orchestrates the whole thing — see the `/darkmux-bootstrap` skill for a guided walkthrough.

## Why "darkmux"

- **dark** — Darkly Energized lineage (the experimental work that motivated it)
- **mux** — multiplexer (well-known engineering jargon for routing N → 1 or 1 → N)

OSS-published under personal GitHub: `github.com/kstrat2001/darkmux`. Darkly Energized is the brand context but darkmux is intentionally independent (no commercial coupling).

The name comes from the multiplexer core — task-class-aware routing of LMStudio loadouts. The project has since grown into an AI-first local-AI orchestrator: small CLI primitives for the routing, plus AI-built-in verbs (`mission propose`, `sprint estimate`, `notebook draft`) that compose those primitives with utility-agent dispatch so the operator gets structured output without writing JSON by hand. Earlier framings of darkmux as *"infrastructure, not an agent framework"* were honest at the time, but the binary today embeds AI dispatch logic internally — calling it AI-first out loud is the honest move.

## Design principles

1. **Compose, don't reinvent.** LMStudio already exposes load/unload via `lms`. Don't replace it; orchestrate it.
2. **Profiles are config, not code.** Named profiles in a JSON file. Add a profile by editing config, not by writing a plugin.
3. **Heuristic classification first, LLM classification later.** Free heuristics (prompt length, channel, agent role, file pattern) get most of the way without burning inference cycles.
4. **OpenAI-compatible everywhere.** Frontend, backend, and config syntax all use the established OpenAI surface so existing agents drop in.
5. **Honest about limits.** A router only beats static configs by routing correctly. We're explicit about what darkmux DOES NOT do (e.g., it doesn't make LMStudio faster; it makes the right LMStudio config available at the right time).

## Hardware profiles

darkmux ships with three Apple Silicon heuristics providers, tuned for different unified-memory tiers:

| Provider | Target RAM | Status |
|---|---|---|
| `m-series-128` | 96 GB+ (M Max / Studio Ultra) | ✅ Validated |
| `m-series-64` | 33–64 GB (M Pro) | ⚠️ Extrapolated from 128GB tier |
| `m-series-32` | up to 32 GB (Mac Studio / MBP) | ⚠️ Extrapolated from 64GB tier |

The `m-series-128` provider's rules are empirically validated against lab measurements. The 64 GB and 32 GB providers use conservative extrapolations — tune down `n_ctx` if you see swap pressure. Non-Apple-Silicon systems fall through to a generic fallback with unvalidated defaults.

## Runtime

`darkmux crew dispatch` uses the **internal runtime** by default — an in-house Rust agent loop running inside a per-dispatch `darkmux-runtime` Docker container with a mounted workspace tempdir. Kernel-enforced workspace isolation, no cross-task context leak by construction. The image is small (~50 MB) and built once from `runtime/`:

```bash
# build the image once from the darkmux repo root
docker build -t darkmux-runtime:latest runtime/
```

Opt into openclaw per-dispatch if you already have it installed:

```bash
darkmux crew dispatch coder --runtime openclaw --message "..."
```

The `lab` subcommand mirrors `crew dispatch`'s contract: internal runtime by default, `--runtime openclaw --runtime-cmd <path>` to opt into any tool exposing a `<cmd> agent --message <text> --json` surface (Aider, Cline, your own wrapper). The `swap` / `status` / `profiles` subcommands don't depend on any runtime at all — they orchestrate LMStudio directly.

When openclaw is in the picture (either as the lab harness or the explicit `--runtime openclaw` dispatch path), `darkmux swap` and `darkmux doctor --fix` patch the openclaw config file in place. Path resolution: any profile's `runtime.config_path` wins; otherwise darkmux honors the `DARKMUX_OPENCLAW_CONFIG` env var; otherwise it falls back to `~/.openclaw/openclaw.json`. Set the env var if your openclaw lives somewhere non-standard:

```bash
export DARKMUX_OPENCLAW_CONFIG="$HOME/work/openclaw-staging/openclaw.json"
```

This means: **darkmux's profile-multiplexing is runtime-agnostic** today; `crew dispatch` ships with a self-contained internal runtime so new users don't need an openclaw install to get going; the lab harness is *runtime-pluggable* via the env var. The empirical findings in the article series happened to be measured against OpenClaw; the routing thesis itself is independent.

### Internal-runtime safety net + model-facing telemetry

The internal runtime watches each dispatch for the failure modes that waste local-AI time, and surfaces what it sees both to the operator (in the trajectory) and to the model (as `[darkmux-runtime]` system-message nudges, the *feedback-injection* channel). All detectors are observability-first — they record and nudge before they ever bail.

- **Struggle detectors** — repeated identical tool calls (*cycle detection*), the same idea re-reasoned in a loop (*reasoning-loop detection*), a tool failing several times in a row (*tool-failure cascade*), and editing one file repeatedly without ever verifying (*cadence drift*). Each writes a trajectory event and, by default, a model-facing nudge.
- **Recovery paths** — well-formed tool calls are *salvaged* when a turn hits the per-call token cap mid-output; runaway "reasoning with no action" turns are dropped, nudged, and retried (*intra-turn stall recovery*); and tool calls the model emitted as plain text instead of structured JSON are *promoted* back to real tool calls.
- **Budget + deadline** — a per-call cap bounds runaway emission; opt-in `--max-turns` / `--max-tokens` (env: `DARKMUX_RUNTIME_MAX_TURNS` / `DARKMUX_RUNTIME_MAX_TOKENS`) bound a whole dispatch; and an inactivity deadline (`DARKMUX_INACTIVITY_TIMEOUT_SECONDS`, default 600) fires a soft model-facing warning at 75% before the host hard-kills at 100%, resetting on any tool call or compaction.

Roles can override the nudge wording per signal via a `feedback_templates` block on the role manifest; operators can disable injection entirely with `DARKMUX_FEEDBACK_INJECTION=0`. The trajectory event reference lives in the `darkmux-analyze-run` skill.

### Cross-machine notebook (multi-environment lab notes)

If you run darkmux on more than one machine and want a single notebook that collates entries from all of them — for example, comparing wall-clock distributions across hardware tiers — point the notebook directory at an iCloud-synced (or otherwise shared) path:

```bash
export DARKMUX_NOTEBOOK_DIR="$HOME/Library/Mobile Documents/com~apple~CloudDocs/darkmux-notebook"
export DARKMUX_MACHINE_ID="m5-max-128gb-home"  # naming is yours; appears in entry headers
darkmux notebook draft <run-id>
```

Set the same `DARKMUX_NOTEBOOK_DIR` on each machine; give each a distinct `DARKMUX_MACHINE_ID`. Entries get tagged with their machine of origin in the header comment, so cross-machine readouts are unambiguous.

If `DARKMUX_MACHINE_ID` is unset, darkmux falls back to an auto-derived fingerprint (e.g. `apple-silicon-128gb`) — fine for casual use, but a named id is recommended when more than one machine of the same tier exists.

You can also override the machine id for a single draft via `--machine`:

```bash
darkmux notebook draft <run-id> --machine my-work-mac
```

To list all entries in the notebook directory (optionally filtered by machine):

```bash
darkmux notebook list           # all entries
darkmux notebook list --machine m5-home  # only this machine's entries
```

`notebook list` outputs columns: **date | machine | run | path** (aligned, newest first). The `--machine` flag filters to only entries matching that machine id.

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

## Why this exists — empirical motivation

Headline findings from the experimental work that produced darkmux's reference profiles:

- **Static config tuning has a floor.** Compaction knobs (`maxHistoryShare`, `recentTurnsPreserve`, `customInstructions`, compactor n_ctx) are tightly coupled — pulling any one of them in isolation regresses the run. Tuning at the config layer eventually stops paying dividends.
- **The "compactor loaded" tax is real.** Keeping a small compactor model warm for offload availability adds ~25s per dispatch on bounded workloads, even when compaction never fires. That cost is fixed and unrelated to compactor context size.
- **Long-task wins are bimodal.** With maximum primary context, multiple dispatches of the *identical* prompt + config split into a fast cluster (single-turn, no compaction fired) and a slow cluster (multi-turn, compaction fired). 3× variance between modes is normal — driven by emergent control-flow decisions inside the model's tool-loop, not by config.
- **Both modes still beat smaller-context baselines.** A router doesn't need to predict which mode a given dispatch will land in; it just needs to pick the right configuration for the *task class*.

The case for darkmux: **once you accept that static configs leave performance on the table — and that the right configuration depends on the task class, not the model — the routing layer becomes the highest-leverage piece of infrastructure missing from the local-AI stack.**

## Status

🚧 **Pre-1.0** — v0.4.0 on `main`; active development; APIs not yet frozen.

**Shipped:**

- ✅ Profile registry + `swap`/`status`/`profiles`/`scan` CLI
- ✅ Lab subcommands (`run`/`inspect`/`compare`/`characterize`/`tune`/`runs`), `WorkloadProvider` trait, embedded smoke workloads, `--instrument` sidecar sampler
- ✅ Lab reproducibility (#487): per-run copy-on-write sandbox isolation (source never mutated), `baseline_hash` + `final_hash` content hashing in the run manifest, a fixture registry with `lab register`/`unregister`/`fixtures`/`doctor` verbs, workload `requires_fixture` resolution, and `scripts/lab-init.sh` + the built-in `demo-tiny-py` fixture
- ✅ Notebook (`notebook draft`/`list`) — cross-machine via `DARKMUX_NOTEBOOK_DIR`
- ✅ Agent-invocable skills bundle (12 skills including `/darkmux-bootstrap`)
- ✅ Crew + Role + Mission + Sprint schema with SQLite-backed index; `mission propose` + `sprint estimate` utility-AI verbs
- ✅ Per-role `agent.model` pinning (#160) with bake-off-derived defaults; doctor surfaces drift
- ✅ Recommendation registry per hardware tier (#159) with `swap recommended` + `model pull-recommended`; doctor surfaces drift
- ✅ Flow substrate: `LocalFileSink` (always) + `AuditFileSink` (BLAKE3 hash chain, verifiable via `flow integrity-check`; opt-in) + `RedisSink` (coordination; opt-in), composed via `TeeSink`
- ✅ `darkmux flow status` + `darkmux flow integrity-check` diagnostic verbs
- ✅ Observability daemon (`darkmux serve`) + `/flow` + `/lab` web viewers
- ✅ Doctor: 20+ pre-flight checks with auto-fix path (`--fix`) for known-safe drift; `--include-openclaw` gates openclaw-specific checks so internal-runtime operators get a clean report, plus a legacy-extras warning that flags profiles still carrying openclaw-shape compaction keys (`mode`, `maxHistoryShare`, …)

**On the roadmap (active):**

- 🚧 Topology view in the web viewer (live + replay diagram of fleet activity; #169)
- 🚧 Fleet primitives (`darkmux fleet add/status/route`) and cross-machine coordination (Phase 5 of #162)
- 🚧 Event-sourced mission state (Phase 8 of #162)
- 🚧 Sibling bootstrap skills: `/darkmux-add-machine` (#176), `/darkmux-enable-audit` (#177), `/darkmux-enable-redis` (#178)
- 🚧 Audit log management: `flow export`, `flow archive`, OS-level append-only flags for audit files
- 🚧 Multi-frontier orchestrator support (Gemini / Codex / Copilot bootstrap paths; #179)

**Aspirational (later):**

- 🚧 Plugin system for community-contributed providers, workloads, role manifests
- 🚧 Per-role bake-offs for non-SWE roles (trip-researcher, health-research, legal-research, …)

## License

MIT

## Author

Kain Osterholt — [@DarklyEnergized](https://x.com/DarklyEnergized) — Darkly Energized LLC

---

*Claude, Claude Code are trademarks of Anthropic PBC. LMStudio is a trademark of Element Labs Inc. darkmux is not affiliated with either.*
