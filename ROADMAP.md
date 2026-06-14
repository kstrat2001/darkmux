# darkmux roadmap

darkmux is past its [1.0](https://github.com/kstrat2001/darkmux/releases) foundations release. This file is the human-readable map of where it's going; the live detail lives in [GitHub Milestones](https://github.com/kstrat2001/darkmux/milestones) and [Issues](https://github.com/kstrat2001/darkmux/issues).

Milestones are **themes**, not release numbers — they're long-lived and decoupled from cargo versions (a release can ship parts of several). The themes are sequenced, but they overlap: foundational and cross-cutting work is pulled into whichever theme activates it.

## The north star

darkmux orchestrates **local** LLMs to do real work — a profile multiplexer + a dispatch-to-PR loop + a unified observability stream, driven by a frontier orchestrator (Claude Code, Cursor, etc.) that "writes the loop." Optimization, not replacement; the harness before the model; the operator always in the loop with full provenance. See [`README.md`](README.md) and [`CLAUDE.md`](CLAUDE.md).

## Now — M4: Dispatch-to-PR loop depth

The lead theme. Make the daily-driver loop rock-solid and formalize it as a first-class capability: the orchestration loop (coder → fresh-context review → fix → frontier sign-off → PR), its failure modes, and its termination discipline.

This theme is **grounded in the loop-engineering literature** — the field crystallized into a named discipline over the last several months, and the research validates the architecture darkmux already ships (observability-driven detectors, cross-context verification). Key items carry the citations:

- [#799](https://github.com/kstrat2001/darkmux/issues/799) — terminate on a **verifiable check**, never agent self-assessment. Process-reward-model research shows step-wise verification catches *silent errors* (code runs, result wrong) that outcome-only checks miss ([arXiv 2604.24198](https://arxiv.org/abs/2604.24198)); self-verification in the same context is mostly confirmatory, not corrective ([arXiv 2602.03485](https://arxiv.org/abs/2602.03485)). **Lead item.**
- [#849](https://github.com/kstrat2001/darkmux/issues/849) — persist corrections into the next brief (doom-loop fix) + recheck-vs-rethink escalation, with **quantified escalation budgets** ([Graph-Harness termination, arXiv 2604.11378](https://arxiv.org/abs/2604.11378))
- [#453](https://github.com/kstrat2001/darkmux/issues/453) — wrong-diagnosis-stuck escalation (concurrent with the #389 watchdog tuning)
- [#48](https://github.com/kstrat2001/darkmux/issues/48) — formalize the loop as the standard dispatch-to-PR skill ([#63](https://github.com/kstrat2001/darkmux/issues/63) core stages = the control-flow backbone)

## Next — the sequence

| Theme | Milestone | What it covers | Research anchor (verified) |
|---|---|---|---|
| **Runtime / agent-loop robustness** | [M5](https://github.com/kstrat2001/darkmux/milestone/6) | the *inner* loop: error taxonomy first, recovery behaviors, compaction tuning, streaming, and the harness that [learns from its own failure record](https://github.com/kstrat2001/darkmux/issues/400) | self-healing = detect + taxonomy + replan ([arXiv 2605.06737](https://arxiv.org/abs/2605.06737)); failure-aware observability ([arXiv 2606.01365](https://arxiv.org/abs/2606.01365)) |
| **Fleet — many machines become one** | [M6](https://github.com/kstrat2001/darkmux/milestone/7) | version-compat first, then event-sourced mission state, cross-machine workspace handoff, topology-driven liveness | event-sourcing as the distributed-agent backbone ([akka.io](https://akka.io/blog/event-sourcing-the-backbone-of-agentic-ai)); capability discovery ([arXiv 2511.19113](https://arxiv.org/abs/2511.19113)) |
| **Observability / viewer** | [M7](https://github.com/kstrat2001/darkmux/milestone/8) | one drill-down viewer over the unified stream; crew roster, sprint burn-down, eureka anomalies; an OTel-GenAI alignment spike | OpenTelemetry [GenAI semantic conventions](https://opentelemetry.io/docs/specs/semconv/gen-ai/) (emerging standard, still in development) |
| **Capability routing** | [M8](https://github.com/kstrat2001/darkmux/milestone/9) | backend abstraction first, then capability vectors, then continuous routing (not hardware-tier labels) | RouteLLM ([arXiv 2406.18665](https://arxiv.org/abs/2406.18665)); Skill Profiles ([arXiv 2602.02386](https://arxiv.org/abs/2602.02386)) |

**Cross-cutting / backlog** (architecture, foundations, onboarding, doctrine) isn't a milestone of its own — those issues get pulled into whichever theme activates them. Examples: typed flow payloads (#511), centralized config (#507), workspace crate split (#463), multi-frontier bootstrap (#179).

## How we decide

darkmux's design decisions are **grounded in published research where it exists** — we'd rather cite a paper than assert from intuition, and we link the source on the issue. The framing is convergence, not priority: independent research and this project keep arriving at the same architecture, and the citations explain why it works. The compaction → [StructuredSlot](DESIGN.md) story is the template; the loop-engineering work on M4 is the latest.

**Citations are verified, not just collected.** This roadmap was re-processed against the 2025-26 literature by a multi-agent pass whose second half does nothing but *re-fetch every cited source and confirm it supports the claim* — because a confident citation under a correctly-recalled label is exactly where fabrication hides. That pass dropped several plausible-but-unverifiable citations (and caught one false-negative — a real paper its tooling couldn't reach). A roadmap that cites dead or misattributed sources reads as *less* researched, not more.

**Where the literature runs out — the white space we lead with.** Nearly all current routing, fleet, and observability research assumes **cloud or homogeneous hardware**. darkmux's premise — a **heterogeneous local fleet of Apple-Silicon Macs over a tailnet**, running quantized models — is genuinely under-served by the literature. Multiple research themes independently flagged the same hole. That gap *is* the differentiator: the detector-driven harness, the event-sourced two-tier local fleet, and capability routing on local quantized models are darkmux's to define, not to follow.

## Contributing

New here? Look for [`good first issue`](https://github.com/kstrat2001/darkmux/issues?q=is%3Aissue+is%3Aopen+label%3A%22good+first+issue%22) and [`help wanted`](https://github.com/kstrat2001/darkmux/issues?q=is%3Aissue+is%3Aopen+label%3A%22help+wanted%22). Read [`CONTRIBUTING.md`](CONTRIBUTING.md) for the dev loop. The tractable, well-scoped M4/M5 items are the best on-ramp.
