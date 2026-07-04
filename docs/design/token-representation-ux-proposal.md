# Token representation UX — local vs. remote/cloud dispatch

**Status:** proposal (react-to, not build-from)
**Author:** ArchitectUX
**Date:** 2026-07-04
**Scope:** the live viewer's token-savings hero (`crates/darkmux-serve/assets/viewer.html`) and its underlying aggregation, plus the marketing copy that mirrors it (`docs/index.html`) — updated for agentic-remote dispatch (v1.15.0, shipped today).

---

## 0. The signal that started this

darkmux shipped agentic-remote dispatch today (v1.15.0, released via PR #1183): a crew role can now be driven by a remote, paid, metered endpoint (Azure OpenAI, OpenAI) instead of only local LMStudio, running the same real tool-calling loop against a different brain. Real dispatches now cost real money — the operator's own dogfooding this session produced 13 real PR reviews against Azure/gpt-5.1 at $0.03–0.55 each, ~$0.61 total.

The savings hero (`tokensOffMeter()`, `crates/darkmux-serve/assets/viewer.html:1018-1043`) was built when every dispatch ran on local LMStudio — "tokens off the meter" was a factual claim with no exceptions. Today it isn't: the function still sums every `telemetry.tokens` record in the window with zero regard for where the model that generated them ran. This isn't stale copy sitting on top of correct data — the aggregation itself has no endpoint dimension. It was written for a single-tier world and never grew a second tier when the world became two.

### A grounding correction, made first because it shapes what follows

The brief for this proposal cited two "already-filed, not-yet-built" issues — **#88** ("FlowRecord endpoint dimension + machine=keymaster semantics") and **#90** ("By-endpoint fleet lens — cloud-usage view") — as prior art to check before designing. I looked them up. Neither exists with that scope:

- **#88** (closed, M3) is "`crew dispatch` reuses session-id; `--help` promises 'default: generated' but reality is per-agent reuse" — a session-hygiene bug, unrelated to endpoints.
- **#90** (closed, M3) is "`darkmux swap` doesn't rewrite `agents.defaults.model.primary`" — a swap-coherence bug, also unrelated.

I traced where the numbers actually came from. `crates/darkmux-crew/src/dispatch_internal.rs` has **13 comments** citing `(#92)` as the tracking issue for agentic-remote dispatch (lines 436, 445, 480, 536, 721, 783, 1181, 1311, 1331, 1458, 1495, 1663, 1961, 4428), and one of them — line 1963, `// future by-endpoint consumer (#90) that reads the terminal record` — is a direct, verbatim hit for the second number in the brief. But issue/PR **#92** on this repo is `feat(serve+flow-viewer): model-status pill + click-to-modal`, merged 2026-05-13 — a real, unrelated, already-shipped PR. The actual tracking issue for agentic-remote dispatch is **#1177** ("feat: dispatch crew roles to remote OpenAI-compatible model endpoints (hosted reviewer)", open), which already contains, verbatim, the exact "machine = keymaster" / "endpoint = where + what service" language the brief attributed to a separate #88:

> Three orthogonal axes, never conflated: **machine** = keymaster (the box that holds the endpoint's Keychain credential and made the call). **endpoint** = where + what service the model ran on... **tier** = actor-role...

So `#88`/`#90` in the brief are not stale references to real issues — they're downstream of a dangling, never-corrected placeholder (`#92`/`#90`) baked into the code itself, most likely written before the feature had a real GitHub issue number and never reconciled once it did. This is worth fixing on its own (§6, immediate bucket) precisely because it just cost a research pass — a future reader (human or agent) hitting those same 13 comments will make the same wrong lookup.

The real, load-bearing prior art is:
- **#1177** — the open tracking issue for agentic-remote dispatch, whose own checklist has no viewer-side aggregation item (its open items are: doctor credential-presence [shipped today as #1182], free-form review mode, `doctor --probe`, and two small follow-ups — none of them a token view).
- **#1181** — merged today, the fix that threads `payload.endpoint` through the container-path `dispatch()` (the agentic-remote path), matching what the single-shot `dispatch_remote()` path already did.
- **#732** — open, M7 — a **different, orthogonal** axis: a frontier-vs-local cost ledger (how much of a loop's spend happened in the frontier orchestrator vs. was delegated to a crew dispatch). Not to be conflated with this proposal's axis (within a crew dispatch, did the model run local-unmetered or remote-metered). See §1.5.
- **#558** — open — the general "one drill-down viewer" epic. The by-endpoint/by-machine/by-role views proposed in §3 are new scope; they don't exist yet anywhere and should nest here or get their own issue, not be assumed already tracked.

There is genuinely no by-endpoint fleet lens on the books today. §3 is new design, not a re-read of existing scope.

---

## 1. Current-surface audit (cited)

### 1.1 The aggregation has no endpoint dimension

`tokensOffMeter()` (`viewer.html:1018-1043`) does one pass over `DATA`, filtering to `category==="telemetry" && source==="tokens"`, summing `total_tokens`/`prompt_tokens`/`completion_tokens` unconditionally, and bucketing by `session_id` only to decompose fresh/re-read/generated (the `#803` turn-sequence logic, lines 1032-1041). It never looks at what produced those tokens. `savingsHero()` (`viewer.html:1123-1154`) renders the result as one 48px number labeled "tokens off the meter" plus a chip row (generated / fresh input / re-read / unclassified / dispatches, via the `sc()` helper at line 1137). Every one of these numbers is currently a mixed local+remote sum.

The `hybridNote()` fallback template at `viewer.html:1078` compounds it: `` `${t.runs} dispatch${...} off the meter — the hybrid loop is humming` `` fires unconditionally whenever there's no tagged orchestrator note, even in a window where every one of those dispatches ran on Azure.

### 1.2 The fix's building block already exists — one session away from the fleet-wide case

`runRegions()` (the single-session run-detail view) already does exactly the cross-reference this proposal needs, just scoped to one session at a time:

```js
// viewer.html:1489-1490
const sidStarts=DATA.filter(r=>r.session_id===sid&&r.action==="dispatch.start"&&T(r.ts)<=state.t)...
const d=sidStarts.length?sidStarts[sidStarts.length-1]:null;
...
// viewer.html:1539
const sp=(d&&d.payload)||{}, dp=(c&&c.payload)||{};
...
// viewer.html:1553-1564
const ep=sp.endpoint;
const route=ep
  ? (()=>{const i=String(ep).indexOf(':');const kind=i>=0?ep.slice(0,i):'';const rest=i>=0?ep.slice(i+1):ep;
      const label=kind==='azure'?'Azure OpenAI':kind==='openai'?'OpenAI':esc(kind||'remote');
      return `${label} <span style="color:var(--muted)">· ${esc(rest)} · off-fleet</span>`;})()
  : `LMStudio <span style="color:var(--muted)">· local · this machine</span>`;
```

This already renders "Azure OpenAI · host · off-fleet" vs. "LMStudio · local · this machine" per session, per the run-detail page shipped alongside #1177. **Nothing new needs to be invented** — the fleet-wide hero needs the same lookup applied once per `DATA` scan across every session instead of one session at a time (§4).

### 1.3 Where `endpoint` actually lives in the schema today

`FlowRecord` (`crates/darkmux-flow/src/schema.rs:160-273`) has no top-level `endpoint` field. The doc comment on `payload` (lines 241-257) is explicit about why: event-specific fields that aren't shared across many event types stay in the JSON blob, not promoted to first-class struct members. `endpoint` is exactly that shape — it lives inside `payload` on `dispatch.start`/`dispatch.complete` records only (per `dispatch_start_payload["endpoint"]` / `dispatch_complete_payload["endpoint"]` in `dispatch_internal.rs:1500`/`1966`, the latter fixed today by #1181 for the container/agentic-remote path — the single-shot `dispatch_remote()` path already had it).

`telemetry.tokens` records carry **no endpoint field** — confirmed against the schema's own version history:

```
// schema.rs:67-77   1.12.0 — added the telemetry.tokens action + source=tokens (#782/#783):
//                    prompt_tokens/completion_tokens/total_tokens in payload. No endpoint.
// schema.rs:78-85    1.13.0 — added turn_seq (#795), per-turn emission. No endpoint.
```

Neither bump ever added an endpoint dimension to the token records themselves — consistent with the task brief's own description of the mechanism, and confirming it's accurate: correlation has to happen via `session_id`.

### 1.4 The join key is sound and already first-class

`session_id` (`schema.rs:171-172`) is a first-class `FlowRecord` field, present on `dispatch.start`, every per-turn `telemetry.tokens` record (#795), and `dispatch.complete` for the same attempt. No new field is required to correlate a token record with the endpoint its dispatch used — the mechanism the brief described (join on `session_id`, read `payload.endpoint` off the session's `dispatch.start`) is real, already partially implemented (§1.2), and sufficient for correctness today (§4 discusses whether it's sufficient for *robustness*).

### 1.5 Orthogonal to #732 — don't conflate the two axes

#732 ("Cost-savings estimation: local-vs-frontier token ledger per loop/mission") tracks **how much of a loop's spend happened in the frontier orchestrator (Claude Code) vs. was delegated to a crew dispatch** — the vertical axis of who did the work. This proposal's axis is horizontal and sits entirely inside "delegated to a crew dispatch": **which crew dispatches ran on local LMStudio (unmetered) vs. a remote paid endpoint (metered)**. A dispatch can be local-tier-delegated *and* remote-metered at the same time (an agentic-remote `code-reviewer` dispatch is still "not the frontier" by #732's accounting, while being "on the meter" by this proposal's accounting). `docs/roadmap/M7.md:32` already describes #732's ledger carefully ("tokens a dispatch ran off the metered frontier") — that phrasing is about the frontier axis and should stay as-is; it is not describing the hero this proposal touches. A future full ledger would cross both axes, but that's out of scope here — flagging so nobody merges the two aggregations under one number later without noticing they answer different questions.

### 1.6 The palette already has the right colors — no new tokens needed

```
// viewer.html:24-26
--good:#5af0a3;   /* green — currently the hero's "off the meter" color */
--amber:#6df1ff;  /* NOT amber — a cyan, and it's the darkmux BRAND accent */
--warn:#ffb86b; --bad:#ff6b85;  /* reserved for actual problems */
```

Despite the variable name, `--amber` is a cyan (`#6df1ff`) used everywhere as the **brand/active accent**, not a warning color: the wordmark (`.brand b`, line 31), badges (line 39), live-state pulsing (line 57), focus rings (line 126), active nav (line 38). It reads as "this is darkmux's own first-class thing," never as "careful." That makes it the natural color for a cloud/remote stat that the operator wants to feel legitimate rather than flagged — `--good` stays exactly what it is today (local, unchanged, preserves the existing screenshot's visual identity), `--amber` (cyan) becomes the cloud number's color, and neither `--warn` nor `--bad` (the only colors that would read as "problem") gets anywhere near this feature.

### 1.7 The density precedent (#1091) constrains the redesign

#1091 (closed) is the operator's own prior instruction on this exact card: *"keep the headline + `generated` + `dispatches run` always-visible; demote `re-read input`/`unclassified` behind a details affordance... the highest-leverage move against the operator's 'noisy' complaint."* Whatever this proposal adds has to read as **one feature, extended** — not two competing widgets stapled together. This directly shapes §2: no second full 48px hero sitting independently with its own five-chip breakdown; the split has to feel like the *same* card doing one more honest thing, not a second card.

---

## 2. The representation scheme

### 2.1 Evaluating "by your fleet" as the headline — explicitly, per the ask

**What works:** "by your fleet" is true regardless of endpoint. Every token counted — local or remote — was processed under a role the operator's own crew dispatched, under darkmux's own orchestration. It matches "Own your AI workforce" positioning cleanly and doesn't need a permanent asterisk.

**What breaks if it becomes the number's *label* without restructuring the aggregation underneath it:** "by your fleet" says nothing about metering. If the headline word changes from "meter" to "fleet" but `tokensOffMeter()` keeps blind-summing everything, the result is *worse* than the current bug, not better — it launders the exact ambiguity that made "off the meter" factually wrong into a label that's technically-true-but-uninformative. A viewer glancing at "9.7M tokens by your fleet" can no longer tell whether that number represents money saved or money spent. The whole reason the hero exists (#783: *"this proves [you don't have to rent every token] with the user's own numbers"*) evaporates the moment the number stops being legible as a proof of anything specific. **A bare rename is the one move that makes this strictly worse than doing nothing** — do not ship a label change without the aggregation change underneath it.

**Verdict:** "by your fleet" earns a place — as a section eyebrow that frames *both* numbers as one hybrid workforce, sitting *above* an honest split, never as a replacement for the split.

### 2.2 Recommended layout — two co-equal, honestly-scoped numbers under one fleet frame

```
                              BY YOUR FLEET                    ← eyebrow, small caps, once

  ┌─────────────────────┐         ┌─────────────────────┐
  │   1,847,203          │         │      142,880         │
  │   tokens off the      │         │   tokens via cloud    │
  │   meter · last 24h    │         │   · last 24h          │
  └─────────────────────┘         └─────────────────────┘
       (--good, green)                  (--amber, cyan)

  generated · fresh input · re-read · unclassified · 22 dispatches   ← unchanged chip row (see 2.2.3)

  Orchestrator note: ...                                              ← unchanged hybridNote()
```

**2.2.1 — Both numbers, same weight.** The local number keeps its existing 48px/30px (desktop/mobile) treatment and green color exactly as shipped, its underlying sum now filtered to sessions whose `dispatch.start.payload.endpoint` is **absent**. The cloud number gets the *same size treatment*, not a demoted chip — deliberately resisting the instinct to shrink the "paid" number smaller than the "free" one, which would visually contradict the operator's explicit ask that both tiers feel legitimate. They sit in the existing `.savrow` flex container (`viewer.html:322`, already `flex-wrap:wrap`, already handles two-things-side-by-side-or-stacked responsively) as two `.savlead`-style blocks instead of one.

**2.2.2 — Zero-state.** When the cloud number is 0 (the common case today — most operators have never configured a remote endpoint), render it neutrally rather than hiding it, matching the existing "always render, even at zero" rationale already documented for the hero (`viewer.html:1125-1129`: *"hiding it until the first record made it pop in late + look absent"*). A visible "0 tokens via cloud" doubles as passive discovery of a real capability without upsell framing, provided the wording stays flat and factual. Flagged as an open question (§7) since it's a real judgment call, not a slam dunk — some operators may find a permanent zero for a feature they'll never use to be noise, which is exactly what #1091 was fighting.

**2.2.3 — The chip row: combined, not doubled.** Doubling the existing generated/fresh/re-read/unclassified breakdown under *each* number (10 chips instead of 5) directly re-triggers #1091's noise complaint. Recommend: chips describe the **combined total** by default (today's behavior, unchanged), with the existing progressive-disclosure pattern already used for the prompt block (`<details class="rr promptdet">`, `viewer.html:1581-1583`) extended to an optional per-tier expand for operators who want the finer cut. This keeps the default view exactly as dense as it is today plus one number, not plus a second breakdown.

**2.2.4 — The hybrid note stays the card's single closing line.** `hybridNote()` (`viewer.html:1067-1080`) needs no mechanism change, only a tier-aware pass on its fallback copy (§5.6) so it stops asserting "off the meter" for windows that include cloud-only activity.

### 2.3 What breaks if you *just* rename the label (the direct answer to the brief's question)

Renaming "tokens off the meter" → any single new label — "by your fleet" or otherwise — while leaving `tokensOffMeter()`'s aggregation untouched fixes nothing structural:

1. The number becomes honestly *labeled* but loses its information content — "N tokens by your fleet" is compatible with $600 of Azure spend and nobody reading the hero would know it.
2. The marketing screenshot (`docs/index.html:261`) — a real, historical capture from a fully-local era ("9,721,748 tokens off the meter... across 22 local dispatches") — still implicitly promises that the *current* live number means the same thing it meant when that screenshot was taken. A label swap on the live viewer doesn't touch that promise; only a scoped, endpoint-aware number does.
3. `hybridNote()`'s procedural templates (`viewer.html:1074-1079`) keep saying "off the meter" verbatim regardless of what actually ran, since they're driven by the same unscoped `t` object.

The aggregation change is the load-bearing fix. The label is presentation on top of it — necessary, but nowhere near sufficient on its own.

---

## 3. New features for paid/cloud usage

Per §0, none of this duplicates existing scope — #1177's own checklist doesn't include a viewer-side aggregation item, and no by-endpoint lens issue exists today. Grounded in what the data already carries (once endpoint is threaded through consistently, §4):

1. **Per-endpoint breakdown.** Group by the `{kind}:{host}` prefix of `remote_endpoint_label()`'s output (`dispatch_internal.rs:846-855`, format `azure:finherogpt.cognitiveservices.azure.com/gpt-4o`). This directly serves #1177's own stated security concern — its issue body warns *"engagement-match the account (FinHero Azure → FinHero PRs; a personal account → personal work) — never point a work account at a public personal-repo review."* A per-endpoint view is the mechanism that would let an operator with both a work Azure endpoint and a personal OpenAI endpoint configured actually *see* whether a personal-repo dispatch ever hit the work endpoint by mistake. This is the single highest-value new view precisely because #1177 already flagged the risk it's designed to catch.

2. **Per-machine keymaster view.** Group by `machine_id`/`machine_uid` restricted to sessions with a remote endpoint — surfaces "which machine actually made the paid calls," per #1177's own machine-is-keymaster axis (the box holding the Keychain credential). Concretely useful on the operator's actual heterogeneous 2-machine fleet: catches an unexpected machine using a credential it shouldn't hold.

3. **Per-role cloud spend.** Group by `handle` (e.g. `darkmux/code-reviewer`) restricted to remote-endpoint sessions. This is the exact shape of the data the operator already assembled by hand this session (13 PR reviews, $0.03–0.55 each, ~$0.61 total) — it's the one view that would have made that entire manual accounting exercise unnecessary, and it earns its place on that basis alone.

4. **`darkmux doctor` ambient line.** A new informational (not pass/fail) check: "N tokens dispatched to M remote endpoints in the last 24h." Cheap, and a natural sibling to `check_remote_endpoint_credential_presence` (shipped today, #1182) — that check answers "can this dispatch reach its endpoint," this one answers "how much has it actually used it."

5. **Explicitly not proposed:** any in-product dollar figure, for any of the above. Even though real per-request cost data exists for this session's 13 reviews (gathered from the Azure billing dashboard, entirely outside darkmux), it has no home in the product per the existing tokens-only rule (§0 of #783, reaffirmed unconditionally for the cloud side in §7's third doctrine point). That data stays anecdotal — article material, not a feature.

**A grounding caveat on item 1:** `remote_endpoint_label()` (`dispatch_internal.rs:853`) only distinguishes `kind ∈ {azure, openai}` — `let kind = if url.contains("azure") { "azure" } else { "openai" };`. Any other OpenAI-compatible host (a LiteLLM proxy, a remote vLLM — both named as design targets in #1177's own issue body: *"a LiteLLM proxy, a remote vLLM"*) gets mislabeled `openai` even when it's neither. Not a blocker for this proposal, but the per-endpoint breakdown will misrepresent non-Azure/non-OpenAI remotes until that's tightened. Worth a small standalone bug fix before or alongside item 1.

---

## 4. Data-model implications

**The question:** is the `session_id` × `dispatch.start.payload.endpoint` cross-reference (§1.2, §1.4) sufficient, or should `telemetry.tokens` carry `endpoint` directly?

**Answer: both, phased. The cross-reference is sufficient for correctness today; a direct field is worth adding for robustness and performance, not because the cross-reference is wrong.**

### Phase 1 — no schema change, ships immediately

Build a `Map<session_id, endpoint>` from every `dispatch.start` record in one pass over `DATA`, mirroring the existing single-session lookup at `runRegions()` (`viewer.html:1489-1490`) but generalized across every session instead of one. `tokensOffMeter()`'s existing `sess` map (built for the fresh/re-read decomposition, `viewer.html:1019-1030`) already groups token records by `session_id` — attaching each session's resolved endpoint at the point that map is finalized is a same-shaped, same-pass change. Zero schema bump; pure viewer-side aggregation logic.

### Phase 2 — schema minor bump, recommended as a fast-follow

Add `endpoint` directly to `telemetry.tokens`'s `payload` at emission time (the runtime already knows which brain served the turn when it writes the per-turn token record). Reasons this earns a bump even though Phase 1 already works:

- **Robustness.** The cross-reference silently degrades for any `telemetry.tokens` record whose session's `dispatch.start` fell outside the currently-loaded window — a partial day-file in playback, a live-window edge, or ordering quirks in a streamed load. A self-contained field never has this failure mode.
- **Efficiency.** The hero re-derives roughly once per second in live mode (the `~1/sec live rebuild cadence` noted at `viewer.html:1474-1476`). Per-session lookup (today's run-detail case) is cheap because it's one session; the fleet-wide hero rescans the whole window every tick, so an O(1) field beats an O(sessions) join at that cadence.
- **Precedent.** `schema.rs:241-257`'s own stated convention — event-specific fields that aren't shared broadly stay in `payload`, exactly where `endpoint` already lives on `dispatch.start`/`dispatch.complete` (§1.3). Adding it to `telemetry.tokens`'s payload is the same move, not a new pattern.

**Bump:** `FLOW_SCHEMA_VERSION` 1.15.0 → 1.16.0 — minor, additive (a new optional payload field; older readers ignore it), consistent with every prior telemetry.tokens bump (1.12.0 added the record family, 1.13.0 added `turn_seq` the same way). No `FlowRecord` struct field change — `endpoint` stays payload-nested, not promoted to a top-level member, matching where it already lives for dispatch records.

---

## 5. Marketing copy implications

| Location | Current text | Status | Recommendation |
|---|---|---|---|
| `docs/index.html:8` (OG title) | `"Own your AI workforce. Local AI, off the meter."` | Arguably already scoped correctly — "Local AI" qualifies "off the meter," it doesn't claim *all* AI is off the meter | Leave as-is, or extend if length allows on social-card truncation (flag as a real constraint — OG titles get cut aggressively) |
| `docs/index.html:240` (hero tagline) | `"...directed by your frontier, off the meter, on your hardware."` | **The one to fix** — reads as if everything darkmux does is off-meter/on-hardware, no longer true | Something like *"...directed by your frontier, running locally off the meter or on the cloud endpoint you choose."* Needs a content-editor pass on the exact wording (Kain's own stated preference: cut em-dashes, facts are mine to apply, style/wording is the editor's call) — flagged as a handoff, not finalized here |
| `docs/index.html:261` (screenshot alt) + `:262` (figcaption) | Describes a real, historical capture (9,721,748 tokens, 22 local dispatches) from before remote dispatch existed | An accurate record of a real moment — not currently wrong | Leave the alt text as an honest description of *that* screenshot. Sequence a **retake** once the two-number hero (§2.2) ships, so the marketing asset matches the live product — retake-don't-redact per existing screenshot-discipline practice |
| `docs/index.html:258` (positioning paragraph) | *"no cloud account, no per-token meter, nothing leaving your disk"* | Same underlying tension, one altitude up — this is home-page positioning architecture, already locked in a prior design pass | **Out of scope for this proposal.** Flagging it explicitly rather than silently absorbing it — a full home-page positioning rewrite is a separate decision, not a copy-fix implied by the hero change |
| `docs/roadmap/M7.md:32`, `:111` | Careful, already-scoped language about the #732 frontier-vs-local ledger | Correct as written — describes a different axis (§1.5) | Leave as-is. Consider one added sentence noting this hero's endpoint-scoping *composes with* (doesn't duplicate) #732's ledger, so a future reader doesn't conflate the two axes |
| `viewer.html:1078` (`hybridNote()` fallback) | `` `${t.runs} dispatch${...} off the meter — the hybrid loop is humming` `` | Product copy, not marketing, but the same failure category | Gate this template on "at least one dispatch in the window was local"; add a tier-aware sibling for cloud-only / mixed windows |

---

## 6. Sequencing

### Immediate — the hero is actively miscounting right now

1. **Make `tokensOffMeter()` endpoint-aware** via the Phase 1 cross-reference (§4). Even before any visual redesign, the simplest correct version excludes remote-endpoint sessions from the existing single number — undercounts total fleet activity, but "off the meter" once again means only unmetered tokens. Shippable today, independent of §2's layout decision.
2. **Fix `hybridNote()`'s line-1078 fallback** so it doesn't assert "off the meter" for cloud-inclusive windows.
3. **Fix the dangling `#92`/`#90` comment references** in `dispatch_internal.rs` (13 instances of `#92`, one `#90` — §0). Point them at #1177 (and whatever new issue tracks §3, once filed). Small, mechanical, and directly prevents the next research pass from repeating this session's wrong-number detour.
4. **Fix the `docs/index.html:240` hero tagline** (§5, row 2) — already a true-statement gap regardless of which visual direction §2 ultimately takes.

### Fuller redesign — needs operator sign-off on §7's open questions

5. The two-number "by your fleet" hero layout (§2.2).
6. The per-endpoint / per-machine / per-role cloud breakdown views (§3) — new issue(s), likely nested under #558 or filed as a sibling to #1177's own checklist.
7. The `telemetry.tokens.payload.endpoint` schema bump (§4 Phase 2, FLOW_SCHEMA 1.16.0).
8. The marketing screenshot retake (§5, row 3) — sequenced *after* item 5 ships, so the new asset matches the shipped hero rather than needing a second retake.

---

## 7. How this composes with darkmux doctrine

- **Operator sovereignty.** The per-endpoint/per-machine/per-role breakdowns (§3) exist so the operator can always answer "where did this number come from" — never a silently-aggregated total. Direct application of *"the operator never has to wonder where a decision came from."*
- **Hybrid by design, not a pivot.** CLAUDE.md's own doctrine section is titled *"AI Tooling Strategy (Hybrid: Frontier + Local)"* — this redesign is the dispatch layer catching up to a hybrid vision the orchestration layer already declared, not an admission that "off the meter" was aspirational.
- **Tokens only, never currency — extended symmetrically.** Nothing in this proposal computes or displays a `$` figure in-product, for either tier. The existing hard constraint from #783 (*"claiming to have saved another person money is a legal liability + unprovable"*) applies with equal force to the cloud side: darkmux shows token counts for cloud usage, never a live cost conversion, even though real cost data exists (gathered ad hoc, outside the product, this session).
- **KISS.** The redesign adds exactly one new number to an existing card, not a new page or five new chips. The per-endpoint/role/machine views are opt-in drill-downs using the existing `<details>` progressive-disclosure pattern, respecting #1091's density precedent rather than fighting it.

---

## 8. Open questions for the operator

1. **Cloud number's label wording** — "tokens via cloud," "tokens via cloud endpoints," "tokens via paid endpoints"? Each carries a different connotation (neutral / precise / directly-honest-about-money-without-naming-a-figure).
2. **Chip-row split** — combined-with-expand (recommended, §2.2.3) or fully doubled per tier? The recommendation leans on #1091's precedent, but it's a real trade-off between density and resolution.
3. **Zero-state for the cloud number** when no remote endpoint has ever fired — always show "0 tokens via cloud" (ambient discovery, consistent with the original hero's own always-render decision) or suppress it until the first cloud dispatch happens? The original #783 debate resolved toward "always show, even at zero" for the *existing* hero; whether an unused-capability zero-state carries the same argument is worth your read.
4. **Home for the new by-endpoint/machine/role views** — a new run-detail-style page, a new lens on the existing fleet view, or deferred until #558's unified viewer lands? I don't have enough of #558's own design specifics yet to recommend a home confidently.
5. **Phase 1 + Phase 2 in one PR, or Phase 2 as an explicit fast-follow?** I lean fast-follow — ship the honest, viewer-only fix now (item 1 in §6), land the schema bump separately once the aggregation shape is proven. No blocker either way; purely a sequencing preference.
6. **Issue filing shape for §3** — one umbrella issue with a checklist (matching #1177's own precedent shape) or several smaller ones? Leaning toward one umbrella, but that's your call to make, not an architecture call for me to make on your behalf.
