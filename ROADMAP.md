# darkmux roadmap

darkmux is past its [1.0](https://github.com/kstrat2001/darkmux/releases) foundations release. This file is the human-readable map of where it's going; the live detail lives in the [`theme:*` labels](https://github.com/kstrat2001/darkmux/labels?q=theme) and [Issues](https://github.com/kstrat2001/darkmux/issues), with milestones tracking [releases](https://github.com/kstrat2001/darkmux/releases).

Themes are tracked as **`theme:*` labels** — concurrent lanes, many-per-issue — not milestones; **milestones mean releases**. The lanes are *prioritized* (one leads), not *sequenced*: work happens across several at once, a release bundles whatever's ready from any of them, and cross-cutting work is pulled into whichever lane activates it.

Per-milestone **charters** — the full goal, research grounding, scope, success criteria, and target release for each theme — live in [`docs/roadmap/`](docs/roadmap/). This file is the map; the charters are the depth.

## The north star

darkmux orchestrates **local** LLMs to do real work — a mission orchestrator and a lab, with a unified observability stream, driven by a frontier orchestrator (Claude Code, Cursor, etc.) that "writes the loop." Every seat runs your own models off the meter, or a hosted endpoint when a role needs frontier weights. Optimization, not replacement; the harness before the model; the operator always in the loop with full provenance. See [`README.md`](README.md) and [`CLAUDE.md`](CLAUDE.md).

## Now — M4: Dispatch-to-PR loop depth

The lead theme. Harden the daily-driver loop and formalize it as a first-class capability: the orchestration loop (coder → fresh-context review → fix → frontier sign-off → PR), its failure modes, and its termination discipline.

📋 **Full charter:** [`docs/roadmap/M4.md`](docs/roadmap/M4.md) — goal, the synthesized research + dogfood grounding, scope clusters, sequence, success criteria, and target release.

This theme is **grounded in the loop-engineering literature**: the field crystallized into a named discipline over the past year, and the research supports the architecture darkmux already ships (observability-driven detectors, cross-context verification). Key items carry the citations:

- [#799](https://github.com/kstrat2001/darkmux/issues/799) — terminate on a **verifiable check**, never agent self-assessment. Process-reward-model research shows step-wise verification catches *silent errors* (code runs, result wrong) that outcome-only checks miss ([arXiv 2604.24198](https://arxiv.org/abs/2604.24198)); self-verification in the same context is mostly confirmatory, not corrective ([arXiv 2602.03485](https://arxiv.org/abs/2602.03485)). **Lead item.**
- [#849](https://github.com/kstrat2001/darkmux/issues/849) — persist corrections into the next brief (doom-loop fix) + recheck-vs-rethink escalation, with **quantified escalation budgets** ([Graph-Harness termination, arXiv 2604.11378](https://arxiv.org/abs/2604.11378))
- [#453](https://github.com/kstrat2001/darkmux/issues/453) — wrong-diagnosis-stuck escalation (concurrent with the #389 watchdog tuning)
- [#48](https://github.com/kstrat2001/darkmux/issues/48) — formalize the loop as the standard dispatch-to-PR skill ([#63](https://github.com/kstrat2001/darkmux/issues/63) core stages = the control-flow backbone)

## The other lanes

These run **concurrently** with M4 — open and worked-on as each is ripe, not queued behind it. M4 just leads. Each links its charter (the depth) and its live `theme:*` issues.

| Theme | Live issues | What it covers | Research anchor (verified) |
|---|---|---|---|
| **[Runtime / agent-loop robustness](docs/roadmap/M5.md)** | [`theme:m5-runtime`](https://github.com/kstrat2001/darkmux/issues?q=is%3Aopen+label%3A%22theme%3Am5-runtime%22) | the *inner* loop: error taxonomy first, recovery behaviors, compaction tuning, streaming, and the harness that [learns from its own failure record](https://github.com/kstrat2001/darkmux/issues/400) | self-healing = detect + taxonomy + replan ([arXiv 2605.06737](https://arxiv.org/abs/2605.06737)); failure-aware observability ([arXiv 2606.01365](https://arxiv.org/abs/2606.01365)) |
| **[Fleet — many machines become one](docs/roadmap/M6.md)** | [`theme:m6-fleet`](https://github.com/kstrat2001/darkmux/issues?q=is%3Aopen+label%3A%22theme%3Am6-fleet%22) | version-compat first, then event-sourced mission state, cross-machine workspace handoff, topology-driven liveness | event-sourcing as the distributed-agent backbone ([akka.io](https://akka.io/blog/event-sourcing-the-backbone-of-agentic-ai)); capability discovery ([arXiv 2511.19113](https://arxiv.org/abs/2511.19113)) |
| **[Observability / viewer](docs/roadmap/M7.md)** | [`theme:m7-observability`](https://github.com/kstrat2001/darkmux/issues?q=is%3Aopen+label%3A%22theme%3Am7-observability%22) | one drill-down viewer over the unified stream; crew roster, phase burn-down, eureka anomalies; an OTel-GenAI alignment spike | OpenTelemetry [GenAI semantic conventions](https://opentelemetry.io/docs/specs/semconv/gen-ai/) (emerging standard, still in development) |
| **[Capability routing](docs/roadmap/M8.md)** | [`theme:m8-routing`](https://github.com/kstrat2001/darkmux/issues?q=is%3Aopen+label%3A%22theme%3Am8-routing%22) | backend abstraction first, then capability vectors, then continuous routing (not hardware-tier labels) | RouteLLM ([arXiv 2406.18665](https://arxiv.org/abs/2406.18665)); Skill Profiles ([arXiv 2602.02386](https://arxiv.org/abs/2602.02386)) |

**Cross-cutting / backlog** (architecture, foundations, onboarding, doctrine) isn't a milestone of its own — those issues get pulled into whichever theme activates them. Examples: typed flow payloads (#511), centralized config (#507), workspace crate split (#463), multi-frontier bootstrap (#179).

## How we decide

darkmux's design decisions are **grounded in published research where it exists** — we'd rather cite a paper than assert from intuition, and we link the source on the issue. The framing is convergence, not priority: independent research and this project keep arriving at the same architecture, and the citations explain why it works. The compaction → [StructuredSlot](DESIGN.md) story is the template; the loop-engineering work on M4 is the latest.

**Citations are verified, not just collected.** This roadmap was re-processed against the 2025-26 literature by a multi-agent pass whose second half does nothing but *re-fetch every cited source and confirm it supports the claim* — because a confident citation under a correctly-recalled label is exactly where fabrication hides. That pass dropped several plausible-but-unverifiable citations (and caught one false-negative — a real paper its tooling couldn't reach). A roadmap that cites dead or misattributed sources reads as *less* researched, not more.

**Where the literature runs out — the white space we lead with.** Nearly all current routing, fleet, and observability research assumes **cloud or homogeneous hardware**. darkmux's premise — a **heterogeneous local fleet of Apple-Silicon Macs over a tailnet**, running quantized models — is genuinely under-served by the literature. Multiple research themes independently flagged the same hole. That gap *is* the differentiator: the detector-driven harness, the event-sourced two-tier local fleet, and capability routing on local quantized models are darkmux's to define, not to follow.

## Contributing

New here? Look for [`good first issue`](https://github.com/kstrat2001/darkmux/issues?q=is%3Aissue+is%3Aopen+label%3A%22good+first+issue%22) and [`help wanted`](https://github.com/kstrat2001/darkmux/issues?q=is%3Aissue+is%3Aopen+label%3A%22help+wanted%22). Read [`CONTRIBUTING.md`](CONTRIBUTING.md) for the dev loop. The tractable, well-scoped M4/M5 items are the best on-ramp.
