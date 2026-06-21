# Roadmap charters

Per-milestone **charters** — the durable explanation of each theme's *goal, grounding, scope, and target*, gathered in one place instead of scattered across issue comments.

The roadmap has three layers, each in the tool that fits it:

- **Charters** (this directory) hold the *stable* part: what a theme is for, why (the research + dogfood grounding), the issue *clusters*, success criteria. They change rarely.
- **`theme:*` labels** hold the *live* per-theme issue set — public, repo-native, and **many-per-issue** so a cross-cutting issue can sit in two lanes at once. This is the live "what's in this lane right now" view.
- **Milestones** mean **releases** (the actual sequential, burn-down thing). Themes are *not* milestones.

Themes are **concurrent lanes, not a sequence** — work happens across several at once, and a release bundles whatever's ready from any of them (as 1.0–1.4 already did). Start at the top-level [`ROADMAP.md`](../../ROADMAP.md) for the map; come here for the depth.

## The arc

### Forward — where it's going

| Theme | Charter | Live issues | Status |
|---|---|---|---|
| **M4 — Dispatch-to-PR loop depth** | [M4.md](./M4.md) | [`theme:m4-loop-depth`](https://github.com/kstrat2001/darkmux/issues?q=is%3Aopen+label%3A%22theme%3Am4-loop-depth%22) | **active (Now)** |
| **M5 — Runtime / agent-loop robustness** | [M5.md](./M5.md) | [`theme:m5-runtime`](https://github.com/kstrat2001/darkmux/issues?q=is%3Aopen+label%3A%22theme%3Am5-runtime%22) | active lane |
| **M6 — Fleet (many machines become one)** | [M6.md](./M6.md) | [`theme:m6-fleet`](https://github.com/kstrat2001/darkmux/issues?q=is%3Aopen+label%3A%22theme%3Am6-fleet%22) | active lane |
| **M7 — Observability / viewer** | [M7.md](./M7.md) | [`theme:m7-observability`](https://github.com/kstrat2001/darkmux/issues?q=is%3Aopen+label%3A%22theme%3Am7-observability%22) | active lane |
| **M8 — Capability routing** | [M8.md](./M8.md) | [`theme:m8-routing`](https://github.com/kstrat2001/darkmux/issues?q=is%3Aopen+label%3A%22theme%3Am8-routing%22) | active lane |
| _Cross-cutting / foundations_ | (pulled into whichever lane activates it) | [`theme:cross-cutting`](https://github.com/kstrat2001/darkmux/issues?q=is%3Aopen+label%3A%22theme%3Across-cutting%22) | — |

"Active lane" means open and worked-on as it's ripe — not "waiting for the lane above to finish." **M4 leads** as the current focus; the rest run in parallel, bounded only by the dependency edges each charter names.

### Completed — the shipped record

Narrated together in [`COMPLETED.md`](./COMPLETED.md) (right-sized, not full charters — the decision-level *why* lives in [`DESIGN.md` → How it got here](../../DESIGN.md#how-it-got-here--the-evolution)).

| Theme | What shipped | Milestone |
|---|---|---|
| **M1 — First usable build** | clean install + docs + warning audit (3 issues) | [milestone/1](https://github.com/kstrat2001/darkmux/milestone/1) (closed) |
| **M2 — The eureka seed** | doctor pattern detection (1 issue) | [milestone/2](https://github.com/kstrat2001/darkmux/milestone/2) (closed) |
| **M3 — Where darkmux became what it is** | AI-first pivot, internal runtime, compaction, mission/sprint (46 issues) | [milestone/3](https://github.com/kstrat2001/darkmux/milestone/3) (retired) |
| **1.0 — Foundations-first release** | dispatch-to-PR verbs, observability unification, config, homebrew (71 issues) | [milestone/4](https://github.com/kstrat2001/darkmux/milestone/4) (closed) |

## Why labels, not theme-milestones

darkmux briefly used milestones as long-lived themes. That ran against the grain — GitHub milestones are release-shaped (a due date, a burn-down bar, one-per-issue, finish-and-close), which is exactly why a milestone list *reads* as sequential. A survey of the top OSS coding-agent projects (OpenHands, Cline, Aider, Goose, Ollama, OpenCode) confirmed the field standard: they lean on **labels** for theme/area, use milestones rarely and only as *release/date* boxes, and none model themes as milestones. So themes moved to `theme:*` labels (public, concurrent, cross-cutting-capable) and milestones are reserved for releases. The Projects board remains an optional private operator view, grouped by those labels.
