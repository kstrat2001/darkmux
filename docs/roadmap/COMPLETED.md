# Completed themes — the shipped arc

> **Nav:** [← Roadmap](../../ROADMAP.md) · [All charters](./README.md) · [Releases](https://github.com/kstrat2001/darkmux/releases)

The forward [charters](./README.md) (M4–M8) say where darkmux is going. This page is the other half: the **completed** milestones, so the whole arc M1 → M8 is legible in one place.

This is the milestone-by-milestone *shipped record*. The decision-level **why** (the sequence of forks that shaped the architecture) lives in [`DESIGN.md` → How it got here](../../DESIGN.md#how-it-got-here--the-evolution); read that for the reasoning. Here we trace what each completed milestone actually delivered, and where its work flows into the themes still open.

A note on shape: these milestones are *not* four coequal themes. M1 and M2 were small early steps; M3 is where most of darkmux was actually built; 1.0 was the foundations-first hardening into a stable release. The sizes below reflect that.

---

## M1 — First usable build · [closed](https://github.com/kstrat2001/darkmux/milestone/1)

*Polish + verification (3 issues).* The smallest milestone: make the `darkmux swap` tool something a stranger could install and trust. Verify a clean `cargo install` from scratch ([#1](https://github.com/kstrat2001/darkmux/issues/1)), document the viewer + flags in the README ([#3](https://github.com/kstrat2001/darkmux/issues/3)), and clear the unused-code warnings ([#7](https://github.com/kstrat2001/darkmux/issues/7)). Not a theme so much as the discipline of finishing the first thing before starting the next.

## M2 — The eureka seed · [closed](https://github.com/kstrat2001/darkmux/milestone/2)

*Automatic eureka detection (1 issue).* A single, consequential idea: surface known-misconfiguration patterns from the lab notebook in `darkmux doctor` ([#4](https://github.com/kstrat2001/darkmux/issues/4)), so the harness could flag the failure shapes the operator kept hitting. This is the genesis of the eureka rules engine: the detector lineage that runs through the runtime's struggle detectors (M5) and the viewer's anomalies panel (M7, [#657](https://github.com/kstrat2001/darkmux/issues/657)). One issue; long shadow.

## M3 — Where darkmux became what it is · [closed (retired)](https://github.com/kstrat2001/darkmux/milestone/3)

*The Article era (46 issues).* Filed under "Article 3 prep" and retired once the article shipped, M3 is misnamed by its label: this is the milestone where darkmux turned from a swap tool into an AI-first orchestrator. The label was a time-box; the work is the substance of the project. What landed here:

- **The AI-first pivot.** Retire the "infrastructure, not an agent framework" framing ([#115](https://github.com/kstrat2001/darkmux/issues/115)); add the AI-built-in proposal pipeline (`external pull` + `mission propose`, [#113](https://github.com/kstrat2001/darkmux/issues/113)).
- **The internal runtime** (the "Independence mission," [#380](https://github.com/kstrat2001/darkmux/issues/380)–[#396](https://github.com/kstrat2001/darkmux/issues/396)): darkmux's own container-bounded agent loop, with the schema-isolation doctrine that keeps it free of openclaw's config shape.
- **The compaction redesign** — structured-slot extraction + graceful-degradation layers + the empirically-won default prompt ([#377](https://github.com/kstrat2001/darkmux/issues/377), [#401](https://github.com/kstrat2001/darkmux/issues/401), [#402](https://github.com/kstrat2001/darkmux/issues/402)), and the plain-text tool-call promoter that fixed the thinking-mode silent bail ([#405](https://github.com/kstrat2001/darkmux/issues/405)/[#406](https://github.com/kstrat2001/darkmux/issues/406)).
- **Mission / sprint lifecycle** — transition timestamps + verbs ([#95](https://github.com/kstrat2001/darkmux/issues/95)), mid-flight sprint growth ([#107](https://github.com/kstrat2001/darkmux/issues/107)).
- **Fleet + observability foundations** — the flow-schema versioning ([#94](https://github.com/kstrat2001/darkmux/issues/94)), tier-decision records, and a run of daemon-hardening fixes ([#273](https://github.com/kstrat2001/darkmux/issues/273), [#288](https://github.com/kstrat2001/darkmux/issues/288), [#291](https://github.com/kstrat2001/darkmux/issues/291), [#293](https://github.com/kstrat2001/darkmux/issues/293)).

It also planted the seeds the forward charters now harvest. The clearest is [#89](https://github.com/kstrat2001/darkmux/issues/89), "crew SIGNOFF can fabricate file-write claims; no orchestrator-side verification," the direct ancestor of M4's lead item ([#799](https://github.com/kstrat2001/darkmux/issues/799), the verifier-failed envelope stamp). When M3 was retired, its still-open issues were re-homed into M5/M6/M7 rather than closed; that re-homing is why the forward milestones carry issue numbers from the M3 era.

## 1.0 — Foundations-first release · [closed](https://github.com/kstrat2001/darkmux/milestone/4)

*The stable-release milestone (71 issues).* The largest completed body of work, and the one with a public artifact: [darkmux 1.0](https://github.com/kstrat2001/darkmux/releases). The bar was deliberately foundations-first: get the core right, then tag. What it delivered:

- **The dispatch-to-PR loop as shipped verbs** — `mission run` / `ship` / `abort` (the loop M4 now hardens).
- **Observability unification** ([#557](https://github.com/kstrat2001/darkmux/issues/557)): one typed flow stream, with telemetry folded in and the per-run instrument format retired.
- **The config surface** ([#661](https://github.com/kstrat2001/darkmux/issues/661)): `~/.darkmux/config.json` with visible defaults, gated features, and secret carve-outs.
- **`worker` → `runner`** ([#595](https://github.com/kstrat2001/darkmux/issues/595)), the Homebrew pipeline, the savings hero, the missions lens, and presence-driven liveness.
- **The hardening drain** — five themed patch clusters (v1.3.1 → v1.4.0) that took the milestone to zero open before moving on.

1.0 is also where the viewer's differentiation features were built *through* `mission run`: the recursive case, where darkmux's observability surfaces were produced by the same loop they visualize.

---

## Where it goes from here

The shipped arc lands at the forward themes:

- **[M4 — Dispatch-to-PR loop depth](./M4.md)** hardens the loop 1.0 shipped (and harvests M3's #89 → #799).
- **[M5 — Runtime robustness](./M5.md)** matures the inner loop M3 built.
- **[M6 — Fleet](./M6.md)** and **[M7 — Observability](./M7.md)** extend the substrate + viewer M3 and 1.0 founded.
- **[M8 — Capability routing](./M8.md)** is the endgame the whole arc was clearing ground for.

See the [charter index](./README.md) for the full table, and [`ROADMAP.md`](../../ROADMAP.md) for the map.
