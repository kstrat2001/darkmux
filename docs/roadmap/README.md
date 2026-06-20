# Roadmap charters

Per-milestone **charters** — the durable explanation of each theme's *goal, grounding, scope, and target release*, gathered in one place instead of scattered across issue comments.

- The **charter** (this directory) holds the *stable* part: what the theme is for, why (the research + dogfood grounding), the issue *clusters*, success criteria, and the target version. It changes rarely.
- The **[GitHub milestone](https://github.com/kstrat2001/darkmux/milestones)** holds the *live* part: the authoritative set of issues and their status. It changes constantly.

A charter **links** to its milestone rather than copying the issue list — so the thing that churns is never stale-copied here. Start at the top-level [`ROADMAP.md`](../../ROADMAP.md) for the map; come here for the depth.

## The arc

The full thematic arc, M1 → M8. The **completed** half is the shipped record (narrated in [`COMPLETED.md`](./COMPLETED.md)); the **forward** half is the per-theme charters.

### Forward — where it's going

| Theme | Charter | Milestone | Status |
|---|---|---|---|
| **M4 — Dispatch-to-PR loop depth** | [M4.md](./M4.md) | [milestone/5](https://github.com/kstrat2001/darkmux/milestone/5) | **active (Now)** |
| **M5 — Runtime / agent-loop robustness** | [M5.md](./M5.md) | [milestone/6](https://github.com/kstrat2001/darkmux/milestone/6) | next |
| **M6 — Fleet (many machines become one)** | [M6.md](./M6.md) | [milestone/7](https://github.com/kstrat2001/darkmux/milestone/7) | sequenced |
| **M7 — Observability / viewer** | [M7.md](./M7.md) | [milestone/8](https://github.com/kstrat2001/darkmux/milestone/8) | sequenced |
| **M8 — Capability routing** | [M8.md](./M8.md) | [milestone/9](https://github.com/kstrat2001/darkmux/milestone/9) | sequenced |

### Completed — the shipped record

Narrated together in [`COMPLETED.md`](./COMPLETED.md) (right-sized, not full charters — the decision-level *why* lives in [`DESIGN.md` → How it got here](../../DESIGN.md#how-it-got-here--the-evolution)).

| Theme | What shipped | Milestone |
|---|---|---|
| **M1 — First usable build** | clean install + docs + warning audit (3 issues) | [milestone/1](https://github.com/kstrat2001/darkmux/milestone/1) (closed) |
| **M2 — The eureka seed** | doctor pattern detection (1 issue) | [milestone/2](https://github.com/kstrat2001/darkmux/milestone/2) (closed) |
| **M3 — Where darkmux became what it is** | AI-first pivot, internal runtime, compaction, mission/sprint (46 issues) | [milestone/3](https://github.com/kstrat2001/darkmux/milestone/3) (retired) |
| **1.0 — Foundations-first release** | dispatch-to-PR verbs, observability unification, config, homebrew (71 issues) | [milestone/4](https://github.com/kstrat2001/darkmux/milestone/4) (closed) |

Milestones are **themes, not release numbers** — long-lived and decoupled from cargo versions ([why](../../ROADMAP.md)). Each forward charter names its own target release.
