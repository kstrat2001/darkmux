# Machine page provenance — where every value comes from

Every figure on the machine page traces to a probe on your own machine, through
a named transformation, to the pixel you see. This document is that trace,
**tested against a live daemon** rather than asserted: each row below was
verified on 2026-08-14 against `http://127.0.0.1:8765` (snapshots preserved
beside this file as `live-resources.json` / `live-specs.json`; verification
method noted per row). This is operator sovereignty (#44) applied to the page
itself — the operator never has to wonder where a number came from.

**The two feeds:**

| Endpoint | Producer | What it carries |
|---|---|---|
| `GET /machine/resources` | `darkmux_profiles::model_ledger::gather()` — served by `machine_resources_handler` (`crates/darkmux-serve/src/lib.rs`), identical to `darkmux machine resources --json` | The memory ledger: residents, potential vs current, pool/limit, pressure, warnings |
| `GET /machine/specs` | `machine_specs_handler` (same file) | Hardware line, machine identity, utility-tier model |

The gather runs **zero model dispatches** — it reads `lms ps --json` /
`lms ls --json` (LMStudio metadata), `vm_stat`, `sysctl`, and `ps` (kernel
counters), each under a bounded timeout. The client polls every 5 s
(`MACHINE_MEM_POLL_MS`, `ui/src/lib/queryKeys.ts`); the server caches the
gather for 2 s (`MACHINE_RESOURCES_CACHE_TTL`) so a poll burst costs one
gather. Both cadences are printed in the page footer ⑳ — cadence is a
recorded knob, never silent.

![Provenance key — circled numbers map to the table](provenance-key.jpg)

## The trace, value by value

Byte figures render through `memBytes()` (`ui/src/lib/format.ts:92`):
**decimal** GB/MB (`bytes / 1e9`, two decimals) — with one deliberate
exception at ①, noted below.

| # | The pixel says | Source | Transformation | Tested |
|---|---|---|---|---|
| ① | `Apple M5 Max · 128 GB` | `/machine/specs` → `cpu_brand` (sysctl `machdep.cpu.brand_string`), `ram_total_bytes` (sysctl `hw.memsize`) | `specOf()` → `ramGiB()` (`format.ts:105`): **binary** GiB, `round(bytes/2³⁰)` | 137438953472 / 2³⁰ = 128 ✓ — same physical quantity as ⑬'s "137.44 GB"; see finding 3 |
| ② | `darkmux:qwen3-4b-instruct-2507 · compaction · …` | `/machine/specs` → `utility_model.id` | verbatim; capability list is static UI copy | field present, id matches ✓ |
| ③ | `registered · not loaded` | `/machine/specs` → `utility_model.loaded` | `utilityLines()` (`memoryLedgerLines.ts:27`): `loaded:false` → this wording | `loaded: false` live ✓ |
| ④ | `32.4 GB — IN USE` (dial center + needle) | `/machine/resources` → `machine.current_bytes` | Σ of per-model attributed RSS (see *machine total* below); needle angle = `current / limit_bytes`, clamped at 100% | Σ model currents = 32378306560 = field ✓; 23.6% of scale ✓ |
| ⑤ | arc ticks `0 · 34 · 69 · 103` | `limit_bytes` | quarter marks of the scale, `limit × k/4` in decimal GB | 137.44 × ¾ ≈ 103 ✓ |
| ⑥ | `137 / LIMIT` at the max position (+ the redline end-cap) | `limit_bytes` + `limit_source` | limit resolution: `budget > physical pool > none` (`compute_ledger`, `model_ledger.rs:383`); word is LIMIT, or BUDGET when `limit_source:"budget"` | `limit == pool.capacity`, source `physical_pool` ✓; **budget arm currently unreachable — finding 4** |
| ⑦ | `╌ committed 24.6 GB (+1 unpriced)` | `machine.potential_bytes`, `machine.unpriced_models` | Σ of **priced** models' potential only; unpriced count appended — the undercount is stated, never hidden | Σ priced = 24565385183 = field ✓; count 1 ✓ |
| ⑧ | `UNKNOWN` state chip | `machine.state` | uppercased verbatim (`modelLines`/port uppercases the string, not CSS) | cascade verified — see *the state cascade* below |
| ⑨ | tell-tale lamps | `machine.state` (STATE), `unpriced_models` (UNPRICED), `pressure.red` (PRESSURE), fetch-failure + `generated_at_ms` age (STALE), `warnings.length` (WARN), `current ≥ limit` (OVER LIMIT — see PROPOSAL §redline) | each lamp keys on exactly one named field; lit = word + border + glow, never color alone | field values reproduce the lit set shown ✓ |
| ⑩ | `87 % free — memory free` | `pressure.memory_free_percent` | sysctl **`kern.memorystatus_level`** — the kernel's pressure headroom, 0–100. Sole red trigger: `pressure.red = level < 15` (`MEMORY_FREE_PERCENT_RED`, `model_ledger.rs:122`) | live sysctl read = 87 = field ✓; 87 ≥ 15 → `red: false` ✓ |
| ⑪ | `5.46 GB — swap used` | `pressure.swap_used_bytes` | sysctl `vm.swapusage` (used), parsed by `parse_swapusage_used_bytes` | rendered = memBytes(field) ✓ — a monotonic high-water mark: reports, never alarms (by design, `model_ledger.rs:462`) |
| ⑫ | `728 MB — compressor` | `pressure.compressor_bytes` | `vm_stat` "Pages occupied by compressor" × page size | rendered = memBytes(field) ✓; same reports-never-alarms rule |
| ⑬ | `limit source · pool · pool free · unpriced` detail row | `limit_source`, `pool.capacity_bytes`, `pool.available_bytes`, `machine.unpriced_models` | capacity = sysctl `hw.memsize`; **available = `vm_stat` "Pages free" × page size** — deliberately conservative (finding 2) | Pages free 373121 × 16384 ≈ live field ✓ |
| ⑭ | model name + `DARKMUX`/`USER` chip | `models[].identifier`, `.owner` | owner = namespace test `swap::is_darkmux_owned(identifier)` — the `darkmux:` prefix IS the ownership record | prefix ⇔ owner on both residents ✓ |
| ⑮ | `ctx · weights · kv@ctx · potential · current` | `models[].loaded_ctx`, `.weights_bytes`, `.kv_bytes_at_ctx`, `.potential_bytes`, `.current_bytes` | ctx from `lms ps`; weights from `lms ls` `sizeBytes`; the rest derived — see *per-model math* below | all identities verified ✓ |
| ⑯ | `UNPRICED · potential unknown` chip | `models[].kv_per_token_bytes == null` | no readable arch facts → kv, potential stay `null`; the bar/dial draws **no committed extent** (absence, never zero) | phi-4: all three null ✓ |
| ⑰ | `kv@ctx — no arch facts` | same null | `modelLines()` renders the reason, not a dash alone | ✓ |
| ⑱ | warning text | `warnings[]` | composed server-side in `compute_ledger` (`model_ledger.rs:368`) — verbatim on the page, never summarized | text matches byte-for-byte ✓ |
| ⑲ | attribution line | `attribution`, `attribution_note` | `attribute_current()`'s self-documenting degradation ladder (per-process RSS → rank-matched → estimated split → unavailable) | note matches live payload ✓ |
| ⑳ | `snapshot Ns ago · gather 438 ms (zero model dispatches) · server cache 2000 ms · polled every 5s` | `generated_at_ms`, `gather_ms`, `cache_ttl_ms` + client constant | the observer stamps its own cost into the payload — "the gather was negligible" is a verifiable claim, not an assumption | fields present; `gather_ms` 438–494 across polls ✓ |

## The derived values, expanded

**Per-model math** (`compute_ledger`, `model_ledger.rs:329-360`):

```
kv_bytes_at_ctx = kv_per_token_bytes × loaded_ctx
potential_bytes = weights + kv_bytes_at_ctx + 750 MB transient margin   (ArchEstimator)
```

Tested live: qwen3.6-35b at ctx 262,144 → 20480 × 262144 = 5,368,709,120 =
the field, and `potential − weights − kv` = exactly 750,000,000. The KV width
is **priced at fp16** (`KV_BYTES_PER_ELEMENT_V1 = 2`) until load-config
provenance (#1257) refines it — kv@ctx is an estimate with a stated width,
not a measurement.

**Machine total:** `current` is the Σ of attributed per-model RSS;
`potential` is the Σ of **priced** models only, with `unpriced_models`
counting what the sum omits. The page must always carry the `(+N unpriced)`
tag with the committed figure — dropping it would turn an honest undercount
into a silent one.

**The state cascade** (`compute_ledger`, `model_ledger.rs:398-410`, in
order):

1. `pressure.red` → **Red**
2. `current_total > limit` → **Red** (this boundary is the redline's
   position — the display re-derives nothing)
3. `Σ potential ≤ limit` **and** no unpriced residents → **Green**
4. `Σ potential > limit` → **Amber** (+ a shrink hint)
5. under the limit on the *known* sum but with unpriceable residents →
   **Unknown** — no fit guarantee exists, and no shrink target is computable
6. no limit readable → **Unknown**

Per-model tint then follows the machine state (shared-fate unified memory),
with one nuance: under machine-Amber, a model whose current has fully
materialized its potential shows Green (its commitment is already paid);
unpriceable models stay Unknown.

**Limit resolution:** `#1243 budget > physical pool capacity > none`
(`model_ledger.rs:383`). See finding 4.

## Findings

1. **`unknown` is the normal state on this machine, and that is correct.**
   The live snapshot takes arm 5 above: the priced sum (24.6 GB) fits the
   137.4 GB limit, but `microsoft/phi-4` is unpriceable, so the ledger
   honestly declines to promise a fit. Consequence: with any user-loaded
   model lacking readable arch facts, **the entire green/amber/red
   vocabulary is inert** — every state chip on the operator's real setup
   reads UNKNOWN. The page renders this truthfully (dim fills, dot patterns,
   the word); if colored states should be common rather than exceptional,
   the improvement is server-side (pricing more models, or a
   partial-fit verdict), never a client guess.

2. **`pool free` and `memory free %` disagree by design, and the labels hide
   it.** `pool.available` is `vm_stat` "Pages free" × page size —
   deliberately conservative, and macOS keeps free pages near zero by
   reclaiming everything into cache, so 4–6 GB on an idle 128 GB machine is
   normal. `memory_free_percent` is `kern.memorystatus_level` — the
   kernel's own pressure headroom (0–100), not a byte count at all. Both are
   true; neither is "how much RAM is left" in the colloquial sense. The page
   keeps them in separate instruments (detail row vs pressure tile) with the
   tile labeled *sole pressure trigger*; a worthwhile follow-up is renaming
   the display labels (e.g. "free pages" / "pressure headroom") so the
   similarity of names stops implying comparability.

3. **The same physical quantity renders as both `128 GB` and `137.44 GB`.**
   The header's RAM figure is binary GiB (`ramGiB`, matching Apple's
   marketing number); every ledger figure is decimal GB (`memBytes`). Both
   derive from the identical `hw.memsize` = 137,438,953,472 bytes. This is
   inherited legacy behavior, verified rather than invented here — worth a
   one-line footnote on the page or a doc note, since it is the first
   question a careful reader asks. (A third figure, specs'
   `ram_free_for_ai_bytes` = 87.5 GB — doctor's reclaimable estimate minus
   residents — is **not rendered on this page at all**.)

4. **The BUDGET limit arm is currently unreachable in production.**
   `compute_ledger` fully supports `limit_source:"budget"`, but `gather()`
   passes `budget_bytes: None` with a comment naming the future wiring
   (`runtime.max_model_ram_gb`, #1243). The redesign's budget renderings
   (`96 / BUDGET` scale end, the budget demo gauge) are real code paths in
   the ledger but cannot occur from a live gather today — a server-side
   prerequisite, stated here so the mockup is not read as a claim about
   current behavior.

5. **No mismatches found.** Every arithmetic identity the page depends on
   verified against the live daemon: Σ current, Σ priced potential, kv@ctx,
   the 750 MB margin, the owner/namespace equivalence, the limit = pool
   equality, the state cascade's arm, and the red threshold (87 ≥ 15).

6. **Snapshots move.** Between the mockups' snapshot and this test's
   snapshot (same day, minutes apart): pool free 4.63 → 6.33 GB, compressor
   728 → 907 MB, gather 438 → 454 ms. Any static rendering of this page is
   one dated poll; the figures in the mockups are labeled with their
   snapshot date for that reason.

## What in the mockups is synthetic

Stated so nothing implies API origin: the **demo strip** in every mockup
(stale banner, green/amber/red/over-limit gauges and their values), the
**scaling demo**'s departed/NEW/EXPECTED rows and its 46.6 GB machine figure,
and every `BUDGET` rendering (finding 4). The EXPECTED row additionally
depends on a server-side `expected[]` set that does not exist yet
(PROPOSAL §8). Everything else in the mockups is the live 2026-08-14
snapshot, transformed exactly as the table above describes.

---

*Verification artifacts: `live-resources.json`, `live-specs.json` (the tested
snapshots), `provenance.html` (the annotated page), `provenance-key.jpg` (the
keyed screenshot). Code references are to this worktree at the time of
writing; line numbers drift, function names rarely do.*
