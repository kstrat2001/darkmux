# Viewer parity harness (Packet 0a)

`goldens/*.txt` is a **frozen specification**: what the *legacy* viewer
(`crates/darkmux-serve/assets/viewer.html`) actually showed, from a real
daemon, at the point it was captured. The React port
(`ui/src/`, built to `crates/darkmux-serve/assets/next.html`) is graded
against these files by the `next-parity*` suites in this directory. A
change to a golden file is a change to the spec — it happens in a reviewed
diff, on purpose, never as a side effect of a code change elsewhere.

**The legacy viewer is retired (#1806).** `viewer.html` is gone from the
tree, along with the extraction harness that regenerated goldens from it
(`extract.spec.ts`, its dedicated `playwright.config.js`, `redprove.spec.ts`,
`verify-goldens.mjs`, `determinism.mjs`). The goldens survive as the frozen
spec; recovering the legacy source they were captured from is:

```bash
git show v2.9.0:crates/darkmux-serve/assets/viewer.html
```

**Rebaselining a golden is now a hand-edit, not a script.** There is no more
regeneration path — a `goldens/<lens>.txt` change is a direct edit, reviewed
like any other diff, checked by hand against real port output (`curl` the
daemon's endpoints, or load the port and eyeball the lens) rather than
trusted because a script produced it.

Self-contained: bun-managed, its own port for the `next-parity*` configs
(47919+, distinct from `tests/e2e`'s 47823 so suites can run concurrently).
Does not touch `tests/e2e/`.

## What's still here

1. **`bun run record`** — hits the operator's LIVE daemon
   (`http://127.0.0.1:8765` by default; override with `DARKMUX_DAEMON_URL`)
   for every endpoint the viewer's lenses fetch, sanitizes every response
   (see below), and writes `corpus/*.json` + `corpus/meta.json`. Refuses to
   write anything if the daemon is unreachable or a response isn't valid
   JSON — no fabricated fixtures. Still useful for capturing a fresh corpus
   to eyeball against the frozen goldens by hand; it no longer feeds an
   automated golden regeneration.
2. **`bun run tripwire`** (== `bun run check`) — independent, standalone
   scan of everything under `corpus/` and `goldens/` for the canary
   substrings in `lib/sanitize.mjs`'s `CANARIES` list (broader than what
   the sanitizer actively redacts — see the field-policy section below).
   `record.mjs` already refuses to write a fixture with a hit; this is the
   second, separate check over what's actually on disk (a hand-edited
   fixture, a stale file, a golden that quoted raw corpus text — all get
   caught here too). It already asserts `corpus/`/`goldens/` are non-empty,
   so it doubles as the existence check a plain rename would otherwise need.
   This is the pre-commit gate for this directory.
3. **`next-parity*` suites** (`next-parity`, `next-parity-runs`,
   `next-parity-catalog`, `next-parity-console`, `next-parity-live`,
   `next-parity-chrome`, `next-parity-graph`) — unchanged by this
   retirement. Each grades the React port's rendered text against the same
   frozen `goldens/*.txt` this README describes, using the extraction/
   normalization logic in `lib/extract-lens.js` (imported verbatim, the same
   module the retired legacy extractor used — see that file's own doc
   comment) — `next-parity-graph.spec.ts` is the one exception, reusing
   `lib/extract-graph.js`'s extractors instead (see item 4 below and that
   suite's own module doc for why). These are the directory's actual
   acceptance gate; run them via their own `bun run next-parity*` scripts.
   **CI runs six of the seven** (`ci.yml`'s `Next-parity suites` step loops
   `next-parity`, `next-parity-chrome`, `next-parity-runs`,
   `next-parity-catalog`, `next-parity-console`, `next-parity-graph`) —
   `next-parity-live` is not in that loop and is not part of the CI gate
   today; run it locally when touching the live/SSE lens. The suites that
   ARE gated bind fixed ports and share `goldens/`, so concurrent runs flake
   for reasons unrelated to the code under test — that's why CI runs them
   serially.
4. **`mission-graph-goldens.spec.ts`** (`bun run mission-graph-goldens`),
   #1868 packet 1: captures `goldens/mission-graph-canvas.txt` /
   `goldens/mission-graph-timeline.txt` from the standalone
   `crates/darkmux-serve/assets/mission-graph.html` page (served live at
   `/mission/:id/graph`, not a build artifact; its own
   `mission-graph-goldens.playwright.config.js` points `baseURL` straight at
   the operator's daemon rather than staging a static bundle), then red-proves
   both against `installBlankRoutes`. This is a CAPTURE suite, not a grading
   one: it writes the goldens it also runs the harness self-test against, the
   same shape the retired legacy `extract.spec.ts`/`redprove.spec.ts` pair
   used before #1806. Because a capture run OVERWRITES the goldens, set
   `GOLDEN_CHECK=1` to flip it into verify mode (compare, never write) and
   always `git diff goldens/` after a capture run: a golden must change only
   as a reviewed hand-edit, never as a side effect of somebody running this
   suite against a locally modified `mission-graph.html`. The suite also
   asserts the corpus mocks actually SERVED both fixtures, because this is
   the one suite whose `baseURL` is a live daemon: without that assertion a
   deleted route branch would fall through to real daemon data and stay
   green (proved by mutation, #1868). These two goldens are the frozen spec `next-parity-graph.spec.ts` (item 5)
   grades the ported lens against, reusing this suite's own
   `lib/extract-graph.js` helpers rather than re-deriving them. Run this
   capture suite locally against a live daemon only when RE-capturing the
   standalone page's goldens (e.g. after a deliberate change to
   `mission-graph.html` itself, which #1868's third packet retires along
   with this suite); not part of the CI gate (needs a live daemon).
5. **`next-parity-graph.spec.ts`** (`bun run next-parity-graph`), #1868
   packet 2: grades `MissionGraphLens` (`ui/src/lenses/mission/`, the
   `#mission=<id>` lens) against the SAME two goldens packet 1 captured —
   `.phasegroup`/`.mnode`/`.steprow`/`.tlphase`/`.tltask`/`.tlt-step` are
   kept byte-identical to the standalone page's DOM on purpose, so those
   sections grade with the EXACT SAME `lib/extract-graph.js` functions
   packet 1's capture used, zero new extraction code. The header and events
   sections use two NEW port-side extractors in that same module
   (`extractPortHeaderText`/`extractPortEventsText`) that normalize to the
   golden's own shape, since the port has different chrome (a real app-shell
   masthead) and a different events pane (`EventLogColumn`'s `.eventlog__rec`
   rows, not the standalone page's `.evrow`) — see each extractor's own doc
   for exactly what's normalized and why. Part of the CI gate (item 3 above).

## Re-recording the corpus

**The real rule (corrected, #1868 packet 1 review): the `next-parity*`
goldens are pinned to the exact DATA in the currently-committed `corpus/`,
not just its shape.** An earlier version of this section said a corpus diff
after re-recording was "expected to be large and not itself a bug"; that
was true while the legacy extractor still regenerated `goldens/` FROM the
corpus in the same step, so the two always moved together. That extractor
retired in #1806. Since then, `goldens/*.txt` is a frozen, hand-edited spec
and `corpus/*.json` is what the `next-parity*` suites replay against it, so
a full re-record changes the replayed data while the goldens stay exactly
as they were. Verified directly: re-recording the whole corpus and running
the six `next-parity*` suites against it failed 3+5+5+3 assertions;
reverting the 21 pre-existing fixture files (keeping only a genuinely new
one) made all of them pass again.

Two different needs, two different commands:

**1. A FULL re-record** (`bun run record`) reflects the operator's live
daemon state at record time (which missions exist, fleet composition,
etc.) into every committed fixture. Treat it as **invalidating every
`next-parity*` golden until a deliberate rebaseline follows**: re-record,
run every `next-parity*` suite, and for each failure either restore the
prior fixture content (if the diff is noise you don't want to lock in) or
hand-edit the affected `goldens/<lens>.txt` file to match the new real
content (see "Rebaselining" above), reviewed like any other diff, on
purpose, never as an automatic side effect of running the recorder. Do not
commit a re-recorded `corpus/` without also committing whatever golden
changes it demands.

```bash
cd tests/parity
bun run record      # needs the live daemon at 127.0.0.1:8765 (or DARKMUX_DAEMON_URL)
bun run check
git diff corpus/    # review before committing
# then run every next-parity* suite and rebaseline any golden it breaks
```

**2. Adding a NEW fixture, or refreshing ONE existing fixture in place**
(the mission-graph parity fixture is the first example) uses the targeted
mode instead: `bun run record -- --only graph` (equivalently `bun run
record:graph`). It records only that fixture's own endpoint(s), writes them
into the EXISTING `corpus/` directory in place (no whole-directory swap),
and updates only that fixture's own transcript entries in `meta.json`;
every other fixture and every other `meta.json` field, most importantly
`frozen_clock_ms` (what every OTHER golden's relative-time text is captured
against), is left untouched. This is the safe way to add or refresh a
single fixture without invalidating the 21 (and growing) fixtures the
`next-parity*` suites already depend on. `--only graph` is the only
targeted mode today; extend `record.mjs`'s `graphFixtureEndpointSpecs()`
pattern with a new `--only <mode>` when a future fixture needs the same
treatment.

Both modes share the same non-negotiable invariants: sanitization and the
residual-canary tripwire check run unconditionally either way, and what
must never differ for the same underlying input is the sanitizer's mapping
for a repeated identifier (same input → same synthetic output, always, see
`lib/sanitize.mjs`'s module doc).

## Sanitization (mandatory, not a flag) — FIELD POLICY, not word-scanning

The corpus is committed to a **public** repo. The first version of
`lib/sanitize.mjs` was entity-scanning: it rewrote any token CONTAINING one
of a few sentinel words (`finhub`, `finsys`, ...). That measures the wrong
thing — anything that doesn't happen to spell a sentinel word passes
verbatim. A post-0a QA review found real leaks that survived it: `bundle_id`
values (`someVerb@app/controllers/some_domain_controller.ts` — a
live client source-tree shape), a ~2,900-char client engagement brief in a
mission's `source_input` field (migration filenames, column names, CI
narrative, an internal ticket prefix the word-scanner had never heard of),
and `sys_NNNN` (underscore form — the old ticket regex only matched the
hyphen spelling).

The fix **inverts the model: content-bearing and path-bearing fields are
unsafe BY DEFAULT.** Every string leaf in the parsed JSON tree (not the raw
text — a text-level regex pass corrupted a JSON escape sequence once, see
the module doc) is classified by its FIELD NAME:

- **`PROSE_FIELDS`** (`description`, `source_input`, `reasoning`, `reason`,
  `prompt`, ...) → wholesale replacement with generated placeholder prose of
  the SAME length, newlines preserved at their original offsets, every real
  word gone — not scrubbed-in-place, replaced entirely.
- **`PATH_FIELDS`** (`bundle_id`) → format-preserving replacement: same
  depth, same extension, synthetic segment names.
- **`UUID_FIELDS`** (`machine_uid`) → a synthetic stable UUID, not a
  character-scramble (a scrambled-but-same-shape UUID would still LOOK like
  a real hardware identifier).
- **`SAFE_FIELDS`** (an explicit curated allowlist — identifier shapes,
  enums, timestamps, versions) → passthrough, but STILL run through the
  entity-token/ticket/SHA/IPv4 scan below as defense-in-depth.
- **Anything not in one of the above** is UNKNOWN, and unknown fields are
  NEVER passed through verbatim — a shape-appropriate default (prose or
  identifier-scramble, guessed from the VALUE since the field name told us
  nothing) applies, and the field name is logged in the record transcript
  (`UNKNOWN-FIELDS=[...]`) so it can be explicitly classified next time. This
  is the actual fix: a new field the policy hasn't been told about is unsafe
  until someone allowlists it, not safe until someone notices a leak.

The old entity-token/ticket/SHA/IPv4 scan is KEPT as defense-in-depth over
every SAFE field too — it's what lets `ansi_text` (the CLI's own rendered
output, which legitimately mixes real mission titles with structural chrome
no field-name policy alone could safely blank without destroying the
golden's value) stay mostly-real while still catching entity references
inside it, and it's what scrubs the tailnet IP in `redis_url_redacted`.

Machine names (`MacBook-Pro`, `m1-max-32gb-studio`) are left alone —
operator hardware names aren't client-identifying and the plan brief says
they may stay. `kstrat2001` (the operator's own public OSS org/repo
identity) is likewise left alone where it appears. Every replacement is a
stable hash of the *original* value (never persisted), so re-recording the
same real content always produces the same synthetic output — a corpus diff
shows real changes, not sanitizer entropy.

`tripwire.mjs` verifies against a DELIBERATELY BROADER canary list than what
the sanitizer actively rewrites (`CANARIES` in `lib/sanitize.mjs`) —
underscore/no-separator sentinel forms plus corpus-specific canaries
(`borrower`, `lender`, `consent`, `inertia/pages`, `admin_`) observed in the
actual leak. It's an independent second gate, not a restatement of the first
one's vocabulary.

## Lens inventory (from viewer.html's own hash grammar)

| Lens | Hash route | Golden(s) |
|---|---|---|
| fleet (default) | `#` (no hash) | `fleet.txt` |
| console | `#lens=console` (`&panel=<id>`) | `console.txt` (default panel: `mission-status`, Packet 0a), plus one golden per remaining allowlisted panel (Packet 6): `console-mission-status-all.txt`, `console-machine-status.txt`, `console-flow-status.txt`, `console-role-list.txt`, `console-config-list.txt`, `console-lab-fixture-list.txt`, `console-doctor-not-run.txt` (the manual-only "not yet run" placeholder — selecting the tab must never auto-fetch, #1286) and `console-doctor.txt` (after clicking "run") |
| runs | `#lens=runs` (`&kind=<all\|mission\|dispatch\|lab>`; legacy alias `#lens=lab`) | `runs.txt` (kind=all), `runs-kind-mission.txt`, `runs-kind-dispatch.txt`, `runs-kind-lab.txt` (all four filter chips, Packet 3), `runs-series.txt` (kind=lab + the `◧ series` toggle — the ONE thing `/lab/runs` actually feeds, see the correction below; Packet 3), `runs-lens-boot.txt` (a FRESH `#lens=runs` boot, exercising `boot()`'s own `lq` deep-link branch rather than a click-through — content is byte-identical to `runs.txt` by design, since both land on kind=all over the same corpus; the golden's value is proving the boot mechanism independently, Packet 3) |
| machine | `#lens=machine` | `machine.txt` (click-navigation path), `machine-deeplink.txt` (fresh boot with `#lens=machine` already set — Packet 2, a genuinely different code path: `boot()`'s `machineQuery()` branch fires before `renderFleet()` ever runs) |
| session drill-in | `#session=<id>` | `session-task-list.txt` |
| catalog picker | `#catpanel` (toggled via `#srcbadge`, not a hash route — global chrome, Packet 4) | `catalog-open.txt` (six regions: topbar/crumb/meta/logscope/stage + the `=== catalog ===` section — see "Extraction target" below) |
| mission graph lens | `#mission=<id>` (#1868, renamed from "mission replay-by-query") | `mission-graph-canvas.txt` / `mission-graph-timeline.txt` (graded by the SEPARATE `next-parity-graph` suite, item 5 above — see that suite's own doc); `mission-replay.txt` survives only as a historical record, see the note below |
| bare-date playback | `#<date>` (Packet 4) | `playback-date.txt` |

**`#mission=<id>` (#1868 — renamed from "mission replay-by-query"; before
that, "out of scope" through Packet 3, see git history for the prior
wording).** Through #1868's second packet, this hash routed to
`MissionReplay.tsx`, whose only job on a live daemon was a full
cross-document navigation to `/mission/<id>/graph` (`mission-graph.html`, a
SEPARATE asset with its own vendored React Flow bundle) — genuinely out of
scope for a `#stage`-text extractor, hence `mission-replay.txt` only ever
captured the DAEMON-LESS fallback (this corpus's `/flow-mission/:id` mock
answers every id with an honest `{records:[],count:0}`, so the golden is
an in-page empty-fleet render, not the populated/navigates-away case).
#1868's mission-graph lens (`MissionGraphLens.tsx`) now renders the mission
graph itself, in-place — no navigation, and no daemon-less fallback branch
of legacy's to stand in for. It's graded against its OWN goldens
(`mission-graph-*.txt`, captured from the standalone page in #1868's first
packet) by `next-parity-graph.spec.ts`, not this suite.
`next-parity-catalog.spec.ts`'s own `#mission=<id>` tests now assert this
port's real honest-error behavior directly (not-found, naming a peer
machine when the flow stream knows one) rather than comparing against
`mission-replay.txt`, which stays committed as the historical record of
what legacy's daemon-less fallback once showed.

ONE id is the exception, as of #1868 packet 1: `GRAPH_FIXTURE_MISSION_ID`
(`lib/graph-fixture.js`) has a real, populated `/flow-mission/` fixture
recorded for the mission-graph goldens, so a `#mission=<that id>` render
would take the POPULATED branch (navigate away) rather than the empty one
described above. Every other id still gets the empty stub. A future golden
that wants the empty branch must not use the graph fixture's id.

`runs-kind-lab` and `session-task-list` are folded into the same suite as
bonus goldens rather than being separately catalogued lenses: the former is
a client-side re-filter of the runs lens's already-loaded data (no new
fetch), the latter is a drill-in state within the fleet render machinery,
not a distinct top-level nav destination. `mission-replay.txt` and
`playback-date.txt` (Packet 4) are similar bonus goldens: fresh boots over
the same fleet render machinery, not new top-level lenses.

## Endpoints recorded

The plan's named set (`/runs /missions /phases /flow-days /flow-missions
/flow/<today> /fleet/machines/live /fleet/sessions/live /machine/resources
/machine/specs /panel/mission-status`) plus three extensions the viewer's own
code demanded for a complete render, not named in the plan's list but
required to reach it honestly:

- `/flow/<yesterday>` — `loadLiveWindow()` (the live-mode boot path this
  harness exercises) fetches `[prevDateUTC(today), today]`, not just today;
  recording only `/flow/<today>` would starve the fleet lens's "last 24h"
  rolling window of half its data.
- `/lab/runs` — the runs lens fetches it *alongside* `/runs` on every entry
  (`Promise.all([loadRuns(), loadLabRuns()])`). **Correction (QA finding,
  post-0a review):** an earlier version of this note claimed `runs-kind-lab`
  needed it to render — that's false; `runsFiltered()` (the function behind
  the flat kind=lab row list, same as every other kind) reads exclusively
  from `RUNS` (`/runs`'s own data), never `LAB_RUNS`. QA proved it by
  blanking the fixture and observing zero golden bytes change. `/lab/runs`
  actually feeds ONE thing: the `◧ series` sub-view
  (`state.runsSeries===true`, the series/knob-diff view over
  `groupLabRunsByTask(LAB_RUNS)`) — a further toggle inside kind=lab that
  this harness's `runs-kind-lab` golden does NOT currently exercise (see
  KNOWN COVERAGE GAPS). It's recorded correctly and sanitized correctly; it
  just isn't exercised by any committed golden yet.
- `/flow-session/task-list` — the concrete target for the `#session=<id>`
  golden. `task-list` was chosen specifically because it carries no client
  identifiers, avoiding URL-encoding a sanitized compound id in route
  matching.
- `/panel/mission-status-all`, `/panel/machine-status`, `/panel/flow-status`,
  `/panel/role-list`, `/panel/config-list`, `/panel/lab-fixture-list`,
  `/panel/doctor` (Packet 6) — the seven console panels 0a didn't record
  (only `/panel/mission-status`, the default tab, was recorded there). Each
  is a real `x-darkmux-panel: 1` GET against the live daemon, sanitized
  through the same `lib/sanitize.mjs` field policy as every other fixture.
  `doctor` was recorded exactly ONCE (its `auto_refresh: false`/manual-run-
  only semantics mean an operator, not a poll, decided when it ran — see
  `panel.rs`'s own module doc for the #1286 rationale); it takes ~2s to
  gather (it probes the machine) but that's a one-time recording cost, not a
  test-suite cost — the extraction/parity specs replay the RECORDED body,
  they never re-invoke the real command.
- `/mission/<id>/graph.json` and `/flow-mission/<id>` (#1868 packet 1): the
  mission-graph parity fixture's own two endpoints, recorded ONLY for
  `GRAPH_FIXTURE_MISSION_ID` (`lib/graph-fixture.js`), a synthetic
  sanity-check mission with no client-identifying content. Every other
  mission id gets the SAME generic stub these routes have always answered
  with (`installBlankRoutes`'s existing `/flow-mission/` fallback; a fresh
  404 for `/mission/:id/graph.json`, which nothing previously handled at
  all, see `lib/mock-routes.js`'s own comment). Consumed by
  `mission-graph-goldens.spec.ts`, not by any `next-parity*` suite.

## Extraction target

Each golden is five labeled regions, joined: `=== topbar ===` (the
masthead — `.top`, brand/build-chip/catalog-trigger/live-badge/refresh/
topnav), `#crumb` (breadcrumb), `#meta` (the badge line), `#logscope`
(event-log scope label), and `#stage` — the actual output target of every
`render*()` function in viewer.html. The full scrolling event log
(`#logbody`, inside the `.loglist` wrapper) is deliberately NOT captured:
it's a per-record stream, not lens-specific content, and would make goldens
huge and timestamp-heavy for little parity value beyond what `#stage`
already covers. Text is whitespace-normalized (trailing space stripped per
line, runs of 3+ blank lines collapsed to one) for stability, not
screenshots.

**Chrome packet addition: `=== topbar ===`, folded directly into the base
extraction (not a sixth opt-in region like catalog below).** `.top` is
global chrome — a body-level sibling of `#stage` — that the original
four-region extractor structurally could not see AT ALL: the masthead could
go missing, or grow a stray element, and every golden would stay
byte-identical. That gap is exactly how a stray unscoped `#logscope`
("FLEET" floating above the hero on `/next`) shipped invisibly — see
`lib/extract-lens.js`'s `extractTopbarText` for the full story and
`normalizeVerbadge` for how the one genuinely volatile piece (the
`#verbadge` build-identifier chip — a version + git SHA that changes on
every release) is handled: normalized to a fixed placeholder, not excluded,
so a real regression inside the chip still shows up as a diff. Folded in
(not opt-in) because `.top`'s content doesn't depend on `state.level` — it
renders identically on every lens — so every existing golden gained this
section on rebaseline rather than one dedicated golden growing it, unlike
catalog below.

**Packet 4 addition: an optional `=== catalog ===` region** (see
`lib/extract-lens.js`'s `extractCatalogText`/`extractLensTextWithCatalog`).
`#catpanel` (the playback-catalog day/mission picker) is a modal overlay —
a body-level sibling of `#stage`, never part of it — so the base extraction
structurally can't see it. This is composed on TOP of the base extraction,
not folded into it: only `catalog-open.txt` carries this section.

## KNOWN COVERAGE GAPS

Named explicitly so this README never claims coverage it doesn't have.
These are real gaps, not hidden ones — follow-up packets, not tonight's
scope:

- **Lab-run detail level** (`state.level==="lab-run"`, reached by clicking
  a `kind=lab` run row — either the flat list's or the `◧ series` view's) —
  **still not exercised (Packet 3 checked and deliberately did not chase
  this).** It needs `/lab/run/detail?dir=<dir>` and `/lab/run/events?dir=<dir>`
  fixtures, and NEITHER is in the corpus — `lib/mock-routes.js` explicitly
  404s both (`"lab-run drill-down not recorded in this corpus"`). Recording
  them for real needs a daemon with a resolvable lab-run dir at `record.mjs`
  time; fabricating them would violate the "never fabricate a fixture"
  discipline (`record.mjs`'s own doc: "Refuses to write anything if the
  daemon is unreachable... no fabricated fixtures"). Clicking a lab row
  against today's corpus doesn't hang or crash — `LAB_DETAIL` stays `null`,
  and `drillLabRun` falls back to the run list with a one-line notice — but
  that fallback state is not the same thing as the real detail render, so it
  isn't captured as a stand-in golden either. Follow-up: re-record with
  `/lab/run/detail` + `/lab/run/events` for one real `dir` included.
- **The `loose` level** (`drillLoose()` — a machine's session-less/unscoped
  records, reached by clicking a link inside the machine lens, not a
  documented hash route) — not exercised (machine-lens territory, not
  runs-lens; left for the machine-lens packet).
- ~~The catalog day-picker (`#catpanel`...) — the extractor doesn't capture
  it at all~~ **CLOSED (Packet 4)** — see the new `=== catalog ===` region
  and `catalog-open.txt`.
- ~~7 of the 8 console panels~~ **CLOSED (Packet 6).** All eight allowlisted
  panels (`crates/darkmux-serve/src/panel.rs::PANEL_IDS`) now have a golden —
  see the lens inventory table above. `doctor` specifically has TWO
  (`console-doctor-not-run.txt`, `console-doctor.txt`) since it's the one
  manual-only panel and both states are real, distinct, reachable behavior.
- **Deep-link boot paths other than `#session=<id>`, `#lens=runs`,
  `#lens=machine`, `#mission=<id>`, and a bare `#<date>`** (`#lens=runs` and
  `#lens=machine` closed by Packets 3 and 2 — `runs-lens-boot.txt` /
  `machine-deeplink.txt`) — `#lens=console&panel=<id>` AS A FRESH BOOT was
  never exercised on the LEGACY side specifically (the now-retired legacy
  extractor only reached every console panel via a CLICK sequence starting
  from the fleet default, same shape every OTHER lens's click-through used
  before its own deep-link-boot golden landed). Precedent (`runs-lens-boot.txt` vs `runs.txt`,
  Packet 3; `machine-deeplink.txt` vs `machine.txt`, Packet 2) is that
  `#stage` content is identical whichever way a lens is reached, so this is
  believed-safe rather than a real content gap — a future packet wanting to
  CLOSE it on the legacy side would add one more `page.goto("#lens=console
  &panel=<id>")` + `extractAndWrite` per panel and diff the result against
  the click-reached golden byte-for-byte. `/next`'s OWN parity spec
  (`next-parity-console.spec.ts`) DOES boot every panel via a fresh deep-link
  (that's its whole point — the CLI emits these links, see
  `panel_deep_link`), so the PORT'S deep-link mechanism is proven even though
  the legacy reference-golden's provenance is click-based.
  `#lens=runs&kind=<mission|dispatch|lab>` specifically as a boot (only
  plain `#lens=runs` is golden-tested as a boot) also remains untested.
  ~~a bare `#<date>` hash (playback-by-date, daemon-only)~~ **CLOSED
  (Packet 4)** — see `playback-date.txt`. ~~`#mission=<id>` (out of scope
  entirely)~~ **PARTIALLY CLOSED (Packet 4)** — see the lens inventory's
  `#mission=<id>` note above: the unknown-id in-page path is now
  golden-tested (`mission-replay.txt`); the populated/navigates-away case
  still needs real per-mission record fixtures this corpus doesn't have.
- **`fleet-sessions-live.json` was recorded empty** (`[]`) — no session was
  live on the operator's daemon at record time, so this corpus fixture has
  never actually exercised the viewer's non-empty rendering path for that
  endpoint. Re-recording while a session is active would close this gap.
