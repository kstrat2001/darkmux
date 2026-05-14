# Claude / Agent guidance for darkmux

This file is for any AI agent (Claude Code, Cursor, OpenClaw, etc.) that's helping a user work with the darkmux source tree. Read this once before doing anything.

## What darkmux is

A pre-1.0 Rust CLI that does two things for users running local LLMs (LMStudio + Ollama + llama.cpp):

1. **Profile multiplexer** — `darkmux swap <name>` switches the loaded model + context length + (optional) compaction settings to a named profile defined in `~/.darkmux/profiles.json`.
2. **Lab harness** — `darkmux lab run <workload>` dispatches a workload against an agent runtime (default: `openclaw`) and records timing + trajectory + verify outcome under `.darkmux/runs/<run-id>/`.

The CLI is the *engine*; the empirical findings in the article series at <https://substack.com/@DarklyEnergized> are what it backs. The reproducibility story is the product story — users should be able to rerun a workload and get numbers comparable to the published claims.

## darkmux's grand vision (agent-facing)

The user-facing **"What darkmux is for"** section in `README.md` is the canonical version of the project's north-star. Below is how the same five claims translate into operational doctrine for an AI agent (Claude Code, OpenClaw, Cursor, etc.) working on darkmux or driving it on behalf of an operator.

1. **Optimization, not replacement.** When the operator asks you to pick a model from `lms ls` or propose a profile, prefer *complement* over *duplicate*. A team where every model is a 35B reasoner is not a team — it's a stack of identical instruments. The same logic applies *within* each role family (see **Project posture → Role families** below): a profile with three different 35B specialists and no 4B admin agent is missing its compactor, scribe, and estimator; conversely, a profile of nothing but admin agents has no specialist to do the actual judgment-dependent work. Read the existing profile registry first; propose additions that fill gaps in the right family (admin: compactor / scribe / estimator / mission-compiler; specialist: coder / reviewer / analyst) rather than swapping like for like.

2. **Harness, then model.** When the operator reports slow or wrong outputs, **check the harness before the model**. Compaction config, context-window mismatches, loaded-state drift, profile-vs-loaded model — all of these have produced 5×+ wall-clock regressions in Article 2's measurements. Default action: run `darkmux doctor`, read the eureka findings, surface those *before* suggesting the operator change models.

3. **The lab + the loop.** darkmux is not just an inspection tool — it's the loop. When you have a tuning hypothesis (e.g., *"primary at 64K instead of 100K might fit this 32GB tier"*), the correct action sequence is: **baseline → single-variable change → re-measure → compare → record in notebook**. Each step has a darkmux primitive. Do NOT skip the baseline. Do NOT change two variables at once. The discipline is the point — without it, the comparison is uninterpretable.

4. **Team integrity is your responsibility.** When proposing config changes, frame them in terms of *how this affects the team's shape*, not just an isolated metric. *"Drop the compactor to free RAM"* reduces working memory; consider whether the remaining team can still handle long-agentic dispatches before recommending. The operator is depending on you to maintain team coherence as new models arrive and hardware changes — that's the principal-engineer posture the maintainer named in [#35](https://github.com/kstrat2001/darkmux/issues/35).

5. **The success criterion is recursive.** A fresh agent session, given only a clean-slate darkmux install + these docs + the bundled skills, should reach the same conclusion about *"what is darkmux for?"* as the maintainer's comment on #35. If you find yourself uncertain or having to infer from primitives, **the docs have drifted from the vision** — surface that to the operator. Doc drift is a bug, not a footnote.

These claims compose with the existing **Anti-patterns** section below: anti-patterns are *what not to do*; the vision is *what to do instead*. If a request would violate both at once (e.g., *"silently roll back the compactor without telling me"*), the vision wins — surface the conflict and let the operator decide.

## Build and test

```bash
cargo build --release    # release binary at target/release/darkmux
cargo test               # unit + integration suite
cargo clippy             # lint
cargo fmt                # format
cargo install --path .   # install to ~/.cargo/bin/darkmux
```

The release binary is self-contained (~1.1 MB). Built-in workloads under `templates/builtin/workloads/*.json` are embedded at compile time via `include_str!` — `cargo install --path .` produces a binary that works from any directory without the source tree.

## Where things live

```
src/
  main.rs                    CLI dispatch (clap)
  types.rs                   Profile / ProfileRegistry / ProfileModel
  profiles.rs                Registry loader + lookup
  swap.rs / lms.rs           Stack swap orchestration + lms CLI wrapper
  runtime.rs                 Runtime config patcher (e.g. openclaw.json)
  init.rs / skills.rs        `darkmux init` + skill installer
  notebook.rs                Notebook draft generator
  lab/
    paths.rs                 Workspace dir resolution (project vs user)
    run.rs                   Workload dispatch
    inspect.rs               Single-run analysis
    compare.rs               Run-vs-run diff
    list.rs                  Recent runs table
  workloads/
    types.rs                 WorkloadProvider trait + manifest types
    load.rs                  Manifest loading (user → on-disk → embedded)
    registry.rs              Provider registry
  providers/
    prompt.rs                Trivial single-prompt provider
    coding_task.rs           Sandbox + verify-command provider
templates/builtin/workloads/  Workload manifests embedded at compile time
skills/darkmux-<name>/        Agent-invokable skill wrappers
tests/cli.rs                  Integration tests (spawn the binary)
```

## Conventions to follow

- **Don't add dependencies casually.** The dep set is deliberately small (`anyhow`, `clap`, `serde`, `serde_json`, `dirs`). A 10-line inline module beats a crate for small one-off needs (see `mod pathdiff` in `src/providers/coding_task.rs`).
- **Trait providers, not feature flags.** New workload kinds go through the `WorkloadProvider` trait in `src/workloads/types.rs`, registered in `src/workloads/registry.rs::register_builtins()`. Don't bolt new behavior into the lab orchestrator.
- **Manifests are JSON.** Workload manifests, profile registries, run manifests — all JSON. The repo briefly used YAML; that switch is done. Don't reintroduce YAML.
- **Tests over prints.** Mutating-state tests (cwd, env vars) need `#[serial_test::serial]` to avoid races. Integration tests in `tests/cli.rs` use `assert_cmd` to spawn the binary.

## Versioning — rules schema

The `eureka` rules engine emits its definitions into `instruments.jsonl` so the viewer can render findings without duplicating rule data in JS. The viewer enforces compatibility on file drop. The contract is plain semver, applied to the rules **data shape** (not to darkmux itself):

| Bump | Meaning | UI behavior |
|---|---|---|
| **Patch** (`1.0.0` → `1.0.1`) | Fully backward-compatible. Bug fix in a message, threshold tweak that doesn't change semantics, typo in a `fix_hint`. | Viewer parser ignores; works unchanged. |
| **Minor** (`1.0` → `1.1`) | Additive. New rule `kind`, new optional field on `RuleDef`. Older viewers can't *evaluate* new rules but can SAFELY IGNORE them. | Viewer soft-warns; renders normally. New rules surface as "checked at pre-flight only" until the viewer gets a JS evaluator. |
| **Major** (`1.x` → `2.0`) | Breaking. Rename/retype a field, change of `RuleKind` enum encoding, new required field. Older viewers cannot trust newer-major data. | Viewer **blocks** the Anomalies panel + shows an upgrade modal with the exact `cargo install --path . --force` command. User must update CLI or downgrade viewer. |

Rule of thumb when changing the schema:

- Adding a new rule? **Minor bump.**
- Renaming or retyping a field on `RuleDef`? **Major bump.**
- Fixing a typo in `fix_hint`? **Patch bump.**

`RULES_SCHEMA_VERSION` lives in `src/eureka.rs` as a single constant. The viewer's `EXPECTED_RULES_SCHEMA_MAJOR` lives in `docs/viewer/index.html` near the top of the script block. **When you bump major, you bump both in the same PR** — the viewer-release-PR is the contract.

## Common tasks for an agent

If a user asks you to:

| Ask | Do |
|---|---|
| "add a new workload" | Drop a JSON manifest at `templates/builtin/workloads/<id>.json`. If it's a `prompt` workload, register it in `EMBEDDED_WORKLOADS` in `src/workloads/load.rs`. coding-task workloads need a sandbox seed dir and CAN'T be embedded. |
| "add a new provider" | Implement `WorkloadProvider` in `src/providers/<name>.rs`, register it in `src/workloads/registry.rs::register_builtins()`. |
| "run the smoke test" | `cargo install --path . && darkmux lab run quick-q`. Should complete in ~6-10s if a model is loaded. |
| "list notebook entries" | `darkmux notebook list` (optionally `--machine <id>` to filter). Enumerates `.md` files, parses headers. |
| "draft a notebook entry" | `darkmux notebook draft <run-id>` (optionally `--machine <id>` to override). |
| "make the build self-contained" | Already is — `include_str!` for embedded workloads, no external assets needed at runtime. |
| "review the diff before commit" | Run the suite (`cargo test`), eyeball `git diff`, propose a commit message — but **do not commit unless explicitly asked**. |
| "run the optimize wizard" | `darkmux optimize` — Phase 1 scaffold prints six steps (all "todo"). For the full plan, see #35. Do NOT attempt full implementation of all six steps in one PR; land one phase at a time.

## Things to ASK before doing

- Anything that mutates `~/.darkmux/profiles.json` — that's user state.
- Anything that calls `darkmux swap` or runs a real lab dispatch — uses real LMStudio resources.
- Anything that does `git push` or `git commit --amend` — irreversible-ish.
- Adding external runtime dependencies — has knock-on effects on install size and license surface.

## Anti-patterns — failure modes observed on fresh sessions

Live findings from cross-machine testing (M1 Max Studio fresh-Claude session, 2026-05-11). Future fresh-Claude sessions: read these BEFORE acting on a user request that touches model selection, version compatibility, or issue tracking.

- **Don't assume models — read the profile registry first.** Models live in `~/.darkmux/profiles.json` (or wherever `darkmux profiles` reports). If an agent role needs a model and one isn't declared, **ask the user**; do NOT pick a model from the LMStudio catalog at random. Older gemma family in particular ("gemma-4-e4b", "Gemma 4 26b", etc.) is known to produce looping garbage that poisons sessions across model swaps — never default-assume gemma. If the user has saved a memory about model selection, that supersedes any inference you'd make from the catalog.

- **Don't silently roll back on regression.** If a feature appears to regress on an unfamiliar OpenClaw / LMStudio version (e.g., `systemPromptOverride` doesn't behave as expected, compaction settings don't take effect), **surface the finding to the user** with the version numbers you observed. Don't quietly revert config overrides "to make things work" — loud beats quiet. The user is debugging an unfamiliar env and needs the signal; a silent rollback hides the real bug.

- **Check existing issues before filing.** Before `gh issue create`, run `gh issue list --search "<keywords>"` (include closed issues with `--state all`) and skim. Duplicates clutter the project board and dilute the eureka-detection roadmap. Default to **commenting on an existing issue** over filing a new one. If you're not sure whether something is a dupe, **ask the user**; don't file-and-hope.

- **Cross-machine version awareness.** darkmux assumes a recent OpenClaw. Before applying any agent config (especially `systemPromptOverride`, compaction settings, or sampler tweaks), check `openclaw --version` and consider whether the feature you're about to use exists on the user's installed version. If you can't verify, ask. The currently-documented minimum is captured in `doctor`'s `runtime version` check (see `MIN_OPENCLAW_VERSION` in `src/doctor.rs`) and in the README's Prerequisites.

- **The empirical findings in Article 2 are load-bearing, not decorative.** When choosing compaction modes, context windows, or compactor pairings, the article's data (`default` mode beats `safeguard` for local; small dedicated compactor at ~68K cuts wall-clock in half) reflects validated configurations, not arbitrary defaults. Don't deviate from a profile's settings without acknowledging the empirical reason — the operator has chosen them deliberately.

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
- **Don't mutate user state without confirmation** — `~/.darkmux/profiles.json`, `~/.openclaw/openclaw.json`, anything operator-owned. Read + propose; never write silently.
- **Namespace everything darkmux brings up in shared state** — LMStudio loaded models, OpenClaw agent definitions, channel routing, anything else darkmux writes into a system other systems also use. Conventions: LMStudio identifiers under `darkmux:<model-id>` (e.g. `darkmux:qwen3.6-35b-a3b`); OpenClaw agent ids under `darkmux/<role>` (e.g. `darkmux/coder`). Then darkmux's own state-mutating operations only touch the namespaced subset — user state is off-limits by construction, not by careful coding. The namespace is the contract.
- **Keyword vocabulary hybrid** — ship a starter; operator augments; system logs misses but never auto-mutates the vocabulary
- **Operator-tunable preferences are numeric scales, not hidden enums** — discoverable via example values; supports continuous tuning; UI-ready

The principle is recursive. It applies to documentation surface (this CLAUDE.md, READMEs), to CLI verbs, to data shapes on disk, to the architecture of future features. When a design decision feels like it should be made automatically by the system, that's the moment to surface it back to the operator instead.

Tracked as #44.

## Namespace convention (darkmux state in shared systems)

When darkmux maintains state in a system other consumers also use — LMStudio loaded instances, OpenClaw agent definitions, channel routing, anything operator-managed — **darkmux-owned entries are namespaced** so they can be recognized at a glance and so darkmux's own state-mutating operations can scope themselves to only the namespaced subset. User state is then off-limits by construction, not by careful coding.

### Current namespaces

| System | Form | Example |
|---|---|---|
| LMStudio loaded identifier (visible in `lms ps`) | `darkmux:<model-id>` | `darkmux:qwen3.6-35b-a3b` |
| OpenClaw agent ids (`agents.list[].id`) | `darkmux/<role>` | `darkmux/coder` |
| OpenClaw channel routing (`channels.modelByChannel.*`) — if darkmux ever manages it | `darkmux/<key>` | `darkmux/<channel-id>` |

Different separators (`:` vs `/`) are deliberate — `:` reads naturally in LMStudio's ecosystem (which uses `:` to separate concepts like `mlx-community/foo:Q4_K_M`); `/` reads naturally in OpenClaw's config (which uses paths and ids that benefit from hierarchy). Both are clearly "this is darkmux's thing."

### Why this matters

Without the namespace, darkmux's operations have to fall back on heuristics or persistent state files to know "did I bring this up, or did the user?" Heuristics are fragile (the user might happen to use the same naming convention); state files go stale (user force-quits, LMStudio restarts, manual unloads). The namespace IS the state — durable, visible, self-describing. If `lms ps` shows `darkmux:qwen3.6-35b-a3b`, that's a darkmux load and `darkmux swap` can unload it. If it shows `qwen3.6-35b-a3b` with no prefix, that's user state and darkmux leaves it alone.

### Transparency at dispatch time

When darkmux loads a model under `darkmux:<id>`, the underlying LMStudio model key is unchanged — `lms ps` shows `identifier=darkmux:foo, modelKey=foo`. Dispatchers calling LMStudio's chat-completion API with the bare model id `foo` still resolve via the `modelKey` match (verified empirically 2026-05-12 against openclaw's lmstudio plugin). **The namespace is invisible at dispatch time** — only visible to darkmux and operators inspecting `lms ps`. Existing dispatcher configs continue to work without migration.

### Conventions for new code

When writing a new feature that mutates state in LMStudio or OpenClaw on the operator's behalf:

1. **Generate the namespaced form** at the point of write. See `swap::namespaced_identifier` for the LMStudio case.
2. **Filter on the namespace** at the point of read/cleanup. See `swap::is_darkmux_owned` for the LMStudio case.
3. **Pass-through explicit overrides** — if the operator sets an explicit identifier in their profile, don't override it. The namespace is the *default*; the operator can opt out.

### Operator-facing commands

- `darkmux model status` — list `lms ps` results grouped by ownership (darkmux-managed vs user state). Read-only.
- `darkmux model eject [--dry-run]` — unload everything in the `darkmux:` namespace; never touches user state. Use to release darkmux's RAM footprint without disturbing other tools.
- `darkmux crew sync [--dry-run]` — reconcile openclaw's `agents.list[]` with the crew role manifests. For each role with both a JSON manifest and `.md` system prompt, ensures a `darkmux/<role-id>` openclaw agent exists with the manifest-derived shape (system prompt + tool palette). Idempotent.
- `darkmux crew dispatch <role-id> --message <text> [--deliver <chan>:<target>]` — dispatch a single turn to the named role. Looks up the role, pre-flight-verifies that the corresponding `darkmux/<role-id>` openclaw agent matches the manifest (bails loud on drift with a `darkmux crew sync` repair pointer), then invokes `openclaw agent` and returns the result.

Tracked alongside operator sovereignty (#44) and issues [#52](https://github.com/kstrat2001/darkmux/issues/52) (LMStudio namespace), [#55](https://github.com/kstrat2001/darkmux/issues/55) (full pre-flight checklist — partial coverage in `crew dispatch` today), and the `qa-review` migration that brought these verbs into the dispatch path.

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
- Translate the soft free-form context into the structured concepts darkmux supports in code (Mission, Sprint, role tilts, preferences) — proposing this translation when it'd help the operator move forward is the orchestrator's by-design job, not a thing to withhold
- Don't pry for structure the operator didn't volunteer — offer a suggestion once, let it land or get redirected, then drop it

Engagements should not be well-defined. They are open-ended dreamscapes where ideas are meant to flourish. darkmux supports the engagements it can support (local dirs, local code work) and stays out of the way for the rest (SaaS, hosted, conceptual, classified). The Rust-level data model in the schema PR (#45) names Role, Crew, Mission, Sprint — concepts the system CAN model uniformly. Engagement isn't in that schema by design; it's the layer above where operator judgment lives.

This is operator sovereignty (above) applied at the project-shape level: the operator decides what their projects look like; the system doesn't impose a schema.

Tracked as #49.

### Engagement never enters CLI arg surface

Concrete doctrine that follows from the above: **engagement context lives in the frontier orchestrator layer (CLAUDE.md files, skills, conversation). It never becomes a `--engagement <hint>`-style CLI arg on any `darkmux` verb.**

Three reasons the rule is load-bearing:

- **CLI args quantize.** A `--engagement <hint>` field forces the operator to compress a dreamscape into a single string-token. *"wife time"* as a token is worse than *"this is my marriage time, not a work trip — focus on relaxation, no aggressive sightseeing"* threaded through the actual intent text. The frontier carries that nuance natively; the CLI surface cannot.
- **Admin agents are the wrong tier for engagement interpretation.** A 4B mission-compiler asked to *"interpret the operator's relationship to this engagement"* is the capability mismatch the admin-vs-specialist split (Beat 21 / role-families) exists to prevent. Engagement nuance interpretation is judgment-bearing work that belongs to the frontier — never to an admin agent and never to a CLI arg the admin agent will read.
- **The frontier already handles it.** *"Plan our Japan trip — focus on relaxation, no aggressive sightseeing, this is for my marriage"* reads richer than `--engagement "wife time"` + `"plan Japan trip"` because the nuance threads through prose, not into a separate enum. A frontier-orchestrator-driven workflow gets engagement-shaping for free; a bare CLI invocation gets it by the operator putting context in the input text itself.

For new CLI verbs that would benefit from "context-aware" output: the operator carries that context into the verb's primary input. No separate `--engagement`, no `--context`, no `--vibe`. If the operator has no frontier orchestrator and wants context-shaping, they write the context into the input prose where the admin agent reads it as part of its bounded structuring job.

### Why the line matters at scale — the lost-in-translation problem

The mechanical reasons above (quantization, capability mismatch, etc.) are downstream of a deeper principle. **The pattern is older than AI:** in any organization, when admins translate vision → tasks, the vision quietly dies in the translation. The admin's role IS narrower — that's why an admin layer can absorb volume — but applying that layer to vision-bearing work is the antipattern. Same dynamic, same failure mode in the AI stack.

What makes the line load-bearing:

- **Engagement is where the *why* lives.** The frontier orchestrator can hold engagements because it can sit in operator context, hold contradictions, and carry nuance across turns. A 4B admin agent can't hold contradictions — it'll resolve them. That resolution is where vision gets lost. A `--engagement "wife time"` flag forces the admin to do that resolution before it has the context to do it well.
- **The admin AI is the basic planning layer, not the strategic layer.** Capacity-matched to its actual job (bounded inputs, structured outputs, throughput). Asking it to ALSO carry *"what does this mean for the operator's broader life / org / book / engagement"* loads it past its capacity. Even when it produces something, that something is the small-picture compression of the big picture.
- **The cost scales with org size.** A solo operator can correct admin output in the next turn — the loop is tight enough that drift gets caught. An organization where the admin layer is making decisions BEFORE the operator/frontier sees them is the scenario where *big dreams get eaten alive by small bugs written by admins who don't have capacity yet to hold the big picture vision.* darkmux's admin layer can have exactly that pathology if its scope leaks into engagement territory; the line drawn here is what prevents it.

The frontier orchestrator's role in this layering is named below as **vision guard** — the layer that protects the operator's engagement-level intent from being compressed before it has been translated into structure the admin can handle. The cultivation discipline (how operators *shape* their frontier to actually hold their vision — CLAUDE.md files, skills, memory, conversation history) is the next-order concern; tracked separately (see related issue, filed alongside this doctrine).

Surfaced 2026-05-14: Sprint 3 of #113 originally added `--engagement` to `darkmux mission propose`; operator caught it pre-merge as a doctrine violation against #49. Removed in the same PR, and the rule made explicit here so future verbs don't re-introduce it. The lost-in-translation framing came from the same exchange — codified here because the *why* is harder to reconstruct from the rule alone, and future verbs that look context-shaped will tempt the same drift.

## Project posture

**darkmux is an AI-first local-AI orchestrator.** It uses local-AI internally to manage your local-AI workflows. The CLI binary embeds dispatch logic to call into LMStudio-loaded admin agents for structuring, planning, and routine bounded reasoning tasks (compaction, sprint estimation, mission proposal, notebook draft). The frontier-AI orchestrator (your Claude Code, Cursor, or OpenClaw session) remains the strategic reasoner; darkmux operates the local tier as a self-contained capability.

The recursive shape is the point: **darkmux uses local-AI to manage your local-AI.** Operators running darkmux are running local-AI dispatches whose orchestration is itself done by local-AI. That's the AI-first move — not "AI bolted on," but AI as the obvious built-in capability of a tool whose reason for existing is local-AI orchestration. Earlier framings of darkmux as *"infrastructure, not an agent framework"* were honest at the time (one-thing-only swap tool, saturated agent-X namespace) but are now aspirational. The current posture matches what the binary does.

### Role families

Two role families compose to make this work, and the distinction matters when picking models or proposing additions to a profile:

- **Admin agents** — small model (4B-class), bounded I/O, high throughput, structured output. Compactor, scribe, task estimator, mission-compiler. Each capability is asymmetric to its compute cost — one small model can fill several admin roles. darkmux dispatches admin agents internally for its own operations; the operator rarely invokes them directly. The category was named in Beat 21 of the lab notebook ("the dependable admin agent") — bounded inputs + structured outputs + low per-call failure cost + throughput matters + bounded reasoning rather than strategy.
- **Specialist agents** — larger model (35B-class+), judgment-dependent, lower throughput, free-form output. Coder, code-reviewer, analyst. Operator's call: which specialist for which sprint, with what tilt. darkmux makes them addressable via `crew dispatch <role>` but doesn't substitute its judgment for the operator's.

CLI primitives stay small and composable; the AI-built-in verbs (`mission propose`, `sprint estimate`, `notebook draft`) compose those primitives with admin-agent dispatches so the operator gets structured output without authoring JSON by hand. Both surfaces are part of the same project — the dual posture (small primitives + AI-built-in verbs) is deliberate.

The default runtime is OpenClaw (`DARKMUX_RUNTIME_CMD=openclaw`) but the lab harness is runtime-pluggable via env var. Users running Aider, Cline, or anything with a `<cmd> agent --message` interface can point darkmux at it.

## When in doubt

Read `README.md` for the user-facing pitch, `DESIGN.md` for the implementation reasoning, `CONTRIBUTING.md` for the dev loop. If something contradicts across files, the code is the source of truth — flag the doc drift to the user.
