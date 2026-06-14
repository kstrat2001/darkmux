# darkmux roadmap

darkmux is past its [1.0](https://github.com/kstrat2001/darkmux/releases) foundations release. This file is the human-readable map of where it's going; the live detail lives in [GitHub Milestones](https://github.com/kstrat2001/darkmux/milestones) and [Issues](https://github.com/kstrat2001/darkmux/issues).

Milestones are **themes**, not release numbers — they're long-lived and decoupled from cargo versions (a release can ship parts of several). The themes are sequenced, but they overlap: foundational and cross-cutting work is pulled into whichever theme activates it.

## The north star

darkmux orchestrates **local** LLMs to do real work — a profile multiplexer + a dispatch-to-PR loop + a unified observability stream, driven by a frontier orchestrator (Claude Code, Cursor, etc.) that "writes the loop." Optimization, not replacement; the harness before the model; the operator always in the loop with full provenance. See [`README.md`](README.md) and [`CLAUDE.md`](CLAUDE.md).

## Now — M4: Dispatch-to-PR loop depth

The lead theme. Make the daily-driver loop rock-solid and formalize it as a first-class capability: the orchestration loop (coder → fresh-context review → fix → frontier sign-off → PR), its failure modes, and its termination discipline.

This theme is **grounded in the loop-engineering literature** — the field crystallized into a named discipline over the last several months, and the research validates the architecture darkmux already ships (observability-driven detectors, cross-context verification). Key items carry the citations:

- [#48](https://github.com/kstrat2001/darkmux/issues/48) — formalize the loop as the standard dispatch-to-PR skill
- [#799](https://github.com/kstrat2001/darkmux/issues/799) — terminate on a **verifiable check**, never agent self-assessment ([why](https://arxiv.org/abs/2602.03485): ~85–95% of self-rechecks are confirmatory)
- [#453](https://github.com/kstrat2001/darkmux/issues/453) — wrong-diagnosis-stuck escalation (the named sycophancy / non-convergence mitigation)
- [#849](https://github.com/kstrat2001/darkmux/issues/849) — persist corrections into the next brief (doom-loop fix) + recheck-vs-rethink escalation policy

## Next — the sequence

| Theme | Milestone | What it covers |
|---|---|---|
| **Runtime / agent-loop robustness** | [M5](https://github.com/kstrat2001/darkmux/milestone/6) | the *inner* loop: recovery behaviors, compaction tuning, streaming observability, lab reproducibility, and the harness that [learns from its own failure record](https://github.com/kstrat2001/darkmux/issues/400) |
| **Fleet — many machines become one** | [M6](https://github.com/kstrat2001/darkmux/milestone/7) | cross-machine workspace handoff, fleet-wide mission management, topology-driven liveness + views |
| **Observability / viewer** | [M7](https://github.com/kstrat2001/darkmux/milestone/8) | one drill-down viewer over the unified stream: crew roster, sprint burn-down, eureka anomalies |
| **Capability routing** | [M8](https://github.com/kstrat2001/darkmux/milestone/9) | model selection by capability not hardware tier; utility-tier decoupling; backend abstraction |

**Cross-cutting / backlog** (architecture, foundations, onboarding, doctrine) isn't a milestone of its own — those issues get pulled into whichever theme activates them. Examples: typed flow payloads (#511), centralized config (#507), workspace crate split (#463), multi-frontier bootstrap (#179).

## How we decide

darkmux's design decisions are **grounded in published research where it exists** — we'd rather cite a paper than assert from intuition, and we link the source on the issue. The framing is convergence, not priority: independent research and this project keep arriving at the same architecture, and the citations explain why it works. The compaction → [StructuredSlot](DESIGN.md) story is the template; the loop-engineering work on M4 is the latest.

## Contributing

New here? Look for [`good first issue`](https://github.com/kstrat2001/darkmux/issues?q=is%3Aissue+is%3Aopen+label%3A%22good+first+issue%22) and [`help wanted`](https://github.com/kstrat2001/darkmux/issues?q=is%3Aissue+is%3Aopen+label%3A%22help+wanted%22). Read [`CONTRIBUTING.md`](CONTRIBUTING.md) for the dev loop. The tractable, well-scoped M4/M5 items are the best on-ramp.
