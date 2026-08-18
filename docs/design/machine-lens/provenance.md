# Machine page provenance — where every value comes from

Every figure on the machine page traces to a probe on your own machine, through
a named transformation, to the pixel you see. This document is that trace,
**tested against a live daemon** rather than asserted: every row below was
re-verified on 2026-08-15 against a live `/machine/resources` +
`/machine/specs` pair on the operator's own machine (the same daemon at
`http://127.0.0.1:8765` this doc has always tested against), with the
verification method noted per row. This is operator sovereignty (#44) applied
to the page itself — the operator never has to wonder where a number came
from.

**This is a same-day reconciliation pass, not a fresh trace.** The page
changed repeatedly on 2026-08-14/15 (#1811, #1818, #1819, #1821 all landed
same-day) and this document was updated piecemeal, mid-flight, by several
different passes. This pass re-read every row against the code as it stands
now and re-ran the two probes; it found real drift (the gauge's center
readout and needle changed SUBJECT, not just units; the tell-tale lamp row
lost a lamp the doc never recorded; the "committed" caption pixel the table
described no longer exists as one string) and one live code defect (a
message string that renders a KB/token rate as `2` instead of `204.8` —
documented as an open finding, not silently corrected here).

The figures are one real snapshot and will not match your machine, or this
machine tomorrow — what is being asserted is the **identity** in each
"Tested" cell (Σ of these fields equals that field; this sysctl equals that
pixel), which is what re-verification actually checks.

**The two feeds:**

| Endpoint | Producer | What it carries |
|---|---|---|
| `GET /machine/resources` | `darkmux_profiles::model_ledger::gather()` — served by `machine_resources_handler` (`crates/darkmux-serve/src/lib.rs`), identical to `darkmux machine resources --json` | The memory ledger: residents, potential vs current, pool/limit, pressure, severity-tagged messages |
| `GET /machine/specs` | `machine_specs_handler` (same file) | Hardware line, machine identity, utility-tier model |

The gather runs **zero model dispatches** — it reads `lms ps --json` /
`lms ls --json` (LMStudio metadata), `vm_stat`, `sysctl`, and `ps` (kernel
counters), each under a bounded timeout. The client polls every 5 s
(`MACHINE_MEM_POLL_MS`, `ui/src/lib/queryKeys.ts`); the server caches the
gather for 2 s (`MACHINE_RESOURCES_CACHE_TTL`) so a poll burst costs one
gather. Both cadences are printed in the page footer ⑳ — cadence is a
recorded knob, never silent.

![Provenance key — circled numbers map to the table](provenance-key.jpg)

> **The key image is stale on more than units and one card, and this pass
> widens the warning rather than narrowing it.** It still shows the decimal
> figures (`137`, `32.4 GB`) that finding 3 describes, and it circles as ②
> and ③ a `darkmux/utility` card that no longer renders at all — ② is now
> the small `utility` badge on a residency row. That much was already known.
> What this pass found in addition: the image's gauge is a SINGLE-RING
> instrument with a plain centered `IN USE` readout and no legend — the dial
> it shows no longer exists in the code at all. The dial is now a stacked
> band (darkmux / other / committed growth) with a three-item legend below
> it, the center readout reads the MACHINE's used memory (not darkmux's own,
> which is what the image's circled ④ actually pointed at), and there is no
> dashed commit tick. **Do not use the image to locate ④ or the gauge's fill
> color** — read the table row and the new "stacked band + legend" section
> below instead. The image is still directionally useful for the lamp row,
> the odometer tiles, and the residency rows below the gauge, which are
> visually close to what it shows. Retake it (and re-circle ④ and the new
> legend) when the annotated overlay is next regenerated — not done in this
> pass, per this doc's own rule against fabricating a replacement.

## The trace, value by value

Byte figures render through `memBytes()` (`ui/src/lib/format.ts`): **binary**
GiB/MiB (`bytes / 2³⁰`, two decimals), and the gauge's glance layer through
`gaugeValueParts()` (same divisor, one decimal). Since #1811 that is the ONLY
convention on the page — see finding 3 for what it replaced and what is left.

**The copy-vs-data rule.** Not every string on a page like this came off a
probe — some is UI copy written by hand. Where the two sit next to each
other, **hardcoded copy must not render indistinguishably from a value that
was read**, because the promise above (every figure traces to a probe) is
falsifiable only if a reader can tell which strings are making it.

This page's current answer is stronger than a styling convention: it stopped
rendering the hand-written strings at all. The `darkmux/utility` card — whose
`handles  compaction · mission-compile · estimate · scribe` line was
hardcoded in the TypeScript, with no capability list on `/machine/specs` to
read (`utility_model` carries only `{id, loaded}`) — was deleted outright as
config rather than machine state. Its gloss survives as a `title` on the
badge at ②. Where live and static values do sit together in future, the
mechanism is brightness: a live value is `--fg`, hand-written copy stays
`--dim`.

The rule is stated here rather than assumed because the code cites it by
name (`memoryLedgerLines.ts` and its tests).

| # | The pixel says | Source | Transformation | Tested |
|---|---|---|---|---|
| ① | `Apple M5 Max · 128 GB` | `/machine/specs` → `cpu_brand` (sysctl `machdep.cpu.brand_string`), `ram_total_bytes` (sysctl `hw.memsize`) | `specOf()` (`lenses/fleet/cards.ts:65`): **binary**, `round(bytes/2³⁰)` — labeled `GB`, see finding 3 | 137438953472 / 2³⁰ = 128 ✓ — and ⑬ now reads `128.00 GiB` for the same field |
| ② | the small `utility` badge on one residency row | `/machine/specs` → `utility_model.id` + the row itself | `utilityModelId()` + `isUtilityTierRow()`: badges the row whose `identifier` **or** `model_key` is the configured id — mirroring the server's own two-field test (`m.identifier == id \|\| m.model == id`). Needs no `loaded` flag: a row exists iff `lms ps` lists the model, so the ROW is the residency claim and the badge only answers identity. Neutral, never a severity color — identity is not a verdict | badge present on exactly the 4b row live ✓; ledger rows == `lms ps` verified 3-for-3 ✓; inverted cases unit-tested ✓ |
| ④ | `104.0 GiB` — `MACHINE USED` (seven-segment center readout + needle; was odometer cells until 2026-08-15) | `/machine/resources` → `pool.used_bytes` (needle + center reading), falling back to `machine.current_bytes` only when `pool` is absent | **Changed subject, not just units (`MachineHealthRegion.tsx:117-132`, #1821).** The center readout and the needle both track the MACHINE's used memory now, not darkmux's own share — the operator caught the two disagreeing (a needle at ~82% beside a readout reading darkmux's own 36.8 GiB) and the fix made both read the same subject. `centerVal = gaugeValueParts(pool.used_bytes ?? machine.current_bytes)`; needle angle = `band.needleAngleDeg = band.usedPct × 1.8°`, where `usedPct = max(pool.used_bytes/scale, machine.current_bytes/scale) × 100` (`computeBandGeometry`, `machineGauge.ts:192-213`) — the `max()` guards against `pool.used` reading below darkmux's own share on a degraded pool read. darkmux's OWN figure is still on the face, but moved to the legend (see "The stacked band + legend" below), not the center. The center cells are centered on the hub with the unit hung outside that centering (`odoLayout`, `MachineHealthRegion.tsx`); since 2026-08-15 each cell is a seven-segment glyph (`sevenSegmentPolygons`, `machineGauge.ts`) — the same glyph table the pressure tiles use, unlit segments ghosted at `SEVEN_SEG_GHOST` (5%, operator-tuned by looking at 0/3/5%) — replacing the boxed odometer digits the operator judged "a generational divide problem" | live: `pool.used_bytes` = 111662612480 → `gaugeValueParts` = 104.0 GiB ✓ = field, rendered center digits; `usedPct` = 111662612480/137438953472 = 81.25% → needle at 146.25° ✓; darkmux's own share (`current_bytes`=39219799696) is 28.5% — the two figures visibly disagree on this machine right now, which is exactly the case the fix targets |
| ⑤ | arc ticks `0 · 32 · 64 · 96` | `limit_bytes` | quarter marks of the scale, `limit × k/4` in whole **binary** GiB (`gaugeTickLabel`, `machineGauge.ts:79-83`) | 128 × ¾ = 96 ✓ — decimal labeled this same arc `0 · 34 · 69 · 103`, finding 3 |
| ⑥ | `128 / LIMIT` at the max position (+ the redline end-cap) | `limit_bytes` + `limit_source` | limit resolution: `budget > physical pool > none` (`compute_ledger`, `model_ledger.rs:895-899` — line drifted from the last-cited `:383` as #1819/#1821 grew the file above it; see the derived section's own note that names by function, not line, for exactly this reason); word is LIMIT, or BUDGET when `limit_source:"budget"` | `limit == pool.capacity` (137438953472 both) ✓, source `physical_pool` ✓; **budget arm currently unreachable — finding 4** |
| ⑦ | ~~`machine total [GREEN · 1 estimated]`~~ — **retired 2026-08-15 (#1854 PR, operator call).** The `GaugeCaption` component, the `machineStateWord()` chip and the `(+N unpriced)` suffix this row described are deleted; the dashed `╌ committed` line had already gone in #1821 | — (`machine.state`, `estimated_models`, `unpriced_models` are still emitted and still drive the lamps and the detail rows; nothing on the FACE reads them as a word any more) | Removed rather than reworded. The operator's finding, in full: *"we're telling someone how to think when the data speaks for itself and the interpretation is user derived based on what they see. A message telling you how to think when you know your system is annoying and almost always wrong to the perceiving user."* The verdict word had already been through `GREEN` → `fit GREEN` → `GREEN (1 at measured)` in one afternoon, each edition needing a footnote to explain itself; the arc, the needle, the seven-segment readout and the two rings under them carry the same numbers with no footnote. The counts the chip folded in are still on the page in the detail rows (`unpriced N models · estimated N · at measured N`) and in `messages[]` (the WARN lamp), where they are DATA rather than a qualifier on a verdict. The verdict itself is still server-side and still visible — the redline (`redlineLit`, red state only) and `darkmux machine resources` — it just no longer has a word on the face | live after the change: no caption element in the DOM (`MachineHealthRegion.test.tsx` — "identical under two verdicts" pins the face rendering byte-for-byte the same for a Green and an Unknown payload); e2e `viewer-machine.spec.js` rewritten to assert absence ✓ |
| ⑧ | ~~`UNKNOWN` state chip~~ — **retired with ⑦ (2026-08-15).** `machineStateWord()` is gone from `machineGauge.ts` | `machine.state` (still emitted; still the redline's and the CLI's input) | The state cascade below is unchanged and still verified — what changed is that no chip on the face renders its word. An Unknown machine now LOOKS like a Green one on the face (same fill, same readout), which is the point: the face answers how full, and "does it fit" is answered where a reader can see the arithmetic (detail rows, CLI, `messages[]`) | cascade verified — see *the state cascade* below; face parity across states pinned by component test ✓ |
| ⑨ | tell-tale lamps | `residencyChanged` (Δ RESIDENCY), `unpriced_models` (UNPRICED), `pressure.red` (PRESSURE), `current ≥ limit` (OVER LIMIT), `resourcesErrored` (STALE — fetch failure ONLY, not `generated_at_ms` age), `messages[]` filtered to `warn`/`error` severity (WARN — **#1821, was `warnings.length`**) | **Six lamps, and the lineup itself drifted today, which the prior draft of this row never recorded.** There is deliberately **no STATE lamp** — it was deleted today (`e0eaf778`/`f9364796`, 2026-08-15): it used to relabel ITSELF with the machine's verdict (`STATE GREEN`/`STATE AMBER`) while ALSO changing its lit-ness, duplicating the machine chip a few inches away in a dimmer color. In its place — not new today, but never listed in this row before — is **Δ RESIDENCY**, keyed on `residencyChanged` (true when this poll's rows include any `new`/`ghost` status; see `advanceResidency`, `MachineHealthRegion.tsx`). Each lamp keys on exactly one named condition (`deriveLamps`, `machineGauge.ts:437-482`); lit = word + border + glow, never color alone. The WARN lamp's condition changed shape in #1821: an `info`-severity message (the #1819 estimate disclosure) no longer counts — only `warn`/`error` do. STALE is fetch-failure only (`resourcesErrored`); the snapshot's age is displayed (footer ⑳, the stale banner) but does not itself light this lamp | live: RESIDENCY unlit (no swap this poll), UNPRICED unlit (`unpriced_models`=0), PRESSURE unlit (`red`=false), OVER LIMIT unlit (36.53 GiB current < 128.00 GiB limit), STALE unlit (poll succeeded), WARN unlit (the payload's one message is `info` severity, not `warn`/`error` — `alarmMessagesCount`=0, which is the exact case #1821 exists to keep this lamp dark) ✓; `alarmMessagesCount` unit-tested against a mixed-severity payload ✓ |
| ⑩ | `85 %` under a `MARGIN` label (odometer tile, `(i)` popover for the note) | `pressure.margin_percent` (renamed from `memory_free_percent`, #1821) | sysctl **`kern.memorystatus_level`** = `(capacity − wired − compressor) / capacity` — the kernel's own pressure headroom, 0–100. Sole red trigger: `pressure.red = level < 15` (`MARGIN_PERCENT_RED`, `model_ledger.rs:244`). Tile label is `margin` (`odometerTiles`, `machineGauge.ts:515-555`) — this project's own NASA register (mass margin, power margin, propellant margin); the redline is where margin runs out | live sysctl read = 85 = field ✓; 85 ≥ 15 → `red: false` ✓ — matches live `pressure.red: false` |
| ⑪ | `7.22 GiB — swap used` (odometer tile) | `pressure.swap_used_bytes` | sysctl `vm.swapusage` (used), parsed by `parse_swapusage_used_bytes` (`model_ledger.rs:1552`) | rendered = memBytes(field) ✓ — a monotonic high-water mark: "reports, never alarms" is now UI copy on the tile's popover note (`machineGauge.ts:540`), not a Rust-side comment at a pinned line — the design rationale for why it isn't a trigger still lives in `model_ledger.rs`'s historical comment (~lines 208–230, where the old swap threshold used to be) |
| ⑫ | `4.43 GiB — compressor` (odometer tile) | `pressure.compressor_bytes` | `vm_stat` "Pages occupied by compressor" × page size (`model_ledger.rs:1531,1544`) | rendered = memBytes(field) ✓; same reports-never-alarms rule; popover note now also disambiguates from darkmux's own compactor (`machineGauge.ts:546-552`) |
| ⑬ | `limit source physical pool (no budget configured) · pool 128.00 GiB · used 103.98 GiB · available 60.29 GiB (48.92 GiB reclaimable) · unpriced 0 models · estimated 1 model` detail row | `limit_source`, `pool.capacity_bytes`, `pool.used_bytes`, `pool.available_bytes`, `pool.free_bytes` (feeds the reclaimable note only), `machine.unpriced_models`, `machine.estimated_models` | **#1821 machine-memory decomposition, one `vm_stat` read, zero added probe cost:** `used = wired + compressor_occupied + (active + inactive − purgeable)` (Activity-Monitor-style, `PoolSnapshot::used_bytes` doc); `available = free + inactive + speculative` (the colloquial "how much is left"); `free = "Pages free" × page size` (truly-free pages, `pool.free_bytes`) — kept in the payload but not given its own clause in this row; instead a `reclaimableNote()` (`format.ts:167-175`, added #1821 same day as the rest of this row) appends `(N GiB reclaimable)` after `available`, where `reclaimable = available − free`. This exists because `used` and `available` **deliberately overlap** (both count inactive pages, from opposite framings) and summed to 152.78 GiB on a 128 GiB machine the first time they were shown side by side — the note is what stops that from reading as broken arithmetic. `estimated N model(s)` clause (#1819) renders only when `estimated_models > 0`, same shape as `unpriced` | capacity/2³⁰ = 128.00 ✓, agreeing with ①; live `used` 103.98 GiB cross-checked against `top -l 1`'s report of `106G used`, sampled seconds apart ✓ (same-instant `top` compressor figure 4522M also agrees with live `compressor_bytes` 4.43 GiB, within sampling drift); `available − free` = 64735068160 − 12209979392 = 52525088768 = 48.92 GiB = the rendered reclaimable figure ✓. **The prior draft's claimed identity `used + free ≈ capacity` does not hold on this snapshot** (103.98 + 11.37 = 115.35 ≠ 128.00, a 12.65 GiB gap — the omitted category is exactly `available − free`'s inactive/speculative pages) and was likely a labeling slip for `used + available` in an earlier snapshot; that sum does not approximate capacity either (103.98 + 60.29 = 164.27, far over 128, BY DESIGN per the overlap above). Replaced with the real, code-asserted identity (`available − free` = the rendered reclaimable figure) rather than repeating an approximation that does not hold today |
| ⑭ | model name + `DARKMUX`/`USER` chip | `models[].identifier`, `.owner` | owner = namespace test `swap::is_darkmux_owned(identifier)` — the `darkmux:` prefix IS the ownership record | prefix ⇔ owner on both residents ✓ |
| ⑮ | `ctx · weights · kv@ctx · potential · current` | `models[].loaded_ctx`, `.weights_bytes`, `.kv_bytes_at_ctx`, `.potential_bytes`, `.current_bytes` | ctx from `lms ps`; weights from `lms ls` `sizeBytes`; the rest derived — see *per-model math* below; rendered by `modelKvLine()` (`machineGauge.ts:737-747` — the retired `modelLines()` this row used to cite is gone, see the module doc at the top of `memoryLedgerLines.ts`) | all identities verified ✓ |
| ⑯ | `UNPRICED · potential unknown` chip | `models[].potential_bytes == null` (the UI keys on this ONE field, `MachineHealthRegion.tsx:507`'s `pot == null` — `kv_per_token_bytes` staying `null` is a consequence of the same unreadable-arch-facts cause, not a second condition the component itself tests) | no readable arch facts AND no catalog size either → kv, potential stay `null`; the bar/dial draws **no committed extent** (absence, never zero). **Since #1819 this is the NARROWER case** — see "The #1819 estimate" below for the sibling `ESTIMATED` chip, which DOES draw a committed extent | live: 0 residents in this state (all 3 loaded models price successfully — 2 arch, 1 estimated); structurally verified via the unit tests's genuinely-unpriceable fixture (no catalog entry either): all three null ✓ |
| ⑰ | `kv unknown (no arch facts)` (the exact live wording — not the `kv@ctx — no arch facts` this row previously paraphrased) | same null | `modelKvLine()` (`machineGauge.ts:738`) renders the reason, not a dash alone: `m.kv_bytes_at_ctx != null ? "kv@ctx <bytes>" : "kv unknown (no arch facts)"` | ✓ — no live row currently exercises this arm (see ⑯); wording confirmed by reading the ternary directly |
| ⑱ | message text, severity-keyed | `messages[]` (renamed AND retyped from `warnings[]`, #1821 — `LEDGER_SCHEMA_VERSION` 1.1 → 2.0) | composed server-side in `compute_ledger`, each entry `{severity, text}` — `info` (a disclosure, e.g. the #1819 estimate note), `warn` (a real degradation, e.g. the unpriceable undercount), `error` (the reading itself is untrustworthy — a probe/enumeration failure). Rendered verbatim, never summarized, with the severity carried as a CSS class (`.memmsg-info`/`.memmsg-warn`/`.memmsg-error`) rather than one uniform amber treatment | text matches byte-for-byte ✓; severity round-trips through JSON ✓; an `info` message renders visually distinct from `warn`/`error` (component-tested) ✓ |
| ⑲ | attribution line | `attribution`, `attribution_note` | `attribute_current()`'s self-documenting ladder — **corrected from the prior draft's 4-stage description, which was wrong on two counts.** It is THREE states, not four (`PerProcess`/`Estimated`/`Unavailable` — `PerProcess` already means rank-matched, not a separate stage before it), and the per-worker figure it attributes is `max(rss, phys_footprint)` (`WorkerProc::memory_bytes`, `model_ledger.rs:600-603`), not bare `RSS` — a change from today (#1821): RSS alone understated the live machine total by roughly 97× on this operator's machine because MLX models hold almost nothing in RSS (weights live in Metal buffers). Rank-matching within `PerProcess` also changed basis, from potential to WEIGHTS (`model_ledger.rs:1079` — "rank by weights, falling back to potential" — weights are the materialized floor a loaded model holds no matter what, where potential includes KV that may not exist yet) | live `attribution_note`: *"3 worker(s) for 3 resident(s) — per-model footprint (max of rss and phys_footprint), workers rank-matched to models by weights (largest worker ↔ largest weights)"* — matches the code path exactly (3 workers, 3 residents → `PerProcess` arm) ✓ |
| ⑳ | `snapshot Ns ago · gather 459 ms (zero model dispatches) · server cache 2000 ms · polled every 5s` | `generated_at_ms`, `gather_ms`, `cache_ttl_ms` + client constant | the observer stamps its own cost into the payload — "the gather was negligible" is a verifiable claim, not an assumption (`stampLine()`, `memoryLedgerLines.ts:81-85`) | fields present; live `gather_ms` = 459 ✓ |
| ㉑ | **the arc's color** (not circled in the key image) | `band.usedPct` positions the fill's END; the color under any point of the arc is a function of THAT POINT's percentage of the scale, not of any figure computed about the machine | **Rewritten 2026-08-15 (#1854 PR): a ramp across the sweep, not a severity per fill.** `gaugeFillSeverity(pct)` — green < 50 ≤ amber < 85 ≤ red — is deleted; the 50/85 edges were thresholds darkmux invented, and a machine at 84% and one at 86% are not different in kind. In its place `gaugeRampStops()` emits an SVG `linearGradient` (`gradientUnits="userSpaceOnUse"`, laid horizontally across the arc's box) from `gaugeFillColor(0)` = green through the palette amber at 50% to red at 100%, and every band on the dial — the machine fill, the OTHER-tenants slice, the growth band — strokes `url(#ramp)`. A band's color is therefore WHERE IT SITS, so "other" spanning 20–60% visibly runs green→amber, which the operator read as a second signal ("other is a big range") for free. **The offsets are cosine-spaced** — `(1 − cos(pct·π))/2` — because a horizontal gradient interpolates in X while the arc advances by angle (`x = cx − r·cos(pct·π)`); linear offsets put the amber stop off the crown of the dial. Three stops (green→amber→red) rather than two so the midpoint is the palette's amber and not an olive mud. Still the one client-derived color on the page, and still keyed on nothing the arbiter said (finding 1's split holds; the ramp is the same whatever the machine is doing) | live: at 81% the fill's END sits in the amber-to-red run and its START is green — the whole progression is on the dial; unit tests pin the endpoints, the hue direction, monotone red, the cosine offsets (25% stop at `(1−cos(π/4))/2`, not 0.25) and totality over NaN/±Infinity ✓ |

## The stacked band + legend — uncircled, and the day's largest gap in this doc

Not one entry in the numbered table above, because the key image predates
it entirely (see the callout above the table). This is real page surface —
three drawn arcs plus a legend below the gauge — that the prior drafts of
this document never traced at all. Reconciled here rather than jammed into
a numbered row, since there is no circled number to hang it on.

**The three arcs** (`computeBandGeometry`, `machineGauge.ts:192-213`;
rendered `MachineHealthRegion.tsx:169-205`), stacked in scale order, each a
`BandSegment { startPct, lengthPct }` as a percent of the gauge's own scale
(`limit_bytes`):

| Arc | Source | Formula |
|---|---|---|
| `darkmux` (from 0) | `machine.current_bytes` | `pctOf(current_bytes)` |
| `other` (stacked on `darkmux`, ending at the needle) | derived, not probed | `[darkmuxPct, usedPct − darkmuxPct]` — the span between darkmux's own end and the machine's used position; drawn, not left as an undrawn gap between two rings (see the two-rings-vs-one-band rationale in the code comment this table paraphrases) |
| `growth` (hatched, beyond the needle) | `machine.potential_bytes − machine.current_bytes` | `[usedPct, min(100 − usedPct, committedPct − darkmuxPct)]`, clamped so it never draws past the scale's own end |

This replaced an EARLIER same-day design (two concentric rings — darkmux
inside, machine outside) that this document never got a chance to trace
before it was itself replaced (`bb0a7b1f` → `f1aca74f`, both 2026-08-15).
The rings were abandoned because "everything else" (`other`) was left as an
undrawn gap between two different-radius arcs, which required cross-radius
mental subtraction to read and under-drew at the inner radius; stacking
makes `other` a visible span and restores additivity (the page's actual
question — "will it fit" — is a sum, and a sum is what a stacked band can
show that two rings could not).

**The legend** (`GaugeLegend`, `MachineHealthRegion.tsx:284-314`) — the
three arcs' only textual companion, since neither the caption (row ⑦) nor
the odometer names them:

| Legend entry | Rendered when | Text | Source |
|---|---|---|---|
| `darkmux` | always | `darkmux <b>{memBytes(current_bytes)}</b>` | `machine.current_bytes` |
| `other` | `other != null` (i.e. both `pool.used_bytes` and `machine.current_bytes` are readable) | `other <b>{memBytes(other)}</b>` | `max(0, pool.used_bytes − machine.current_bytes)` — computed independently in the component, not read off `machine.other_used_bytes`, though the two formulas are the same arithmetic (see finding below) |
| `committed +…` | growth band has positive length | `committed +<b>{memBytes(potential−current)}</b> → <b>{memBytes(projected)}</b>` | `potential_bytes − current_bytes`, and `pool.used_bytes + max(0, that delta)` |

**Live, this snapshot:** `darkmux 36.53 GiB` · `other 67.46 GiB` · `committed
+17.79 GiB → 121.77 GiB`. Cross-checked: `other` = 111662612480 −
39219799696 = 72442812784 = 67.46 GiB, which equals `machine.other_used_bytes`
from the JSON exactly (72442812784) — the two independent computations
(one server-side for the cascade, one client-side for the legend) agree
byte-for-byte on this payload, which is worth stating as a checked identity
rather than an assumption, per this doc's own covenant. `committed`'s
target, 121.77 GiB, equals `machine.projected_total_bytes` (130764723188 =
121.77 GiB) — again independently computed, again agreeing.

**Ownership note for a future edit:** the legend's `other`/`projected`
figures duplicate arithmetic the server already exposes as
`other_used_bytes`/`projected_total_bytes` (see #1821 below). They agree
today because both sides implement the same formula, not because one reads
the other — a future change to either formula could silently diverge them.
Not fixed here (this pass is documentation, not code — see the constraint
against editing `.tsx`/`.rs` in this pass's instructions); named as a
finding instead (see Findings, below).

## The derived values, expanded

**Per-model math** (`compute_ledger`'s per-model loop, `model_ledger.rs:785-822`
— line drifted from the last-cited `:329-360` as #1819/#1821 grew the module
above it; still the same loop, unrenamed):

```
kv_bytes_at_ctx = kv_per_token_bytes × loaded_ctx
potential_bytes = weights + kv_bytes_at_ctx + 750 MB transient margin   (ArchEstimator)
```

Tested live (same identity, same model, still true today): qwen3.6-35b at
ctx 262,144 → 20480 × 262144 = 5,368,709,120 = the field, and
`potential − weights − kv` = exactly 750,000,000. The KV width is **priced
at fp16** (`KV_BYTES_PER_ELEMENT_V1 = 2`) until load-config provenance
(#1257) refines it — kv@ctx is an estimate with a stated width, not a
measurement.

**Machine total:** `current` is the Σ of attributed per-model footprint
(`max(rss, phys_footprint)` since #1821 — see row ⑲; the prior draft of
this paragraph still said "RSS", which understated the live total ~97× on
this machine before the fix);
`potential` is the Σ of **priced** models — arch-measured AND
size-estimated (#1819) — with `unpriced_models` counting what the sum still
omits (the genuinely unpriceable remainder). The page must always carry the
`(+N unpriced)` tag with the committed figure — dropping it would turn an
honest undercount into a silent one. Since #1819 it must ALSO carry an
`(N estimated)` tag whenever `estimated_models > 0` — a different honesty
gap (counted, but by a labeled guess rather than a measurement) that must
not be silently folded into the "priced" bucket the way a reader would
otherwise assume it is.

**#1821 — darkmux is not the machine's only tenant.** Two new fields, both
emitted so the cascade's own arithmetic is checkable from the JSON, never
just trusted:

```
other_used_bytes      = pool.used_bytes − machine.current_bytes   (floored at 0)
projected_total_bytes = other_used_bytes + machine.potential_bytes
```

`other_used_bytes` is everything ELSE on the machine, right now. A missing
`current_bytes` (attribution unavailable) is treated as 0, which makes
`other_used_bytes` an OVERESTIMATE of other tenants rather than an
underestimate — the safe direction when darkmux's own share is unknown.
`projected_total_bytes` answers the question this cascade has always been
trying to answer: *if darkmux's own commitment fully materializes while
everything else holds what it holds now, what is the machine's total?*

Tested live, this snapshot: `other_used_bytes` = 111662612480 − 39219799696
= 72442812784 = the field ✓; `projected_total_bytes` = 72442812784 +
58321910404 = 130764723188 = the field ✓, and 130764723188 ≤ 137438953472
(the limit) with `unpriced_models == 0` — arm 3 fires, `machine.state` =
`green`, matching the live payload exactly.

**The state cascade** (`compute_ledger`, `model_ledger.rs`, in order — line
numbers drift with #1819/#1821's additions, so given by name rather than
pinned):

1. `pressure.red` → **Red**
2. `current_total > limit` → **Red** (this boundary is the redline's
   position — the display re-derives nothing; unchanged by #1821 — this arm
   is still darkmux's own current against the limit, not the projection)
3. `projected_total ≤ limit` **and** no unpriced residents → **Green**
   (**#1821 — was `Σ potential ≤ limit`**, which silently assumed darkmux
   was the machine's only tenant)
4. `projected_total > limit` → **Amber** (+ a shrink hint, itself now
   computed against `projected_total`, not `Σ potential` alone — a
   suggested cut has to close the REAL gap, including other tenants, or it
   would land exactly on the old target and still leave the machine over
   the limit)
5. under the limit on the *known* projection but with unpriceable
   residents → **Unknown** — no fit guarantee exists, and no shrink target
   is computable. Also reached when `other_used_bytes`/`projected_total`
   themselves are unreadable (`pool.used_bytes` missing, e.g. `vm_stat`
   failed) — the cascade never silently falls back to the pre-#1821
   darkmux-only comparison
6. no limit readable → **Unknown**

**Unchanged by #1819, and that is the load-bearing fact.** Arm 3's gate is
`unpriced_models == 0` — never `estimated_models == 0`. An estimated
resident's potential IS counted in `Σ potential` (and so in
`projected_total`), so it can carry the machine straight to Green (arm 3)
or Amber (arm 4) exactly like a measured one; it never forces arm 5. Only a
GENUINELY unpriceable resident (no arch facts and no catalog size —
nothing left to estimate FROM) still forces Unknown. See "The #1819
estimate" below for what changed and what deliberately did not.

Per-model tint then follows the machine state (shared-fate unified memory),
with one nuance: under machine-Amber, a model whose current has fully
materialized its potential shows Green (its commitment is already paid);
GENUINELY unpriceable models stay Unknown. An ESTIMATED row is priced, so it
carries no such exception — it follows the machine state like any other
row.

**Limit resolution:** `#1243 budget > physical pool capacity > none`
(`model_ledger.rs:895-899`). See finding 4.

## The #1819 estimate — the covenant's first exception

This document's opening line is a covenant: *every figure on the machine
page traces to a probe.* Until #1819, that was true without qualification —
a figure was either a probe's own reading, an arithmetic transform of one,
or absent. #1819 introduces the first figure on this page that is **neither
a probe reading nor absent**: a resident's `potential_bytes` can now be a
**labeled estimate**, computed from a catalog size the daemon DID read plus
a constant the daemon did NOT measure on this model.

**What triggers it.** `ArchEstimator` needs a resident's own `config.json`
to price it (`num_hidden_layers`, `num_key_value_heads`, `head_dim`,
`layer_types`). A GGUF download carries its architecture inside the binary
weights file instead of a sidecar `config.json`, so `ArchEstimator` comes up
empty — the exact trace this whole page's #1819 predecessor issue is built
from (`microsoft/phi-4` resolving to a GGUF with no `config.json`, while its
MLX sibling `mlx-community/phi-4-8bit` prices normally because MLX builds DO
ship one).

**The formula.** `ArchWithSizeFallback` (`model_ledger.rs:714-763`) tries
`ArchEstimator` first (a measurement); only when that returns `None` does it
fall to `V1Estimator`, then adds the SAME post-load transient margin
`ArchEstimator` already includes — the fallback and the measured path price
on one basis, never a cheaper one for the guess:

```
kv_rate         = fallback_kv_rate_for_size(catalog.size_bytes)   -- SIZE-TIERED, see below
potential_bytes = catalog.size_bytes
                 + kv_rate × loaded_ctx
                 + DEFAULT_TRANSIENT_MARGIN_BYTES
```

**This pseudocode is itself a same-day correction.** The prior draft of this
section showed the KV rate as the single flat constant
`V1_FALLBACK_KV_BYTES_PER_CTX_TOKEN` — true when #1819 first merged, no
longer true after a same-day follow-up (`FALLBACK_KV_TIERS`,
`model_ledger.rs:148-196`, its own merge-gate finding, 2026-08-15): a single
constant UNDER-reserved the population most likely to hit this fallback in
the first place — large dense GGUF downloads. Working the crate's own
`kv_per_token` formula over published architectures found a 70B model short
by ~4 GB at 32K ctx and ~16 GB at 128K, none of it absorbed elsewhere. The
rate is now selected from the model's own catalog `size_bytes`
(`fallback_kv_rate_for_size()`, `model_ledger.rs:190-196`):

| size tier | KV rate | representative class |
|---|---|---|
| ≤ 15 GiB | `V1_FALLBACK_KV_BYTES_PER_CTX_TOKEN` = 204,800 B/token | phi-4 class |
| ≤ 45 GiB | 327,680 B/token | 70B/72B class |
| above | 409,600 B/token | 123B class and up |

`V1_FALLBACK_KV_BYTES_PER_CTX_TOKEN = 204_800` bytes/token (204.8 KB/token,
decimal) remains the TIER-1 rate and is still derived the way the rest of
this section describes — traceable, not invented, and traced to the exact
model this issue is ABOUT: `microsoft/phi-4`'s own architecture, published
verbatim by Microsoft (`huggingface.co/microsoft/phi-4/raw/main/config.json`,
fetched 2026-08-15): `num_hidden_layers: 40, num_attention_heads: 40,
num_key_value_heads: 10, hidden_size: 5120` (`model_type: "phi3"` — Phi-4
reuses the Phi-3 architecture class; no `sliding_window`, no
`rope_scaling` — a homogeneous DENSE decoder). `head_dim = 5120 / 40 = 128`,
so `2 × 40 layers × 10 kv_heads × 128 head_dim × 2 bytes fp16 = 204_800`.

An earlier draft of this constant used the crate's #1286 devstral-24B
referent (163,840 B/token) instead — a real probed number, but one that
undershoots phi-4 itself by roughly 20%, which would have meant the
fallback UNDERPRICED the exact resident that motivated the feature, on the
very first machine it runs on. Deriving from phi-4's own published numbers
instead closes that gap for the model the issue traces; it remains a
REFERENT rather than a proven ceiling over every dense architecture
(caught in review, 2026-08-15 — see finding 7).

**Stated where the covenant demands it: even the size-tiered table has a
named gap.** Pre-GQA multi-head-attention architectures (Llama-2-13B: 40
layers × 40 kv_heads × 128 = 819,200 B/token) exceed every tier here,
because MHA's kv_heads equals its attention-head count instead of a small
fraction of it — the size looks small while the true KV rate is enormous,
and no size-derived rate can catch that. Reading arch facts directly out of
the GGUF header (a named follow-up, #1820) is the real fix; the tiered
table is the honest approximation until then, and the live `info` message
(row ⑱; see the open finding below about its rendering) states this gap in
its own text.

**Tested live:** phi-4's weights (9,053,136,497 bytes ≈ 8.43 GiB) fall in
the ≤ 15 GiB tier → rate 204,800. Reversing the arithmetic from the live
payload confirms it exactly: `(potential_bytes − weights_bytes −
DEFAULT_TRANSIENT_MARGIN_BYTES) / loaded_ctx` = `(13158579697 − 9053136497
− 750000000) / 16384` = `3355443200 / 16384` = `204800.0` — the tier-1 rate,
to the byte.

**The assumption, named where the covenant demands it.** This formula
assumes DENSE attention — every layer holds a KV cache. That is knowingly
wrong for a hybrid linear-attention model (the Qwen 3.5/3.6 generation,
#1286's own finding): those hold a KV cache on as few as 1 in 4 layers, so
pricing one at the dense rate OVERSTATES its true cost, sometimes 4× or
more. That is the deliberately chosen failure direction (#1819 decision 3):
this fallback only fires when the real architecture is unreadable, and
reserving MORE than a hybrid model actually needs is the safe mistake — the
alternative (assuming hybrid, underpricing a genuinely dense GGUF model)
is not.

**How a reader tells it apart on screen — every consumer of this figure,
named:**

| Surface | What changes |
|---|---|
| Row chip | `ESTIMATED` — a THIRD chip family alongside `.is-state` (outline+hue=status) and `.is-identity` (fill+grey=identity): `.is-estimated` is a DASHED outline in the neutral `--dim` hue, the one axis neither existing family had claimed. Title states the dense-attention assumption. |
| Row kv line | `potential ~12.25 GiB (estimated)` (live, phi-4 today) — the `~` and suffix travel WITH the number, not just beside it in a separate badge, so the figure can't be read (or copy-pasted) out of its own caveat. |
| Row hint | A `↳ estimated: …` line beside the row, mirroring the existing `↳ unpriceable: …` hint's shape and position. |
| Machine chip | ~~`GREEN · 1 estimated`~~ — **the chip is retired (2026-08-15, row ⑦).** Decision 1 still holds on the SERVER (an estimated resident may produce a decided `machine.state`), and the count still travels with the verdict where the verdict is rendered — the CLI's machine line and the viewer's detail row (`estimated N models`) — but there is no longer a face-level word to fold it into. |
| Machine detail row | `unpriced 0 models · estimated 1 model` — the SAME row that already named the unpriced count, extended rather than replaced. |
| `messages[]` | A dedicated `info`-severity message naming the estimated resident(s) and the assumption, SEPARATE from the existing `warn`-severity unpriceable-resident message — the two are different facts (counted-via-guess vs. genuinely-uncounted) AND different severities (a disclosure vs. a real degradation) and must never share one sentence or one lamp (#1821 — this is the fix to the WARN-lamp-lights-on-a-disclosure defect the estimate feature itself surfaced). **This message currently renders its KB/token figure wrong — see the open finding below; the message otherwise lands correctly (right severity, right lamp behavior, right resident named).** |
| CLI (`darkmux machine resources`) | The row's STATE column carries `(estimated)`; the POTENTIAL column itself carries the same `~` prefix the kv line uses; the machine line's parenthetical names both counts when both are nonzero: `(+1 unpriced, 2 estimated)`. Not independently re-verified against a live CLI invocation this pass — flagged as unverified, not asserted. |
| Amber shrink hint | An estimated resident CAN be the shrink target — `hint_target_key`/`shrink_hint` read a row's kv rate through `effective_kv_rate()` (`model_ledger.rs:1150-1164`), which for an estimated row now resolves `fallback_kv_rate_for_size(row.weights_bytes)` — the SAME size-tiered rate the row was priced with — rather than the flat `V1_FALLBACK_KV_BYTES_PER_CTX_TOKEN` this table previously described (that flat read was accurate before the size-tiering follow-up landed same day; see "The formula" above). Reading the flat constant here today would reintroduce, one level down, the exact defect this function exists to prevent — a shrink saving computed at a different rate than the commitment it is shrinking. An earlier draft also omitted estimated rows from shrink-hint targeting entirely, which could render the FALSE "no shrinkable context" line — caught in review. When the target itself is estimated, the hint's own saving figure carries a parenthetical noting it's computed from the same conservative assumption. |
| `darkmux doctor` | A dedicated check (`resident pricing`) names residents that are STILL genuinely unpriceable after the fallback — i.e. no catalog size either — and hints the concrete remedy (load an MLX build of the same model, when one exists). It does NOT warn on an estimated resident; estimation is the intended, working path, not a defect to flag. |

**What #1819 deliberately did NOT build.** Reading the GGUF header directly
would convert this from an ESTIMATE into a MEASUREMENT — strictly better
where it applies, since the real architecture genuinely lives inside the
file. That is a named follow-up issue, not built here: the size-based
fallback is the honest floor for every unreadable architecture in the
meantime, GGUF or otherwise, and never silently claims to be more than that.

**Addendum (#1820, 2026-08-15) — the GGUF-header follow-up landed.** This
section's own text above is left UNCHANGED — it is a dated record of the
#1819 decision, not a claim about today. What changed: `GgufFactsReader`
(`crates/darkmux-profiles/src/gestalt_host/gguf_facts.rs`) now parses a
GGUF download's own binary metadata header directly — `<arch>.block_count`,
`<arch>.attention.head_count_kv`, `<arch>.embedding_length` /
`<arch>.attention.key_length` — and feeds the result into the SAME
`ArchEstimator` a `config.json` reading does. `gather_with_bin`'s resolution
order is now `config.json` → GGUF header → the #1819 size-tiered estimate
above → genuinely unpriceable. Verified against the real
`lmstudio-community/phi-4-GGUF/phi-4-Q4_K_M.gguf` — this section's own
motivating trace: the header reader reproduces the exact `204_800`
B/token figure `V1_FALLBACK_KV_BYTES_PER_CTX_TOKEN` above was DERIVED from,
except now as a genuine per-model measurement (`potential_source: "arch"`)
rather than a tier estimate applied to that model. The `ESTIMATED` chip, the
`info`-severity message, and every other consumer in the table below are
UNCHANGED — they still fire, just now only for a GGUF this reader itself
can't parse (corrupt/truncated download, an ambiguous multi-file directory,
or a format neither reader understands), not for "GGUF" as a category.

## Findings

Each finding below now opens with an explicit **Status** line — open,
addressed, or superseded, and by what — per this pass's instructions to
reconcile rather than just append. Findings 1–9 predate this pass; 10–14 are
new, found while re-verifying every row against the code.

1. **Status: addressed for the fill (row ㉑); the underlying UNKNOWN
   prevalence is unchanged and not a defect.** `unknown` is the normal state
   on this machine, and that is correct — which is why the fill stopped
   keying on it. The live snapshot takes arm
   5 above: the priced sum (42.06 GiB) fits the 128 GiB limit, but
   `microsoft/phi-4` is unpriceable, so the ledger honestly declines to
   promise a fit. Consequence: with any user-loaded model lacking readable
   arch facts, **the entire green/amber/red vocabulary is inert** — every
   state chip on the operator's real setup reads UNKNOWN.

   The page always rendered that truthfully, and *originally rendered it into
   the gauge too*: the arc fill carried `is-${machine.state}`, so the hero
   instrument swept from empty to full in permanent dim grey. Truthful, and
   useless — a gauge exists to show you the tank filling without reading a
   number, and this one had traded that away to re-encode a verdict three
   other elements already carried. The operator reported it from the live
   page on 2026-08-14 ("seems very grey... I was hoping it would have more of
   a color coded value as it fills up").

   The fix splits the two questions rather than guessing at either (row ㉑):
   **how full** is client arithmetic on two server numbers and says nothing
   about health; **what state** stays server-only, in the chip, the seven
   lamps, the face caption and the redline. An amber fill never means the
   arbiter said amber, and a component test pins exactly that — an
   UNKNOWN-state payload rendering a red fill while the chip still reads
   UNKNOWN and the redline stays dark. Making the *verdict* colored more
   often is still server-side work (pricing more models, or a partial-fit
   arm), never a client guess.

   **Addendum (#1819, 2026-08-15) — the "pricing more models" work named
   above landed.** This finding's own live snapshot is left UNCHANGED above
   — it is a dated record of what was true on 2026-08-14, not a claim about
   today. What changed: `microsoft/phi-4` is no longer unpriceable on a
   machine where it has a resolvable catalog size (it did, in that same
   snapshot — `9,053,136,497` bytes) — it now prices via the size-based
   `ArchWithSizeFallback` and shows `ESTIMATED`, not `UNPRICED`. The
   green/amber/red vocabulary is no longer permanently inert on a machine
   whose only unpriceable resident is a GGUF download with a catalog entry;
   it stays inert only for a resident with NEITHER arch facts NOR a
   resolvable catalog size — narrower, and rarer. See "The #1819 estimate"
   above for the mechanism and decision 1's own honesty requirement (the
   estimated count travels with the verdict everywhere it appears, so this
   fix could not simply repaint UNKNOWN as GREEN without saying why).

   **Addendum (2026-08-15, #1854 PR) — the split held; the chip did not.**
   The two-question split this finding made (how full = client arithmetic,
   what state = server verdict) is unchanged and is what made the next
   change safe: the verdict's FACE-level rendering (the caption chip, rows
   ⑦/⑧) was removed on the operator's call, and the fill's three-bucket
   severity was replaced by a continuous ramp across the sweep (row ㉑) for
   the same reason the #1835 margin floor was unwound in the same PR — the
   50%/85% edges and the 10% margin floor were thresholds darkmux invented,
   and darkmux describes its own state, it does not adjudicate the
   operator's. The state cascade, the redline, the lamps and the CLI verdict
   are all still there; the face just stopped saying a word about them.

2. **Status: addressed (#1821), and see finding 12 for a second layer of the
   same defect this reconciliation pass found in the gauge itself.**
   `pool free` and `memory free %` disagreed by design, and the labels hid
   it. `pool.available` is `vm_stat` "Pages free" × page size —
   deliberately conservative, and macOS keeps free pages near zero by
   reclaiming everything into cache, so a couple of GiB on an idle 128 GiB
   machine is normal (1.84 GiB in this snapshot). `memory_free_percent` is `kern.memorystatus_level` — the
   kernel's own pressure headroom (0–100), not a byte count at all. Both are
   true; neither is "how much RAM is left" in the colloquial sense. The page
   keeps them in separate instruments (detail row vs pressure tile) with the
   tile labeled *sole pressure trigger*; a worthwhile follow-up is renaming
   the display labels (e.g. "free pages" / "pressure headroom") so the
   similarity of names stops implying comparability.

   **Addendum (#1821, 2026-08-15) — addressed.** This finding predicted
   exactly this outcome, and the operator declined the rename at the time
   because the argument then was GB-vs-GiB pedantry. A later live
   measurement made it a different case: the SAME instant, `pressure`'s
   tile read 82% while `pool free` read 30.8% — a 51-point gap under names
   that both implied "how much RAM is left". Three honest names now exist
   where two ambiguous ones did: `margin` (the renamed pressure tile —
   `kern.memorystatus_level`, not a byte count), `available` (NEW — the
   colloquial `free + inactive + speculative`, now the headline figure in
   the machine detail row), and `free` (the renamed byte-count field —
   truly-free pages, kept in the payload but deliberately not given prime
   space in the row: two figures both reading "how much is left" was the
   defect, not something a rename alone fixes if the row still shows both).
   See rows ⑩ and ⑬ above, and "The pool decomposition" below for the full
   derivation.

3. **Status: addressed, with one named remainder still open (the `specOf`
   unit-suffix mismatch, below) — the operator has already declined to
   force it.** The same physical quantity used to render as both `128 GB` and
   `137.44 GB` — fixed by moving the page to binary (#1811). The header's
   RAM figure was binary (matching Apple's marketing number); every ledger
   figure was decimal (`memBytes`). Both derive from the identical
   `hw.memsize` = 137,438,953,472 bytes, so the page showed one quantity as
   two numbers — and the gauge inherited it, labeling its arc `0 · 34 · 69 ·
   103 · 137` on a machine everyone calls a 128 GB machine. The operator
   raised it against the live page on 2026-08-14: *"137 isn't going to make
   sense for a lot of users. we're all used to 128 powers of 2."*

   `memBytes`/`gaugeValueParts`/`gaugeTickLabel` are now binary and labeled
   `GiB`. The arc reads `0 · 32 · 64 · 96 · 128`, the ledger reads
   `128.00 GiB`, and the reconciling ` (128 GiB)` parenthetical that used to
   sit beside the pool figure is gone with the confusion it patched.

   **What remains:** `specOf` still labels its (always-binary) figure `GB`,
   so the header says `128 GB` where the ledger says `128.00 GiB`. Same
   number now — which is what the finding was actually about — but a
   mismatched suffix. Relabelling that one token is gated on retiring the
   machine stage's last byte-exact parity tie to legacy, so it stays an
   operator call. (A third figure, specs' `ram_free_for_ai_bytes` — doctor's
   reclaimable estimate minus residents — is **not rendered on this page at
   all**.)

4. **Status: open, unchanged — still a server-side prerequisite (#1243), not
   touched by any of today's other changes.** The BUDGET limit arm is
   currently unreachable in production.
   `compute_ledger` fully supports `limit_source:"budget"`, but `gather()`
   passes `budget_bytes: None` with a comment naming the future wiring
   (`runtime.max_model_ram_gb`, #1243). The redesign's budget renderings
   (`96 / BUDGET` scale end, the budget demo gauge) are real code paths in
   the ledger but cannot occur from a live gather today — a server-side
   prerequisite, stated here so the mockup is not read as a claim about
   current behavior.

5. **Status: superseded by this pass's own findings 10–14 — the claim "no
   mismatches" no longer holds and should not be read forward from this
   entry.** At the time this finding was written (2026-08-14), every
   arithmetic identity the page depended on verified against the live
   daemon: Σ current, Σ priced potential, kv@ctx, the 750 MB margin, the
   owner/namespace equivalence, the limit = pool equality, the state
   cascade's arm, and the red threshold. This pass (2026-08-15) re-ran the
   same class of check against the code as it now stands and found one real
   mismatch (finding 10 — a message renders the wrong number) plus several
   places where the DOC had drifted from correct code (findings 12–13, and
   the several table-row corrections above) — a different failure mode from
   "the code is wrong," but the entry's original claim ("no mismatches
   found," full stop) is no longer an accurate standing summary of this
   document's own trustworthiness. Read this entry as a dated snapshot of
   one day's verification, not a durable guarantee.

6. **Status: standing note, not a bug — still true today.** Snapshots move.
   Between the mockups' snapshot and that day's live test (same day, minutes
   apart): pool free 4.63 → 6.33 GB, compressor 728 → 907 MB, gather
   438 → 454 ms. Any static rendering of this page is one dated poll; the
   figures in the mockups are labeled with their snapshot date for that
   reason. This pass's own live snapshot (2026-08-15, gather 459 ms) is
   already a different poll from every number quoted in findings 1–9 above —
   the same lesson, one day later.

7. **Status: addressed, and see finding 11 for a THIRD merge-gate catch on
   the same feature, same day, not originally recorded here.** #1819's own
   implementation shipped one real bug and one weaker-than-claimed constant,
   both caught by an independent review before merge — recorded here because
   the covenant this document keeps applies to the PR that adds the estimate
   too, not just to the feature once it's already in.

   The bug: the first draft's fallback estimate omitted the 750 MB
   transient margin every arch-priced row already includes, so an
   ESTIMATED row priced 750 MB cheaper than a measured row with identical
   weights and ctx — making Green systematically EASIER to reach for a
   guess than for a measurement, the exact silent optimism this feature's
   whole framing is against. Fixed by adding
   `DEFAULT_TRANSIENT_MARGIN_BYTES` to the fallback arm, with a test
   pinning that an estimated row and an arch-priced row sharing weights,
   ctx, and kv rate now price byte-for-byte identically.

   The weaker-than-claimed constant: the first draft derived
   `V1_FALLBACK_KV_BYTES_PER_CTX_TOKEN` from the crate's #1286 devstral-24B
   probe (163,840 B/token) and asserted it "overstates" dense models in
   general. Independent verification against `microsoft/phi-4`'s own
   published `config.json` — the exact model this issue's trace names —
   found phi-4 itself costs 204,800 B/token, ~20% ABOVE the devstral
   referent. Re-derived from phi-4's own architecture instead ("The
   #1819 estimate" above has the arithmetic); the doc language was also
   softened to "a referent, not a proven ceiling" rather than an
   unconditional overstatement claim, since dense-model KV rates vary with
   head/layer geometry the same way #1286 first found for the hybrid case.

   Also fixed in the same pass: `estimated_models` gained `#[serde(default)]`
   (a required, non-`Option` field added in a MINOR schema bump breaks
   contract 5's lenient-on-read guarantee for cross-fleet reads without it
   — `darkmux machine resources <peer>` against an older machine would
   otherwise hard-fail and silently fall back to raw JSON); the amber
   shrink hint's target search was extended (`effective_kv_rate()`) so an
   estimated resident can be picked as a shrink target instead of
   potentially rendering a false "no shrinkable context" line; and the CLI
   table's POTENTIAL column now carries the same `~` prefix the UI's kv
   line does, so the caveat travels with the CLI figure too.

8. **Status: addressed, and re-verified live by this pass (see the #1821
   arithmetic block above — `projected_total_bytes` matched the field
   exactly on today's snapshot too).** #1821 — the machine-total cascade
   measured darkmux's commitment against the WHOLE machine's capacity, as
   though darkmux were the only tenant.
   `Σ potential ≤ limit` (arm 3, pre-#1821) compares darkmux's own models
   against `hw.memsize`. Live: darkmux's committed ~42 GiB fit inside 128
   GiB — GREEN — while ~44 GiB was already held by other processes, so real
   headroom for darkmux was ~58 GiB, not 128. The verdict never looked at
   that.

   This is finding 2 one layer down: the pressure tile mislabeled darkmux's
   usage as the machine's; the cascade measured darkmux's commitment
   against the machine's whole capacity. The cascade is the one that
   matters more — it drives the chip, the lamps, the redline, and the
   whole green/amber/red vocabulary a reader actually trusts.

   Fixed by introducing `other_used_bytes` (everything else the machine is
   holding, right now) and `projected_total_bytes` (`other_used_bytes +
   Σ potential`), and rewriting cascade arms 3–4 to key on
   `projected_total`, not `Σ potential` alone — see "The state cascade"
   above. Verified NOT to flip the verdict on the operator's own machine at
   the figures the issue traced (used 69.3 GiB, darkmux current 36.78 GiB,
   darkmux potential 54.32 GiB → projected 86.8 GiB ≤ 128 GiB limit →
   still Green) — the fix corrects the ARITHMETIC the cascade performs, it
   does not by itself change today's verdict on a lightly-loaded machine;
   it changes the verdict on a machine where other tenants are large
   enough to matter, which is exactly the case arm 3 used to get wrong
   silently.

9. **Status: addressed, and re-verified live by this pass (the live payload
   today is `schema_version: "2.0"` with a single `info`-severity message
   that correctly leaves the WARN lamp unlit — see row ⑨).** `messages[]`
   replaces `warnings[]` — a rename AND a retype (`LEDGER_SCHEMA_VERSION`
   1.1 → 2.0), because a disclosure and a degradation were rendering
   identically. `warnings: Vec<String>` had no
   severity channel, so the #1819 estimate note — a disclosure, working
   exactly as designed, permanently true on any machine with a GGUF
   resident — lit the same amber WARN lamp as a genuine degradation (the
   unpriceable-resident undercount). `messages: Vec<{severity, text}>`
   with `info`/`warn`/`error` fixes the chip-color inconsistency the
   operator spotted, one layer below the color: the WARN lamp now keys on
   `warn`+`error` only. Named `info`, not `note` — `darkmux flow note` is
   an existing verb, and reusing the word would give one term two meanings
   in the same product (the same collision class as compactor/compressor,
   finding 2's sibling). Tolerable as a MAJOR bump: every consumer (CLI,
   `/machine/resources`, the viewer) ships in this same binary; no
   external reader is stranded.

10. **Status: OPEN — a live code defect, flagged per this pass's
    instructions rather than fixed (docs-only pass).** The #1819 estimate
    `info` message (row ⑱, the "How a reader tells it apart" table above)
    renders its KB/token rate wrong. Live text today: *"potential assumes
    dense attention at a size-tiered **2** KB/token"* — should read `204.8`.

    **Root cause, read directly from the format string**
    (`model_ledger.rs:860-887`): the outer `format!` applies a `{:.1}`
    specifier to the 4th positional argument, which is itself the RESULT of
    an inner `match` that already returns a pre-formatted `String` (e.g.
    `"204.8"`). In Rust's `std::fmt`, a precision specifier on a *string*
    argument means "truncate to at most N characters," not "N decimal
    places" — that meaning only applies to numeric types. So `{:.1}` on the
    string `"204.8"` truncates it to its first character, `"2"`. The
    underlying arithmetic is correct throughout (confirmed above, reversed
    from the live payload: exactly 204,800 B/token for phi-4) — this is a
    display-only defect, not a pricing defect, and it does not affect
    `machine.state`, any chip, any lamp, or any byte figure — only this one
    sentence's own number.

    No test pins this message's literal text against its numeric inputs,
    which is why it shipped. Not corrected in this document by substituting
    the intended value — the covenant requires reporting what the live
    daemon actually returns, not what it was meant to return.

11. **Status: addressed by this pass — a real same-day code change that no
    prior draft of this document recorded.** The #1819 fallback KV rate is
    SIZE-TIERED (`FALLBACK_KV_TIERS`, `model_ledger.rs:148-196`,
    `fallback_kv_rate_for_size()`), not the single flat constant every
    earlier version of "The formula" (above) described. A single rate
    under-reserved large dense models — the population most likely to need
    the fallback in the first place (a 70B model priced short by ~4 GB at
    32K ctx, ~16 GB at 128K) — which is the exact "sum comes in low, cascade
    promises Green, machine overruns at materialization" failure this
    feature exists to refuse. Tiers now key on catalog `size_bytes`; see
    "The formula" above for the table and the live cross-check.

12. **Status: addressed by this pass — the gauge's center readout, needle,
    and fill hue all changed SUBJECT (not just style) same day (#1821), and
    no prior draft of this document recorded the change.** The center
    readout used to read `machine.current_bytes` (darkmux's own share); it
    now reads `pool.used_bytes` (the whole machine's used memory), labeled
    `MACHINE USED` rather than `IN USE`. The needle moved with it — from
    `current / limit` to `max(pool.used, current) / limit`. This is finding
    2 and finding 8's own shape recurring a third time, this time in the
    gauge itself: the operator caught a needle at ~82% sitting beside a
    readout of 36.8 GiB (darkmux's own figure) — the same "two things
    sharing one instrument, only one of them labeled" defect that findings 2
    and 8 already named at the pressure-tile and cascade layers. See row ④
    above for the corrected trace, and the new "stacked band + legend"
    section for the darkmux/other/growth decomposition that replaced the
    figure the needle used to show alone.

13. **Status: addressed by this pass.** The tell-tale lamp row (row ⑨) lost
    its STATE lamp today (an operator-caught duplicate of the machine chip
    that also broke the "a tell-tale never relabels itself" rule — it
    printed the verdict word in a dim, unlit color) and this document never
    recorded that the row's remaining six lamps include a Δ RESIDENCY lamp
    that predates today's changes but was never listed in this row by any
    prior draft. Net effect on the documented lineup: STATE removed,
    RESIDENCY added for the first time to this doc — both real, but only
    one of them is new to the CODE; the other was always there and simply
    never traced here.

14. **Status: OPEN, minor — flagged for the operator, not fixed in this
    pass (docs-only).** The new gauge legend (see "The stacked band +
    legend" above) computes `other` and `projected` client-side from
    `pool.used_bytes`/`machine.current_bytes`/`machine.potential_bytes`
    (`GaugeLegend`, `MachineHealthRegion.tsx:286-291`) rather than reading
    the server's own `machine.other_used_bytes`/`machine.projected_total_bytes`
    fields, which carry the identical formula (#1821). They agree
    byte-for-byte on today's live payload because both sides currently
    implement the same arithmetic — but nothing enforces that the two stay
    in lockstep if either formula changes later. Not a live bug today;
    named because this document's own covenant is to trace every figure to
    ONE named source, and this figure currently has two independent ones
    that happen to agree.

## The pool decomposition (#1821)

All three pool figures come from ONE `vm_stat` read the gather already
performs — zero added probe cost, per this page's own observer-effect
constraint (constraint 1: zero model dispatches; this extends the same
discipline to zero added kernel probes).

```
used_bytes      = wired + compressor_occupied + (active + inactive - purgeable)
available_bytes = free + inactive + speculative
free_bytes      = free
```

`used_bytes` is Activity-Monitor-style. Cross-checked live (2026-08-14)
against `top` the same instant: 69.3 GiB from this formula vs `top`'s
~66 GiB used — far closer than the implied `capacity - free` this page
leaned on before (73.4 GiB the same instant, and the reason `pool free`
alone swung between 1.8 and 61 GiB across one earlier session: `free` is
the most volatile of the three page classes, since macOS reclaims
aggressively into cache). **Re-checked this pass (2026-08-15):** live
`used_bytes` = 111662612480 = 103.98 GiB, `top -l 1`'s same-instant
`PhysMem:` line read `106G used` — same-shape agreement, different
snapshot, one day later.

**A prior draft of this document also claimed `used + free ≈ capacity`
as a cross-check identity elsewhere (row ⑬'s Tested cell) — that does not
hold, on this snapshot or in general, and has been corrected there.**
`used` and `available` (not `free`) deliberately OVERLAP — both count
inactive pages, from opposite framings — which is why the UI now renders
a `reclaimableNote()` (`format.ts:167-175`, #1821, same day) after the
`available` figure: `available − free` names the overlap explicitly
(`reclaimable = inactive + speculative`) so `used` and `available` summing
to MORE than capacity (152.78 GiB the first time they were shown side by
side on this 128 GiB machine) reads as by-design rather than broken
arithmetic. The one identity that DOES hold by construction is
`available − free = reclaimable`, cross-checked live above (row ⑬):
64735068160 − 12209979392 = 52525088768, the exact figure the note
renders.

`available_bytes` is the colloquial "how much is left" — reclaimable
inactive and speculative pages counted as available, matching what a
person actually means by the phrase (the Linux `MemAvailable` / Activity
Monitor model). Neither `free_bytes` alone (too strict — charges ~26 GiB
of reclaimable pages as not-free) nor `margin_percent`
(`kern.memorystatus_level`, too generous — counts only wired+compressor as
"not available") answers this question; before #1821 it was not on the
page at all.

Every figure is parsed independently and degrades independently: a single
missing `vm_stat` label (an unfamiliar build, a truncated read) takes down
only the figures that need it, never panics, and never substitutes a
plausible-looking wrong number for a genuinely missing one.

## What in the mockups is synthetic

Stated so nothing implies API origin: the **demo strip** in every mockup
(stale banner, green/amber/red/over-limit gauges and their values), the
**scaling demo**'s departed/NEW/EXPECTED rows and its 46.6 GB machine figure,
and every `BUDGET` rendering (finding 4). The EXPECTED row additionally
depends on a server-side `expected[]` set that does not exist yet
(PROPOSAL §8).

**The closing claim that used to sit here — "everything else in the mockups
is the live 2026-08-14 snapshot, transformed exactly as the table above
describes" — is no longer true, and this pass is not the one that can fix
it.** The table above now describes the STACKED-BAND gauge (darkmux / other
/ growth, a legend, a `MACHINE USED` center readout) that landed AFTER the
mockups this section describes were made; the mockups (and the key image)
show the earlier single-ring gauge with a plain `IN USE` caption. The
mockup HTML files themselves (`level3.html`, `scaling.html`) are not
tracked in this repo (`docs/design/machine-lens/` holds only `proposal.md`,
`provenance.md`, and `provenance-key.jpg` — confirmed via `git log`, no
history for any `.html` under this path), so this document cannot verify
or update their content directly; it can only flag that any surviving copy
of them is now stale in the same way the key image is. If they still exist
somewhere outside this repo, they need the same disclosure the key image
callout carries at the top of this document.

---

*Verification artifacts: `live-resources.json`, `live-specs.json` (the
2026-08-14 snapshots the earlier findings cite), `provenance.html` (the
annotated page), `provenance-key.jpg` (the keyed screenshot, now stale per
the callout above). This pass's own live re-verification (2026-08-15) was
read directly from `http://127.0.0.1:8765/machine/resources` +
`/machine/specs` rather than saved to a new snapshot file — the specific
field values it cites are reproducible by curling the same daemon. Code
references are to this worktree at the time of writing; line numbers
drift, function names rarely do.*
