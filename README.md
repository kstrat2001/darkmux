# darkmux

**[darkmux.com](https://darkmux.com) · Turn your frontier AI assistant into an engagement-aware orchestrator that distributes AI work across your machines.**

darkmux is the substrate layer: profiles per role, missions per engagement, audit per record, coordination across the fleet. Your frontier holds the engagement context; darkmux gives it the local-AI fleet to operate on.

Built for operators who need to see what their AI fleet did, when, and why.

- 🔒 **Hash-chained audit trail with edit detection.** Every dispatch, decision, and review is captured in a BLAKE3 hash-chained per-machine log; `darkmux flow integrity-check` surfaces any post-hoc edit to the chain (exits 2 on chain break, suitable for cron / CI gating).
- 🤝 **Engagement-aware coordination.** Sessions running on different machines share a flow stream, so two Claude Code sessions on two laptops compose into one fleet view rather than two siloed runs.
- 🎯 **Methodology-driven role specialization.** Per-role models selected through documented bake-off methodology; evaluation criteria recorded before the comparison runs.
- 🔧 **Operator sovereignty by design.** Defaults are overridable, writes are auditable, suggestions are explainable.
- 📊 **Reproducible benchmarks.** `darkmux lab run <workload>` captures timing + trajectory + verify outcome, so anyone with the binary can re-run the empirical claims from the article series.

**AI-first local-AI orchestrator.** darkmux uses local-AI internally to manage your local-AI workflows. Task-class-aware profile multiplexing, utility-agent dispatch verbs (like `mission propose`), and a mission/phase lifecycle, on top of LMStudio. Developed on Apple Silicon. **Assumes a frontier orchestrator** (Claude Code): the engagement work happens in the frontier session.

> **Heads up: read before running.**
> darkmux orchestrates AI tools that execute on your machine. It sends commands to your local LMStudio server and, in lab mode, runs AI-generated code in a working directory that is **not a security sandbox**. AI agents can behave unexpectedly. Use it on a machine where that is acceptable. Performance numbers in this README and the accompanying articles are measured on the author's hardware (M5 Max, 128 GB) and will differ on yours. See [DISCLAIMER.md](./DISCLAIMER.md) for details. MIT licensed, no warranty, use at your own risk.

## What darkmux is for

Local AI is good at things frontier models aren't: fast iteration, repeatability across hardware, an offline foundation. darkmux exists to make frontier models *better* by giving them complementary teammates running locally. **Diversity-as-synergy, not replacement.**

**The bet.** Faster and better LLM results require solving the *harness* problem, and go hand-in-hand with the *modeling* problem. The [Genesis series](https://darklyenergized.substack.com) on Darkly Energized (three Substack posts) walks from casual comparison through controlled experimentation to the architectural reveal that produced this tool. The same model on the same machine ranged from 25 minutes to 5 minutes depending on the compaction settings the operator chose. darkmux is the engine that captures what the series found.

**The Genesis series.**

- **[Darkmux Genesis I](https://darklyenergized.substack.com/p/can-a-35b-local-model-write-your)**: *Can a 35B Local Model Write Your Unit Tests?* Sweep many models on the same workload, pick a few that work locally.
- **[Darkmux Genesis II](https://darklyenergized.substack.com/p/part-2-charting-the-wake)**: *Charting the Wake.* Tune the knobs scientifically to get the most out of the right model. Empirical finding: configuration drift around the model matters more than the model.
- **[Darkmux Genesis III](https://darklyenergized.substack.com/p/darkmux-genesis-iii-hybrid-by-design)**: *Hybrid by Design.* The architectural reveal: what the operator-orchestrator-darkmux-local-stack continuum actually looks like once the protocol survives the orchestrator deciding where work goes. Closes the Genesis arc.

Posts after the Genesis series will appear on the same Substack but stand alone. The trilogy is the *why this exists* story; subsequent work is the *what's next*.

**The long view.** Once a local stack is optimized, the next step is collaboration: frontier, local, and specialized models on the same task. The optimizing agent (Claude, by default) is part of the team being optimized; it maintains the team's integrity as new models arrive and hardware changes. Optimization is not the destination; it's how the team stays sharp.

**Measurable goal.** A fresh frontier-model context, after a clean-slate `cargo install --path .`, should reach this same conclusion about darkmux's purpose without external prompting. If it doesn't, the docs (this README + `CLAUDE.md`) have drifted from the vision, and that's the next thing to fix. See [#35](https://github.com/kstrat2001/darkmux/issues/35) for the maintainer's long-form answer.

> **Results will vary based on your frontier configuration.** The frontier models you use as the orchestrator need proper guidance to make the most out of darkmux. This README and the [user guide](https://darkmux.com/guide/) are a starting reference, not doctrine to enforce. Contradictory statements between this guide, your project's `CLAUDE.md`, and other frontier configs will cause more harm than good. Configure to your own strategy and goals; treat what's here as inspiration, not commandments. See [#112](https://github.com/kstrat2001/darkmux/issues/112) for the architectural reasoning.

## Who darkmux is for

Hobbyists building local-AI workflows on their own Macs. Individual engineers who want a serious agent stack running across the machines they already own. A few Macs over a tailnet (Tailscale, ZeroTier, WireGuard, your call) is the natural deployment shape: one operator who trusts every machine in their own fleet.

Not *designed* as team tooling or a multi-tenant platform. The technical surface (no auth on `DARKMUX_REDIS_URL` beyond what your mesh VPN already provides, operator-asserted provenance fields, cross-machine state on a shared substrate) assumes everyone reachable on the substrate is you. If team scope is interesting to you, the substrate is a reasonable starting point: fork it, layer in auth where you need it, and the project's design will likely benefit from the lessons. Bigger orgs have their own infrastructure for the multi-tenant problem, and darkmux stays focused on the one-operator-many-Macs case; that's not a fence, it's a focus.

## How darkmux runs

With just Docker + LMStudio, darkmux dispatches through its own built-in, container-bounded runtime — no external agent runtime to install or configure. This is the only dispatch path: `darkmux crew dispatch`, `darkmux lab run`, and the mission/phase lifecycle all run through it. (Earlier versions offered an opt-in shell-out to a separately-installed agent runtime; that path was removed pre-1.0 to keep the build and test surface small — see [#1405](https://github.com/kstrat2001/darkmux/issues/1405).)

See [DESIGN.md](DESIGN.md) for the implementation reasoning.

## Many machines become one

If you have more than one Mac, darkmux makes them work as a single development environment. Operator hands off a role; the first available runner runs it. Open the topology viewer from any node and you see the whole fleet. Open the fleet status from any node and you see specs, RAM, loaded models per machine.

Concretely, the capabilities the multi-machine substrate ships today:

- **Single-stream fleet dispatch.** Every dispatch routes onto one global work stream (`darkmux:work`), and the first available runner claims any job, with no tier configuration to maintain. `darkmux crew dispatch coder --machine <id>` is an *advisory* hint when you want a specific machine; any runner may still claim it. Capability-based auto-routing (match work to the machine best suited to run it) is the planned successor, building on the [#590](https://github.com/kstrat2001/darkmux/issues/590) capability layer.
- **Fleet status with specs.** `darkmux fleet status --deep` fans out across every reachable peer's `/machine/specs` endpoint (RAM-free, loaded models, OS, darkmux version, redacted Redis URL) in one table (#275).
- **Decentralized flow UI.** The daemon hosts the observability viewer at its own origin: `http://localhost:8765/` on every machine running `darkmux serve`. The viewer pulls from the daemon's `/flow/<date>` endpoint which aggregates events from every machine writing to the shared `darkmux:flow` Redis stream, so you see the fleet, not just the host (#270 + #554).
- **`/darkmux-add-machine` skill.** Walkthrough for joining a new Mac to an existing fleet: env vars, roster setup, smoke test. Run `darkmux init` to install all skills locally (#176).

Deployment shape that this assumes: a couple of Macs on a tailnet you control (Tailscale, ZeroTier, WireGuard, your call), with Redis running on the always-on member. Redis is optional; without it, single-machine usage works fine and `LocalFileSink` captures provenance on disk per-machine.

If your hub machine drops off the network, the substrate degrades gracefully: flow writes fall back to `LocalFileSink`, dispatch bails loud with operator-actionable hints, `darkmux doctor` surfaces the degraded state, and the SSE Redis tail exits cleanly after a bounded number of failures rather than leaking spawned tasks. The verification discipline that matters here is "make sure your hub is hardened for the absences you plan": `pmset` config + Tailscale "Run at login" + auto-login user. macOS defaults assume "laptop closed = sleep"; that's wrong for a 24/7 hub.

### Seeing your fleet

The observability viewer is hosted by `darkmux serve` itself, not by the public site. Three URL patterns depending on what you're looking at:

| URL | Role | When to use |
|---|---|---|
| [`darkmux.com/demo`](https://darkmux.com/demo) | Demo with bundled sample scenario | First impression: see what the viewer does before installing anything |
| `http://localhost:8765/` | Your own daemon, live | Single-machine fleet, or local-only ops view on a multi-machine fleet |
| `https://<hub>.<your-tailnet>.ts.net/` | Hub's daemon via Tailscale Serve | Multi-machine fleet: load the hub's fleet view from any peer on your tailnet |

The third one is opt-in (the daemon binds localhost by default for safety). To expose it across your tailnet, see the [secure hub guide → the cross-tailnet viewer](https://darkmux.com/guide/always-on-hub.html#viewer). Tailscale Serve is the recommended path: it terminates HTTPS at the tailnet node and proxies to the daemon, which stays bound to localhost (so darkmux's remote-auth gate doesn't block a browser page-load). Never bind the daemon to `0.0.0.0`: keep it on loopback and let the tailnet do the reaching.

## Quick start

### Prerequisites

**Out of the box, darkmux works with LMStudio + Docker.** Nothing else is required for the full dispatch + lab path. Other agent runtimes are opt-in.

| Required | Why | Install |
|---|---|---|
| **[LMStudio](https://lmstudio.ai/)** | Loads/unloads models. darkmux drives it via the `lms` CLI. | macOS / Windows / Linux installer |
| **At least one model in LMStudio** | Nothing to swap to without one. | Download via the LMStudio UI; verify with `lms ls`. |
| **[Docker](https://www.docker.com/products/docker-desktop)** | Hosts darkmux's internal Rust runtime, the default for `darkmux crew dispatch` and `darkmux lab run`. Each dispatch runs in a per-invocation `darkmux-runtime` container with kernel-enforced workspace isolation. darkmux pulls the image from GHCR on demand (or `docker build -t darkmux-runtime:latest runtime/` from a source checkout). **Required only for that dispatch + lab path:** the `swap` / `status` / `profiles` core needs only LMStudio + a model. | Docker Desktop or equivalent daemon |

> **`brew install` needs no toolchain.** Homebrew handles the build for you (and bottled binaries, once published, ship precompiled). The **Rust toolchain** is required only if you build from source (Option B below), which documents `rustup` at its first step.

| Optional | When you'd want it |
|---|---|
| **[Claude Code](https://claude.com/claude-code)** | The recommended way to drive darkmux. A frontier orchestrator (Claude Code, Cursor, Gemini, Antigravity, Codex, Copilot) operates the CLI verbs and the `/darkmux-*` skills; standalone CLI use works for scripting and cron, but orchestrator-driven dispatch is the design. |

darkmux is developed and tested on Apple Silicon. Linux should work; Intel Mac is untested.

### Install + bootstrap

**Option A: via Homebrew tap** (recommended; tap lives at [`kstrat2001/homebrew-darkmux`](https://github.com/kstrat2001/homebrew-darkmux)):

```bash
brew tap kstrat2001/darkmux
brew install darkmux                  # stable release
# brew install --HEAD darkmux         # or build from the latest commit on main

# Optional: hub posture (Redis as the coordination substrate)
brew install redis
brew services start redis

# Optional: run the daemon under launchd (KeepAlive + RunAtLoad)
brew services start darkmux
```

> **If `brew install` refuses with "untrusted tap":** newer Homebrew gates third-party taps behind an explicit trust step. Run `brew trust kstrat2001/darkmux` once, then re-run the install. (Older Homebrew versions don't require this and won't show the prompt.)

The brew formula installs both the `darkmux` binary AND a keychain-aware wrapper script (`libexec/darkmux-serve-wrapped`) that resolves `DARKMUX_REDIS_URL` from macOS Keychain at process-start, so the Redis password never lives in the launchd plist. See [the always-on hub guide](docs/guide/always-on-hub.html) for the production-grade setup.

**Scope of the brew install.** What you get: the `darkmux` CLI (swap, profiles, status, doctor, fleet, flow, init), the `serve` daemon, the keychain wrapper, and the bundled skills. The `darkmux-runtime` Docker image that `darkmux crew dispatch` / `darkmux lab run` need is **not bundled in the formula** but you don't build it by hand: on the first dispatch with no local image, darkmux **pulls the version-pinned image from GHCR on demand** (`ghcr.io/kstrat2001/darkmux-runtime:<version>`, [#759](https://github.com/kstrat2001/darkmux/issues/759)). You just need Docker running. (A `runtime/` source checkout + `docker build` is the offline/dev alternative.) So the brew path is a complete install end to end: the `swap` / `status` / `profiles` core, the hub posture (Redis + serve), **and** local dispatches.

**Option B: from source via cargo** (for dev work, contributors, or if you need the `darkmux-runtime` Docker image alongside the binary):

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
darkmux init                # writes ~/.darkmux/config.json + ~/.darkmux/profiles.json,
                            # installs agent skills (incl. /darkmux-bootstrap, a guided
                            # first-time setup workflow you run in your Claude Code
                            # session after install). Never overwrites existing files.
```

If `cargo` is already on your PATH, skip Step 1. The `source "$HOME/.cargo/env"` line is the one most often missed by first-time-Rust users. Without it, a fresh `cargo install` fails with `command not found: cargo` in the same shell that just ran the rustup installer.

### Verify your setup

```bash
darkmux doctor          # pre-flight checks: registry, LMStudio, models, runtime, RAM, power,
                        # flow substrate, audit integrity, model-pin drift, recommendation drift, …
```

Doctor returns exit 0 if everything's wired up, exit 1 if a fail-level check needs fixing. Fail/warn lines include actionable hints.

Once doctor is green, point your profiles at real models. The fastest path is `darkmux profile scan`: it lists the models LMStudio has downloaded that aren't yet in any profile and suggests which are worth adding, so you don't have to hand-match ids:

```bash
darkmux profile scan        # see downloaded models not yet in a profile, with suggestions
```

Or edit `~/.darkmux/profiles.json` directly and replace each `<your-worker-model-id>` placeholder with an actual id from `lms ls`. Either way, doctor will warn if profiles don't match your loaded models; that's the moment to fix them.

### Configuration

`darkmux init` also writes **`~/.darkmux/config.json`**, your one place to configure darkmux. It's self-documenting: every common setting is written with its default visible, so you tune the file instead of hunting through docs.

```json
{
  "machine_id": "studio",
  "lmstudio_url": "http://localhost:1234",
  "redis":   { "enabled": false, "host": "127.0.0.1", "port": 6379 },
  "audit":   { "enabled": false, "dir": "~/.darkmux/audit" },
  "runtime": { "inactivity_timeout_seconds": 600, "check_updates": true }
}
```

Optional integrations (Redis coordination, the audit log) are blocks you turn on by flipping `"enabled": true`; the connection knobs are already there to edit. Every setting also accepts a `DARKMUX_*` environment-variable override (handy for CI or a one-off shell); the precedence is **env var > `config.json` > built-in default**, and `darkmux doctor` shows where each value resolved.

**Secrets stay out of the file.** A Redis password is never written to `config.json`. It lives in the macOS Keychain (store it once: `security add-generic-password -a "$USER" -s darkmux-redis -w`), read at runtime and never logged. On non-macOS, pass a full `DARKMUX_REDIS_URL` instead.

> `config.json` (darkmux's settings) is a separate file from `profiles.json` (your swap profiles). Point at a non-default profiles registry with `--profiles-file <path>` or `DARKMUX_PROFILES`.

### First useful commands

```bash
darkmux profile list                  # list configured profiles
darkmux status                        # what's loaded; which profile (if any) matches
darkmux swap fast                     # swap to the "fast" profile (loads model into LMStudio)
darkmux lab characterize              # one-command "QA my Mac": dispatch a smoke workload, get a verdict
darkmux lab run quick-q               # the smoke workload directly
darkmux lab runs --limit 5            # see your recent runs
darkmux lab inspect <run-id>          # full per-run breakdown
darkmux lab notebook draft <run-id>   # ask the active role to author a lab-style notebook entry
darkmux mission propose --from-stdin   # AI-built-in: vague intent → a structured mission config
darkmux mission launch <id>            # mint + start a running mission instance from a config
```

Using Claude Code? Run `darkmux init --with-claude-md ~/.claude/CLAUDE.md` to install the skills *and* teach Claude Code about darkmux at session start. Then run **`/darkmux-bootstrap`** in your Claude Code session: it walks through detecting your hardware tier, downloading the recommended models, registering profiles, and validating the end state. (The recommendation registry's picks are surfaced via `darkmux recommendations show`; the underlying methodology is bake-off: head-to-head comparison with evaluation criteria recorded before the runs.) Operator-sovereign: the skill reads + proposes; you run the commands.

### Updating darkmux

**If you installed via Homebrew tap:**

```bash
brew upgrade darkmux                  # picks up the latest tagged release
brew services restart darkmux         # if you're running the daemon
```

To update: `brew upgrade darkmux`. For `--HEAD` installs, `brew upgrade --HEAD darkmux` pulls the latest commit on main instead.

**If you installed from source via cargo:**

```bash
git pull
cargo install --path . --force
```

The `--force` flag tells cargo to replace the existing binary even when the source path or version metadata hasn't changed. Without it, cargo can silently skip the reinstall and leave you running an older binary while reporting the same `darkmux --version`. If a new feature (like `lab register`) is missing despite a fresh `git pull`, that's the most likely cause. Re-run with `--force`.

## Why this exists

The long-form answer is the [Genesis series](https://darklyenergized.substack.com) on Darkly Energized (three Substack posts that walk the genesis story end-to-end). The README is the short version.

**AI-first because today you'd be crazy not to.** Pre-AI, integrating a new source or structuring an unstructured intent meant writing a bespoke parser; tools for local-AI orchestration meant operators hand-authoring JSON for missions, phases, and profiles. With AI in the loop (specifically a small, fast, dependable *utility agent* loaded locally), that authoring tax mostly evaporates. darkmux dispatches utility agents internally for compaction, phase estimation, and (per [#113](https://github.com/kstrat2001/darkmux/issues/113)) mission proposal, so the operator gets structured output from vague intent without leaving the local tier. The frontier orchestrator stays on strategy; the utility agent absorbs the routine.

The other half of the answer is the original one: local-AI users hit a real workload-tax problem when they go agentic. A single static configuration can't be optimal across:

- **Bounded tasks** (TODO fills, single-turn reviews): want a slim primary, no compaction overhead, fast decode
- **Long agentic tasks** (multi-file refactors, exploratory test authoring): want big context to avoid compaction cliffs, even at the cost of bigger KV pre-allocation
- **Mid-range tasks**: want compaction-tuned middle ground

Empirical data behind this (from the work that motivated darkmux):

| Workload | Slim config (no offload, 32-64K) | Mid config (101K + 68K compactor) | Big config (262K + 120K compactor) |
|---|---|---|---|
| Bounded TODO | **60s** ✓ | 87s | 82s |
| Long agentic (n=6) | (would risk overflow) | 478s baseline | **mean 406s, fast 222s, slow 773s** |

Bigger context wins long tasks. Slim config wins bounded tasks. **No static config is optimal across both regimes.** But a router can be.

## What darkmux does

darkmux is a CLI binary, not an HTTP proxy. Your frontier session (Claude Code) invokes `darkmux` verbs directly to operate four substrates:

1. **Profile multiplexing.** `darkmux swap <profile>` unloads + loads models in LMStudio according to a named profile in `~/.darkmux/profiles.json`. `darkmux swap recommended` resolves the active hardware tier to the bake-off-validated profile + pre-flight-checks the required models are downloaded. `~10s` wall to swap.

2. **Crew + mission + phase lifecycle.** `darkmux crew dispatch <role>` invokes a per-role-pinned agent (coder, code-reviewer, scribe, …) via the in-house container-bounded runtime. `darkmux mission propose` + `darkmux phase estimate` are utility-AI verbs that turn vague intent into a structured mission config without the operator authoring it by hand; `darkmux mission launch <id>` mints the running mission instance from that config. Each dispatch emits a flow record carrying provenance: `machine_id`, `orchestrator`, role, model, mission, phase.

3. **Flow substrate.** Every dispatch, decision, and review is recorded as a structured JSONL event. `LocalFileSink` (always-on) writes to `~/.darkmux/flows/`. `AuditFileSink` (opt-in via `DARKMUX_AUDIT_DIR`) adds a BLAKE3 hash chain whose edits `flow integrity-check` detects (un-anchored: detects edits absent a full re-chain). `RedisSink` (opt-in via `DARKMUX_REDIS_URL`) adds a cross-machine coordination stream. `darkmux flow status` introspects the substrate; `darkmux flow integrity-check` walks the audit chain.

4. **Observability daemon.** `darkmux serve` is a local HTTP daemon (default bind `127.0.0.1:8765`) that serves flow records + mission/phase state + the new `/flow-status` endpoint to the `/flow` + `/lab` viewers. Endpoints: `/health`, `/flow/<date>(.jsonl)`, `/flow/<date>/stream` (SSE tail), `/model/status`, `/missions`, `/phases`, `/flow-status`. Foreground process: run in a separate terminal tab. `darkmux doctor` includes a `daemon: reachable` check; dispatches print a one-line stderr nudge when the daemon isn't reachable.

Both `crew dispatch` and `lab run` use the internal Docker-bounded runtime. The frontier session (Claude Code) orchestrates the whole thing: see the `/darkmux-bootstrap` skill for a guided walkthrough.

## Why "darkmux"

- **dark**: Darkly Energized lineage (the experimental work that motivated it)
- **mux**: multiplexer (well-known engineering jargon for routing N → 1 or 1 → N)

OSS-published under personal GitHub: `github.com/kstrat2001/darkmux`. Darkly Energized is the brand context but darkmux is intentionally independent (no commercial coupling).

The name comes from the multiplexer core: task-class-aware routing of LMStudio loadouts. The project has since grown into an AI-first local-AI orchestrator: small CLI primitives for the routing, plus AI-built-in verbs (`mission propose`, `phase estimate`, `notebook draft`) that compose those primitives with utility-agent dispatch so the operator gets structured output without writing JSON by hand. Earlier framings of darkmux as *"infrastructure, not an agent framework"* were honest at the time, but the binary today embeds AI dispatch logic internally, so calling it AI-first out loud is the honest move.

## Design principles

1. **Compose, don't reinvent.** LMStudio already exposes load/unload via `lms`. Don't replace it; orchestrate it.
2. **Profiles are config, not code.** Named profiles in a JSON file. Add a profile by editing config, not by writing a plugin.
3. **Heuristic classification first, LLM classification later.** Free heuristics (prompt length, channel, agent role, file pattern) get most of the way without burning inference cycles.
4. **OpenAI-compatible everywhere.** Frontend, backend, and config syntax all use the established OpenAI surface so existing agents drop in.
5. **Honest about limits.** A router only beats static configs by routing correctly. We're explicit about what darkmux does NOT do (e.g., it doesn't make LMStudio faster; it makes the right LMStudio config available at the right time).
6. **Config on an existing kind beats a new type.** Missions run as `Task`/`Step` graphs (`crates/darkmux-crew/src/step_kinds/`), and a `Step`'s `kind` is a registered Rust implementation. Before writing a new one, check whether the actual need is just new VALUES on an existing generic kind (`dispatch.internal`, `dispatch.single_shot`, `procedural.shell`, `procedural.noop` — the `builtins.rs` default). Only when the control-flow *shape* itself is genuinely new does it earn a new type, and even then: a reusable shape with a pluggable domain algorithm belongs in `step_kinds/patterns/` (e.g. the multi-pass-confirm and dedup-with-strategies patterns), while a genuinely single-purpose shape stays physically co-located with the mission module that owns it, not the shared crate. A team where every extension point compiles a new bespoke type is fighting the same "hard-wire every use case" failure mode this project exists to avoid at the model-orchestration layer — don't let it recur at the code-extension layer.

## Hardware profiles

darkmux ships with three Apple Silicon heuristics providers, tuned for different unified-memory tiers:

| Provider | Target RAM | Status |
|---|---|---|
| `m-series-128` | 96 GB+ (M Max / Studio Ultra) | ✅ Validated |
| `m-series-64` | 33–64 GB (M Pro) | ⚠️ Extrapolated from 128GB tier |
| `m-series-32` | up to 32 GB (Mac Studio / MBP) | ⚠️ Extrapolated from 64GB tier |

The `m-series-128` provider's rules are empirically validated against lab measurements. The 64 GB and 32 GB providers use conservative extrapolations; tune down `n_ctx` if you see swap pressure. Non-Apple-Silicon systems fall through to a generic fallback with unvalidated defaults.

## Runtime

`darkmux crew dispatch` uses the **internal runtime** by default: an in-house Rust agent loop running inside a per-dispatch `darkmux-runtime` Docker container with a mounted workspace tempdir. Kernel-enforced workspace isolation, no cross-task context leak by construction. The image is small (~50 MB) and built once from `runtime/`:

```bash
# build the image once from the darkmux repo root
docker build -t darkmux-runtime:latest runtime/
```

The `lab` subcommand mirrors `crew dispatch`'s contract — the internal runtime, no external agent runtime to install or configure. The `swap` / `status` / `profiles` subcommands don't depend on any runtime at all. They orchestrate LMStudio directly.

This means **darkmux's profile-multiplexing needs nothing beyond Docker + LMStudio**: `crew dispatch` ships with a self-contained internal runtime, so a new user never installs a second agent-runtime tool to get going. The empirical findings in the article series were measured against this runtime.

### Internal-runtime safety net + model-facing telemetry

The internal runtime watches each dispatch for the failure modes that waste local-AI time, and surfaces what it sees both to the operator (in the trajectory) and to the model (as `[darkmux-runtime]` system-message nudges, the *feedback-injection* channel). All detectors are observability-first: they record and nudge before they ever bail.

- **Struggle detectors.** Repeated identical tool calls (*cycle detection*), the same idea re-reasoned in a loop (*reasoning-loop detection*), a tool failing several times in a row (*tool-failure cascade*), and editing one file repeatedly without ever verifying (*cadence drift*). Each writes a trajectory event and, by default, a model-facing nudge.
- **Recovery paths.** Well-formed tool calls are *salvaged* when a turn hits the per-call token cap mid-output; runaway "reasoning with no action" turns are dropped, nudged, and retried (*intra-turn stall recovery*); and tool calls the model emitted as plain text instead of structured JSON are *promoted* back to real tool calls.
- **Budget + deadline.** A per-call cap bounds runaway emission; opt-in `--max-turns` / `--max-tokens` (env: `DARKMUX_RUNTIME_MAX_TURNS` / `DARKMUX_RUNTIME_MAX_TOKENS`) bound a whole dispatch; and an inactivity deadline (`DARKMUX_INACTIVITY_TIMEOUT_SECONDS`, default 600) fires a soft model-facing warning at 75% before the host hard-kills at 100%, resetting on any tool call or compaction.

Roles can override the nudge wording per signal via a `feedback_templates` block on the role manifest; operators can disable injection entirely with `DARKMUX_FEEDBACK_INJECTION=0`. The trajectory event reference lives in the `darkmux-analyze-run` skill.

### Cross-machine notebook (multi-environment lab notes)

If you run darkmux on more than one machine and want a single notebook that collates entries from all of them (for example, comparing wall-clock distributions across hardware tiers), point the notebook directory at an iCloud-synced (or otherwise shared) path:

```bash
export DARKMUX_NOTEBOOK_DIR="$HOME/Library/Mobile Documents/com~apple~CloudDocs/darkmux-notebook"
export DARKMUX_MACHINE_ID="m5-max-128gb-home"  # naming is yours; appears in entry headers
darkmux lab notebook draft <run-id>
```

Set the same `DARKMUX_NOTEBOOK_DIR` on each machine; give each a distinct `DARKMUX_MACHINE_ID`. Entries get tagged with their machine of origin in the header comment, so cross-machine readouts are unambiguous.

If `DARKMUX_MACHINE_ID` is unset, darkmux falls back to an auto-derived fingerprint (e.g. `apple-silicon-128gb`); fine for casual use, but a named id is recommended when more than one machine of the same tier exists.

You can also override the machine id for a single draft via `--machine`:

```bash
darkmux lab notebook draft <run-id> --machine my-work-mac
```

To list all entries in the notebook directory (optionally filtered by machine):

```bash
darkmux lab notebook list           # all entries
darkmux lab notebook list --machine m5-home  # only this machine's entries
```

`lab notebook list` outputs columns: **date | machine | run | path** (aligned, newest first). The `--machine` flag filters to only entries matching that machine id.

## Instrumentation

Cross-layer telemetry is always-on (#557): no flag, no sidecar file. The internal runtime and crew dispatch emit it as `category=telemetry` flow records on the flow stream (sources: `lms`, `process`, `detector`, `runtime`, `context`, `compaction`), capturing what LMStudio had loaded, where the runtime process sat across the run, detector signals, and compaction events.

View it in the observability viewer the daemon serves: run `darkmux serve` and open `http://localhost:8765/`. The viewer reads live flow records straight from the daemon; there's nothing to drag and drop. A demo instance lives at [darkmux.com/demo](https://darkmux.com/demo).

![darkmux observability viewer: the live savings dashboard showing 9.7M tokens kept off the frontier meter over 24 hours (split into generated, fresh input, and re-read input) across 22 local dispatches on two machines, with the orchestrator's note concluding the day's work.](docs/media/savings-hero-live.png)

## Why this exists: empirical motivation

Headline findings from the experimental work that produced darkmux's reference profiles:

- **Static config tuning has a floor.** Compaction knobs (`maxHistoryShare`, `recentTurnsPreserve`, `customInstructions`, compactor n_ctx) are tightly coupled; pulling any one of them in isolation regresses the run. Tuning at the config layer eventually stops paying dividends.
- **The "compactor loaded" tax is real.** Keeping a small compactor model warm for offload availability adds ~25s per dispatch on bounded workloads, even when compaction never fires. That cost is fixed and unrelated to compactor context size.
- **Long-task wins are bimodal.** With maximum primary context, multiple dispatches of the *identical* prompt + config split into a fast cluster (single-turn, no compaction fired) and a slow cluster (multi-turn, compaction fired). 3× variance between modes is normal, driven by emergent control-flow decisions inside the model's tool-loop, not by config.
- **Both modes still beat smaller-context baselines.** A router doesn't need to predict which mode a given dispatch will land in; it just needs to pick the right configuration for the *task class*.

The case for darkmux: **once you accept that static configs leave performance on the table (and that the right configuration depends on the task class, not the model), the routing layer becomes one of the highest-leverage pieces of infrastructure missing from the local-AI stack.**

## Status

✅ **v1.18.5**, semver-stable — two fixes surfaced by a real production 37-flag funnel run: the run-level `degenerate` gate no longer over-fires on a minority remote-judge dispatch error (a single flag's transient failure out of 37 was discarding the whole run's real, valid findings — 9 confirmed + 9 needs-check lost, CI false-alarmed on a run that actually completed correctly, #1329); and funnel provenance now stamps the model an endpoint actually **served**, not just the requested deployment name — an aliased Azure deployment is now surfaced in the posted footer instead of silently hidden (#1300). **v1.18.4** was two fixes surfaced by running the funnel on a private production repo: config.json now resolves to **user scope**, never a project-local shadow — a stray project `.darkmux/` on a self-hosted-runner checkout had been silently disabling Redis + the tamper-evident **audit log** on every review dispatch (#1323); and the review footer no longer claims "darkmux dogfooding itself in public" on private repos. **v1.18.3** was a one-fix patch: the review funnel's confirmed findings now **anchor as inline comments** instead of falling into the summary's general section (the mis-anchor half of #1299 — `resolve_anchor` gains a substring fallback so a quoted sub-expression resolves to its diff line; a real preserved review envelope replayed 0 inline / 7 general → 6 inline / 1 general). **v1.18.2** was the **production-hardening patch**: a memory-ledger correctness fix (prices models whose LMStudio path metadata is wrong, #1309) plus the credential-and-hang surface the first Azure-review day surfaced — a dependency-free **dispatch liveness floor** so a hang before flow init isn't a black box (#1311), **bounded Keychain reads** (a locked headless-runner keychain fails fast, not a multi-minute freeze), and **`EndpointAuth.key_env`** to pull an endpoint's key from an env var so a self-hosted runner never touches the Keychain (#1312, `PROFILES_SCHEMA` 1.4→1.5). v1.18.1 was the **review-output patch**: everything the first real production Azure review surfaced about how the funnel *presents* its findings (no change to what it finds). Confirmed findings now post a **non-blocking `COMMENT`** review — the same class Gemini uses, inline comments intact — instead of a `REQUEST_CHANGES` merge gate darkmux couldn't clear itself (#1302); the posted footer names where seats **actually ran** rather than falsely claiming "no cloud API" on a cloud review (#1298); frontier-worded **duplicate findings collapse** and the needs_check tier clusters (#1299); and a **`passes: 1` judge knob** cuts frontier cost (#1266). `PROFILES_SCHEMA` 1.2→1.4 (additive `passes` + `request_changes`). v1.18.0 was the **frontier-staffing release**: any review-funnel seat can be staffed by a hosted model, so a machine with LM Studio shut down does PR review entirely off a cloud endpoint (remote seats + an optional `review-verify` adjudication stage + per-execution token buckets, #1260/#1177). It also ships a **memory ledger** showing each loaded model's committed-vs-materialized RAM (`darkmux model ledger`, `/machine/memory`, the `#lens=machine` view, #1286) and a new model-lifecycle core — a pure planner, wave scheduler, architecture-aware KV estimator, and deadline-bounded `lms` adapter — present but not yet wired to the live dispatch path (that cutover lands in a later 1.18.x, #1274/#1276/#1285). `PROFILES_SCHEMA` 1.1→1.2 (optional `n_ctx` for endpoint models, #1282). v1.17.1 was the canary-day patch (registry blast-radius, cycler residency reconciliation, funnel dispatch visibility — #1269/#1271/#1272). v1.17.0 is the **review-funnel release**: PR review becomes a measured prosecution-and-judgment pipeline — `darkmux pr-review run` bundles the change (callee/sibling bodies + mechanical facts), fans strong-prior probe seats across it, dedups by mechanism, double-confirms every finding with an independent judge pass, and posts a three-tier review (blocking confirmed comments carrying a "needs frontier verification" marker, a non-blocking worth-a-double-check section, everything archived in the envelope artifact) — staffed by **crews** saved in `profiles.json` (#1222). The lab grew the observability to tune it: per-run event streams, host telemetry, staffing snapshots, and the viewer's third lens — the machine-local **lab observer** with run series, knob-diff provenance, and live run detail (`#lens=lab`, #1247). Funnel prompts are byte-locked to the measured reference implementation by golden tests (#1256). `FLOW_SCHEMA` 1.15.0→1.17.0 (additive), `PROFILES_SCHEMA` 1.1 (additive `crews`). v1.16.0 was the **production-review release**: a freeform review contract that works WITH tools (`MUST FIX`/`CONSIDER [path] \`anchor\`` markers + a `VERDICT:` line — a grammar-constrained `output_schema` combined with tools makes a model skip tool-calling and fabricate, so the agentic reviewer writes markers instead, #1192), the `pr-reviewer-agentic` role (repo checked out, read/exec tools, explores callers and siblings before concluding, #1192), `darkmux doctor --probe` (live-verifies a remote endpoint credential with one minimal completion through the exact dispatch path — shows round-trip, the model the endpoint says served it, and the probe's own token cost, #1191), `pr-review render` `mode: degraded` so a produced-nothing review can never read as a green check (#1193), and the live viewer's **local/cloud token split** — paid cloud tokens no longer count as "off the meter"; the hero shows local tokens and cloud tokens as two honest numbers (#1189). No schema changes. v1.15.0 added **agentic-remote dispatch** — a tool-granting role (e.g. `code-reviewer`) driven by a remote OpenAI-compatible endpoint (Azure OpenAI, OpenAI, …) as its "brain," running the same real tool-calling loop local models get via the internal container runtime, not just a single-shot chat completion (#92) — plus a `darkmux doctor` check for remote-endpoint credential presence (#85/#91) and a viewer host-load meter (#1064). `FLOW_SCHEMA` 1.14.0→1.15.0 (additive host-load telemetry fields; older binaries tolerate it). v1.14.1 was a viewer performance hotfix for a long-open live tab (memoized session lookups + incremental SSE reconcile, #1173). v1.14.0 adds cross-day **playback discoverability** — a mission/dispatch catalog + `?mission=`/`?session=` replay-by-query so you replay by the thing that ran, not by calendar day (#691) — and overhauls the run-detail **telemetry panel**: CPU and context on one wall-clock axis, a context step-area with per-turn markers + a fullness gradient + a compaction-threshold line, CPU shown in cores-busy, and the `model (lms)` telemetry section fixed (#1169). Drop-in over 1.13.1 (no schema change). v1.13.1 was a stability patch from a review-swarm audit of the recently-shipped code: compaction no longer orphans a tool-result at the tail boundary (an opaque HTTP 400 that hard-failed otherwise-productive dispatches, #1158), a dispatch panic can't silently kill the fleet runner (#1159), a job published before a runner exists isn't lost (#1160), `/diff` offloads to the blocking pool and streams git output under its 256KB cap (#1161), and the live viewer stops leaking per-session ids on a long-lived tab (#1162) — no schema changes, so it drops in over a v1.13.0 install. v1.13.0 was the fleet-foundation + self-diagnosing-doctor release: declare a machine's fleet position (`fleet.mode` hub/peer/standalone, #933), set config from the CLI instead of hand-editing JSON (`darkmux config set/get/list`, #937), and `darkmux doctor` now catches cross-setting traps (a stale env var shadowing config, a brew/cargo split-brain) + leads with an `● ok / needs attention / broken` verdict (#934) + shows where to open the viewer (loopback + tailnet, #1155) — plus a live-view UX pass: tokens-in/out accumulate live, an SSE backstop surfaces new runs without a refresh, mobile layout fixes, and `10m/1h/4h/24h` activity-timeline presets (#1151). No `FLOW_SCHEMA` change (stays `1.14.0`, cross-machine compatible with a v1.12.0 peer); `CONFIG_SCHEMA` 1.0→1.1 (additive `fleet{}` block, lenient-read). v1.12.0 was a build-visibility + run-observability + production-hardening release: build identity in `darkmux --version` / `doctor` / the viewer header (with a Homebrew `(release)` tag, #1129), a clearer run drill-down page (status pill + `run · <role>`, a run brief, tokens in/out tiles, the dispatch prompt + container image, #1125/#1127/#1126), an "about" modal off the version chip (#1132), `darkmux doctor` issues-only by default (#1130), and the production-hardening fixes surfaced by darkmux's first brew-stable user — `crew dispatch` now loads the model at the profile's `n_ctx` instead of LMStudio's silent default (#1135), the runtime recovers from an empty `finish_reason=tool_calls` completion instead of hard-failing (#1123), and internal-path dispatch errors carry the stderr text so a remote failure is diagnosable from the flow stream (#1042). v1.11.2 was a bug-fix + accessibility + security patch from a board triage: live "in flight" now derives from presence so a hard-killed dispatch stops reading "running" forever (#857), a truthful/de-duplicated live status line + consolidated headline (#1103/#1105), dispatch-error records that carry the stderr text (#1042), keyboard navigation + a non-color status cue for the viewer (#1090/#1092), and argument-injection guards on `gh pr create --label` and `external pull` (#1111/#1112). v1.11.1 was a focused viewer + UX pass — a cleaner, mostly-mobile dashboard with one status-color convention everywhere (green = success, yellow = running, orange = canceled, red = failed/killed, #1071), icon-first chrome (#1067/#1098), local-timezone timestamps (#1069), and a full-width savings hero on the missions tab (#1096) — plus a local-PR-reviewer reliability fix (the role no longer copies its own worked example, #1084). v1.11.0 made the local **PR reviewer** materially better and self-contained: it assesses each change against its **stated intent** (the PR title + description) so it stops flagging the very bug a fix removes (#1053), anchors findings by **quoting the line** and resolving that quote to a coordinate in the harness — local models name the construct well but guess line numbers badly (#1053) — and moves the whole review render into the binary as `darkmux pr-review render`, versioned with the role schema instead of a per-repo script (#1060). darkmux also reviews its own PRs in public, on a local model, via a self-hosted runner (#1047/#1057), and `crew dispatch --profile <name>` lets a caller name a profile the machine resolves (#1054). v1.10.0 turned a local model into an automated **PR reviewer** (the tool-less `pr-reviewer` role + grammar-constrained `output_schema` so a small model can't emit malformed JSON, #1037/#1039/#1044). v1.9.0 completed the engagement-context cure: staleness-aware cautions (#1001/#1002), a fraction-of-window injected-context budget (#1011), `darkmux lessons edit/remove/export/import/recall` curation (#1003), and `darkmux lab loop --ab` for the verdict-shift A/B (#1004), plus `crew dispatch --message-from-file` (#386). v1.8.0 laid the foundation (the cautions + lessons store + `mission debrief`, #994/#1000); v1.7.0 added the loop lab (#986); v1.6.0 persisted corrections into the next brief (#849); v1.5.0 added the verifier-fabrication gate (#799). The dispatch-to-PR loop, the missions lens + `mission status`, live observability, and the Homebrew pipeline are the stable surface.

**Shipped:**

- ✅ Profile registry + `swap`/`status`/`profiles`/`scan` CLI
- ✅ Lab subcommands (`run`/`inspect`/`compare`/`characterize`/`tune`/`runs`), `WorkloadProvider` trait, embedded smoke workloads, always-on cross-layer flow telemetry (#557)
- ✅ Lab reproducibility (#487): per-run copy-on-write sandbox isolation (source never mutated), `baseline_hash` + `final_hash` content hashing in the run manifest, a fixture registry with `lab register`/`unregister`/`fixtures`/`doctor` verbs, workload `requires_fixture` resolution, and `scripts/lab-init.sh` + the built-in `demo-tiny-py` fixture
- ✅ Notebook (`notebook draft`/`list`), cross-machine via `DARKMUX_NOTEBOOK_DIR`
- ✅ Agent-invocable skills bundle (12 skills including `/darkmux-bootstrap`)
- ✅ Crew + Role + Mission + Phase schema with SQLite-backed index; `mission propose` + `phase estimate` utility-AI verbs; mission configs + `mission launch` as the config-launched instance-creation path
- ✅ Per-role `agent.model` pinning (#160) with bake-off-derived defaults; doctor surfaces drift
- ✅ Recommendation registry per hardware tier (#159) with `swap recommended` + `model pull-recommended`; doctor surfaces drift
- ✅ Flow substrate: `LocalFileSink` (always) + `AuditFileSink` (BLAKE3 hash chain, verifiable via `flow integrity-check`; opt-in) + `RedisSink` (coordination; opt-in), composed via `TeeSink`
- ✅ `darkmux flow status` + `darkmux flow integrity-check` diagnostic verbs
- ✅ Observability daemon (`darkmux serve`) + `/flow` + `/lab` web viewers
- ✅ Doctor: 30+ pre-flight checks with actionable hints, plus a legacy-extras warning that flags profiles still carrying pre-#380 compaction keys (`mode`, `maxHistoryShare`, …)

**On the roadmap (active):**

- 🚧 Topology view in the web viewer (live + replay diagram of fleet activity; #169)
- 🚧 Fleet primitives (`darkmux fleet add/status`) and cross-machine coordination (Phase 5 of #162)
- 🚧 Event-sourced mission state (Phase 8 of #162)
- 🚧 Sibling bootstrap skill: `/darkmux-enable-redis` (#178). (`/darkmux-add-machine` and `/darkmux-enable-audit` shipped, in the skills bundle above.)
- 🚧 Audit log management: `flow export`, `flow archive`, OS-level append-only flags for audit files
- 🚧 Multi-frontier orchestrator support (Gemini / Codex / Copilot bootstrap paths; #179)

**Aspirational (later):**

- 🚧 Plugin system for community-contributed providers, workloads, role manifests
- 🚧 Per-role bake-offs for non-SWE roles (trip-researcher, health-research, legal-research, …)

## Security

darkmux is a single-operator, local-first tool. See [SECURITY.md](./SECURITY.md)
for the threat model (what it does and doesn't defend) and how to report a
vulnerability privately. Short version: keep `darkmux serve` on loopback, and
treat AI-generated code the way you'd treat any untrusted script.

## License

MIT

## Author

Kain Osterholt · [@DarklyEnergized](https://x.com/DarklyEnergized) · Darkly Energized LLC

---

*Claude, Claude Code are trademarks of Anthropic PBC. LMStudio is a trademark of Element Labs Inc. darkmux is not affiliated with either.*
