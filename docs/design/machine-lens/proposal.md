# Machine lens redesign — review + three levels of flare

Design review of `#lens=machine` (the memory ledger region), a proposal in three
escalating visual treatments, and a staged implementation path. Mockups in this
directory, all self-contained HTML/CSS built from the darkmux `/next` token set
and the **real** `/machine/resources` snapshot taken 2026-08-14 (which happens
to exercise the awkward cases: an unpriced user model, all-`unknown` arbiter
states, and `potential < current` at the machine level).

- `index.html` — side-by-side comparison, desktop + 390 px phone frames
- `level1.html` — restrained · `level2.html` — instrument · `level3.html` — the works
- `scaling.html` — the scaling rule at its stress point (N=5 mid-swap; the
  revised level-3 shape — see §8)
- View: `python3 -m http.server` in this directory (a server was left running on
  `:8917` this session), or open the files directly — they are fully standalone.

---

## 1 · Review of the current view

The operator's "raw numbers" complaint is accurate, and it is specifically a
regression: legacy's `.membar` was a four-layer metering instrument
(`viewer.html:286-303`); the React port flattened everything into classified
text lines (`ui/src/lenses/machine/memoryLedgerLines.ts` → one `<div>` per
string). Three e2e specs asserting `.memcard`/`.membar`/`#memstamp`/`.memwarn`
are `test.fixme`'d for exactly this (#1806). Concretely, at a glance the page
cannot answer:

1. **"How full is the machine?"** The single most decision-bearing pair — pool
   137.44 GB / **free 4.63 GB** — is the *last clause of a six-clause meta
   line* in 11 px muted text: `Σ potential 24.57 GB (+1 unpriced) · Σ current
   32.38 GB · limit 137.44 GB (physical pool (no budget configured)) · pool
   137.44 GB / free 4.63 GB`. Everything important and everything incidental
   share one line, one size, one color.
2. **Proportion is invisible.** `potential 24.57 GB · current 19.34 GB` as
   prose requires mental division; the whole point of the ledger — current
   sitting *inside* its commitment, headroom against the limit — is a spatial
   relationship rendered as arithmetic homework.
3. **Key/value pairs are flattened into sibling lines.** "swap used" / "5.46
   GB" / "compressor" / "728 MB" stack as six short lines; the stylesheet's own
   comment concedes "CSS cannot re-pair them; that needs the builders to emit
   structure." The pressure block spends ~8 rows on 3 numbers.
4. **Chips outrank numbers.** Each model shows two stacked chips (owner,
   state) that occupy more vertical space than its five figures; `UNKNOWN`
   is the loudest element on the page while carrying the least information.
5. **No glance path.** Uniform 11-12 px mono gives the eye no hierarchy
   between alarm (warnings), verdict (state), figure (bytes), and provenance
   (attribution/stamp). Car-dashboard logic is precisely about this: big
   needles for what you check while driving, tell-tales for the rest.
6. **Desktop wastes ~40 % of the width; phone wraps the ` · ` meta lines into
   soup.**

What legacy got right and the port must recover: **the four-layer single-axis
meter** — current fill, potential extent with dashed edge, limit marker, pool
marker. That is a genuinely good instrument (current-inside-commitment is the
interesting axis) and all three proposals keep its semantics intact.

---

## 2 · Shared information architecture (identical across all three levels)

A glance (<1 s) answers: *how full is the machine, is anything red or amber,
what's loaded and whose is it.* A second look gives exact figures. Order:

1. **Header** — fleet › machine · name — specs (unchanged)
2. **Utility tier** — one row: name · model · capabilities · status chip
3. **Machine total (the hero)** — state chip; three large readouts (**in use ·
   measured**, **committed · priced (+N unpriced)**, **pool free**); the
   four-layer meter with quarter ticks, committed-extent dashed edge, limit
   marker; k/v detail row (limit source, pool, pool free, unpriced count)
4. **Per-model instruments** — name + owner chip + state chip; the same meter
   scaled to the largest per-model figure (legacy's `perScale`); re-paired k/v
   row (ctx, weights, kv@ctx, potential, current); shrink/unpriced hints
5. **Pressure** — memory free (labeled *sole pressure trigger*), swap used and
   compressor (labeled *high-water · reports, never alarms* — the
   row-colored-by-its-own-condition lesson from viewer.html:1908-1920 carried
   into copy, not just color)
6. **Warnings** — full text, always (never only a lamp)
7. **Provenance footer** — attribution · **snapshot age** (new, from
   `generated_at_ms`) · gather ms · cache ttl · poll cadence

### State vocabulary (identical at every level)

| State | Source | Encoding (never hue alone) |
|---|---|---|
| ok | `state:"green"` | solid green fill/arc + chip word `GREEN` |
| watch | `state:"amber"` | amber + **45° hatch** + chip word |
| red | `state:"red"` / `pressure.red` | red + **135° dense hatch** + chip word |
| unknown | `state:"unknown"` | dim slate, dot pattern (bars) / low-opacity dim arc + dim needle (dials) + chip word |
| unpriced | `kv_per_token_bytes:null` | **no committed extent drawn at all** (absence, not zero) + amber `UNPRICED · potential unknown` chip + hint line |
| stale | fetch error w/ cached data | dashed amber banner with snapshot timestamp + instruments desaturated (`filter: saturate(.3) opacity(.72)`) |

**Honesty rule that governs all color:** the client never invents a judgment.
Fill/arc/lamp color comes only from server fields (`state`, `pressure.red`);
"pool free 4.6 GB" renders neutral even when it looks scary, because the
server did not call it red. Client-side thresholds would put a decision on
screen whose provenance the operator can't trace (#44).

Every meter/dial carries `role="img"` + an `aria-label` with the full textual
reading, so the instrument never says less than the text lines it replaces.

---

## 3 · The three levels

### Level 1 — restrained (`level1.html`)

Legacy's bar done properly, flat and typographic. Panel cards, 26 px tracks
(36 px on phone) with quarter ticks painted as a static gradient, hero
readouts in 22 px, re-paired k/v rows, flat pressure stat tiles, a one-line
fill legend. Obviously darkmux; the safe floor.

*Cost:* static CSS paint only — gradients and borders, zero animation, zero
JS beyond the existing 5 s poll re-render, ~35 extra DOM nodes over today.
Idle cost is literally nothing (no timers, no compositing layers).

### Level 2 — instrument (`level2.html`)

Same layout, more meter: recessed tracks (inset shadow), beveled fills with a
bright leading edge, a **caret pointer riding the current value** (a needle
for a linear gauge), labeled GB scale under the machine meter (hidden at
phone width — endpoints + ticks carry it), glowing limit tick and state
chips, and one 270° SVG arc for memory-free (it is the sole trigger and has a
natural 0-100 scale). Depth without chrome.

*Cost:* identical class of cost to level 1 — shadows/gradients are paint-once;
the arc is one static SVG. Optional polish: fill/caret movement animated with
compositor-only `transform: scaleX/translateX` transitions (~300 ms), firing
only when a poll changes a value, disabled under `prefers-reduced-motion`.
Idle: nothing.

### Level 3 — the works (`level3.html`) — REV 3, after operator feedback (direction settled at rev 2)

Rev 1 wrapped a 270° dial in a machined conic-gradient bezel with the tick
values cramped inside it, and a dial face reading `IN USE · POOL FREE 4.6`
plus a separate committed figure. Operator, verbatim: *"probably don't need
the bezel, and not sure what in use, pool, and free are doing. the needle and
values on the semi-circle should tell the story. Value in the center could be
xx / yy. the bezel just takes up too much space."* All three notes taken:

- **The bezel is gone.** The hero is a bezel-less **semicircle** — every
  pixel the chrome ring occupied now goes to arc radius, which is what makes
  the next point possible.
- **The scale lives ON the arc.** Tick values (0 · 34 · 69 · 103 · 137) sit
  outside the sweep at full label size; the needle's position against them is
  the reading. The committed extent remains one dashed tick on the arc.
- **The center readout is the current value only: `32.4 GB — IN USE`**
  (rev 3, operator: *"drop the 'out of' value… it's right there on the max
  position. current value in the center… max on max needle position"*). The
  max is already rendered as the arc's final tick, so printing it in the
  center said it twice — and the larger, dominant number was the less
  interesting one. Dropping the denominator orphaned the caption that
  explained what the scale *means*, so the meaning moved to where the number
  already sits: **the max tick is labeled** — a stacked flag reading `137 /
  LIMIT`, and `96 / BUDGET` when a #1243 budget is configured (the red demo
  gauge shows exactly that). The scale is never a bare number.

**The denominator, argued** (why the scale runs to the limit and not any
other figure): the needle shows Σ current — the measured figure. The scale
end is `limit_bytes` — the cap the residency arbiter actually enforces, so
needle-against-scale literally reads "how much of my allowance is in use,"
and under a #1243 budget the gauge becomes utilization-of-allowance with the
scale end AT the budget. Current / pool-capacity was rejected because it
implies `capacity − in use` is free, which is false (the rest of the box is
using it); current / pool-free was rejected because those two aren't on one
axis at all.

**The number audit** (the operator's real brief — every figure justified or
cut; "the API returns it" is not a reason to render it):

| Figure | Verdict |
|---|---|
| Σ current (32.4) | **needle + the center value** — the reading |
| limit (137.4) | **at the scale end, labeled** (`137 / LIMIT`, or `96 / BUDGET`) — moved off the face in rev 3, not cut: it renders once, at the max needle position |
| committed 24.6 (+1 unpriced) | **one dashed tick + one caption line** — the growth commitment; the unpriced tag must ride with it (undercount honesty) |
| pool free (4.63) | **off the face** → k/v detail row. Could not be explained on a dial in three words without lying: it is not `total − in use`, and it disagrees with `memory free 87 %` by construction. Its semantics stay flagged as the server-caption open question. |
| pool capacity | **off the face** → k/v row (equals limit here; when a budget exists the dial deliberately reads allowance, not capacity) |
| limit source | provenance → k/v row |
| memory free / swap / compressor | stay in their own instruments (odometers/tiles) — never on the dial |

The rest of rev 1 survives unchanged: the **tell-tale lamp row** (full alarm
vocabulary faintly visible unlit, lit = word + border + glow), **odometer
digit cells** for the two high-water marks, model **rows** per the scaling
rule (level3.html now shows the corrected shape at live N=2; scaling.html at
N=5), and every honesty state — unknown = dim low-opacity arc + dim needle +
the word on the face, unpriced = no committed tick + amber word, stale =
banner + desaturation + lamp with age. (Component note kept from rev 1: the
value arc's *length* is carried in `stroke-dasharray` — normalized via
`pathLength="100"` — so a dash-pattern override for "unknown" would repaint
the full arc; encode unknown as opacity, never dashes.)

*Cost:* unchanged — static paint, needles positioned by a one-time
`rotate()`, zero at idle; and strictly *less* paint than rev 1 (no conic
gradients, no per-dial shadow stack).

### The redline (rev 4 — operator: keep lamps + odometers; add a redline)

Operator, verbatim: *"one more flare i'd ask for is a redline that lights up
with a glow when the needle reaches"*, later scoped down to: *"for now just
make the value a glowing bright red when in the redline. the redline arc
itself can be red to match when the text changes."*

**What it is:** an end-cap arc segment at the limit position — the last
~2.5 % of the track's path length, which is rendering affordance (minimum
visible width), not a threshold claim. It sits at the scale end because that
is where the server's own red condition begins: `model_ledger.rs` goes Red
when `pressure.red` **or** `current_total > limit_bytes`, and the second
disjunct's boundary IS the limit — the position the arc already ends at.
A zone starting any earlier would be a client-invented percentage, which §7
rejects.

**Where the threshold comes from (the provenance decision):** the lit state
keys on **`machine.state == "red"` — one server field, zero client
arithmetic.** The center value turns bright red with a static glow and the
redline segment turns red **in the same flip** (the operator's "to match
when the text changes"): one condition, two elements, no independent
thresholds, no staged transition. When the red verdict came from the
over-limit disjunct, the needle is visibly pegged at the now-red line and
the tachometer metaphor completes itself; when it came from `pressure.red`,
the needle sits mid-scale and the PRESSURE lamp says why. Distinguishing the
two disjuncts on the lamp row (the `OVER LIMIT` lamp) requires either the
client evaluating `current ≥ limit` — defensible, since it is the server's
own published rule applied to two server-supplied numbers and is the exact
arithmetic that already clamps the needle at 100 %, and it claims only that
geometric fact if the server's rule ever drifts — or a server-side
`red_reason` field, noted as an **optional server enhancement** (unlike
`expected[]`, nothing here is blocked without it; the lamp can ship on the
client comparison and migrate).

**Cost:** the glow is a static `drop-shadow` on two elements — one repaint
on state entry, zero at idle, **never pulsed**. A pulsing glow would run
continuously at precisely the moment the machine is under memory pressure:
the observer joining the observed in its purest form. If any motion is ever
added it is a single one-shot transition on entering the state,
reduced-motion-gated.

**Color is not the only channel:** the same flip is carried by the state
word on the face (`RED · PRESSURE` / `RED · OVER LIMIT`), the lamp row
(PRESSURE or OVER LIMIT lighting with word + border), the needle position,
and — in the over-limit case — the center value numerically exceeding the
labeled max beside it. The demo strip shows both red variants lit.

**Convergence, stated plainly:** with the bezel gone and models as rows,
levels 2 and 3 have converged considerably. The remaining real differences
are the hero form (semicircle gauge vs linear meter with caret), the
tell-tale lamp row, and the odometer treatment of the high-water marks —
each a genuine difference, but the honest framing is now **two real options
plus a swappable hero**: level 2's layout would accept level 3's semicircle
as its machine instrument with no other change, and that hybrid may be the
best of both.

---

## 4 · Mobile

All three are single-column already. Level 1/2: tracks grow to 36-38 px
(legacy's own ≤768 px move), tiles stack, k/v rows wrap as label-over-value
pairs, mid-scale labels hide (level 2). Level 3: gauges wrap to a vertical
stack with the machine dial full-width; lamps and odometers wrap. Verified in
the 390 px frames in `index.html` — no horizontal document scroll at any
level (the render-sanity contract in `styles.css`).

---

## 5 · Accessibility

- Severity is triple-encoded: hue + pattern (solid/45°/135°/dots) + the state
  word in a chip or on the dial face. Ran the skill validator on the darkmux
  status set against the panel surface: green↔amber deutan ΔE is **8.2** —
  passing but near the floor, which makes the pattern/word channel mandatory,
  not decorative. All five colors pass ≥3:1 contrast on the panel surface.
- Value labels always sit on the track/face in `--fg`, never on a colored
  fill.
- `role="img"` + full-sentence `aria-label` per instrument; the k/v rows
  remain real text.
- Unlit tell-tales at level 3 keep a visible outline (not color-alone
  presence).

---

## 6 · Staged implementation path (for an implementer who is not me)

**Stage 1 — structure + the level-1 meter (most of the benefit).**
Replace the string-array builders with typed card objects: a new
`memoryLedgerCards.ts` returning `{kind:"machine"|"model"|"pressure"|
"warnings"|"foot", state, cur, pot, scale, limit, pool, kv: [...]}` — the
percent math (clamping included) lives in the builder, testable without DOM.
`MachineLens.tsx` renders `<MemCard>`/`<MemBar>`/`<KvRow>` components.
**Reuse legacy's class names** (`memcard`, `membar`, `memwarn`, `memstamp`)
so the three fixme'd e2e specs in #1806 un-fixme nearly verbatim. ~150 lines
of CSS in `styles.css`'s machine-lens section. Two consequences to handle in
the same PR: the parity goldens pin today's flattened `innerText` and must be
updated (this is the deliberate divergence the port's own comments predicted:
"proper k/v rows need the builders to emit structure, which is a follow-up"),
and `lineClass()`'s content-pattern classifier shrinks or disappears because
structure is now emitted, not inferred.

**Stage 2 — hierarchy.** Hero readouts (the three big numbers), pressure
tiles, snapshot-age in the stamp (computed at render from `generated_at_ms`,
re-rendered by the existing poll — no per-second timer), stale
banner + desaturation, hatch/dot patterns, the fill legend.

**Stage 3 — the chosen flare level.** Level 2's depth + caret + arc, or level
3's cluster (dials/lamps/odometers as three small components: `<Dial>`,
`<LampRow>`, `<Odometer>`). Optional data-change transitions, gated behind
`prefers-reduced-motion`.

Stages 1-2 are worth shipping regardless of which level wins; level choice
only decides Stage 3.

---

## 7 · Deliberately rejected

- **A charting library / any new dependency** — forbidden by the bundle
  contract (`include_str!`, React + TanStack Query only). Everything here is
  CSS + inline SVG.
- **Client-side thresholds** ("free < 8 GB → amber") — puts an untraceable
  judgment on screen; the server's `state`/`red` fields are the only color
  sources. If `unknown` dominates real snapshots (as in this one), the fix is
  server-side state enrichment, not client cosmetics.
- **Stacked per-model segments inside the machine bar** — tempting, but it
  makes color do two jobs at once (model identity *and* status) on one
  instrument, and the unpriced model breaks the "whole." Per-model detail
  stays in per-model instruments.
- **Donut/pie of per-model share** — poor comparison accuracy vs aligned
  bars; same unpriced-breaks-the-whole problem.
- **Sparklines / usage history** — `/machine/resources` is a snapshot;
  client-side history buffering invents a recording feature the lab/fleet
  boundary doesn't have, and a chart that redraws every poll works against
  the observer doctrine. If history is ever wanted, it is a server/artifact
  feature first.
- **Continuous animation of any kind** (needle sweep loops, pulsing glows,
  animated hatches) — the observer must not join the observed. All motion is
  data-change-only, compositor-only, reduced-motion-gated.
- **Radial gauges for the per-model *bars* at levels 1-2** — the linear meter
  is strictly better at showing current-inside-committed-inside-scale on one
  axis; dials earn their place only when the whole view commits to the
  cluster idiom (level 3), where the committed marker becomes an arc tick.
- **A live "free RAM" countdown or per-second age ticker** — needs a 1 s
  timer for a number whose source refreshes every 5 s; fake precision at real
  cost.

---

## 8 · Scaling scenarios — how many meters, and what happens during a swap

The operator's question ("does this UI have to scale the number of meters?")
exposes the real gap in the level-3 sketch, and answering it changes the
design. Ground truth first (measured, not guessed): profiles declare 1 or 2
models (all 22 in the registry: fourteen declare 1, eight declare 2, never
more); 2 are resident right now; the user can hand-load anything outside the
namespace, so N is small but genuinely unbounded — realistic is 2-5, and the
design must not break at 8. And the decisive fact: **the API has no loading
state.** `LedgerState` is exactly `Green | Amber | Red | Unknown`; a model
mid-load is simply absent one poll and present the next.

### The scaling rule

> **Exactly one dial — machine total. Models are always constant-height
> bar-rows, grouped darkmux-first, sorted alphabetically within group.
> At every N, with no threshold and no mode switch.**

This revises level 3: the flanking model dials in `level3.html` are
**superseded** (kept there as the N≤2 beauty shot; `scaling.html` shows the
corrected shape). Why one dial, argued both ways per the coordinator's
scenario 4:

*For per-model dials:* current-inside-committed per model is the
darkmux-specific reading, and at the typical N of 2-3 a dial row looks
genuinely great — that is what the level-3 mockup shows.

*Against, and winning:* (a) a dial spends most of its pixels on bezel, not
data, and radial positions compare poorly across instruments — aligned linear
bars are simply the better form for "compare five models' fills" (the same
reason the dataviz method reserves radial forms for a single hero value);
(b) dials have a fixed footprint with no graceful degradation — at N=5 you
either shrink them to illegibility or wrap them, and a wrapping flex row is
exactly the geometry that reflows violently when a model appears or departs;
(c) a bar-row list has **no readability cliff**, so no N+1 rule is ever
needed — the rule is that there is no threshold. The machine total is the one
number read at a glance; making it the only dial makes N a non-problem *by
construction* and keeps the glance layer (dial + lamps + odometers)
above the fold at any N.

### Scenario 1 — N = 1, 2, 3, 5, 8

Same layout at every N: hero cluster, then `⌈groups⌉` + N rows. A row is
~64 px (phone ~76 px). N=1-3: everything on one desktop screen. N=5: rows
start needing one scroll on a phone; the glance layer is unaffected
(`scaling.html` shows exactly this). N=8: the page is a scrolling ledger —
~2 phone screens of rows — which is the correct shape for 8 of anything;
the hero never moves. Group headers ("DARKMUX-MANAGED · n" / "USER-LOADED ·
n") render only when both groups are non-empty, so today's common case adds
no chrome. Row detail (the k/v line) never collapses at high N — hiding
figures to fake density would violate record-exhaustively-display-selectively
in the wrong direction; the page just scrolls.

**Sort stability is part of the instrument.** Rows sort by (owner group,
identifier) — never by size or current usage, because a live figure as a sort
key reorders rows *while the operator is looking at them*, which is the same
trust-loser as a vanishing gauge. An arriving model inserts at its
alphabetical slot (rows below shift by one constant-height slot — visible but
orderly); it does not append at the end, which would silently break the
"where do I find model X" invariant.

### Scenario 2 — a swap mid-glance (A unloads, B loads between polls)

What the raw data gives: A's row would vanish and B's would appear, with a
reflow in between. Three client-side mitigations, all derivable from poll
history alone (no API change, no timers — all state advances on the existing
5 s poll):

1. **The departed-model ghost.** A model present last poll and absent this
   poll keeps its row for one further poll cycle, rendered dimmed with an
   empty dashed track and `DEPARTED · last seen HH:MM:SS`, plus its last
   observed current as *text* ("last observed 15.1 GB"). This is honest — it
   states exactly what was observed and when — and it converts a silent
   vanish into an announced departure. The row then retires and the list
   closes up once, not twice.
2. **The Δ RESIDENCY tell-tale.** A lamp in the cluster lights (amber, word +
   glow) whenever the resident set changed within the last poll or two, so
   the operator's eye is *told* the ground shifted even if they were looking
   at the dial. Poll-derived, costs nothing.
3. **The tachometer never leaves the dash.** The machine dial is unaffected
   by any residency change — its scale is the pool, not the model set — which
   is the deeper reason it must be the only dial: it is the one instrument
   with a stable identity across every scenario here.

### Scenario 3 — a model appearing

It arrives fully-formed (the ledger only reports residents), at its sorted
slot, with a `NEW · first seen Ns ago` accent chip and a 2 px accent left
border for its first poll cycle — enough to say "this was not here before"
without theater. No entry animation by default: an animating arrival costs
render on the measured host and is invisible anyway unless the operator
happens to be watching at that second; the NEW chip persists a full poll
cycle, which a 300 ms animation does not. (If any motion is ever added it is
a single opacity fade, compositor-only, reduced-motion-gated — polish, never
load-bearing.) Space is *not* reserved ahead of arrival — the client cannot
know what is coming from the current API. That leads to:

### The loading signal — flagged plainly as SERVER-SIDE prerequisite work

darkmux knows what a dispatch's staffing declares; the ledger knows what is
resident; the difference is derivable server-side as **"expected but not yet
resident."** If `/machine/resources` ever carries an `expected[]` set (or a
`Loading` ledger state), the row vocabulary already has its slot: a
dashed-outline placeholder row with an empty track, `EXPECTED · not yet
resident`, and **no fabricated figures** — it reserves layout space, gives a
real loading signal, and upgrades in place to a live row on the poll where
the model lands (no reflow at all for staffed loads). `scaling.html` renders
this row **explicitly labeled as prerequisite** — nothing else in the design
depends on it, and the client must not guess at it from profile data it
happens to have cached, because the profiles registry is not the dispatch's
staffing (#1135 taught what happens when the client assumes the load config).

### Scenario 5 — ownership as the grouping dimension

Already the natural structure and already in the data: `owner` is the
namespace contract surfaced. darkmux-managed lists first (this is darkmux's
own instrument panel; "what I brought up" is the primary read), user-loaded
second ("what else is on this box" — visible, never touched, per the
namespace doctrine). At low N with a single group present, headers disappear
and the owner chip on each row carries the information alone. This grouping
also future-proofs the one realistic high-N case — a user who hand-loads a
zoo of models around a small darkmux set — by keeping darkmux's own residents
in a short stable list at the top regardless of what the zoo does.

### What changed in the deliverables

`scaling.html` — one added mockup showing the rule at its stress point (N=5
mid-swap: ghost + NEW + EXPECTED-placeholder + Δ RESIDENCY lamp + grouped
rows, hero gauge unmoved). Levels 1 and 2 already comply with the rule (their
models are bar-cards); level 3's per-model dials were the one casualty —
`level3.html` rev 2 now renders the corrected shape (hero semicircle + rows)
at live N=2. The stage-1 implementation note gains one requirement: the row
component keeps previous-poll residency state (`Map<identifier,
lastSeen>`) to derive ghost/NEW — a few lines in the lens, no new query.

## 9 · Known open questions for the operator

1. **Which level.** (Or: level 2 as default with level 3 behind a viewer
   toggle — noted as an *add-on*, not baked into any level, per the
   same-IA comparison rule.)
2. `pool free 4.63 GB` vs `memory free 87 %` read as contradictory to a fresh
   eye (they measure different things). The k/v labels here ("pool free" vs
   "memory free · sole pressure trigger") are the mockups' answer; if that's
   not enough, the fix is a server-side `attribution_note`-style caption, not
   a client guess.
3. The machine snapshot rendered `state:"unknown"` everywhere while fully
   measured — if that is the common real-world case, the green/amber/red
   ledger states may deserve a server-side look, since every level of this
   design inherits whatever the arbiter emits.
