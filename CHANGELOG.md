# Changelog

All notable user-facing changes to darkmux are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

darkmux follows semver, stable since **1.0.0**; breaking changes are called out
explicitly in each entry (pre-1.0, the no-compat-baggage policy shipped breaks
without deprecation shims). Roadmap **milestones** (`M1`/`M2`/`M3`…) are
intentionally decoupled from these version numbers, and the `RULES_SCHEMA` /
`FLOW_SCHEMA` / `LEDGER_SCHEMA` data-shape contracts version on their own
cadence (see `CLAUDE.md`) — a major bump in one of those is a breaking change
to that payload, called out in the entry, and does not by itself force a major
darkmux release.

## [3.4.0] - 2026-08-30

The crawler becomes a mission, and the machine tells you what it is doing.

Crawling a codebase against a rule set is now just the crawler role's work as
a mission: a **workspace spec** names the sources and the file filters, rules
are a template kind, and `mission launch crawl --dry-run` previews the plan.
Every finding, step, and mission record can be routed anywhere through
**hooks**, an event-agnostic flow sink that POSTs matching records to a
loopback receiver; this release's first receiver is a small local issue
tracker. Underneath, darkmux now reads the Apple Silicon host properly: a
~7 ms **host probe** (mach ticks, IOReport power and clocks, thermal state)
replaces a `top` shell-out that cost ~780 ms per sample and reported a
since-boot average as "current CPU"; the daemon keeps a ten-minute ring and
the viewer shows thermal state, power per rail, and CPU clusters in a
**Machine info** modal and a new phone **bottom sheet** with the event log.
`runtime.turn_delay_ms` rests the GPU between turns, recorded so a rested
run is never misread as a slow model.

### Added

- **Workspace spec + rules kind + crawl mission** ([#2108](https://github.com/kstrat2001/darkmux/pull/2108), [#2096](https://github.com/kstrat2001/darkmux/pull/2096), [#2098](https://github.com/kstrat2001/darkmux/pull/2098), [#2099](https://github.com/kstrat2001/darkmux/pull/2099); part of [#1959](https://github.com/kstrat2001/darkmux/issues/1959)).
  No `crawl` verb and no "corpus": the manifest is a generic workspace spec
  mission input (`sources` + `include`/`exclude`, materialized as read-only
  worktrees under `<root>/workspaces/<name>`), rules live under
  `templates/builtin/rules/` with a user tier at `~/.darkmux/rules/`, and
  the launcher (`templates/builtin/mission-configs/crawl.json`) dispatches one
  unit per (source, rule) with `record_context` on every record. Crawl
  records are the generic `mission start/close` and `step start/complete`
  vocabulary, not a private `crawl.*` one.

- **Hooks: a flow sink that POSTs matching records to a loopback receiver** ([#2097](https://github.com/kstrat2001/darkmux/pull/2097), [#2103](https://github.com/kstrat2001/darkmux/pull/2103), closes [#2093](https://github.com/kstrat2001/darkmux/issues/2093)).
  `config.hooks.rules[]` match any record (`action`, `category`, dotted
  `payload.*` predicates) and deliver it over HTTP to `127.0.0.1` only
  (userinfo refused, no redirects); an outbox and cursor per rule survive a
  receiver outage, `flow status` shows delivery state, and `flow drain`
  flushes it. Hooks observe; they never dispatch.

- **`runtime.turn_delay_ms`: rest the GPU between inference turns** ([#2097](https://github.com/kstrat2001/darkmux/pull/2097), closes [#2094](https://github.com/kstrat2001/darkmux/issues/2094)).
  A global inter-turn sleep on every local dispatch, recorded as
  `dispatch.rest` (`rest_ms`, `rests`, `turn_delay_effective_ms`) and
  counted as proof-of-work for the inactivity watchdog; `wall_ms` stays wall
  time. Never applied to agentic-remote dispatches; clamped against the
  inactivity timeout.

- **Apple Silicon host probe** ([#2108](https://github.com/kstrat2001/darkmux/pull/2108), part of [#2107](https://github.com/kstrat2001/darkmux/issues/2107) and [#1833](https://github.com/kstrat2001/darkmux/issues/1833)).
  `host_probe/` reads CPU as mach tick deltas (a true mean over the interval,
  per cluster by `hw.perflevel`), power per rail and cluster/GPU MHz from
  IOReport (loaded at runtime, degrades to null), GPU busy and memory from
  IOKit in-process, thermal state from `ProcessInfo` and the CPU speed limit
  from `IOPMCopyCPUPowerStatus`. About 7 ms per sample. The daemon samples
  every `runtime.host_sampler_interval_ms` (5 s) into a ten-minute ring
  served as `load` on `GET /machine/resources`; `dispatch complete` records
  carry `host.thermal`, `host.power`, and `host.energy_mwh` (flow schema
  1.28.0, additive). `darkmux doctor` names which sources resolved and the
  measured cost.

- **Machine info modal and the phone bottom sheet** ([#2108](https://github.com/kstrat2001/darkmux/pull/2108)).
  The masthead ⓘ opens a Machine info modal (gauges with avg/max, thermal
  pill, power per rail, CPU cluster tiles); phones get a tabbed sheet
  (Machine info | Events) anchored under the masthead with the event log's
  list, a row tap that pushes the record's detail, and follow mode that
  always shows the list. The Machine lens renders the same block plus its
  own depth.

### Fixed

- **The CPU column was never a measurement** ([#2108](https://github.com/kstrat2001/darkmux/pull/2108)).
  `top -l 1` blocked ~780 ms per sample and its first sample is a since-boot
  average, so every earlier `host.cpu.*` value was a lifetime smoothing. Gone.
- **Fleet lens: the summary row is back on the tab row** (regression against
  the 2026-08-27 screenshot), the machine lens no longer repeats the machine
  name above its own breadcrumb, hero token figures no longer collide in the
  two-column band, the viewer's tab favicon is the two-input glyph.
- **Crawl finding records carry one rule id** ([#2103](https://github.com/kstrat2001/darkmux/pull/2103)); a receiver's per-record rejection is surfaced as `hook.fired.receiver_rejected`.
- **Docs**: the home page hands the brand off from the hero to the nav bar on scroll, and the guide header is one row on phones ([#2115](https://github.com/kstrat2001/darkmux/pull/2115)).

### Schema

- `FLOW_SCHEMA_VERSION` 1.22 → 1.28 (hook records; generic mission/step crawl payloads; `dispatch.rest`; workspace vocabulary; host thermal/power/energy). All additive.
- `CONFIG_SCHEMA_VERSION` 1.11 → 1.14 (`hooks`, `runtime.turn_delay_ms`, `runtime.host_sampler_interval_ms`).

[3.4.0]: https://github.com/kstrat2001/darkmux/releases/tag/v3.4.0

## [3.3.0] - 2026-08-28

The demo plays a real mission, and playback rides on every route.

[darkmux.com/demo](https://darkmux.com/demo) now replays a real review mission,
the crew reviewing a merged darkmux PR, with the mission graph, the runs, the
fleet, and the event log all reading from one committed day. The playback
transport sits on a sticky row with the tabs on every route, its speed is an
honest multiplier, and a daemon page for a finished dispatch gets the same
day chip, badge, and transport the demo has. Most of this release was found
by tapping through the demo on a phone: a fleet card that landed on an empty
runs lens, run rows that errored, a playback view that jumped as events
streamed in, a top chrome that was mostly text. Each of those is fixed below.

### Added

- **The demo replays a real review mission** ([#2062](https://github.com/kstrat2001/darkmux/pull/2062), closes [#2032](https://github.com/kstrat2001/darkmux/issues/2032)).
  `scripts/demo-env/import_mission.py` imports a finished mission from a
  real `~/.darkmux`, scrubs identity once at import (machine ids, host paths,
  hostnames, tailnet names, absolute timestamps; a scrub miss fails the
  import, and CI's public-leak guard fails the PR), and the static build
  captures each mission graph into `docs/demo/demo-graphs.json`. The subject
  is always darkmux's own public code, never an engagement's, so a missed
  scrub still exposes nothing foreign.

- **Sticky tabs and a playback transport on every route** ([#2080](https://github.com/kstrat2001/darkmux/pull/2080), closes [#2071](https://github.com/kstrat2001/darkmux/issues/2071)).
  The shell owns the playhead, so a run's detail page, the fleet, and the
  runs board all follow the same clock; the tabs and the transport stay
  pinned while the page scrolls. A run rewound to before it started says so
  instead of rendering a header for nothing.

- **A daemon dispatch or mission page names its day** ([#2089](https://github.com/kstrat2001/darkmux/pull/2089)).
  The chip shows the day the run started, the `▶ PLAYBACK` badge says which
  mode the page is in, and a finished dispatch gets the transport. A run
  that is still going stays a live view. When no date resolves the chip
  reads `RESULT`.

- **`init` verifies the worker model against LM Studio** ([#2054](https://github.com/kstrat2001/darkmux/pull/2054), closes [#2053](https://github.com/kstrat2001/darkmux/issues/2053)).
  The shipped default is written only when LM Studio actually has it; the
  next-steps text no longer says `docker build`.

### Changed

- **Playback speed is a real multiplier** ([#2081](https://github.com/kstrat2001/darkmux/pull/2081)).
  The transport advanced a fixed fraction of the day per tick, so every
  recording played in twelve seconds and "1×" was thousands of times real
  time. It now advances recorded time by the measured wall-clock delta times
  the speed, labeled as recorded time per second: `1h/s` (default), `10m/s`,
  `1m/s`.

- **One date chip on every build** ([#2085](https://github.com/kstrat2001/darkmux/pull/2085), [#2074](https://github.com/kstrat2001/darkmux/pull/2074), closes [#2072](https://github.com/kstrat2001/darkmux/issues/2072), [#2073](https://github.com/kstrat2001/darkmux/issues/2073)).
  Demo and daemon render the same outlined pill with the bare date (the
  `FLOW ·` prefix was noise), and the phone chrome drops from 203 to 153
  pixels: the meta line keeps the mission and the census, the idle line is
  gone from the static build, the masthead packs the chip and badge to the
  right on phones only.

- **Uniform tabs, matching transport controls** ([#2082](https://github.com/kstrat2001/darkmux/pull/2082), [#2084](https://github.com/kstrat2001/darkmux/pull/2084)).
  Tab cells share a width on every screen and fill the row on portrait
  phones; the transport marks are SVG paths in identically sized buttons,
  and play is outlined like the rest.

- **One source resolver, one day hook** ([#2087](https://github.com/kstrat2001/darkmux/pull/2087), closes [#2086](https://github.com/kstrat2001/darkmux/issues/2086)).
  `lib/source.ts` is the only place the viewer decides whether it is a
  static page or a daemon page; `hooks/useDay.ts` is the only loader for a
  day of records. The build-type branch left the lenses.

- **Guide front door and radio page** ([#2056](https://github.com/kstrat2001/darkmux/pull/2056), [#2051](https://github.com/kstrat2001/darkmux/pull/2051), [#2052](https://github.com/kstrat2001/darkmux/pull/2052)).
  The guide index and getting-started page are one captured session on a
  fresh home, with no frontier-orchestrator assumption; the radio page is
  distilled to what works, with verbatim captures; `acp --help` stops
  calling a shipped feature a spike.

### Fixed

- A fleet card tap on the demo lands on the machine's runs instead of an
  empty board ([#2064](https://github.com/kstrat2001/darkmux/pull/2064), closes [#2063](https://github.com/kstrat2001/darkmux/issues/2063)).
- Every run row on the demo opens ([#2066](https://github.com/kstrat2001/darkmux/pull/2066), closes [#2065](https://github.com/kstrat2001/darkmux/issues/2065)): mission rows
  gate on the captured graph, dispatch rows slice their session out of the
  committed day.
- The mobile playback view no longer jumps and flickers as events stream in
  ([#2069](https://github.com/kstrat2001/darkmux/pull/2069), closes [#2068](https://github.com/kstrat2001/darkmux/issues/2068)): the hero row wrapped by the digit width of one
  tile, the inspector resized on every followed record. Cumulative layout
  shift on a full replay went from 1.21 to 0.21.
- Fleet cards on the demo read their hardware from a committed snapshot
  instead of saying it was not reported ([#2070](https://github.com/kstrat2001/darkmux/pull/2070), closes [#2067](https://github.com/kstrat2001/darkmux/issues/2067)).
- Mission graph siblings no longer overlap, phase bands share a width, and
  the canvas fits its viewport ([#2059](https://github.com/kstrat2001/darkmux/pull/2059)).

## [3.2.0] - 2026-08-28

Radio becomes interactive help, and the front door stops lying.

The home page now promises three lines to a first answer: install, `init`,
`darkmux radio "do you have a brain?"`. This release is what it took to make
that promise true on a fresh Mac, plus the page itself. It was measured the
way a new user would meet it: ten questions a first-day user would type, run
against radio before and after, graded on one thing, whether the answer names
a command they can actually run. Before: 3 of 10. After: 9 of 10.

### Added

- **Radio is grounded in the full verb index** ([#2043](https://github.com/kstrat2001/darkmux/pull/2043)).
  The answering seat was handed top-level `--help` truncated at 1,600
  characters, so `serve`, `init`, `lab run inspect`, and `mission propose`
  did not exist as far as it knew, and it invented ids to fill the gap. It is
  now handed the whole command tree, walked from clap at call time: every
  runnable verb, its options, one sentence each, 64 lines. Capability is a
  lookup, not an inference. The index is the last generic section dropped
  under the bundle's hard cap, because for a help tool "how do I" grounding
  outlives "what am I working on" grounding.

- **`init` fills in the worker model** ([#2047](https://github.com/kstrat2001/darkmux/pull/2047), closes [#2038](https://github.com/kstrat2001/darkmux/issues/2038)).
  `init` wrote a registry whose worker profiles named `<your-worker-model-id>`,
  and nothing filled the blank, so every fresh install failed at its first
  dispatch with a message that blamed LM Studio. `init` now asks LM Studio
  what is downloaded and loaded, picks a loaded model if there is one (the
  operator chose it) or else the largest downloaded model under 60% of RAM,
  writes it into every placeholder slot, and says which one and where. A
  registry the operator has already edited is never touched. No `lms`, or
  nothing downloaded: the placeholder stays and the message names the fix.

- **The home page, rebuilt for the Apple Silicon push**
  ([#2035](https://github.com/kstrat2001/darkmux/pull/2035), [#2037](https://github.com/kstrat2001/darkmux/pull/2037), [#2039](https://github.com/kstrat2001/darkmux/pull/2039), [#2040](https://github.com/kstrat2001/darkmux/pull/2040), [#2041](https://github.com/kstrat2001/darkmux/pull/2041), [#2046](https://github.com/kstrat2001/darkmux/pull/2046)).
  "The AI runtime built for Apple Silicon." An origin story instead of a
  token-rent argument, a six-row comparison against the class of harness
  built around a frontier API, Get going third instead of last, every command
  on the page one tap to copy, two-sentence feature sections, and a Get going
  card that is one real session: two radio answers captured verbatim on an
  M5 Max, attributed to the model and the setting that produced them.
  Nothing on the page bounces to Substack; nothing names a competitor.

- **The event log is a collapsible mainstay on every tab**
  ([#2026](https://github.com/kstrat2001/darkmux/pull/2026), [#2025](https://github.com/kstrat2001/darkmux/pull/2025), [#2024](https://github.com/kstrat2001/darkmux/pull/2024)),
  with a filter badge that says how many events the filters are hiding and
  filters that survive a refresh. Console panel selection is in the URL.

- **A mark** ([#2020](https://github.com/kstrat2001/darkmux/pull/2020), [#2023](https://github.com/kstrat2001/darkmux/pull/2023), [#2031](https://github.com/kstrat2001/darkmux/pull/2031), [#2033](https://github.com/kstrat2001/darkmux/pull/2033)):
  the multiplexer, four channels in and one out, on the site, in the tab, and
  on the viewer's own masthead.

- **The demo world is committed** ([#2015](https://github.com/kstrat2001/darkmux/pull/2015)),
  so the screenshots the docs are shot from are reproducible, and the static
  demo renders its own data on every lens instead of a 404 page
  ([#2019](https://github.com/kstrat2001/darkmux/pull/2019), [#2021](https://github.com/kstrat2001/darkmux/pull/2021)).

### Changed

- **Radio ships at humor 50** ([#2045](https://github.com/kstrat2001/darkmux/pull/2045)).
  The default was 65, a value carried over from the author's own persona
  override, never chosen. Sampled on one question: under about 40 the model
  reads as plain, 50 is the first setting with a line in it, 100 is the full
  persona. One constant now, one test, so the number cannot drift across its
  copies again. `radio.humor` is the dial.

- **The answering seat gets a budget a reasoning model can use** ([#2044](https://github.com/kstrat2001/darkmux/pull/2044)).
  It was capped at the single-shot path's 4096 tokens, and a 35B thinking
  model spent exactly that reasoning about a one-line question and returned
  nothing. The seat now honors `runtime.max_tokens_per_call` and otherwise
  uses 16,384. An empty answer is a failure that names the budget and the
  knob, not a blank line and exit 0.

- **Tooling uses the window** ([#2016](https://github.com/kstrat2001/darkmux/pull/2016)):
  the two lens width caps are gone.

### Fixed

- **First-inference failures say the fix once and exit 1** ([#2042](https://github.com/kstrat2001/darkmux/pull/2042)).
  Probed with a fresh home for each case: no `init`, the placeholder model, a
  model not downloaded, the LM Studio server down, no `lms`. Every one printed
  the same error twice, ended on a command listing, and exited 0. One defect:
  a routing dispatch that could not run was recast as a model refusal, and
  the answering seat then failed the same way. `RouteDecision::Unavailable`
  separates "the model declined" from "the model was never reached." The
  messages are fixed at their source, so `dispatch` and `lab` get them too:
  the placeholder is named as a blank `init` left, a missing `lms` names
  `lms bootstrap` instead of guessing at RAM, a refused connection names the
  URL and `lms server start`.

- **Six guards that did not guard** ([#2027](https://github.com/kstrat2001/darkmux/pull/2027)),
  found by a five-agent QA pass briefed to falsify claims rather than walk a
  checklist: a sentinel-vocabulary parse that failed open on a comment, an
  isolation test that scanned lines instead of tags, a drift guard that passed
  vacuously when it could not see what it guarded, and three more. Every one
  was a check that passed when you planted the thing it existed to catch.

- **The event log collapses to the side** instead of blanking its pane, the
  collapse button no longer covers content, and the follow toggle shows its
  state ([#2029](https://github.com/kstrat2001/darkmux/pull/2029)).

- **A machine's label is its most recent name**, not the first one found in
  the stream ([#2030](https://github.com/kstrat2001/darkmux/pull/2030)).

### Schema

No data-shape changes. `FLOW_SCHEMA_VERSION` stays at 1.22.0 and
`CONFIG_SCHEMA_VERSION` at 1.11; the radio flow record's `decision` field
gained the value `unavailable`, which older readers pass through.

## [3.1.0] - 2026-08-27

Three fixes about darkmux telling the truth about what happened.

3.0.0 shipped a run-detail lens good enough to look at closely, and looking
closely is what found these. Each one is a place where the system recorded or
reported something other than what occurred.

### Fixed

- **Tool results are persisted, not just measured** ([#2007](https://github.com/kstrat2001/darkmux/issues/2007)).
  Every `tool.completed` record carried `result_chars` and threw the result
  away. A run's trajectory could tell you a tool returned 4,182 characters and
  not one of them. That is the same shape as the tool-args and session-record
  findings before it, and it is now the third time this project has discovered
  it holds a value, takes its length, and drops the value. The result now
  rides the record, capped at 64 KB.

  The cap **truncates rather than drops**, and it elides from the MIDDLE at a
  3:1 head:tail ratio. A tool result's two useful ends are the command that ran
  and how it finished; head-only truncation reliably discards the second one.

- **A red test is not a broken tool** ([#2008](https://github.com/kstrat2001/darkmux/issues/2008)).
  The failure-cascade detector classified any non-zero exit from `bash` as a
  tool failure. A model running a test suite under TDD, where a red suite is
  the expected result, would accumulate cascade signals and get told its tool
  "could not run" while the tool ran perfectly and reported exactly what it was
  asked to report.

  `ToolOutcome` replaces the boolean and distinguishes three states: the tool
  worked (`Ok`), the tool worked and the command it ran reported a non-zero
  exit (`Reported`), and the tool itself could not run (`Failed`, with a
  reason). Only the third feeds the cascade detector. The feedback template was
  rewritten to match: it now names what actually happened rather than asserting
  a falsehood the model can see is false.

- **A finished run stops running** ([#2011](https://github.com/kstrat2001/darkmux/issues/2011)).
  Two defects, one root: the run-detail lens had no way to learn a run had
  ended.

  Its wall clock rendered `close.ts - startTs`, the gap between two flow
  records as the *viewer* received them, while the run's own recorded `wall_ms`
  sat unread on the completion payload. The metric was never ticking; only the
  rendering was.

  And the view stayed RUNNING until a manual reload, because liveness comes
  from presence heartbeats and the presence key is deleted BEFORE
  `dispatch complete` is written. The poll that would have fetched the terminal
  record is exactly the poll that stops. A bounded grace window now holds
  polling open across that gap.

### Schema

`FLOW_SCHEMA_VERSION` **1.20.0 → 1.22.0** (two minor bumps, both additive).
`tool.completed` gained `result`, `outcome`, `exit_code`, and
`failure_reason`. Older readers ignore what they do not know; the viewer
renders pre-1.22 records the way it always did, since a record written before
the distinction existed cannot be re-interpreted after the fact.

## [3.0.0] - 2026-08-27

A major, and the reason is one rename.

The gate that decides whether a mission config may shell out on your behalf was
named for GitHub. The MECHANISM never was — `GhConfig`'s own doc already said
"GitHub never enters darkmux core … just a list of operator-chosen VERB NAMES",
and the check did nothing but compare a string a config declares against a list
you allowlisted. But the NAME is what people build on: a GitLab user was
allowlisting `mr-merge` under `gh.allowed`, and a config gating `terraform
apply` — which wants this gate exactly as much — had to declare a GitHub-shaped
field to get a check with nothing to do with GitHub.

Renaming it now cost one schema major and **zero migrations**, because no
built-in and no user document had declared the field yet. The same rename once
it has users costs a real migration. That is the whole argument for a major
release with an empirically empty blast radius.

Alongside it: the viewer stopped rendering in Courier for almost everyone, and
`doctor` stopped running off the side of the screen.

### BREAKING

- **`MissionConfig.gh_verb` is now `cmd`** — mission-config schema `2.3` → `3.0`.
  A document still declaring `gh_verb` is a loud validation **Error**, never a
  silent overflow into `extras`. That distinction is load-bearing: this gate
  fails OPEN for configs that declare nothing (correct — most configs touch
  nothing outside darkmux), so a document left on the old name would silently
  lose its gate and run the shell-out it was protecting as if you had approved
  it. Rename the field and set `schema_version` to `"3.0"`.
- **`gh.enabled` / `gh.allowed` are now `cmd.enabled` / `cmd.allowed`** — config
  schema `1.10` → `1.11`. `darkmux config set gh.enabled …` reports
  `unknown config key` with a suggestion rather than failing obscurely.
- **`DARKMUX_GH_ENABLED` / `DARKMUX_GH_ALLOWED` are now `DARKMUX_CMD_ENABLED` /
  `DARKMUX_CMD_ALLOWED`.**

`FLOW_SCHEMA_VERSION` stays at `1.20.0` — nothing on the wire changed shape, so
a peer on 2.12.0 still reads this machine's records.

### Fixed

- **The viewer rendered in Courier for anyone without JetBrains Mono installed**,
  which is nearly everyone: the CSS named that font 89 times and shipped no
  webfont for it. Measured with CDP, the alternate `ui-monospace, SFMono-Regular,
  monospace` stack resolved to Courier too — neither `ui-monospace` nor
  `SFMono-Regular` resolves in Chrome on macOS. One `--font-mono` token behind
  all 107 declarations, landing on Menlo/Consolas/Liberation Mono. No font is
  packaged; every family is already present on its platform.
- **The type was too small to read.** 115 of ~167 font declarations were 11px or
  smaller, against a 16px browser default. One `--fs-scale` knob behind 169
  declarations, as a uniform multiplier so every existing size relationship is
  preserved exactly.
- **`.machine-lens` was never centered** — `max-width` with no `margin-inline`,
  so every pixel the cap withheld pooled on one side: 16px of gutter left and
  224px right at 1440.
- **A count could be severed from its unit.** The limit-source strip is a flat
  text run, and at the wider type the browser broke *inside* a value, rendering
  `unpriced 0` on one line and `models` on the next.
- **The savings headline could overflow a phone.** Its width is data, and a fixed
  size assumes a digit count — at 320px the 9-digit total ran 31px past the
  viewport. Now fluid, so it cannot overflow at any total.
- **`darkmux doctor` ignored the width it was asked for.** `panel.rs` passes the
  client's measured width as `COLUMNS` and every other panel verb honors it;
  doctor emitted a 2031-character line at every width, overflowing the console
  panel by 533px at a 1440 viewport. Now wrapped with a hanging indent — the
  verdict banner too, which quotes the worst check's whole message and was
  therefore the single longest line it emitted.
- **The verdict banner is one line.** It had quoted the worst check's entire
  message — measured at 9,726 characters across 50 lines.
- **Identical registry findings state their explanation once.** 15 configs
  trailing the schema by one minor produced the same ~600-character paragraph
  fifteen times; it is one fact about fifteen documents, not fifteen facts.
- **The console asked for the wrong render width.** `panelCols()` divided by a
  hardcoded 7.2px per character, calibrated against the old 12px mono — at the
  new size a 1406px panel asked for 191 columns when 164 fit. It measures the
  element's own font now.

### Added

- **`DESIGN.md` covers the command gate and ACP.** ACP had no coverage at all
  despite being a shipped surface; the section names what is still spike-grade
  rather than papering over it.
- **Real screenshots in the guide**, which had zero images across ten pages, and
  a social card that is no longer two releases stale. Every capture is shot from
  a fixture fleet by `scripts/demo-env`, so a public page never carries
  hostnames, tailnet addresses or workspace paths — and so the whole set can be
  re-shot after a UI change.

## [2.12.0] - 2026-08-26

The run-detail lens, rebuilt around one question the page could not answer:
which of these numbers describe the model, and which describe darkmux around
it. Reading `model (lms)` beside TURNS / TOKENS / WALL CLOCK, there was no way
to tell — and the page was throwing away most of what it had been handed.

Also settles the work-unit vocabulary. `run`, `dispatch`, `step` and `session`
each denoted a grain and nothing said which, so every consumer picked its own
reading; `dispatch` in particular named both a top-level run kind and the
innermost unit. See `CLAUDE.md`'s contract registry entry 8 and `DESIGN.md`.

No schema bump: `FLOW_SCHEMA_VERSION` stays at `1.20.0`. Nothing on the wire
changed shape.

### Added

- **The prompt is readable.** It was held, measured, and discarded — the page
  rendered `prompt · 1430 chars` while holding the string. Now an expander,
  with the record's authoritative length so a truncated brief still reports
  its real size (#1984).
- **MODEL and SYSTEM metric panes.** Turns, tokens and context describe the
  model's work; wall clock, compactions and host load describe the system
  around it. A step that ran no model — a `procedural.shell` step — shows no
  model pane at all rather than `0 COMPACTIONS`, which asserts something that
  cannot happen there (#1984, #1996).
- **Host CPU / RAM / GPU peaks.** Already in the flow stream, fetched by the
  page and explicitly discarded. Peaks rather than latest, because the
  question asked of a finished run is whether it saturated the machine, and
  absent rather than zero when a run predates the sampler (#1996).
- **SIGNALS**, replacing `detections`. Grouped by kind with a count,
  severity-coded, and stamped with run-relative times. The severity was always
  in the payload and always thrown away, so a recovered stall rendered with
  the same warning glyph as a doom loop (#1985).
- **A clock that moves and a pulse that beats.** Elapsed time derived from the
  newest record's timestamp, so it advanced only when a record arrived and
  froze during exactly the stalls worth timing. A run silent longer than the
  watchdog's kill timeout is treated as abandoned rather than live (#1986).
- **`loaded models`**, naming the primary. The dispatch record already carried
  the resolved model; secondaries read `also loaded` rather than being guessed
  at by size or load order (#1991).
- **`#dispatch=<id>`** replaces `#session=<id>`, with `session=` kept as a
  one-release parser alias and rewritten to the canonical form on arrival.
  Every detail route is now named for the `RunKind` it opens (#1977).

### Fixed

- **Runs → Dispatch → a row now reaches the detail view.** A tracked dispatch
  mints a crew-of-one mission, so it routed to a single-node graph that showed
  less than the detail page and had no click handler (#1996).
- **A signal rendered one character per line on mobile.** A non-shrinking
  sibling starved the detail column to zero width; reported from a phone with
  1013 tests green (#1987).
- **Accessibility pass on the same lens.** The pill and the pulse could
  describe the same run as `RUNNING` and `finished` simultaneously; signal
  severity reached sighted users only; the metric pane names existed solely as
  CSS-generated content; a timestamp failed AA contrast; the pulse was a live
  region that could flap once a second (#1987).
- **A malformed detector payload is named, not mangled** — `undefined` as a
  signal heading, `[object Object]` where structured diagnostic data had been
  (#1992).
- **A malformed or clock-skewed timestamp no longer strands a finished run.**
  Either made a completed dispatch read RUNNING forever, and the first also
  erased the brief that would have explained it. A repaired timeline now says
  so instead of presenting itself as sound (#1993).
- **The detail page polls a live session** instead of fetching once, so turns,
  tokens and signals advance while a run is in flight (#1994).
- **A `StepKind` is asked for its dispatch session** rather than a consumer
  re-deriving it from kind strings with a silent fallthrough, and a conformance
  test pins each kind's answer by value (#1981).
- The runs board's status no longer reads a mission's liveness off a different
  mission's activity clock (#1981).

### Changed

- Denser layout throughout: the run brief flows as a definition grid instead of
  stacking six label/value pairs, and the metric tiles fill their row — a pane
  label spanning every grid track had prevented `auto-fit` from collapsing the
  empty ones (#1996).
- The mobile event list has a floor measured in rows rather than a fraction of
  the viewport (#1996).

[3.3.0]: https://github.com/kstrat2001/darkmux/releases/tag/v3.3.0
[3.2.0]: https://github.com/kstrat2001/darkmux/releases/tag/v3.2.0
[3.1.0]: https://github.com/kstrat2001/darkmux/releases/tag/v3.1.0
[3.0.0]: https://github.com/kstrat2001/darkmux/releases/tag/v3.0.0
[2.12.0]: https://github.com/kstrat2001/darkmux/releases/tag/v2.12.0

## [2.11.0] - 2026-08-26

### Added

- **`darkmux dispatch` envelopes now report what darkmux OBSERVED, not just
  what the model said** (#1955, #1958). The four pathology detectors wrote to
  no orchestrator-reachable channel — not stdout, not stderr, only a trajectory
  file in a temp dir — so a dispatch that tripped a cycle detector returned an
  envelope byte-indistinguishable from a clean one. The envelope now carries
  `detections` (always present, `[]` when nothing fired), `host` peaks, and a
  `checkpoints` reduction.

- **A `crawler` role and a `report_finding` tool** (#1959). The role scans a
  bounded scope for ONE named pattern and records each match structurally
  instead of narrating it in prose. The tool reads the cited line and its ±30
  surrounding lines **off disk itself** and records them alongside the model's
  rationale, so a downstream reviewer judges real source rather than the
  crawler's account of it, and "cite the line" holds by construction rather
  than by later check. Back-pressure runs on two levels: the return value tells
  the model how much budget remains, and a hard cap bounds it regardless.

  A citation that does not resolve — a missing file, a line past end-of-file, a
  quote that disagrees with the line — is REJECTED at report time with the
  actual line handed back, costs no budget, and never reaches an artifact. The
  realistic cause of a mismatched quote is a wrong line NUMBER, and silently
  recording the file's version would attach evidence the model never examined
  to a rationale describing different code.

  Findings are copied into the lab run directory beside the trajectory, and the
  envelope reports `findings: {count, path}`. That block's ABSENCE is
  meaningful: the file is created on the first successful call, so no block
  means the reporting channel was never used.

### Fixed

- **The per-turn-cap salvage no longer dispatches the tool call it truncated**
  (#1961). The #479 salvage counted well-formed tool calls but never filtered
  them. The cap lands mid-serialization, so the last call of a salvaged turn is
  routinely cut to `arguments: ""` — and all of them were dispatched. The empty
  call failing was harmless; the damage was that the unparseable argument
  string stayed in the transcript, and the model host answered the NEXT
  streaming request with HTTP 500. **A recoverable mid-turn truncation became a
  total loss of the run**, several turns later, with a 500 as the only symptom.
  Observed live: a dispatch ended at 67 seconds with no envelope at all.

- **The viewer shows what a tool call actually did again** (#1960). The React
  port kept the filter that depends on the per-activity icon map and left the
  icon map behind, and rendered no `tool_name` or arguments at all — so every
  tool call in the event log read "tool call" and nothing else. Restores the
  glyphs and the row preview (name, arguments, result size, failure marker),
  and adds a glyph for reasoning checkpoints, which the legacy map predates.

- **A live session drill-in no longer freezes** (#1960). The session query had
  no refetch interval and hardcoded itself as historical, so opening a RUNNING
  session fetched once and never again: no new events, and nothing derived from
  them could advance, while the fleet view kept moving. Liveness now comes from
  presence heartbeats.

- **`dispatch --json` no longer reports a container path the caller cannot
  open**, and the Redis startup banner no longer prints the connection address
  (#1957).

### Changed

- **`checkpoints.last_tail_ratio` is replaced by `min_tail_ratio` and
  `mean_tail_ratio`** (#1959). Reporting the LAST measurement did not merely
  fail to signal, it INVERTED: measured on two real crawls, a run that decayed
  through fourteen checkpoints to 0.193, tripped the degeneracy gate, and then
  recovered reported `0.997`, while a clean four-checkpoint run reported
  `0.976`. The degenerate run looked healthier on the field an operator reads
  first. `min` answers "did this run ever degenerate"; `mean` answers "how much
  of it was compromised", and neither substitutes for the other.

  **Breaking, for anything parsing the dispatch envelope:** `last_tail_ratio`
  is gone rather than deprecated, per the pre-1.0 no-compat-baggage policy. The
  only consumer is an orchestrator reading the envelope; nothing in the viewer
  reads it.

## [2.10.0] - 2026-08-24

### Fixed

- **The lab run root now has one resolver, and a `cargo test` no longer writes
  into your real run store** (#1882). Five call sites — `lab run`, `lab run
  list`, `lab run inspect`, `lab notebook draft`, and the review bench —
  resolved the lab root themselves instead of through
  `config_access::lab_dir()`. Two consequences, both live: test builds wrote
  real run directories into `~/.darkmux/runs` (251 had accumulated since July,
  and recent ones rendered as live `RUNNING` rows in the viewer), and
  `DARKMUX_LAB_DIR` / `config.dirs.lab` were ignored on the WRITE side while
  honored on the READ side, so runs landed in a root the reader never scanned.
  `DarkmuxPaths.runs` is now `pub(crate)`, making the bypass a compile error
  rather than a convention.

  **Behavior change, if you set `DARKMUX_LAB_DIR` or `config.dirs.lab`:** those
  verbs now write AND read under the configured root. Runs recorded before this
  release still live under `~/.darkmux/runs` and will not appear in `lab run
  list` until moved. Neither setting is written by `darkmux init` and
  `DARKMUX_LAB_DIR` is undocumented, so most installs are unaffected. The empty
  list now names the directory it actually scanned instead of always claiming
  `.darkmux/runs/`.

### Added

- **A thinking model's turn is no longer discarded when it reasons past the
  per-call bound** (#1221). This is the headline. A model that kept reasoning
  past `max_tokens_per_call` used to have its ENTIRE turn thrown away — a
  measured 43-50% of turns on the review corpus, including one 51K-character
  turn that was tracing real code and naming a real bug when it was cut.

  darkmux now CHECKPOINTS instead: at the bound it hands the model its own
  accumulated output back as an assistant prefill, so the model RESUMES rather
  than restarting, and a distinct-12-gram novelty ratio over the accumulation
  decides whether to hand it back with the think block still OPEN (keep going)
  or CLOSED (conclude from what you have). Many checkpoints in one thinking turn
  remain ONE turn, so `runtime.max_turns` keeps meaning what it says.

  The check-in is SILENT — the model is never told a boundary happened. That is
  not a stylistic choice: a model invited to wrap up wraps up, and measured on a
  real review it produced a four-point summary with zero findings where the same
  model uninterrupted found real ones.

- **`runtime.reasoning_checkpoint_interval_tokens`** — how far the model reasons
  between check-ins (`DARKMUX_RUNTIME_REASONING_CHECKPOINT_INTERVAL`, default
  1000). Deliberately separate from `runtime.max_tokens_per_call`, because the
  two want opposite values: a checkpoint interval wants to be small so a
  reasoning loop is caught early, an answer bound wants to be large so a long
  answer is not chopped. One number serving both is what the split fixed
  (#1221).

### Added

- **`darkmux mission config list` / `show <id>`** — a `role list`/`role show`
  equivalent for the mission-config registry (#1860). `list` enumerates every
  config id across the user → on-disk → embedded tiers `mission launch`
  searches, one row each with name, source tier, phase/task counts, and
  whether it's panel-advertised; a config that fails to load prints as a row
  naming the error rather than vanishing. `show <id>` renders the whole
  graph — every phase, task, and step, whether this binary can construct
  each step's kind (the identical check `mission launch` exits `4` against),
  and, per role, the profile + model it resolves to RIGHT NOW plus the
  resolution's provenance (a launch override, the `role_profiles` map, or
  `default_profile`) and whether that model is currently loaded, reusing
  `darkmux_gestalt::decide_residency` (#1274) for the residency verdict —
  the same ownership + ctx-sufficiency arbiter every real acquire path
  plans against — and `ProfileModel::require_n_ctx` for the same local-model
  gate every dispatch path applies, rather than re-deriving either. `--param
  <role>=<profile>` previews a planned override on the review route only
  (the only route `mission launch` itself applies it on); any other config
  gets the override neutered with a warning naming why, never a false
  parity claim. Read-only end to end; no new data or resolution logic, just
  a surface for what `mission launch`/`dispatch` already resolve silently.
- **Mission-graph parity goldens**: `tests/parity/mission-graph-goldens.spec.ts`
  captures frozen canvas- and timeline-mode text goldens from the standalone
  `/mission/:id/graph` page against a sanity fixture, so the graph lens's
  future port into the React viewer (#1868) has a spec to grade against
  before any of its own code changes. Dev/test infrastructure only; no
  runtime behavior changes. (#1868)
- **The mission graph is now a real lens in the React viewer** — `#mission=<id>`
  renders `MissionGraphLens` in-place (a React Flow canvas on desktop, a
  vertical timeline on phones), replacing the old redirect that navigated
  away to the standalone `/mission/:id/graph` page. Same node/edge/step
  vocabulary, the same live status/token/turn metrics fold, the same
  peer-machine-naming honesty on a 404 — now inside the same app shell as
  every other lens, with its events pane sharing `EventLogColumn` (the
  component every other lens's event log already uses) instead of a
  second, separate implementation. `reactflow` is now a real `ui/`
  dependency (bundled by Vite), matching the pinned version the standalone
  page's vendored bundle already used. (Superseded by the Removed entry
  below, landing in this same [Unreleased] window: an earlier version of
  this entry said the standalone page and its route would stay "unchanged
  in this release... until the port has had a release cycle to prove
  itself." That condition was never met — the port has shipped zero
  release cycles — and darkmux is pre-1.0 with no compat-baggage policy, so
  the retirement lands in the same cycle instead of waiting on one.) (#1868)

### Fixed

  A note on how these were found, because it is the useful part: the runtime
  suite was fully green through every one of them. What surfaced them was
  watching one real 66-call dispatch (which generated 26,181 completion tokens
  and delivered 1,116 characters, starting mid-sentence) and two review passes
  briefed to FALSIFY a named claim rather than walk a checklist.

- **A model that closes its own `</think>` no longer strands its answer**
  (#1221). The region tracker was built on a measured fact — under
  `response_format` the grammar forbids the model from emitting `</think>` — that
  turned out to be narrower than assumed: 17 of the 29 built-in roles declare no
  `output_schema`, and on those (`coder`, `code-reviewer`, `analyst`) the inline
  qwen-3.x family closes its own block freely. Everything after that delimiter
  was filed as reasoning and never delivered, and on a terminal turn the trailing
  scratch plus a dangling `</think>` shipped AS the answer.

- **The deliverable no longer depends on how the run happened to end** (#1221).
  A turn ending on `stop` and the same turn ending at a token cap produced
  different content. One rule now serves both: the answer region when the thought
  was closed (scratch is separable, so it stays out), the accumulation when it
  never closed (it is not separable, and shipping only the last slice is the
  discard-the-turn bug this feature exists to end).

- **A repeating answer is bounded, and is never deleted** (#1221). Degeneracy
  detection was disabled once the thought closed, leaving a post-conclude loop
  with no gate — measured at 337 checkpoints with no terminal reached, backstopped
  only by a SIGKILL that produces no envelope at all. Separately, a degenerate
  verdict used to DELETE the accumulation, and that verdict is wrong for whole
  classes of legitimate output (an enum-valued JSON array, a block of identical
  match arms, an ASCII table frame all score as degenerate). Repetition now stops
  the turn and escalates for handoff with everything banked attached.

- **An empty completion no longer discards the work already banked** (#1221).
  Losing five productive checkpoints because the sixth call came back blank was
  the same bug one layer down. This also covers the `finish_reason=tool_calls`
  with an empty array shape, which popped the message the fold had just written
  the whole accumulation into.

- **A response with no `usage` object no longer kills the dispatch** (#1221).
  It made every checkpoint boundary read as a context overflow, which is a hard
  error — no envelope, no metrics, no deliverable. "Cannot tell" now reads as a
  cap hit.

- **The event stream shows one turn per turn** (#1949). A single long reasoning
  turn wrote one `dispatch.turn` record per API call, so the metrics tile read
  `TURNS 1` while the stream showed 66 — and the stream is what an operator
  reads. Continuations emit `dispatch.checkpoint`, which is what they are.

- **The run card names its machine again** (#1949). The header rendered
  `(<session> on )` with a dangling "on" — a hardcoded stub, not a data gap.

- **"Model only" shows model work while it is happening** (#1945). The filter
  omitted `heartbeat`, so during a long first turn — 171 heartbeats and zero
  per-turn records — it showed an EMPTY list at the exact moment the most was
  happening. `checkpoint` joins it for the same reason.


- **A judge stage that ruled on most of a review's flags discarded all of
  it and posted "the review produced no signal."** A judge whose remote
  token budget exhausted before the whole docket was judged — 123 of 134
  flags ruled, 7 confirmed findings, 67 needs-check, all complete with
  evidence — set the same `degenerate` flag a genuinely dead judge sets,
  because the old gate treated ANY skipped call as fatal regardless of how
  much else was judged. `darkmux mission launch review` now treats a
  judge-stage skip as a coverage fact by default: the flags that WERE
  judged still render (inline comments, the summary fallback, everything),
  with a prominent banner naming the shortfall in the run's own numbers,
  posted as `mode: "partial"` — a CI check that posts and then fails,
  never a silent clean pass. The mission board and `darkmux mission
  status` now agree with what the PR comment says (a partial run reads
  `Degraded`, matching probe/verify exhaustion's existing treatment); the
  flow record's `dispatch complete` payload agrees too, flipping
  `result_class` from `"ok"` to `"partial"` so the same shortfall reaches
  the viewer, the Redis fleet stream, and the hash-chained audit sink,
  not just the posted comment. (The workflow's own CLI exit code was
  already, and remains, unaffected either way — `mission launch review`
  always exits `0`; CI-facing pass/fail has always come from the rendered
  payload's `mode` field, not the process exit status.) An operator who
  wants the old "any skip is fatal" behavior sets
  `review.judge_fail_on_any_skip` (env
  `DARKMUX_REVIEW_JUDGE_FAIL_ON_ANY_SKIP`), surfaced with provenance by
  `darkmux doctor`. Also note: a `judge-pass2`-only exhaustion (every flag
  WAS judged; some confirms were conservatively demoted to needs-check
  because their confirmation pass was skipped) now fails the CI check too
  — previously indistinguishable from a clean run, now correctly `mode:
  "partial"` like a pass-1 shortfall. That's the safe direction (a
  demotion-only run is real, postable signal with a real gap, same as a
  pass-1 shortfall), but it does change when the check goes red on a run
  where every flag was judged. (#1876, #1877)

- **The machine page's fit projection believed a number it had already
  disproved.** `potential` is the contract "the most this resident will ever
  hold", and it can be wrong: an idle MLX resident measured 28.40 GiB against
  a priced 22.88 GiB, steady to the byte across repeated samples, with the
  estimator's own arithmetic verified exact from the model's `config.json`.
  The projection summed the prices, so the fit figure was optimistic by the
  whole overage — in the one direction that makes an operator load another
  model. It now counts `max(potential, current)` per resident, and says so:
  the row carries a footnote naming the overage and what the projection now
  counts, and a warning carries both figures, because a silently-corrected
  estimate is one nobody ever fixes. (#1854)
- **The shrink hint promised savings a context reduction cannot deliver.**
  Cutting a resident's `ctx` lowers its price, but the projection floors every
  row at its measured footprint — so once the shrunken price drops under what
  the model is already holding, further cutting reclaims nothing. A fixture's
  hint promised 4.70 GB and delivered 4.12 GB. Found by running the hint and
  recomputing, not by reading it; the rounding cushion in the suggested `ctx`
  had always absorbed the difference. (#1854)
- **The margin tile printed the word "margin" twice** — `92 % margin` above a
  `MARGIN` label. A leftover from the #1821 rename, where the unit had read
  `% free` against that label and did not collide. Now a bare `%`, matching
  its two siblings where the unit is a unit and the label is the subject.
- **An unreadable figure could render as nothing at all.** The center readout
  shows `—` when neither memory source can be read; the new seven-segment
  cells had no glyph for it, so absence drew a blank hub instead of being
  visible as absence.

### Changed

- **The machine gauge no longer renders a verdict.** The `machine total
  GREEN` chip, and the fill's green/amber/red buckets at 50% and 85%, are
  gone. Both interpreted data the reader can already see, and the buckets'
  edges were thresholds darkmux invented — a machine at 84% and one at 86%
  are not different in kind. What remains is the arc, the needle, the limit,
  and the figures. The lamp row still reports server-declared *conditions*
  (pressure, over-limit, unpriced), which are facts rather than an assessment
  of whether the machine is doing well. Extends #1839's rule from `doctor` to
  the page: darkmux describes its own state; the reading is yours.
- **The gauge's color ramp is painted across the arc's sweep** — green at 0,
  amber at mid-scale, red at the limit — fixed to the dial and identical on
  every machine and every poll, with the filled band revealing its own slice.
  A band's color travel therefore also states its width. The stops are
  cosine-spaced, because a horizontal gradient interpolates along X while an
  arc advances by angle, so the mid-scale color lands on the mid-scale tick.
- **Seven-segment readouts** replace the boxed odometer digits on the center
  figure and the three pressure tiles. Boxed cells quote a mechanical counter;
  seven-segment quotes an instrument, which is what the rest of this page
  already is. Drawn as polygons rather than an embedded font, so the unlit
  segments render too — that ghosting is what anchors a narrow `1` in its
  cell. The pressure tiles carry a visually-hidden text copy of each figure,
  since the glyphs are decorative shapes.

- `LEDGER_SCHEMA_VERSION` **2.0 → 2.1** (minor, additive): `ModelRow.
  over_price_bytes` and `MachineTotals.over_price_models`. A 2.0 reader
  tolerates the payload unchanged, and a leniency test pins that a payload
  missing both keys still parses — a real path on a fleet where one machine is
  a release behind. `MachineTotals.potential_bytes` is now summed as
  `max(potential, current)` per resident: a value change inside an unchanged
  field, and the fix above.
- `FLOW_SCHEMA_VERSION` **1.19.0 → 1.20.0** (minor, additive): a new
  `"step timing"` action (#1877's final wiring step). The scheduler now
  streams one companion flow record per step, live, for every mission that
  runs through `run_step_graph`, including coder-phase, with no change to
  its own module. A pre-1.20.0 reader ignores the unknown action entirely;
  no struct/field change on any existing action. One transient
  fleet-visible effect: until every machine has upgraded past this build,
  `darkmux flow status` / `darkmux doctor` reports `schema_skew_detected`
  (the same live-stream version comparison every prior
  `FLOW_SCHEMA_VERSION` bump has produced) until the whole fleet catches
  up. Note the comparison is symmetric: `live_foreign` is any observed
  version that differs from the running binary's, in EITHER direction, so
  the warning appears on the machine that has NOT upgraded too. One
  upgraded machine writing a single 1.20.0 record to a shared Redis stream
  is enough to flip a still-on-stable peer's flow-sink health from Pass to
  Warn. The hint says to upgrade the lagging writer without naming which
  peer, so from the lagging machine's own seat the message reads as
  pointing at itself. It is a Warn, never a Fail, and it clears once the
  fleet is on one version.

### Removed

- **The legacy viewer (`crates/darkmux-serve/assets/viewer.html`, 319 KB)
  is deleted** (#1806), completing the UI transition #1800/#1804 started.
  Nothing served it after the route flip (#1800) moved `/` and
  `/play/:date` onto the React port; it survived only as the parity
  harness's extraction source and the reference the port's remaining 11
  `test.fixme`s (#1806's own list) named as blocking its removal. All
  eleven are now built and passing on the port — the filters/notes/about
  modal system and its focus trap, the machine lens's memory-ledger bars,
  a clickable affordance into a session view, the lab-run detail
  fallback-to-list, the lifecycle-drill tail, and the XSS walk's
  previously-unreachable surfaces — closing the gap #1806 measured.
  - `tests/parity/`'s legacy extraction path retires with it:
    `extract.spec.ts`, its dedicated `playwright.config.js` (served
    `viewer.html` with `darkmux-mode=live` injected), `redprove.spec.ts`,
    `verify-goldens.mjs`, and `determinism.mjs` are deleted, along with
    the `extract`/`rebaseline`/`verify`/`redprove`/`determinism` `package.json`
    scripts. **`goldens/*.txt` survives as a FROZEN spec** — what the
    legacy viewer actually rendered against a real daemon, captured once
    and now locked in — and the `next-parity*` suites keep grading the
    React port against those same files, unaffected. `record.mjs` and
    `tripwire.mjs` (now aliased as `check`) remain for capturing and
    scanning `corpus/` fixtures. Rebaselining a golden is no longer a
    regeneration script; it is a direct hand-edit of `goldens/<lens>.txt`
    in a reviewed diff, checked against real port output.
  - `crates/darkmux-serve/src/lib_tests.rs`: of the five Rust tests that
    `include_str!`'d `viewer.html`, one (`viewer_has_no_inline_event_handlers`,
    the general "no inline `on<event>=` HTML attribute" XSS guard) is
    retargeted at `next.html` — its premise holds for any served document,
    and React's synthetic event system emits no such attributes, verified
    empirically before the retarget (it is blind to an escaped-quote
    `onerror=\"…\"` inside a JS string, or a no-leading-whitespace
    `{onclick:"…"}` object literal — its practical value is guarding
    `ui/index.html` shell regressions and a stray `dangerouslySetInnerHTML`,
    not a general XSS proof). The other four
    (`viewer_has_no_raw_record_interpolations`, `live_tail_dedups_records`,
    `savings_hero_breakdown_is_classed_and_currency_free`,
    `wt_sum_panel_is_live_gated_and_escaped`) asserted on exact legacy
    source text — function names, variable names, hand-written `${...}`
    template syntax — that has no analog in a bundled, minified React app,
    and are deleted rather than retargeted. XSS/escaping coverage for the
    port lives in `tests/e2e/viewer-xss.spec.js`; live-tail dedup and the
    tokens-only savings-hero copy get their own port-shaped regression
    coverage instead (`ui/src/lib/flow.test.ts`'s `buildFlowWindow dedup
    (#794)` suite; `ui/src/lenses/fleet/FleetLens.test.tsx`'s tokens-only
    hero test). The wt-sum panel is **not ported** — `ui/src` has no
    consumer of `GET /worktree-summary/:session_id` at all (the daemon
    still routes it; nothing in the React port calls it), so there is no
    port-side behavior for a test to cover.
  - The legacy file's source is recoverable with
    `git show v2.9.0:crates/darkmux-serve/assets/viewer.html`.
- **The standalone mission-graph page is deleted** (#1868 third packet),
  completing the arc this same release's mission-graph lens (Added, above)
  started: `crates/darkmux-serve/assets/mission-graph.html` (~2,100 lines of
  hand-written `React.createElement` JS) and its vendored bundle
  (`crates/darkmux-serve/assets/vendor/` — React + ReactDOM + reactflow as
  one minified IIFE, plus the upstream MIT `LICENSE-*` files) are gone.
  Their MIT notices already travel with the artifact that reaches users
  independently of that directory: `ui/vendor-licenses/LICENSE-reactflow`
  (added alongside reactflow becoming a real `ui/` dependency) is prepended
  into `next.html` the same way react/react-dom/@tanstack/react-query's
  notices already were, verified byte-identical to the deleted copies
  before removal.
  - `GET /mission/:id/graph` is now a **308 permanent redirect** into the
    port's own `#mission=<id>` hash route (`/#mission=<id>`) rather than a
    second HTML document — every bookmark and shared link minted against
    the old path still lands on the mission's graph, now rendered inline by
    `MissionGraphLens`. `GET /mission/:id/graph.json` (the data endpoint)
    and the `/vendor/reactflow-bundle.min.{js,css}` routes: the JSON route
    is unchanged; the vendor routes are deleted along with the bundle they
    served.
  - `tests/e2e/mission-graph-*.spec.js` (8 specs) are deleted — superseded
    by the `mission-lens-*.spec.js` suite #1871 already shipped against the
    ported lens (plus one new mobile-legend spec with no legacy analog).
    The `.served/mission-graph.html` + `/vendor/*` harness wiring in
    `tests/e2e/playwright.config.js` that existed only to serve those specs
    is removed with them; the lens specs already run against the port's
    own `index-live.html`.
  - `tests/parity/mission-graph-goldens.spec.ts` (the capture suite that
    recorded `goldens/mission-graph-{canvas,timeline}.txt` from the live
    standalone page) is retired, its own config and `package.json` script
    removed — same shape as `viewer.html`'s own extraction harness
    retirement above. **The two goldens it captured survive as a frozen
    spec**, same as `viewer.html`'s: `next-parity-graph.spec.ts` (shipped in
    this same release, see Added above) keeps grading the ported lens
    against them, byte-for-byte on the DOM regions kept identical on
    purpose. Rebaselining either is now a direct hand-edit, not a
    re-capture. Recoverable with
    `git show v2.9.0:crates/darkmux-serve/assets/mission-graph.html` and
    `git show v2.9.0:tests/parity/mission-graph-goldens.spec.ts`.

## [2.9.0] - 2026-08-16

A remediation release. An audit of every user-facing surface asked one question
— does darkmux describe its own state, or does it render verdicts on yours? —
and found that in several places it did the second. It also found 323 KB of
MIT-licensed code shipping with no attribution. Those are the release.

### Fixed

- **`darkmux config get <secret>` told you a secret was not in your config
  without opening the file.** It short-circuited on the known secret keys and
  answered "darkmux never stores it in config.json". That claim was made
  without looking, and it is false in a reachable state: `config set` refuses
  to write these keys, but a hand-added one is preserved verbatim by the
  lenient whole-file writeback on every subsequent `set`. So the one command
  you would run to check whether a secret leaked into your config actively
  reassured you it had not. It now reads the file and reports presence either
  way — and when the key IS there, says to remove it and treat the value as
  exposed.
- **`darkmux config list` printed it.** A hand-added secret went straight to
  the terminal, defeating the entire point of the Keychain carve-out. Secret
  keys now render as `(redacted …)`; the KEY still shows, because its presence
  is exactly what you need to know. Fixed in the shared reader, so the radio
  answering seat's config grounding is covered by the same change.
- **`darkmux doctor` stopped adjudicating your setup.** Three strings went:
  "Safe as-is for a single machine" (conditionally true, and false for the
  reverse-proxy setup the guide recommends — the check never looked at the
  proxy), "Password-less is fine for a local/Tailnet-trusted Redis" (a verdict
  on an unverified condition, in the hint of a Warn, i.e. telling you the
  warning was safe to ignore), and a clause volunteering a compliance
  interpretation of a dropped audit write. What each check reports about
  darkmux's own configuration is unchanged.
- The `serve daemon auth` check is renamed **`serve daemon token`**. Both arms
  return Pass by design — loopback-only with no token is the ordinary
  single-machine state — but a check that named a security concern while being
  structurally incapable of any other status read, inside `● ok — every check
  passed`, as a security check that had cleared. It never checked that.
- **The always-on hub guide was wrong in two places.** It said a plain LAN
  substitutes for Tailscale; it does not — the password-less Redis posture in
  that guide depends on the network being an authentication boundary, and a
  reader following it would have ended up with an unauthenticated Redis on
  whatever network the machine joined. And it recommended enabling automatic
  login under a "harden the OS" heading, which is a security-weakening step
  (it defeats FileVault across a reboot) presented as hardening.
- The Homebrew formula's caveats claimed `flow integrity-check` surfaces "any
  post-hoc edit". It does not, by SECURITY.md's own account — tail truncation
  and whole-file deletion are undetectable. That text prints in
  `brew info darkmux`. `SECURITY.md`'s supported-versions table also still
  said `1.x (current)`.

### Legal

- **The built viewer ships the MIT notices for the code it embeds.** `next.html`
  bundles react, react-dom and @tanstack/react-query and is compiled into the
  binary, served at `GET /`, and republished on the website — and it carried
  **zero copyright notices**, because the minifier strips `@license` banners.
  MIT requires the notice to travel with copies. The notices are now prepended
  at build time from vendored license texts, and the build FAILS if that
  directory is missing rather than silently shipping unattributed code. The
  mission-graph bundle had the mirror-image gap — React's banners present, no
  React Flow notice at all — now fixed, with the prepend written into its
  rebuild recipe as a named step.

### Added

- **A peer's darkmux version now rides the presence heartbeat**, so it is
  readable over the shared Redis without that peer's HTTP daemon being
  reachable at all. Presence already carried the flow-schema version for the
  same reason; this is the other half of the same question, and it was the
  half that went missing exactly when it was most wanted — a hub that is up
  and heartbeating but that nothing can reach. A peer on an older build
  reports no version rather than failing to parse.


## [2.8.0] - 2026-08-15

The machine page's numbers were wrong, and now they are not. Per-model memory
was read from a counter (`ps rss`) that does not count MLX weights at all, so
the gauge reported **271 MiB held while three loaded models held ~25 GiB** —
understated roughly 97×, with a green verdict beside it. That single defect,
and everything it touched once the real number was visible, is almost the
whole of this release; the gauge itself was also redrawn twice along the way
as each fix changed what there was to look at.

### Breaking

- **`LEDGER_SCHEMA_VERSION` 1.1 → 2.0** (#1821). Anyone parsing
  `/machine/resources` or `darkmux machine resources` output directly is
  affected:
  - **`warnings: string[]` is replaced by `messages: {severity, text}[]`**
    (`info` / `warn` / `error`). The old field rendered every entry the same
    amber regardless of whether it was a real degradation or a note about how
    a figure was derived; severity is now explicit instead of implied by
    which field it landed in.
  - **`pool.available_bytes` changed meaning.** It used to be the truly-free
    page count (`vm_stat` "Pages free"); it now means the colloquial "how much
    is left" (free + inactive + speculative). The old meaning is preserved
    under a new name, **`pool.free_bytes`** — if your integration wants the
    old number, read that field instead.
  - **`pressure.memory_free_percent` is renamed `pressure.margin_percent`.**
    It was always a 0–100 kernel pressure reading (`kern.memorystatus_level`),
    not a byte count, and the old name read as one next to the pool's byte
    figures.
  - **New, additive fields**: `pool.used_bytes` (Activity-Monitor-style: wired
    + compressor + (active + inactive − purgeable)); `machine.other_used_bytes`
    and `machine.projected_total_bytes` (what everything *besides* darkmux is
    holding, and what the machine would total if darkmux's own
    committed-but-unmaterialized models fully load); `potential_source` per
    model (`"arch"` for a measured estimate, `"estimated"` for the size-tiered
    fallback below, omitted when a model has no potential at all) and
    `machine.estimated_models` (counted separately from `unpriced_models` —
    an estimated resident is priced and does not block a green verdict; only
    a resident with no arch facts *and* no catalog size still forces the
    machine to unknown).

  The new fields are purely additive and ignorable by an old reader.
  `warnings` and `memory_free_percent` are gone from the payload outright — a
  reader keyed on those exact names gets nothing back and needs to move to
  `messages` and `margin_percent`. `available_bytes` is the sharper case: the
  field is still present under the same name, but its **value now means
  something different** — a reader that kept using it silently gets the
  colloquial figure instead of the truly-free one it was reading before. Move
  to `free_bytes` for the old meaning.

### Added

- **An unpriceable resident gets an estimate instead of blocking the machine's
  verdict forever.** Pricing a resident needs its `config.json` (hidden layer
  count, KV heads, head dim); a GGUF download carries that architecture inside
  the binary instead of a sidecar file, so one such resident — even a small
  one — forced the *entire* machine's fit verdict to `unknown` permanently,
  regardless of how comfortably everything else fit. A fallback estimator now
  prices those residents from catalog size alone, selecting a KV-cost rate
  tiered to size (larger dense models get a higher per-token rate, since a
  flat rate under-reserved exactly the large downloads most likely to hit this
  path). The estimate is disclosed everywhere the verdict appears — a dashed
  `ESTIMATED` chip on the model row, an `info`-severity message, the CLI table
  and its `~`-prefixed POTENTIAL column, and a new `darkmux doctor` check
  naming any resident that is still genuinely unpriceable after the fallback
  (no arch facts and no catalog size), with the fix (load an MLX build of the
  same model when one exists). **Known limit, stated rather than hidden:** the
  size-tiered rate under-reserves pre-GQA multi-head models such as
  Llama-2-13B, where KV-head count equals attention-head count instead of a
  small fraction of it — no size-derived rate can catch that shape. The
  estimator is now the *second* fallback rather than the first: see the GGUF
  header reader below. (#1819, #1823)
- **A GGUF resident is priced from its own architecture, read out of the
  binary.** The estimate above is a floor, not an answer — so darkmux now
  parses the GGUF metadata header directly for the same three facts a
  `config.json` would carry. Resolution order is `config.json` → GGUF header
  → size-tiered estimate → genuinely unpriceable, and a GGUF-derived row
  reports `potential_source: "arch"` because it is a measurement like any
  other. Verified against a real 9 GB `phi-4-Q4_K_M.gguf`: 40 layers, 10 KV
  heads, head dim 128, matching the published config exactly. Only the header
  is read, never the tensor data — parsing that file costs ~7 ms. It also
  prefers the header's own `key_length` over deriving head dim from embedding
  size, which is what keeps models like gemma-4-E4B correct (its derived
  value would be 320 where the true one is 512). Every file-supplied length
  and count is bounds-checked before it can allocate or loop, and any
  malformed, truncated or ambiguous file declines to a labeled estimate
  rather than failing. **Known limits:** GGUF v1's differing wire format is
  unsupported (v2/v3 only); GGUF carries no per-layer attention-pattern
  field, so hybrid-attention models are assumed dense — an overprice, the
  same safe direction the estimator chose. (#1820, #1831)
- **The viewer's dialogs are back: filters, notes, and about.** The React
  viewer shipped without them — the event log offered a one-shot "model only"
  quick filter in place of the real checkbox-per-facet modal, a named cut
  rather than a half-build. All three dialogs now exist on a shared shell with
  managed focus: Tab cannot walk out of an open dialog, Shift+Tab wraps,
  Escape closes it, and focus returns to whatever opened it. The old viewer
  failed the first of those — 31 Tab presses from an open filter panel landed
  on the page header, underneath an opaque backdrop, so a keyboard user was
  operating controls they could not see. Session and mission drill-in routes
  land alongside them. (#1640, #1829)

### Changed

- **The machine page reads in binary GiB, so its numbers match the machine you
  bought** (#1811). A 128 GB MacBook Pro was rendering its own memory ceiling as
  `137.44 GB`, and the gauge inherited it — labeling its arc `0 · 34 · 69 · 103
  · 137` on the one screen whose whole job is telling you how much room you
  have. Every figure in the memory ledger and on the gauge face now divides by
  a power of two and is labeled `GiB`/`MiB`: the arc reads `0 · 32 · 64 · 96 ·
  128`, the pool reads `128.00 GiB`, and the ` (128 GiB)` parenthetical that
  used to patch the mismatch is gone along with it. The stage header's own RAM
  figure keeps its `GB` label for now — it was always computed in binary, so
  it now agrees numerically; only the suffix still differs.
- **The gauge is a stacked band, not a single needle over one number.** It
  used to fill from darkmux's own committed memory alone, against a scale
  ending at the machine's *whole* RAM — so a near-empty darkmux on an
  87%-full machine still read green, because the fill never accounted for
  what anything else on the machine was holding. Two intermediate redesigns
  (a color-only fix, then a pair of concentric rings) were each superseded
  once the real problem was visible: the dial now stacks darkmux's own
  memory, everything else on the machine, and darkmux's committed-but-not-yet-
  materialized growth (hatched) in one band, so the sum — the actual "will it
  fit" question this page exists to answer — is legible at a glance instead
  of requiring cross-radius mental arithmetic. The needle lands on the
  machine's real current usage; the center readout shows that same figure
  (labeled `MACHINE USED`), not darkmux's share of it; the fill color follows
  the machine's overall state, not darkmux's alone; and a legend pairs each
  band with the figure it represents. The always-on `IN USE` caption is gone
  — the caption now appears only when it has something to say (which disjunct
  put the machine in red), the same way the per-row state chip and the
  now-deleted `darkmux/utility` card were quieted.
- **The `darkmux`/utility block moved, then was deleted outright.** It first
  moved from the top of the page (config given priority it hadn't earned)
  to below the ledger, then — on a closer look — was cut entirely: it
  described what the utility tier is *responsible for*, a property of
  configuration rather than of this machine's memory, and every fact it
  carried already existed elsewhere (the model's own ledger row, or
  `darkmux doctor`). What survives is a single neutral `utility` badge on
  that resident's own ledger row. (#1818)
- **The pressure tiles' explanatory notes are behind an `(i)` popover** instead
  of always-on 8.5px text at the bottom of the page — readable on request
  instead of illegible by default, and rewritten while there: the memory-free
  reading is now named as the only figure that can trigger red, and is a
  0–100 pressure reading rather than a byte count; the compressor note now
  spells out that it is macOS's own memory compressor, not darkmux's
  compaction. The `STATE` lamp — a second, less-informed copy of the verdict
  the machine chip already carries — is deleted. (#1822)
- **The per-row `UNKNOWN`/`ESTIMATED` state chip only renders where a row's
  state actually disagrees with the machine's overall verdict**, instead of
  stamping every row with the same word regardless of whether that row was
  the reason for it. On a healthy or uniformly-unknown machine, no row
  carries it at all. (#1818, #1819)

### Fixed

- **Per-model memory now reads what a model actually occupies.** `ps rss`
  does not count MLX model memory at all — MLX places weights in
  Metal/IOAccelerator buffers, which only `phys_footprint` sees; llama.cpp's
  GGUF weights are memory-mapped as evictable file-backed pages, which only
  `rss` sees. Neither counter alone is correct for the mix of backends
  darkmux actually runs. Per-worker memory is now `max(rss, phys_footprint)`,
  with both raw figures kept in the payload so the number is checkable rather
  than trusted. Workers are also now paired to models by weight size rather
  than by projected potential (potential includes context that may not be
  materialized yet, which could rank two residents in the wrong order and
  swap their reported figures).
- **The machine's memory is decomposed into real, distinct quantities instead
  of calling three different things "free."** There was no machine-wide
  "used" figure at all, so the operator's own read of darkmux's usage stood
  in for the machine's; the truly-free-pages percentage and the kernel's own
  pressure-margin percentage — both plausibly "how much is free" — differed
  by 51 percentage points (31% vs 82%) on the same screen, for the same
  machine, at the same instant. See **Breaking** above for the field-level
  detail.
- **The fit verdict now accounts for everything else running on the machine**,
  not just darkmux's own commitment against the machine's total capacity. A
  machine with tens of GiB held by other processes could previously read
  green as though those processes did not exist; the green/amber cascade and
  the amber shrink-hint's own arithmetic now key off the projected total
  (everything else, plus darkmux's own commitment), not darkmux's commitment
  alone.
- The gauge's aria label no longer announces a fabricated "0% full" when the
  current reading is unreadable — it now reports the reading as unreadable,
  the same thing the visible dial does.
- A popover tile note no longer pushes the rows below it down the page when
  opened, and the gauge's needle no longer stops short of the band it is
  meant to point at.

## [2.7.0] - 2026-08-14

The release where **the new viewer becomes the viewer**. `/next` graduated: the
React port now serves `/` and `/play/<date>`, it runs with no daemon behind it
at all, and the machine page stopped being a wall of numbers.

### Added

- **The React viewer is now what darkmux serves.** `GET /` and
  `GET /play/<date>` render the port; `/next` becomes a permanent redirect to
  `/` rather than disappearing, so every bookmark, phone shortcut and tailnet
  link minted during the port keeps working. The gate was a number, not a
  judgement: 21 of 22 goldens recorded from the legacy viewer asserting real
  byte parity in a real browser. (#1800)
- **The viewer runs with no daemon.** It reads the static-source metas a
  daemon-less build injects (`darkmux-flow-src`, `-runs-src`, `-lab-runs-src`),
  parses the committed flow file directly, and suppresses every live poll —
  which is what makes darkmux.com/demo the real viewer rather than a fork of
  it. The demo now also DERIVES which viewer it ships from the daemon's own
  source instead of naming a file, so it can never again silently lag the flip.
  (#1801)
- **The machine page is an instrument.** It became the residency room it was
  always described as — its runs list moved to the runs lens — and gained a
  real gauge: a semicircle reading current against the limit, a tell-tale lamp
  row, odometer cells for the monotonic pressure counters, and a redline keyed
  on the server's own state rather than a threshold invented in the browser.
  Unpriceable models render with **no** committed extent rather than a
  zero-width one, and `unknown` is a designed state instead of a blank.
  (#1806, #1809)
- **The runs lens takes a machine pin** — `#lens=runs&machine=<uid>`,
  composable with the kind filter, with a clearable chip naming the machine.
  A fleet card for a remote machine now drills straight there. (#1508, #1809)

### Fixed

- **A machine's identity is its uid, not whichever name it logged under.** One
  machine carries several `machine_id`s over its life — the hostname's short
  and `.local` forms, or a rename — so every check that asked "is this machine
  me?" by matching names failed on a machine with two aliases. It classified
  itself as remote, hid its own residency ledger, advised viewing the machine
  page on the machine you were already using, and reported "hardware not
  reported" for its own CPU. Four sites, one rule. (#1809)
- **A daemon-less build no longer polls a daemon.** The static demo opened an
  SSE stream and hit `/fleet/machines/live` every five seconds indefinitely,
  showing "reconnecting" on a page with nothing to reconnect to. (#1801)
- **The machine page's runs link no longer claims a count it cannot know** — it
  read "0 runs" while its destination listed 282, because the two counted
  different things over different windows. (#1809)
- **A stale reading survives an unreachable daemon.** An errored poll discarded
  the last good snapshot, so the stale banner was unreachable code and the
  figures vanished exactly when the daemon blinked. (#1812)
- **A navigation that changed destination as data loaded.** The local fleet
  card briefly routed to the runs lens before specs resolved — a blink on
  loopback, longer over a tailnet, and silent either way. (#1809)
- **The demo's icons and manifest 404'd under its subpath**, and the machine
  page printed `hw.memsize` twice in two byte conventions (`128 GB` and
  `137.44 GB` are the same number). (#1811)

### Notes

- The end-to-end viewer suite now grades the **shipped** viewer rather than the
  legacy one it was written against — 71 passing, with 8 kept as `test.fixme`
  naming behaviors the port does not have yet rather than deleted (#1806).
  The legacy `viewer.html` deliberately stays on disk as the reference
  implementation for exactly those, and is unreachable at runtime.
- `FLOW_SCHEMA_VERSION` is unchanged at 1.19.0 — no cross-machine schema lock
  needed for this upgrade.


## [2.6.0] - 2026-08-12

The release where you can **talk to darkmux**. Two new front doors — an agent
panel inside your editor, and plain-English routing onto your own commands —
plus a rebuilt viewer and a materially harder audit chain.

### Added

- **radio — say what you want instead of memorizing verbs.** `darkmux radio
  "what PRs are open"` routes free text onto exactly ONE advertised command and
  runs it, printing the route it chose before executing so the choice is never
  silent. Two seats do the work: a small local model classifies, a larger one
  answers when nothing matches. A message that doesn't clearly map onto one
  command **refuses and lists what's available** rather than guessing — a wrong
  refusal costs you a step, a wrong route runs the wrong command. `--dry-run`
  shows the route without executing. (#1698)
- **darkmux as an ACP agent — the Zed agent panel.** Your commands appear as
  slash commands in the panel, run as real missions, and stream back into the
  thread. Includes an operator sign-off gate on steps (fail-closed by default),
  cancellation, session pruning, and an idle exit. (#1388, #1684)
- **PR-flow panel verbs.** Author `/pr-list`, `/pr-view`, `/pr-comments-list`,
  `/pr-comment-resolve`, `/pr-approve`, `/pr-merge`, `/pr-ship` as ordinary
  mission configs: a per-verb `gh` allowlist (`gh.enabled` + `gh.allowed`, both
  fail-closed), a sign-off dialog carrying real CI and review facts, and a flow
  record for every executed verb. darkmux holds no GitHub credential — every
  verb shells out to your own `gh`. (#1685)
- **The viewer, rebuilt in React, behind `/next`.** Every lens ported with a
  parity harness as its executable spec; the legacy viewer at `/` is untouched.
  Adds a drill-in level: machine detail (local and remote), lab-run detail, and
  a per-session run view. Plus a staleness marker, so an empty panel is never
  silently empty. (#1665)
- **`flow integrity-check --strict`** — exit 3 when a file could not be
  content-verified at all, kept distinct from exit 2 (a real chain break).
  "Verified" and "could not verify" are different claims, and a cron keyed on
  the exit code can now tell them apart. (#1775)
- **A second reference bundler plugin** (`darkmux-bundler-edge`) alongside the
  `--bundler` escape hatch, for reviewing languages the built-in TypeScript
  bundler doesn't read. (#1686, #1757)

### Fixed

- **The review bundler silently dropped code from its excerpts.** Changed lines
  with no enclosing function were skipped entirely, so review seats reasoned
  about a window that was missing the code under review — and reported honestly
  about what they were shown while the pipeline promoted it into claims about
  the file. Also fixes a second, hidden size cap that stopped large functions
  from ever being located. (#1751–#1756)
- **The audit chain now hashes the stored bytes** rather than a re-serialization
  of a parsed record, closing a confirmed bypass where a record carrying an
  unrecognized enum value skipped content verification entirely and its other
  fields could be rewritten while the chain still validated. (#1768, #1769)
- Mission board is recent-first by default. (#1713, #1717 for radio's grounding)
- Viewer mission/run aggregations read the whole fleet stream, not only the
  local machine. (#1705)
- Accessibility and styling passes on `/next`, including keyboard-operable
  controls and a real identity (favicon, touch icon, manifest).

### Changed — read before upgrading

Both of these degrade gracefully: nothing errors, and no action is required.

- **The audit hash format changed** (`prefix-blake3-v1`). Files written by
  2.5.x and earlier are still READ, and reported honestly as legacy — but their
  content is not re-verified, because recomputing the old format would repeat
  the lossy round trip that made the bypass possible. `--strict` (above) is how
  you make that visible to automation. (#1772)
- **The orchestrator provenance field is removed** — the `DARKMUX_ORCHESTRATOR`
  env var, the `orchestrator` config field, and the flow-record field. It was
  stamped from machine-scoped config to describe an invocation-scoped fact, so
  every record on a machine carried the same value regardless of what actually
  drove it, and nothing read it. A stale export is now a no-op; an old config
  still loads. (#1758)

### Documentation

- README rewritten as a landing page (8,716 words → 978, nothing deleted).
- A full guide page for radio, and one for the PR-flow verbs.
- **Every audit and privacy claim a user actually meets was re-checked against
  the code and corrected.** darkmux describes what its mechanisms do and names
  their known gaps; it makes no claim about anyone's compliance with any
  regulatory framework. See `SECURITY.md` for the audit chain's limits, stated
  plainly.
- The MIT disclaimer now names both copyright holders, so it covers the
  distributor as well as the author.


### Added

- **`darkmux-bundler-edge`** — the second reference `--bundler` plugin: zero-dependency Python, Edge.js template spans + differential facts (interpolations, directives, class-attribute churn) + cross-template manifests, proving the frozen `--bundler` contract (#1319) at N=2 — a second language, for a template DSL rather than a systems language (#1686).
- **PR-flow panel-verb machinery** (#1685): a per-verb `gh` allowlist (`config.gh.{enabled,allowed}`, `darkmux doctor` provenance, `MissionConfig.gh_verb`), a flow-record audit entry (`action: "gh.verb.executed"`) per executed verb, and `--param args=<value>` now delivers into a config's `reads: ["__panel_args__"]` task from a direct `darkmux mission launch <id>` the same way it already did from the ACP panel (previously CLI-only launch hard-failed `interpret` for any config using that convention). GitHub never enters darkmux core: `pr-list`/`pr-info`/`pr-approve`/`pr-merge` are operator-authored example `procedural.shell` configs documented in the new [PR-flow guide](docs/guide/pr-flow.html), not built-ins.

## [2.5.1] - 2026-08-11

Hotfix off the v2.5.0 tag. The review bundler was silently dropping code from
the excerpts it hands to review seats, so reviews were reasoning about a window
that omitted the very lines under review.

### Fixed

- **Top-level changed lines reached a seat instead of vanishing.** A changed
  line with no enclosing function was skipped entirely when building the
  excerpt, so imports, constants, type aliases and module-level statements
  never reached a reviewer. (#1751–#1756)
- Unchanged context lines are no longer bundled as though they were top-level
  code, which had the inverse effect of padding excerpts with untouched lines.

## [2.5.0] - 2026-08-06

The honesty release. Nearly every fix here is one defect wearing different
clothes: **something was lost or wrong, and nothing said so.** A dead run that
read `running` forever. An errored dispatch that reported `ok: true`. A green
review with zero findings and no explanation. A config knob that was settable,
typed, documented, and read by nothing. A fleet boot test that takes 63 seconds
when it actually runs, reporting `ok` in eight milliseconds. The tests meant to
catch these had their own version of the same problem — passing for reasons
other than the ones they named.

### Breaking

- **Mission-config schema 2.0 — the `expand` primitive is retired** (#1550).
  Its only consumer moved to explicit per-role tasks in 2.3.0, leaving a
  declared, documented, unfeedable field. Removal is the breaking part: because
  `TaskConfig` carries a `#[serde(flatten)]` overflow, a config still declaring
  `expand` would have **parsed perfectly and silently lost its fan-out** — no
  error, a graph quietly missing tasks. `validate` now emits an **Error**
  naming the field, the schema that removed it, and the migration. Lenient on
  read, loud at validate. See **Migration** below.
- **A dangling `role_profiles` binding is now an error** (#1547). It used to
  fall through to `default_profile` in silence, so a typo'd role id bound
  nothing and looked fine.
- **The serve daemon's auth gate fails closed** (#1663). A request that carries
  no peer address is treated as **remote** (token required), not as loopback.
  It previously did the opposite, which meant the entire remote gate rested on
  a single wiring call in `run()` guarded by nothing but a comment: downgrade
  it in any refactor and every peer looks like loopback, serving flow records,
  machine specs, mission state, and worktree summaries unauthenticated — with
  every test green and nothing visible to the operator, because the viewer
  keeps working either way. That same refactor now produces 401s on the first
  remote request instead of silence. **Loopback-only installs are unaffected**
  — with no token configured the gate isn't in the stack at all.

### Migration — mission configs

After upgrading, `darkmux doctor` warns once per user-tier mission config whose
`schema_version` is a 1.x major. The configs still **load and run**; the warning
is about interpretation drift, not breakage. Two cases:

1. **Your config does not declare `expand`** (the common case — it had no way to
   be fed since 2.3.0). Migration is the version line alone:

   ```bash
   # what still needs migrating
   grep -L '"schema_version": *"2' ~/.darkmux/mission-configs/*.json

   # any config that genuinely uses the retired primitive — handle these by hand
   grep -l '"expand"' ~/.darkmux/mission-configs/*.json

   # bump the rest
   for f in ~/.darkmux/mission-configs/*.json; do
     grep -q '"expand"' "$f" && { echo "skip (declares expand): $f"; continue; }
     python3 - "$f" <<'PY'
   import json, sys
   p = sys.argv[1]
   d = json.load(open(p))
   d["schema_version"] = "2.0"
   json.dump(d, open(p, "w"), indent=2)
   open(p, "a").write("\n")
   print("bumped", p)
   PY
   done

   darkmux doctor    # confirms clean
   ```

2. **Your config declares `expand`.** Replace the template task with the tasks
   it used to fan out into, written explicitly — one task per expansion, each
   naming its own `role_id`. The built-in review config's probe stage is the
   reference shape (`templates/builtin/mission-configs/review.json`), and
   `darkmux doctor` names every offending file and field until it's done.

Nothing else in `~/.darkmux/` needs touching; `config.json`, `profiles.json`,
and the flow/audit stores are unchanged by this release.

### Added

- **`Task.reads` — a run-scoped output ledger** (#1619). A task can read any
  completed task's output without a dependency edge being drawn between them,
  so cross-phase data flow stops requiring cross-phase arrows in the graph.
  The review config's judge/verify/synthesis stages moved onto it.
- **Mutation testing and coverage in CI** (#1635) — the suite could not audit
  itself. Mutation runs on the PR diff every time and sweeps the workspace
  nightly; coverage reports which lines never execute.
- **Browser fixtures generated from the wire types** (#1637), so a viewer test
  and the Rust struct it renders cannot drift apart.
- **The fleet e2e suite actually runs** (#1662). All six `e2e_*` binaries opened
  with a `redis_available()` guard whose false arm printed and **returned** — a
  silent pass — and no workflow ever installed redis. The daemon boot test that
  takes 63 seconds locally was completing in eight milliseconds on CI, which is
  this project's merge gate: the whole fleet layer was guarded by nothing while
  loudly reporting that it was guarded. They now run against a real redis on an
  ubuntu runner, and a missing redis in CI is a hard failure rather than a skip.
  Finding this required compiling darkmux on **Linux**, which had never once
  happened because every workspace-compiling job was macOS. It did not compile.
  Now it does.

### Fixed

- **Runs, missions, and phases agree on what "alive" means** (#1642, #1633,
  #1621, #1632). One liveness decision now serves all three run kinds; a dead
  lab run stops reading `running`; and the generic launcher closes its phases
  like the bespoke ones always did.
- **A benign-empty review no longer reads as retry-worthy** (#1605, #1654). A
  PR with nothing reviewable produced the same signal as a broken run, so the
  session waiting on it would re-run — and since the input is unchanged, that is
  an unbounded retry loop. The PR comment now leads with the fact (the bundler
  ran and worked as expected; re-running will produce the same result), and the
  board reads `Clean` rather than `Degenerate`. An **error**-empty run still
  reads `Degenerate` — the carve-out narrows what gets flagged, never what gets
  recorded.
- **URL userinfo is stripped from route labels** (PR #1661). A route label rides
  to public artifacts; two of its three construction paths left sanitizing to
  the caller and would have carried an endpoint credential into one. The
  chokepoint now strips it regardless of who calls.
- **`mission_id` is stamped at the producer** (#1641) rather than inferred
  downstream, and `mission abort` announces the terminal it actually writes
  (#1660) — it printed `→ Finalized` while storing `Aborted`.
- **Three dead operator surfaces became real** (#1548, #1547, #1550):
  `runtime.feedback_injection` had no reachable off switch — no accessor host
  side, and the host never forwarded it into the container, so both tiers were
  inert.
- **A user-tier mission config newer than the binary now warns** (#1648)
  instead of being read with fields silently dropped.
- **Viewer**: a peer's mission says where it ran instead of 404ing (#1466); the
  session drill-down is addressable (#1639); the mission-graph header stopped
  crediting unattributable tokens to local (#1626); a teardown stopped reading
  as a success (#1627, #1628); plus liveness, silent truncation, mobile input
  zoom, and the first keyboard tests (#1640).
- **Docs state the backend truthfully** (#316) — darkmux drives LMStudio, and
  only LMStudio. The prior claim of "LMStudio + Ollama + llama.cpp" was
  aspiration, and a fresh agent session reading it would confidently propose
  work against backends that do not exist.

### Fixed — caught by this release's own dogfood

The release gate requires verifying each feature live rather than trusting a
green suite. Running it turned up two false statements, both of which shipped
would have been this release's own theme happening to it.

- **A posted review no longer claims a runner it has no evidence of** (#1676).
  The footer's provenance clause has four cases; three are derived from the
  envelope's member records, and the fourth — *no member records at all* —
  returned the fixed string "on a self-hosted runner". Launching a review from
  a laptop shell put that sentence into a public comment. The clause is now
  omitted entirely when there is nothing to derive it from, which is what the
  function's own documentation had claimed all along. #1298 fixed the three
  evidence-derived cases after a footer falsely claimed "no cloud API" about a
  cloud review; this fourth one survived because the no-dispatch path was rare
  until #1605 made benign-empty a normal outcome that posts a comment.
- **`darkmux doctor` no longer prescribes a model load that is unnecessary and
  namespace-breaking** (#1675). The unloaded-utility-model warning said
  compaction would fail without a manual load and suggested a bare
  `lms load <id>`. Since #1616 the dispatch path loads the compactor itself, at
  its declared context, under the `darkmux:` namespace — and a bare `lms load`
  creates precisely the un-namespaced resident darkmux will not reuse and
  `machine eject` cannot reclaim, so following the advice could cause the
  problem the namespace exists to prevent. The check remains; its remedy now
  states only what is true.

## [2.4.0] - 2026-08-03

The observability release: CLI panels in the browser, a mission board that says
what a mission IS, and a batch of producer-side defects that made the data
underneath all of it quietly wrong.

### Added

- **CLI panels in the viewer** — a `console` lens rendering real `darkmux`
  command output as styled DOM, served from an allowlisted `GET /panel/:id`
  (#1569 packets B1-B3). The CLI is the single source of truth; the viewer
  renders it rather than reimplementing it, which is the twin-drift that
  #1561 already was.
- **OSC 8 terminal hyperlinks** — mission ids in `mission status` are
  clickable through to the viewer (#1569 packet A).
- **One runs lens** — the nav catches up to `/runs`, four tabs collapse to
  three (#1584).
- **Phases render as containers** in the mission graph rather than sibling
  cards (#1594).

### Changed

- **The mission board row says what a mission IS** (#1612). It led with the
  mint id (`dispatch-code-reviewer-1785589698-5d6a-0`), which on a phone ate
  two thirds of the width for the least informative thing on the line. Rows
  now carry a name (from the description, populated on every real mission and
  previously unshown), a graph-vs-single-role glyph, the id's own short
  discriminator, and an age. The id stays one click away on the row's link,
  and any row needing an id typed still prints it verbatim beneath.
- `mission status` defaults answer a question instead of enumerating the
  store (#1569), and drift suggestions survive a paste (#1582).

### Fixed

- **A dispatch could unload a model the OPERATOR owned** (#1609). The preflight
  matched residents on the bare model key, and `lms ps` reports darkmux's copy
  and a hand-loaded copy with the same key — so on the sanctioned duplicate
  path it took whichever came first, which is yours. Ownership now means "the
  identifier this profile declares", which also honors the documented
  `identifier` opt-out.
- **A namespaced `internal.utility` binding made the compactor unloadable**
  (#1615). The namespace is a load-time decoration, so a prefixed string can
  never resolve as a model key — the load failed AND the residency check never
  matched its own resident. Compaction fell through to a JIT-load at the model
  default with truncated summaries, on the path that exists to save long
  dispatches. Verified live: the compactor now loads namespaced.
- **A starved judge grant deleted a real finding, silently** (#1610). A grant
  too small to hold a ruling produced a truncated response that read as "no
  finding" — and because the call "succeeded", no degraded gate fired. Now
  denied and counted. The same floor was missing from the `dispatch.map`
  bucket the probe stage rides.
- **A newer peer's flow record read as chain corruption** (#1611). An unknown
  enum value failed the whole record, and the audit checker reported that as a
  broken chain — a false tamper alert on the compliance substrate. Records are
  now lenient on read, and a record this binary cannot content-verify is
  reported as unverifiable-pending-upgrade rather than as evidence of
  tampering. Chain linkage is still enforced across it.
- **`dispatch.map` emitted no liveness bookends** (#1607), so a production
  path doing model work was invisible while it ran. Now RAII-guarded, terminal
  on every exit path. The savings hero also counted hosted tokens as "off the
  meter"; cloud, local and unknown are now distinguished honestly.
- **A review read `0/N` for its entire run** (#1620). Phases started lazily and
  never closed, so every touched phase sat Running until the mission finalized
  and reconciled them in bulk — `0/3` on a mission whose judge was working,
  indistinguishable from one that never started. Phases now close at their
  earned outcome as the run advances.
- **The phone asked for more columns than it had** (#1613, #1614). A 390px
  screen fits ~52 columns; both ends of the negotiation floored at 60, so nine
  columns hung off the right edge. Machine-scope status also wrapped below the
  tab strip, where a global line read as the selected tab's content.
- **The mission graph's last phase read as an empty lane** (#1618) — an
  invisible container border (1.2:1 against the background), task columns
  indented by global rather than per-phase depth so later phases ran
  off-screen, and a minimap that overflowed its viewport.
- 247 lab runs were invisible because `lab_dir` had no config tier (#1585);
  the review summary fallback that never fired (#1583); `dispatch.map` session
  ids (#1524); plus seven audit fixes from a crate-by-crate sweep (#1595-#1601).

### Notes

- `FLOW_SCHEMA` stays 1.18.0. The `#[serde(other)]` catch-alls are additive
  and no record is ever written carrying one, so existing audit chains survive
  without rotation.
- The release dogfood ran a real long-agentic coder dispatch to convergence
  (`wall=798s, verify=pass`) and confirmed #1615's compactor load live.
  Compaction itself did not trigger in that run (0 compactions in 47 turns —
  the `deep` profile's threshold is 131k tokens and per-turn context stayed
  well under it), so the path downstream of the load is unexercised by this
  release's dogfood.


## [2.3.1] - 2026-07-29

**A degraded Redis can no longer take the dashboard down with it**, plus three
fixes so the viewer stops claiming things it cannot back up. Patch release; no
schema changes (FLOW `1.18.0`, CONFIG `1.5`, MISSION_CONFIG `1.3`).

### Fixed
- **An unhealthy Redis wedged the viewer.** Only the *connect* phase was
  bounded, so a peer whose TCP port accepted but which never answered a command
  blocked the read until the route's 30s timeout returned `408` — and the
  local-file fallback was never reached, because a hang is not an error. A
  response deadline now bounds the command itself. Measured on a real
  unreachable-but-accepting hub: the viewer's two-day boot fetch went from
  `0.45s + 30s/408` (hung) to `3.06s`, both `200`.
- **A recovered Redis erased history.** Redis results replaced the local file
  wholesale, but Redis is not a superset — it rides a `MAXLEN` cap and is
  missing everything written while it was unreachable. So the outage window
  vanished from the view the moment Redis came back. The two sources are now
  unioned. Measured: 1080/1080 local records served, zero dropped.
- **The viewer asserted PLAYBACK before it knew its mode**, flashing a scrubber
  on every live load that it would never use.
- **A dropped live connection was invisible and inescapable.** It now shows
  `reconnecting`, refetches automatically when the connection returns or the
  page wakes, and has a refresh control — the escape hatch for the
  home-screen app, which has no address bar and no pull-to-refresh.
- **The idle headline hid its own recency.** "last run 18h ago" was suppressed
  past one hour to avoid looking stale, which inverted: the headline then read
  "ready" with no time reference at all, indistinguishable from a fleet that
  had never dispatched.

[2.11.0]: https://github.com/kstrat2001/darkmux/releases/tag/v2.11.0
[2.10.0]: https://github.com/kstrat2001/darkmux/releases/tag/v2.10.0
[2.9.0]: https://github.com/kstrat2001/darkmux/releases/tag/v2.9.0
[2.8.0]: https://github.com/kstrat2001/darkmux/releases/tag/v2.8.0
[2.7.0]: https://github.com/kstrat2001/darkmux/releases/tag/v2.7.0
[2.6.0]: https://github.com/kstrat2001/darkmux/releases/tag/v2.6.0
[2.5.1]: https://github.com/kstrat2001/darkmux/releases/tag/v2.5.1
[2.5.0]: https://github.com/kstrat2001/darkmux/releases/tag/v2.5.0
[2.4.0]: https://github.com/kstrat2001/darkmux/releases/tag/v2.4.0
[2.3.1]: https://github.com/kstrat2001/darkmux/releases/tag/v2.3.1

## [2.3.0] - 2026-07-28

**Composable mission graphs.** Step kinds are now stateless singletons in one
registry, launchers route on what a graph *declares* rather than what it is
*named*, and every pipeline produces its own input inside the graph instead of
in a bespoke pre-launch prelude. The practical upshot: you can store review
variants and launch them by name, and a graph can be extended from either end.

No schema-version bumps (FLOW `1.18.0`, CONFIG `1.5`, MISSION_CONFIG `1.3`, all
lenient-on-read) — a 2.2 install upgrades in place, and a 2.2 peer stays
wire-compatible.

### Added
- **Named mission-config variants launch by name.** Store
  `~/.darkmux/mission-configs/review-lean.json` (fewer probe seats, different
  judge passes, different models per role) and run `darkmux mission launch
  review-lean`. Launch routes on the step kinds a config declares, not on a
  hardcoded id, and the launched document is the one that executes.
- **Per-seat probe prompts are live.** `review-probe-high.md` / `-mid.md` /
  `-low.md` now drive their own seats, each falling back to `review-probe.md`.
  Previously all three files existed and were silently ignored — editing one
  did nothing. Byte-identical by default, so this changes nothing until you
  edit one.
- **Graph composition is checked before anything runs.** A graph whose step
  kinds require a run-scoped artifact nothing supplies now fails up front,
  naming the artifact, the kinds that need it, and how one is supplied —
  instead of panicking mid-run after the mission had already minted.
- **`demo-quickstart` ships in the binary.** The first command the quickstart
  documents (`darkmux lab run demo-quickstart`) previously failed "not found"
  for anyone who installed via Homebrew or `cargo install`.

### Changed
- **Errors on the composition surface name the fix.** A coder-phase graph
  missing one of its three required steps, a review config whose probe task
  doesn't lead with its render step, or a renamed step the launcher locates by
  id — each now reports what is wrong, which id it looked for, and the template
  to copy, rather than aborting with a bare panic.
- **The review pipeline's degenerate-run message names reachable causes.** It
  previously told you to check a per-seat `selector` and a "probe expansion" —
  both of which had become unreachable, sending you after knobs that no longer
  exist.
- **`darkmux doctor`'s mission-config finding** no longer says these documents
  don't execute. They do; a finding there is a config that will fail at launch.

### Fixed
- **`mission finalize` / `mission abort` targeted the wrong worktree** for a
  coder-phase config launched under any id other than `coder-phase` with a
  custom `workdir` — the run itself was correct, but no `Task.workdir` was
  persisted, so the terminals fell back to the derived path.
- **Posted PR review comments no longer carry local absolute paths** or raw
  stderr from an external `--bundler` plugin. The full detail stays in the
  envelope, flow records, and local output.
- **A bundling failure is no longer silent** — it previously printed nothing
  and exited 0, leaving the cause only inside the emitted JSON.
- **Prompt-building steps no longer show a token meter** in the mission graph.
  They dispatch no model, so the idle `· tok` placeholder — which means "this
  step spends tokens, just not yet" — was a false signal on every probe seat.
- **A URL with inline credentials no longer leaks its userinfo** through a
  seat's recorded endpoint host.
- **`ProfileModel.capabilities` was documented as inert; it is not.** Model
  selection scores against it, so populating it changes which model a role
  dispatches to.
- **The dashboard was slow to load AND could show stale data** — the viewer is
  now served with `Cache-Control: no-cache` plus an `ETag`, so a reload
  revalidates in zero bytes instead of re-fetching ~256 KB, and can never serve
  a stale page. Previously it carried no cache metadata at all, leaving the
  browser to choose between the two.
- **41% of local tokens were counted but never classified.** Map-dispatched
  work (the review probe and verify seats) reported only a total, so the fleet
  card's `generated` / `fresh input` / `re-read` chips silently under-reported
  against their own headline. The per-call prompt/completion split now travels
  with the result. Providers that report only a total still leave the split
  absent rather than claiming a zero.
- **The 24h activity window read `11:38:41–11:38:41`** — a time-only label
  can't distinguish two instants exactly a day apart. The range now carries the
  date when the window straddles a day boundary.

[2.3.0]: https://github.com/kstrat2001/darkmux/releases/tag/v2.3.0

## [2.2.0] - 2026-07-25

### Added
- **`/runs` aggregator** (#1523) — one flat, kind-tagged read-model unioning missions + lab runs + flow into a single per-request view (the data layer for the upcoming Runs lens). Read-side union only; no new persistence.
- **Unified machine page** (#1522) — the machine tab and fleet-drill converge on one lightweight page: a live residency/RAM health region plus a runs list, honest about local-vs-remote ("not reported from here" for a machine probed elsewhere).

### Fixed
- **Remote runs render honestly** (#1518) — a run served off-fleet on a hosted endpoint (e.g. Azure) no longer shows the box's incidental local LMStudio residency as the run's model; route + model resolve from the run's endpoint.
- **Concise review comments** (#1528) — the per-finding "needs frontier verification" note now renders once on the verdict line instead of repeating on every finding.
- **Review runs stamp `mission_id`** (#1523) — the review pipeline's dispatch bookends now carry their mission id, so a review appears as exactly one row in `/runs` (no spurious untracked-ghost duplicate) with its route on the right row.

[2.2.0]: https://github.com/kstrat2001/darkmux/releases/tag/v2.2.0

## [2.1.0] - 2026-07-22

**Config-composable review + concurrency-safe residency.** The review pipeline
stops being hard-coded and becomes pure config-composed primitives — you declare
the graph, darkmux runs it — and the residency arbiter learns to evict stale
orphans and protect a concurrent command's in-use model, so heavier cross-family
crews run without a manual `machine eject`. A `darkmux dispatch` is now a
first-class run through the same engine as missions. No schema-version bumps
(FLOW `1.18.0`, CONFIG `1.5`, MISSION_CONFIG `1.3`, PROFILES `1.5`, all
lenient-on-read) — a 2.0 install upgrades cleanly.

### Added
- **Reconcile-to-need residency + a concurrency-safe lease registry** (#1487) — the residency arbiter (`darkmux-gestalt`) now evicts darkmux-owned models a dispatch's staffing doesn't need (no more monotonic growth), and protects a model a *concurrent* darkmux command is mid-dispatch on: each command writes a per-pid lease at the load chokepoint (`~/.darkmux/residency/<pid>.lease`), read by every other command's reconcile so a busy model is never yanked. `lms ps` stays residency truth; the lease is a subordinate busy-overlay. Heavy cross-family crews (a 62 GB probe + a 35B judge) now load and run without a manual `machine eject`.
- **The review pipeline is config-composed** (#1513) — the probes are now explicit one-role tasks in `review.json`, not a hard-coded three-seat template. The probe COUNT is config-driven (compose a lean one-probe review for a constrained-RAM tier), step kinds are swappable per step, and there is no "probe role" concept in the code — a probe is just a role on a task with a probe step, emergent from your composition. Staffing is the generic machine-local `role_profiles` map (the same one judge/verify already used).

### Changed (behavior — surface preserved)
- **A mission run is a first-class record** (#1503) — each `mission launch` mints a **unique run id** (matching the lab's `run_id` convention), and the old input-hash that was the id is demoted to a queryable `spec` fingerprint for grouping. Re-launching a spec now **mints a fresh run** (history preserved for comparison) instead of reopening the prior one — AI runs are non-deterministic, so input-hash-as-identity was a category error. The mission-id scheme changes from `<config>-<inputhash>` to a minted run id; existing missions on disk load unchanged.
- **`darkmux dispatch` routes through the engine as a crew of one** (#1510) — a dispatch is now a full Mission→Phase→Task(role)→Step at cardinality one, so it mints a first-class run, emits mission/step flow records (it shows up in `mission status` and the missions lens), and — the load-bearing part — participates in the #1487 lease/reconcile regime, closing a gap where a raw dispatch could be evicted by a concurrent mission. **The `--json` output contract is byte-identical.**
- **`k` (probe draw-multiplication) retired** (#1513) — one role = one task = one dispatch; recall breadth is varying the *set* of probe roles, not drawing one model `k` times. `darkmux lab eval`/`review-bench --k>1` is now a typed rejection pointing at the config, so a recall sweep can't silently produce a flat-but-mislabeled series.

### Fixed
- **Phase↔step terminality invariant** (#1504) — a phase can never be persisted `Complete` while a step it contains is still live, and a launch that fails after minting reconciles to a terminal `Error` instead of stranding an Active mission. Enforced at the single `lifecycle.rs` chokepoint, with a loud warning when the backstop actually fires.
- **The darkmux self-review workflow** (#1515) drops the deleted roster-profile/seat-pin/`k` params and staffs from the `role_profiles` map — matching the current model.

### Migration (2.0.0 → 2.1.0)
No schema bumps; a 2.0 install upgrades in place. The behavior changes to know:
- **A dispatch is now a run.** `darkmux dispatch` appears in `mission status`, the missions lens, and the flow stream, and writes a `~/.darkmux/missions/dispatch-*` record. The `--json` output is unchanged.
- **Re-launching a spec mints a fresh run.** The prior run is left on disk for analysis; there is no implicit reuse/reopen. Mission ids are now minted, not input-hashed.
- **`--k>1` is rejected.** Vary the probe-role set in the review config for recall breadth.
- **Review configs**: `review.json` now declares explicit probe tasks — a lean N-probe review is a config edit (delete/add tasks). Staffing is the `role_profiles` map; per-run overrides remain `--param review-probe-high=<profile>` (and `-mid`/`-low`/`review-judge`/`review-verify`).

## [2.0.0] - 2026-07-18

**darkmux 2.0 — the mission orchestrator.** The 1.x line grew a swap tool into a
dispatch tool into a review pipeline; 2.0 unifies all of it under one model.
Config-defined **missions** launched with `darkmux mission launch <config>` run
as a live **Task/Step dependency graph** on a real scheduler, with concurrent
local dispatch bounded by a residency arbiter that loads exactly what each seat's
staffing declares — and a React Flow **mission-graph lens** that draws the graph
light up as it runs. The PR-review funnel and the coder pipeline both became
missions; residency (the founding profile-multiplexer) became an internal
capability underneath, not a verb the operator drives. The verb surface shrank
hard, the openclaw shell-out path is gone, and review seats staff from a
machine-local role→profile map. This is a **major** release: many verbs were
renamed or retired without deprecation shims — see **Migration** below.

### Added
- **`darkmux mission launch <config>`** — configs mint mission instances; `mission propose` emits configs; the whole pipeline runs as a real **Task/Step DAG** with a dependency-graph scheduler, concurrent local dispatch, and per-mission `MissionEnvelope` finalization (#1284, #1230, #1352). Built-in configs: `review` (bundle → probe → dedup → judge → verify → synthesis) and `coder-phase` (worktree → coder → verify).
- **Mission-graph lens** — a live React Flow Phase/Task/Step diagram is now THE mission view: direct nav + standalone shell, per-seat model chips + step metrics, an events panel, a mobile vertical timeline, refresh/reconnect, a minimap toggle, live turn/tool-call metrics for agentic seats, and **phase→phase order arrows** (#1384, #1403, #1404, #1431, #1485, #1491, #1497).
- **`darkmux dispatch <role>`** as a top-level verb (promoted from `crew dispatch`) — one role, one turn, through the internal Docker-bounded runtime; `--image <tag>` runs the seat in any Linux environment you name (#1435, #703).
- **Residency arbiter (`darkmux-gestalt`)** wired as the production resource planner — grow-only additive acquisition, wave scheduling, an architecture-aware KV estimator, and a deadline-bounded `lms` load/unload adapter, all scoped to the `darkmux:*` namespace so user-loaded models are never touched (#1230 packet 1, #1274/#1276).
- **`darkmux doctor` staleness checks** — a running daemon vs. the installed binary vs. the source tree vs. the runtime image, so a stale-binary "bug" is caught structurally (#1461).
- **`darkmux-bundler-rust`** — the reference `--bundler` plugin: Rust function-boundary scanning + differential call-site facts, so the review funnel can bundle a Rust diff with real context (#1319).
- **Mission staleness / dead-dependency drift detection** in `mission status` — an Active mission stalled at zero complete phases, or a phase permanently unreachable behind an abandoned dependency, is surfaced with a reconcile suggestion (#1230 packet 5).

### Changed (breaking)
- **Review seats staff via a role→profile map** — crews dissolve; each seat (`review-probe-high`/`-mid`/`-low`, `review-judge`, `review-verify`) resolves to a profile through a machine-local `role_profiles` map in `config.json`, with a single `--param <role>=<profile>` per-run override (`Overridden` provenance). The roster-scoring resolver and seat pins are gone (#1475, #1438).
- **Verb collapse** — `dispatch` and `machine` are the new top-level families. `swap`, `status`, `recommendations`, and the old `fleet` family fold into `machine` (`machine list/add/remove/status/eject`); the `profile` family merges; `notebook` joins `lab`; `lessons` becomes `memory` (`memory lesson` / `memory correction`); `review-bench` becomes `lab eval`; `--crew` becomes `--roster-profile` (#1426, #1430, #1435, #1437, #1462, #1470).
- **Mission lifecycle** — `mission run` collapses into `mission launch`; `mission ship` retires; `mission close` → **`mission finalize`** (which now reconciles its phases honestly from step statuses); the standalone `phase` verb family retires; `MissionStatus::Closed` → **`Finalized`** (existing "closed" records read lenient) (#1439, #1463, #1468, #1498, #1406).
- **`WorkJob` schema bump** — the `deliver` and `runtime` fields retire; a version-first mismatch now errors loudly instead of mis-parsing (#1440).
- **`FLOW_SCHEMA` 1.17.0 → 1.18.0**, **`CONFIG_SCHEMA` → 1.5**, mission-config schema, all additive / lenient-on-read.

### Removed (breaking)
- **The openclaw shell-out dispatch path** — `--runtime openclaw`, the per-dispatch `--runtime-cmd`, and `crew sync` are gone. The internal runtime reads role manifests directly and is the one and only dispatch path (#1405).
- **`GET /diff/:session_id`** and the viewer's live-diff panel (#1387) — replaced by `GET /worktree-summary/:session_id`, a numbers-only endpoint (`files`/`adds`/`dels`/`base`/`path` from `git diff --numstat`, never diff content) that rides the general remote-read gate. The session view renders a "what changed" totals line + the worktree path with a copy button and an (inert-by-default) `zed://` anchor.
- **`optimize scaffold`** and **`doctor --fix`** — two dead surfaces (#1419).

### Fixed
- **The mission graph tells the truth mid-run** — parallel seats show elapsed units + a model chip + per-seat completion (no more "all-or-nothing" wall-clock), planned steps show no phantom tokens (server-side fold gates on step-started), phase status rolls up from its tasks instead of a stale persisted value, and an error/untracked-mission state keeps the nav reachable (#1488, #1493, #1472, #1496).
- **A 0-member probe stage fails loud with per-seat reasons**, routed to a degraded run, not a silent Clean (#1486).
- **Liveness guards on the utility dispatch paths** (propose, phase review, timeout narration) + metering on the `dispatch.single_shot` hosted arm (#1413, #1412).
- **The 2.0 upgrade path** — `darkmux init` prunes retired skills and refreshes its managed doc blocks; 2.0 identity swept through source-embedded strings, clap `about`, and init templates (#1467, #1449).

### Migration (1.18.x → 2.0.0)
No deprecation shims — old verbs error rather than silently mis-run. The map:

| 1.x | 2.0 |
|---|---|
| `darkmux crew dispatch <role>` | `darkmux dispatch <role>` |
| `darkmux crew sync` | *(gone — internal runtime reads role manifests directly)* |
| `darkmux swap <profile>` | *(gone — gestalt loads what each seat's staffing declares)* |
| `darkmux pr-review run` | `darkmux mission launch review` |
| `darkmux mission run` / `mission ship` | `darkmux mission launch coder-phase` |
| `darkmux mission close` | `darkmux mission finalize` |
| `darkmux notebook draft/list` | `darkmux lab notebook draft/list` |
| `darkmux review-bench` | `darkmux lab eval` |
| `darkmux lessons …` | `darkmux memory lesson …` / `memory correction …` |
| `swap` / `status` / `recommendations` / `fleet` | the `machine` family |
| `--crew <profile>` | `--roster-profile <profile>` |
| `--runtime openclaw` / `--runtime-cmd` | *(gone — internal runtime only)* |

**Review staffing config.** 2.0 review needs a `role_profiles` map in
`~/.darkmux/config.json` binding each seat to a profile — e.g.
`{"review-probe-high":"…","review-probe-mid":"…","review-probe-low":"…","review-judge":"…","review-verify":"…"}`.
`darkmux init` on 2.0 writes the block; `darkmux doctor` surfaces an unstaffed seat.

**Data.** Existing `~/.darkmux/missions/*` load unchanged (mission status
`"closed"` reads as `Finalized` via a serde alias; `closed_ts` reads as
`finalized_ts`). Flow records and profiles are lenient-on-read across the bump.

## [1.18.5] - 2026-07-12

Two fixes surfaced by a real production 37-flag funnel run on a private repo, plus a provenance gap found on the very first Azure review.

### Fixed

- **The funnel's run-level `degenerate` gate no longer over-fires on a minority remote-judge dispatch error** (#1329) — a transient dispatch failure on even ONE flag out of many (e.g. 1 of 37) was forcing the ENTIRE run degenerate, discarding every other flag's real, valid adjudication and posting "no review signal" on a run that actually completed correctly. The per-flag outcome was always handled safely (a pass-1 failure archives just that flag, a pass-2 failure demotes it to NeedsCheck — never a silent fake confirm); only the run-level gate over-reacted, and did so asymmetrically — the same failure class via `Unparsed` (garbage output surviving its retry) was already exempt and rendered fine. Fixed by folding the dispatch-error reason into the existing `usable == 0` gate `Unparsed` already relies on — a consistency fix, not new policy. A minority dispatch error is now surfaced as an `env.warnings` entry (matching the probe stage's existing precedent) rather than going fully silent on an otherwise-healthy run.
- **Funnel provenance now stamps the model an endpoint actually SERVED, not just the requested deployment name** (#1300) — an Azure deployment named e.g. `gpt-4o` can alias to a different underlying model; every downstream provenance surface (the posted footer, the audit envelope) previously inherited only the declared/requested id, discarding `SingleShotReply.model` (the response body's ground truth) entirely. `MemberRecord` gains `served_model: Option<String>`, threaded through all three seats (probe/judge/verify) and gated to remote seats only (a local LMStudio response is also OpenAI-compatible and echoes a `model` field — `lms ps` stays the only ground truth for local dispatch). The posted footer now surfaces both when they differ ("requested gpt-4o, served gpt-4o-2026-08-01"); agreement (the common case) still shows just the one name.

## [1.18.4] - 2026-07-12

Two fixes surfaced by running the funnel on a private production repo.

### Fixed

- **config.json resolves to user scope, never a project-local shadow** (#1323) — a stray project-local `.darkmux/` (created for project-tier missions/sprints/lessons) silently flipped config resolution to Project scope under `ResolveScope::Auto`, so on a self-hosted-runner checkout **every review dispatch ran with Redis *and* the tamper-evident audit log silently disabled** — a real audit-trail hole, not a telemetry gap (diagnosed via the #1311 liveness markers: `config-resolved … redis=off audit=off`). config.json is user/machine-level (redis/audit/lms/machine_id) with no legitimate per-project variant, so both `DarkmuxConfig::load_resolved` AND the `darkmux config` CLI now `ForceUser`. A conformance test guards against regression (proven to fail under `Auto`). Same shadowing class as #1012/#1016 — the config/flow-sink resolution path they missed.
- **The review footer no longer claims "darkmux dogfooding itself in public"** — the default tagline was posted verbatim on every review, including private repos, where it's both wrong and awkward. The default is now generic ("Advisory, not a merge gate."); darkmux's own public self-review opts the flourish back in via `--attribution`.

## [1.18.3] - 2026-07-12

A one-fix patch: the review funnel's confirmed findings anchor as inline comments instead of falling into the summary's general section.

### Fixed

- **Fragment anchors resolve to inline comments** (#1299, the mis-anchor half — the dedup half shipped in 1.18.1) — the funnel's prosecutor quotes the offending code in backticks, and `extract_new_side_anchor` stores that SPAN as the finding's anchor. A span is often a *sub-expression* of a changed line, not the whole line, so it matched the diff by **substring** at extraction time, but `resolve_anchor`'s **exact whole-line** lookup missed it — dumping the finding into the non-anchored "general" body section instead of posting inline. Frontier-staffed funnels (mechanism-level findings) hit this hardest. `resolve_anchor` gains a substring fallback symmetric with extraction: after the exact lookup fails, anchor to the new-side line whose whitespace-collapsed content *contains* the collapsed span — only when exactly one distinct line matches (never guess between candidates). An 8-char floor refuses short fragments; the fallback runs only after the exact path fails, so whole-line anchors (single-model reviews) are byte-identical to before. Offline replay against a real preserved review envelope: 0 inline / 7 general → **6 inline / 1 general**.

## [1.18.2] - 2026-07-11

The production-hardening patch — a ledger correctness fix plus the credential-and-hang surface the Studio's first Azure-review day surfaced.

### Fixed

- **The memory ledger prices models whose LMStudio path metadata is wrong** (#1309) — a model whose `lms ls`/`ps` reports a directory that doesn't exist on disk (e.g. devstral: reported `mistralai/devstral-small-2-2512`, real dir `mlx-community/Devstral-...`) was unpriceable, and the machine total silently undercounted. A content-scan fallback resolves the real config dir by token-subset match, with an ambiguity guard (multiple matches → stay unpriced, never guess a wrong config). Verified live: devstral went from unpriced to a correct 20.24 GB.

### Added

- **A dependency-free dispatch liveness floor** (#1311) — a heartbeat file (`<home>/liveness/<pid>.log`) plus `[darkmux-liveness]` stderr markers at each dispatch phase boundary, with NO dependency on config/Redis/audit/flow. A dispatch that hangs *before* flow-sink init (the #563 incident: an Azure review that froze 19 min with zero trace) now leaves "started, last alive at phase X" instead of a black box. Phase markers carry resolved detail (sinks, crew/seat-count/endpoint hosts, keychain item names, bundle counts, elapsed) — secrets never logged. `DARKMUX_LOG=debug` adds per-call host/model/token/wall detail.
- **`EndpointAuth.key_env`** (#1312, PROFILES_SCHEMA 1.4 → 1.5) — declare which environment variable holds an endpoint's API key (any provider). Resolution is `env(key_env) > per-dispatch cache > Keychain`; with the var set, `security` is never spawned — the standard headless-CI-secrets fix, so a self-hosted runner needn't read the macOS Keychain at all. Matches darkmux's existing `DARKMUX_REDIS_URL`/`DARKMUX_SERVE_TOKEN` env-over-Keychain pattern.

### Changed

- **All three Keychain reads are now bounded** (#1311) — the Redis-password read (during flow-sink init, the leading #563 freeze point), the serve-token read, and the endpoint auth spawn `security` with a 15s timeout: a locked/hung login keychain on a headless runner fails fast and actionable instead of a multi-minute freeze. The endpoint credential is also cached per-dispatch (it was read per hosted call — dozens of `security` spawns per review).


## [1.18.1] - 2026-07-11

The review-output patch — everything a first real production Azure review (on a private engagement repo)
surfaced about how the funnel *presents* its findings. No behavior change to what it finds; four
fixes to how it posts.

### Changed

- **Confirmed findings post a non-blocking `COMMENT` review by default, not `REQUEST_CHANGES`** (#1302) — the funnel was submitting a formal `REQUEST_CHANGES` (a real GitHub merge gate via branch rulesets) while its footer claimed "advisory," and darkmux could never clear its own block (a clean re-run posts a plain comment, which doesn't update `reviewDecision`), forcing manual dismissals that a compliance-monitored org must document. Confirmed findings now post the same non-blocking `COMMENT` class Gemini uses — inline comments intact, never a merge gate. A crew-level `request_changes: true` opts back into blocking (documented: no automated resolution path until #1260's verify-seat lifecycle exists). No workflow change — the YAML forwards the binary's review verbatim, so one binary upgrade fixes every consuming repo. `PROFILES_SCHEMA_VERSION` 1.3 → 1.4 (additive `request_changes`).
- **A configurable judge `passes` count replaces the hardcoded double-confirm** (#1266) — `passes: 1` on a judge seat runs a single pass (the frontier cost lever — a stable frontier judge needs double-confirm less than the local judge it was designed for), `passes: 2` is today's double-confirm (default), `passes: N` is unanimous consensus with early-exit. `PROFILES_SCHEMA_VERSION` 1.2 → 1.3.

### Fixed

- **The posted-review footer no longer claims "local model (no cloud API)" on a cloud review** (#1298) — the dispatch-provenance line is now derived from the run's actual seats (`env.members`): a remote crew reads "via a hosted cloud endpoint (`<models>`)", never "no cloud API"; all-local keeps the honest local claim; mixed names both. The old hardcoded claim was an audit-integrity problem on the first all-Azure review.
- **Frontier-worded duplicate findings collapse; the needs_check tier clusters instead of walling** (#1299) — the dedup was calibrated on local models and let a frontier judge's restatements through (9 "confirmed" that were 3 bugs; a 25-item needs_check wall). Collapse now keys on file + mechanism-family + overlapping-symbol + overlapping-location — conservatively (missing location never collapses; a collapse unions both locations and the absorbed finding's text, so a mistaken merge degrades to "one bullet, two framings" never a vanished bug), and the needs_check tier clusters by (file, mechanism) with the count conserved.


The frontier-staffing release: any review-funnel seat can be staffed by a hosted model, so a
machine that can't (or shouldn't) run local inference does PR review entirely off a cloud
endpoint. Underneath it, a new model-lifecycle foundation — a pure planning core, a
memory ledger, and a deadline-bounded host adapter — ships tested but not yet wired to the
live dispatch path (that cutover lands in 1.18.1).

### Added

- **Remote (frontier) staffing for any funnel seat** (#1260, #1177) — a `crews` seat whose profile carries an `endpoint` block dispatches to that hosted model instead of a local one. No new syntax: endpoint-presence is the whole signal. Remote seats **skip model cycling entirely** — nothing is loaded or unloaded, zero LM Studio contact — so an all-remote crew runs PR review with LM Studio shut down (verified end-to-end against Azure with the `lms` binary and local URL both disabled). Message assembly is byte-identical to a local seat (only the HTTP transport differs); provenance stamps the model and endpoint **host** only, never credentials.
- **The `review-verify` seat** (#1260) — an optional fourth funnel stage. When a crew declares it, each double-confirmed finding gets one frontier adjudication pass: `verified` posts without the "needs frontier verification" marker, `refuted` demotes to archived, `uncertain` keeps the marker. A crew without the seat behaves exactly as before.
- **Per-execution remote token buckets** (#1260, #1186) — `remote.max_tokens_per_execution` (default 500000, written visibly by `init`) caps each pipeline stage's hosted spend; exhaustion stops that stage's remaining remote calls with a named envelope reason (a load-bearing stage degrades the run honestly, never a silent pass). Remote tokens are accounted separately so savings surfaces never count cloud spend as "off the meter." The agentic-remote container loop is not metered in this release (#1293).
- **Memory ledger — potential vs. current** (#1286) — `darkmux model ledger [--json]`, a `/machine/memory` serve endpoint, and a mobile-first `#lens=machine` viewer lens show, per loaded model, what its config *commits* (weights + KV-cache-at-loaded-context + margin, from the model's own architecture) against what has *materialized*, color-coded green / amber ("made it by luck") / red, with a shrink hint that names the context reduction to reach green. Observability is read-only and dispatch-free by design (kernel counters + `lms` metadata only; the gather stamps its own cost) — codified in the new CLAUDE.md "the observer must not join the observed" doctrine.
- **GestaltManager model-lifecycle core** (#1274, present but not yet wired — cutover in 1.18.1): a pure planning crate (`darkmux-gestalt`) that decides load / unload / reuse / reconcile / block from abstract facts (memory pools as data, an ownership namespace, a RAM budget) and emits an inspectable plan with a typed reason on every action; a **wave scheduler** (#1285) that partitions co-resident models into parallel-or-sequential waves under a budget (which doubles as a hardware-tier emulator); an **architecture-aware estimator** (#1286) that computes true KV-cache cost per model; and an **`LmsHost` adapter** (#1276) that makes every `lms` call deadline-bounded — the unbounded-load hang becomes structurally impossible.

### Changed

- **`ProfileModel.n_ctx` is now optional** (#1282, profiles schema 1.1 → 1.2, minor) — endpoint-bearing models omit it (the provider owns the context window); a local model still requires it, enforced at resolution time with a named error and surfaced by `darkmux doctor`, never on the load path.
- **The profile registry is lenient per entry** (#1282) — one structurally-broken profile or crew entry is quarantined (with serde's exact field error) instead of failing the whole file; siblings load normally, `darkmux doctor` lists each quarantined entry, and a dispatch that names a quarantined profile hard-fails with that entry's parse error instead of silently substituting a model.

### Fixed

- **Endpoint-bearing profiles no longer break crew resolution** (#1269 → #1270, shipped in 1.17.1; the schema unblock here completes the story).

## [1.17.1] - 2026-07-10

The canary-day patch: three production findings from the Studio's first hours running the
funnel, fixed same-day.

### Fixed
- One invalid crew in `profiles.json` no longer fails the ENTIRE registry load — crew
  validation moved to resolve time (its doctrinal home); `darkmux doctor` gains a per-crew
  validation check; a genuine registry parse failure is one clear hard error instead of the
  deprecated probe fallback (#1269, #1270).
- The funnel's sequential cycler reconciles a same-model resident loaded at a different
  context (darkmux-owned: unload + reload at the required ctx; user-owned: an actionable
  error naming the instance) instead of attempting a doomed second load that LMStudio's
  guardrail refuses on 32 GB machines; explicit-alias residents reuse correctly; reuse at a
  larger ctx leaves a log breadcrumb (#1271, #1275).
- Production funnel runs now emit `dispatch.start`/terminal bookends (RAII-guaranteed on all
  exit paths, `source: "funnel"`) so a live PR review is visible as a running dispatch in the
  viewer's fleet and machine views (#1272, #1277).

[2.1.0]: https://github.com/kstrat2001/darkmux/releases/tag/v2.1.0
[2.0.0]: https://github.com/kstrat2001/darkmux/releases/tag/v2.0.0
[1.18.5]: https://github.com/kstrat2001/darkmux/releases/tag/v1.18.5
[1.18.4]: https://github.com/kstrat2001/darkmux/releases/tag/v1.18.4
[1.18.3]: https://github.com/kstrat2001/darkmux/releases/tag/v1.18.3
[1.18.2]: https://github.com/kstrat2001/darkmux/releases/tag/v1.18.2
[1.18.1]: https://github.com/kstrat2001/darkmux/releases/tag/v1.18.1
[1.18.0]: https://github.com/kstrat2001/darkmux/releases/tag/v1.18.0
[1.17.1]: https://github.com/kstrat2001/darkmux/releases/tag/v1.17.1

## [1.17.0] - 2026-07-10

The review-funnel release: PR review graduates from a single reviewer dispatch to a measured
prosecution-and-judgment pipeline, with the lab observability to watch and tune it.

### Added
- **The review funnel** (#1222 Phase B): `darkmux pr-review run` — procedural bundling
  (built-in Rust bundler with callee/sibling bodies, param-flow facts, external-symbol
  manifests; `--bundler <cmd>` plug-in contract) → strong-prior probe seats with k draws →
  mechanism-family dedup → double-confirm judge → three-tier synthesis (double-confirmed
  inline REQUEST_CHANGES comments carrying a "needs frontier verification" marker;
  needs_check as a non-blocking section; everything archived in the envelope artifact).
  Sources: local `--worktree` or `--github` + `--head-sha` (GitHub API, no checkout).
  (#1229, #1231, #1235, #1236, #1239, #1250)
- **Crews registry** (#1231): `crews` in `profiles.json` — saved seat assignments
  (`review-probe`/`review-judge`) staffed per profile/model with `k`, `max_tokens`, and
  `bundle_selector`; profiles schema 1.1.
- **Funnel review workflow** (#1232): `darkmux-review.yml` replaced with the funnel form —
  inputs `pr`/`crew`/`mode`/`k`, one `pr-review run` invocation, envelope uploaded as an
  artifact, crash-before-emit guard; Studio migration checklist in the runner docs (#1261).
- **`review-bench --funnel`** (#1238): the release-guard validation mode — corpus scoring
  unchanged, per-case funnel console line, `funnels.json` artifact.
- **Lab run observability** (#1247): funnel flow-record emission through a sink-agnostic
  emitter (production → flow stream; bench → per-run-local `funnel-events.jsonl`),
  per-case atomic envelope streaming, staffing snapshots (incl. `n_ctx`) for series
  comparison, crash-safe step bookends, host telemetry sampling during funnel runs
  (#1248, #1253, #1264). Flow schema 1.17.0.
- **The lab observer lens** (#1262): third viewer lens, machine-local — run list grouped
  by case with knob-diff provenance between runs (two-variable changes warn), live run
  detail (pipeline stages + ruling feed + host load), `#lens=lab` deep links, served from
  `darkmux serve --lab-dir <path>`.
- **Dialectic review-bench mode** (#1223, #1224): the P→D→J three-seat chain.
- **Agentic + free-form review-bench modes** (#1206, #1179); hosted single-shot dispatch
  gains `reasoning_effort` (#1204), 429/capacity-shed retry classification (#1207, #1211),
  and Google-compat fixes (index-less streaming deltas, stop-turn tool calls,
  thought_signature round-trip) (#1212, #1213, #1214).
- **Runtime**: per-call token cap override `DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL` (#1225);
  tool-call arguments recorded in trajectory + flow and surfaced in the viewer (#1220).
- **Viewer**: activity filter facet + per-activity icons (#1215, #1218), new-run
  affordances on the machine view (#1208).

### Fixed
- Funnel prompt assembly byte-matches the measured Phase A prompts (per-seat code-slice
  formats, prior in the user message, intent-free probes), enforced by golden tests
  generated from the reference implementation; role texts re-frozen on the measured
  versions (#1258, #1263).
- `--bundler`/`--k` warn when ignored with `--from-envelope`; example crew staffs three
  distinct models (#1250).
- Judge pass-2 step records open when pass-2 rulings actually begin (#1264); shakedown
  fixes for wrapped-line anchors, fence-aware quote spans, cliff recovery, watchdog
  streaming (#1226, #1228); clippy 1.97 toolchain lints (#1259).

[1.17.0]: https://github.com/kstrat2001/darkmux/releases/tag/v1.17.0

## [1.16.0] - 2026-07-05

**The production-review release** — everything the self-hosted QA pipeline needs to run agentic, cloud-backed PR review honestly: a freeform review contract that works WITH tools (a grammar-constrained `output_schema` combined with tools makes a model skip tool-calling and fabricate — verified empirically), a `pr-reviewer-agentic` role that explores the checked-out repo before concluding, a `doctor --probe` that live-verifies a remote credential actually works, and a render pipeline that can no longer present a produced-nothing review as a clean pass. Plus the live viewer stops counting paid cloud tokens as "off the meter." `FLOW_SCHEMA` / `RULES_SCHEMA` / `CONFIG_SCHEMA` unchanged.

### Added

- **`darkmux doctor --probe`** (#1177, #1191) — live-verifies each profile model's remote endpoint with ONE minimal chat completion through the exact URL/auth/POST path a real hosted dispatch uses: DNS, TLS, credential validity, deployment routing, api-version — not just Keychain presence (the free offline check, unchanged). Opt-in because each probe is a real API call; the result line shows the probe's own token cost, round-trip time, and the model the endpoint SAYS served the request — which surfaces deployment-name vs served-model drift as a one-liner. One probe per distinct (url, model, api-version, keychain) declaration; failures print the endpoint's error verbatim and exit 1.
- **Freeform review contract + `pr-reviewer-agentic` role** (#1113, #1192) — `MUST FIX`/`CONSIDER [path] \`anchor\`` marker blocks plus a `VERDICT: pass|flag` line, parsed by `pr-review render` AFTER the JSON contract (structured roles unchanged) and resolved to inline comments by the same quote-the-line machinery. The new builtin role has read/exec tools, deliberately no `output_schema` (a loader test pins that invariant), and an explore-before-concluding directive. Live-verified end-to-end: a real Azure dispatch followed the contract exactly on first exposure and all anchors resolved to correct inline lines.
- **`pr-review render --attribution <text>`** (#1192) — the posted footer's claim about where the model ran is now the workflow's to make (it knows; darkmux doesn't guess). The default footer is unchanged; the body header drops its "(local model)" suffix — locality is the footer's job.
- **Local/cloud token split in the live viewer** (#1186, #1189) — the savings hero's single "tokens off the meter" number was blind to agentic-remote dispatches and counted paid Azure tokens as off-meter. The hero now shows two co-equal numbers under a "by your fleet" frame: **local tokens** (green) and **cloud tokens** (cyan), resolved per-session via the dispatch's `endpoint` field, with a fallback so tool-less single-shot hosted dispatches (which emit no token telemetry) count from their completion records. Tokens only, never currency, on either tier. Labels renamed from "off the meter" ("what meter?" — the phrase presumed a known meter; "local" explains itself), and the same reasoning retired "off the meter" from user-facing copy (the home-page tagline now says "no API bill").

### Fixed

- **A produced-nothing review can no longer read as a green check** (#1113, #1193) — `pr-review render` emits `mode: "degraded"` (distinct from `review`/`comment`) for a missing/empty envelope, an empty reply, or a vacuous pass (zero findings, no summary, and anything but an explicit `flag` verdict). The posted comment states loudly that no automated review happened; the self-review workflow posts it and fails the run. The motivating incident: a SIGKILLed dispatch (exit 137, zero tokens) rendered identically to "clean review, no findings" on a production-deploying repo.
- **Redundant GitHub Pages deploy workflow removed** (#1190) — it raced the built-in branch-based Pages pipeline on every docs push; the loser failed with "Deployment failed, try again later" noise while the site stayed current throughout.
- **Dangling issue citations corrected** (#1187, #1188) — code comments shipped with agentic-remote dispatch cited `#92`, an unrelated merged PR; #1187 is the retroactive tracking issue and all citations now point at it. *Correction to the 1.15.0 entry below: its "(#92, #1180)" citation should read "(#1187, #1180)" — left in place as published, corrected here.*

## [1.15.0] - 2026-07-04

**Agentic-remote dispatch** — a tool-granting role (e.g. `code-reviewer`) can now be driven by a remote OpenAI-compatible endpoint (Azure OpenAI, OpenAI, …) as its "brain," running the SAME real tool-calling loop (multi-turn `tool_calls`, `bash`/`read`/`write`/`edit`/`search`) local models get via the internal container runtime — not just a single-shot chat completion. Tool-less roles (e.g. `pr-reviewer`) are unaffected; they stay on the existing light single-shot `dispatch_remote` path from 1.13. Also carries forward #1172 (deferred from 1.14.1) and a viewer host-load meter. `FLOW_SCHEMA` **1.14.0 → 1.15.0** (additive — new CPU/RAM/GPU host-load telemetry fields; an older binary tolerates the newer schema, no breaking change). `RULES_SCHEMA` / `CONFIG_SCHEMA` unchanged.

### Added

- **Agentic-remote dispatch** (#92, #1180) — the remote endpoint's auth credential is piped over the container's stdin once at spawn, immediately consumed, never written to any file or env var: a mounted secret-bearing file would be reachable by the container's `bash` tool (no `/workspace`-escape check on `bash`, unlike `read`/`write`/`edit`), so stdin closes that exposure entirely. Live-verified against a real Azure endpoint: a genuine multi-turn `tool_calls` round-trip, and confirmed no auth artifact exists on host or container at any point (including the model's own attempted `cat` of the old file path failing outright).
- **`darkmux doctor` — remote endpoint credential presence check** (#85, #91) — surfaces a profile model that declares a remote endpoint whose Keychain credential is missing or absent, before the first real dispatch bails on it. Read-only; never touches the secret value.
- **Host-load meter** (#1064, #1176) — CPU, RAM, and GPU utilization in the run view, sampled alongside existing telemetry.
- **`pr-review-bench` multi-finding parity scoring** (#1119, #1172) — corpus-wide recall/precision against a labeled corpus, not just single-anchor pass/fail.

### Fixed

- **Agentic-remote dispatches were missing the `endpoint` flow-record field** (#1181). The light single-shot `dispatch_remote` path already recorded which remote endpoint served a dispatch; the new agentic-remote container path didn't, so the viewer rendered every agentic-remote dispatch as a local LMStudio run regardless of where the model actually ran. Caught live, watching the viewer during the first real agentic-remote dispatches.
- **Compaction now always uses a local-only client, never the dispatch's remote brain.** An agentic-remote dispatch was routing its compaction requests through the SAME client as its primary loop — silently mis-billing the remote deployment (Azure ignores the request body's `model` field; the deployment is in the URL) or hard-failing the whole dispatch outright (OpenAI-style endpoints validate `model` server-side) the moment compaction fired, which is exactly the long, tool-heavy dispatch this feature exists for. Found by an independent security audit; regression-locked with a two-mock-server test, and live-verified with a real forced-compaction dispatch against Azure (confirmed via a differential test: unloading the local compactor model makes the dispatch fail with an error naming the local LMStudio URL, not Azure).

## [1.14.1] - 2026-07-03

A viewer performance hotfix — the live observability tab degraded over a long-open day (multi-second loads, laggy clicks). Released as a clean patch off `v1.14.0` (this entry lands on `main` for continuity; the tag itself was cut from the 1.14.0 line, excluding the concurrently-merged #1172 which rides the next minor). Drop-in; no `FLOW_SCHEMA` / `RULES_SCHEMA` / `CONFIG_SCHEMA` change.

### Fixed

- **The live viewer no longer degrades over a long-open day** (serve + viewer, #1173). Two independent costs, both profiled on a real daemon (160 sessions, ~4.8k records): (1) every click and the initial paint paid a ~2.5s `render()` because `liveSessionSet()` fell back to an O(sessions×records) scan (`flowLiveSessions`) when Redis presence was empty, and the fleet timeline + crew cards called it hundreds of times per render — it's now memoized per render (keyed on the data snapshot + a 2s wall-clock bucket) → ~20ms (123×); (2) the 20s SSE-backstop reconcile re-fetched and re-parsed both full day files (multi-MB) on the main thread every tick — `GET /flow/:date` now accepts an optional `?since=<ts>` and the reconcile requests only the recent tail, so the parse cost no longer grows with the day.

## [1.14.0] - 2026-07-02

Cross-day playback discoverability + a run-detail telemetry-panel overhaul. Drop-in over 1.13.1 — no `FLOW_SCHEMA` / `RULES_SCHEMA` / `CONFIG_SCHEMA` change.

### Added
- **Cross-day mission/dispatch catalog** (#691) — playback is now navigable by the *thing that ran*, not just by calendar day. Disk-backed endpoints `GET /flow-missions` (a rollup across every day file), `GET /flow-mission/:id`, and `GET /flow-session/:id` (#1166), plus a viewer catalog with a missions section and `?mission=`/`?session=` replay-by-query that stitches a mission's records across every day it touched (#1167).

### Fixed
- **Run-detail telemetry panel** overhaul (#1169): CPU and context charts now share one wall-clock time axis (they were on different scales); context is a step-area with a marker at every turn — visible even when a turn's token delta is sub-pixel — left-anchored at t=0, with a green→amber→red fullness gradient and a labeled window ceiling; CPU is shown in **cores busy** (docker's per-core % scales past 100% on many-core machines, so a 100% floor was useless); a dashed line marks the compaction-trigger level; the `model (lms)` panel populates on the dispatch's first sample instead of reading "no telemetry yet"; and long session ids in crew cards wrap instead of overflowing.

Note: cargo `1.14.0` numerically coincides with `FLOW_SCHEMA` `1.14.0` — these are independent version lines (the binary vs. the flow-record data shape), not coupled.

## [1.13.1] - 2026-07-01

A stability patch from a review-swarm audit of the recently-shipped code: five
bug fixes across the runtime, fleet queue, serve daemon, and viewer, plus two
message/comment cleanups. No schema changes (`FLOW_SCHEMA` stays `1.14.0`,
`CONFIG_SCHEMA` `1.1`), so it stays fully compatible with a v1.13.0 peer/hub.

### Fixed

- **Compaction no longer orphans a tool-result at the tail boundary** (runtime, #1158). Compaction preserved a fixed head + tail using raw indices; when the preserved tail began on a `tool` result whose parent assistant was in the summarized middle, the next model request failed with HTTP 400 — hard-failing an otherwise-productive dispatch (an opaque "LMStudio returned 400", non-deterministic so it read as flaky). Boundaries now snap off tool-call groups.
- **A dispatch panic no longer silently kills the fleet runner** (fleet, #1159). A panic (not an `Err`) in the dispatch path unwound the runner thread; the daemon kept serving and the presence heartbeat kept emitting, so it looked healthy while the machine stopped claiming work forever. The claim loop now catches the panic, releases the queue lease, and continues.
- **No more silently-lost jobs published before a runner exists** (fleet, #1160). A `--machine` dispatch to a target whose daemon had never run (or a fresh Redis) was dropped, because `XGROUP CREATE … $` parks the group cursor after the message. `publish_job` now ensures the consumer group exists before the `XADD`.
- **`/diff` no longer blocks the async runtime or over-allocates** (serve, #1161). The handler ran three `git` subprocesses inline on an async worker (executor-starvation risk) and buffered git's entire stdout before truncating to 256KB. It now offloads to the blocking pool and streams stdout under the cap.
- **The live viewer no longer leaks per-session ids on a long-lived tab** (viewer, #1162). `runtimeUids` escaped the rolling-window age-out and grew unbounded on an always-on tab (phone dashboard / hub viewer); it's now pruned alongside the window trim.
- The daemon-unreachable nudge is brew-aware ("start the daemon: `brew services start darkmux`" instead of "run `darkmux serve` in another tab"), and the serve-wrapper header comment no longer describes pre-#661 Redis behavior (#1163).

## [1.13.0] - 2026-06-30

The fleet-foundation + self-diagnosing-doctor release: declare a machine's fleet
position, set config without hand-editing JSON, and let `darkmux doctor` catch
the cross-setting traps + tell you where to open the viewer — plus a live-view
UX pass. **No `FLOW_SCHEMA` change** (stays `1.14.0`), so cross-machine flow
stays compatible with a v1.12.0 peer/hub. **`CONFIG_SCHEMA` 1.0 → 1.1** (additive
`fleet{}` block; lenient-read, so an older binary tolerates a newer config).

### Added
- **`fleet.mode` — hub | peer | standalone (#933).** A machine's declared place
  in a multi-node fleet, a `fleet{}` block in `config.json`. The operator
  declares it; `darkmux doctor` shows it with provenance. Downstream fleet
  tooling keys on it.
- **`darkmux config set/get/list` (#937).** Read/write `config.json` from the CLI
  (`darkmux config set redis.host <addr>`, `… fleet.mode peer`) — the key is
  validated against a registry (a typo is surfaced with a suggestion, never
  silently written) and the value coerced to the field's type. Secrets are
  refused with a pointer to the Keychain `security` form.
- **`darkmux doctor` L1 — cross-setting coherence + a verdict banner (#934).**
  New rules catch traps no single check sees: a stale `DARKMUX_*` env var
  shadowing an enabled `config.json` block, and a brew/cargo binary split-brain
  (a daemon serving an older schema than the CLI). Doctor now leads with an
  `● ok / needs attention / broken` verdict naming the highest-severity finding,
  not a flat list.
- **Doctor surfaces the viewer URL (#1155).** The `daemon reachable` line shows
  where to open the viewer — the loopback URL plus, when `tailscale serve` is
  proxying to the daemon, the tailnet/phone URL.
- **Live token tiles + activity-timeline presets (#1151).** The run view's
  tokens-in/out accumulate live (per-turn telemetry) instead of dashing until the
  run ends; the fleet activity timeline gains `10m/1h/4h/24h` presets with a
  now-anchored axis.

### Fixed
- **New runs surface without a manual refresh (#1151).** An SSE backstop re-pulls
  the bounded live window so a run dropped during a Redis reconnect-gap
  self-heals, instead of needing a page refresh.
- **Viewer state survives the live rebuild (#1147 / #1149).** Expanded
  `<details>` no longer snap shut, and the run view's scroll + open state
  survives the ~1/sec live update (render-once + targeted-update).
- **Mobile viewer layout (#1151).** Shortened savings-hero labels, left-aligned
  the breakdown when it wraps, and packed the LIVE badge onto the brand row so
  both machine timelines fit on a phone.

### Changed
- **darkmux self-review profile default → `diff-review` (#1150).** The
  `darkmux-review.yml` workflow now dispatches with the `diff-review` profile by
  default (was `review`).

## [1.12.0] - 2026-06-29

A build-visibility + run-observability release, plus the production-hardening
fixes surfaced by darkmux's first brew-stable production user. **No `FLOW_SCHEMA`
change** (stays `1.14.0`), so cross-machine flow stays compatible — but the
`runtime/` image **is** rebuilt this release (the empty-`tool_calls` recovery),
so a `brew upgrade` pulls a new `darkmux-runtime` image.

### Added
- **Build identity in three places (#1129).** `darkmux --version`, the lead
  `build` line of `darkmux doctor`, and a chip in the observability viewer header
  all show `<version> (<git-sha>)` — or `<version> (release)` on a Homebrew build
  — plus the `flow_schema` version. The package version alone doesn't change
  between releases, so it couldn't tell you whether a running daemon had your
  latest code; the git SHA does.
- **Run drill-down page clarity (#1125).** The per-run page now leads with a
  status pill + `run · <role>` (not "subsystem"), a run brief (runtime / model /
  workspace / mission / timing), **tokens in / out** tiles, and a done-aware
  context tile (a finished run shows peak, not a misleading "now").
- **About modal (#1132).** The header build chip opens an "about · darkmux" modal
  consolidating build / flow-schema / connection / mode / machine / hardware +
  links.
- **The dispatch prompt + runtime image in the run brief (#1127 / #1126).** The
  run page shows the dispatch's prompt (collapsed) and the container image it ran
  in — both previously absent or a dead reference.
- **`darkmux doctor` is issues-only by default (#1130).** It shows the build line
  + any warnings/failures and collapses the passing checks to a count;
  `darkmux doctor -v` prints the full list.
- **`darkmux lab review-bench` (#1119).** A reproducible PR-reviewer eval — a
  labeled diff-mix fixture + a scoring provider — so model bake-offs for the
  review role are repeatable, not one-off.

### Fixed
- **`crew dispatch` honors the profile's `n_ctx` (#1135).** The dispatch resolved
  the model id but let LMStudio JIT-load it at the **model default** (e.g. 4096),
  silently truncating large inputs (a pr-review diff overflowed → garbage review,
  no error). darkmux now loads the selected model at the profile's declared
  context before dispatching (reusing a sufficient resident load, reloading a
  too-small one), and surfaces a clear RAM-hinting error if the load fails. Also
  fixes a latent `lms load` quiet-flag bug that leaked the load spinner into a
  `--json` envelope (and into `darkmux swap --json`).
- **Compaction meter no longer double-counts (#1122).** Each compaction emits two
  flow records (a work event + a token-telemetry record); the viewer folded both
  into the compaction count, reporting 2×. The token-telemetry record is now
  canonical.
- **The runtime recovers from an empty `finish_reason=tool_calls` (#1123).** A
  model returning a wholly empty completion under a `tool_calls` finish reason
  hard-killed the dispatch; it now routes through the same intra-turn stall
  recovery (nudge + retry, bounded) as the empty-`length` case.
- **Internal-path dispatch errors carry the stderr text (#1042).** The internal
  runtime (the default) emitted only `stderr_chars`; it now carries a bounded
  stderr tail excerpt on error, like the openclaw path — so a failed dispatch is
  diagnosable from the flow stream alone.

## [1.11.2] - 2026-06-28

A bug-fix + accessibility + security patch from a board triage. No schema change
(`FLOW_SCHEMA` stays `1.14.0`) and the `runtime/` image is unchanged from
`1.11.0` — a pure `brew upgrade`, no image pull.

### Fixed
- **Live "in flight" derives from presence, not flow records (#857).** A
  hard-killed or orphaned dispatch could read "running" / "dispatch in flight"
  forever. All live-mode activity derivations (fleet card, timeline bars,
  burn-down "+N in flight") now key on presence via one `sessionRunning()` helper
  — an orphan ages out on its own (TTL); playback still uses the durable
  close-edges.
- **Truthful, de-duplicated live status line (#1103).** Dropped the "live"/"today"
  that the badges already show; "last run" now measures real wall-clock elapsed
  (it was stuck on "just now"); the backwards-looking clock range became the
  window scope ("last 24h"); machine presence is decoupled from the record count.
- **Consolidated live headline (#1105).** Dropped the "fleet" wording (wrong for a
  solo local machine) and folded the machine count into a chip glyph.
- **Dispatch-error records carry the stderr text (#1042).** The openclaw-path
  error record had `stderr_chars` (a count) but not the text, so you couldn't see
  *why* a dispatch failed; it now carries a bounded stderr tail excerpt
  (null on success).

### Accessibility
- **Keyboard navigation for the drill cards (#1090).** Fleet → machine → session
  cards were mouse/touch-only; they're now focusable (`role=button` + tabindex via
  a delegated observer), Enter/Space-activatable, with a visible focus ring.
- **Non-color status cue (#1092).** Timeline bars now carry a per-state pattern
  (diagonal/solid/vertical/cross-hatch) and the active cycle stage a dot — state
  is no longer color-only, including under `prefers-reduced-motion`.

### Security
- **`pr_labels` flag-injection guard (#1111).** A repo-declared PR label starting
  with `-` (e.g. `--config`) was passed unvalidated to `gh pr create --label` and
  parsed as a flag; labels are now validated (non-empty, no leading dash) like
  branch names already were.
- **`external pull` argument-injection guard (#1112).** A `--gh`/`--url` target
  starting with `-` was passed unvalidated to the `gh`/`curl` subprocess; targets
  are now rejected before spawn. (The SSRF hardening of `curl -L` remains tracked
  + deferred for the operator-typed threat model.)

## [1.11.1] - 2026-06-28

A focused **viewer + UX pass**, mostly mobile, plus one local-PR-reviewer
reliability fix. The dashboard reads cleaner on a phone, the status colors mean
one thing everywhere, and the chrome is icon-first instead of word-cluttered.

No schema change (`FLOW_SCHEMA` stays `1.14.0`) and the `runtime/` image is
unchanged from `1.11.0` — a pure `brew upgrade`, no image pull.

### Changed
- **Unified status-color convention (#1071).** Cards, recent-runs rows, and the
  activity timeline now share one enum: green = success/complete, yellow + pulse
  = running, orange = canceled, red = failed/killed. A watchdog kill reads as
  red, not as a disabled-gray "complete".
- **Icon-first chrome (#1067).** The filters/history/follow/back/play controls
  are now compact icons; the filter is a funnel (not a settings gear) and follow
  is a clock to read as real-time (#1098). History opens from the "today" badge,
  retiring the button that looked like a stop control.
- **Local-timezone timestamps (#1069).** Absolute times render in the browser's
  zone instead of the record's machine zone.
- **Fleet machine cards redesigned (#1095).** Uniform size, a default machine
  icon, and a tighter stat line with state on its own row.
- **Savings hero on the missions tab, full-width (#1096).** It now shows on
  missions (not just fleet) and spans the column with no dead right gutter,
  aligning with the timelines below it.
- **Default avatar on crew role cards (#565).** A person icon stands in until a
  role-specific avatar is set.
- **Dropped the redundant "Live" word from the source badge (#1065)** and
  consolidated the savings-hero green onto the `--good` token (#1083).

### Fixed
- **Mobile log pane (#1100, regression from #1089).** The event list gets room
  again instead of being squeezed to two or three visible events.
- **Mobile responsive hardening (#1089).** Fixed-width elements no longer
  overflow the viewport on phones; icon-only controls meet touch-target size
  (#1087).
- **Back button shows only when there's somewhere to go (#1072/#1074)** and is
  otherwise removed — the breadcrumb and lens tabs already cover navigation
  (#1094).
- **Empty-state placement (#1070).** The "no activity" hint drops below the crew
  cards instead of crowding beside them, and a spurious stray label is gone.
- **Accessibility:** an `aria-label` on the rewind glyph button (#1080).
- **PR reviewer no longer copies its own example (#1084).** The role prompt's
  worked-example finding was being emitted verbatim by small models as a real
  (false-positive) finding; the response grammar already enforces output shape,
  so the copyable example is gone.

## [1.11.0] - 2026-06-27

darkmux's local **PR reviewer** got materially better and self-contained. It now
reads each change against its **stated intent** (the PR title + description), so it
stops flagging the very bug a fix removes; it anchors findings by **quoting the
line** and resolving that quote to a coordinate in the harness (local models name
the construct reliably but guess line numbers badly); and the whole review-render
step now lives **in the binary** (`darkmux pr-review render`), versioned with the
role schema, instead of a copied script every repo had to keep in sync. darkmux
also reviews **its own PRs** in public, on a local model, via a self-hosted runner.

No schema change (`FLOW_SCHEMA` stays `1.14.0`) — a clean `brew upgrade`. The
`runtime/` image is rebuilt for the reasoning-content fix below, so a fleet on the
internal runtime pulls the new `darkmux-runtime` image.

### Added
- **`darkmux crew dispatch --profile <name>` (#1054).** Select a named profile
  from the machine's registry for a dispatch's model + context-window resolution;
  a name not defined on this machine falls back to `default_profile` (with a
  note). Lets a machine-agnostic caller (a CI workflow) name the profile it wants
  while each machine owns which lab-validated model that maps to.
- **Intent-aware PR review (#1053).** The `pr-reviewer` role now assesses the diff
  against the PR's stated purpose (title + description, fetched procedurally — no
  AI), flagging only where the change *fails* its intent, not the problem it's
  solving. Validated head-to-head: an 8B and a 122B both stopped false-flagging a
  correct fix once given the intent — input-shaping over raw model size.
- **Quote-the-line anchoring for review findings (#1053).** Findings carry an
  `anchor` (a verbatim quote of the line) instead of a line number; the harness
  resolves it to the exact new-side line. Mis-located inline comments go away;
  file-level findings post as general comments instead of onto a guessed line.
- **`darkmux pr-review render` (#1060).** Binary-owned generation of the GitHub
  review payload from a dispatch envelope + diff (resolve anchors → inline
  comments + summary). Replaces the per-repo `pr-review-post.py` copy, so the
  render versions *with* the role's output schema and never silently drifts; the
  workflow keeps the `gh` post, and `--emit` writes the payload for full control.
- **darkmux self-review workflow (#1047) + overridable `role`/`profile` inputs
  (#1057).** darkmux reviews its own PRs on a local model (no cloud API), on a
  self-hosted runner, posting native inline comments — `workflow_dispatch`-only
  for public-repo safety. `-f role=` / `-f profile=` override the dispatch per run.

### Fixed
- **Thinking models no longer return empty reviews (#1050).** qwen3_5-family
  models routed their whole answer to `reasoning_content`, leaving message
  `content` empty; the runtime now promotes terminal reasoning to content (guarded
  so it never disables the length-runaway stall recovery).
- **Viewer phantom "unknown" machine card (#1048).** The flow stream's
  schema-header line was bucketed as a machine in the topology view; it's now
  skipped.

## [1.10.0] - 2026-06-26

A local model can now run as an automated **PR reviewer**: a tool-less role
reviews a diff and emits a structured, cite-the-line JSON review that CI posts
back as native inline pull-request comments — and the runtime can now
grammar-constrain any role's output to a declared schema, so a small local
model cannot emit malformed JSON.

### Added
- **Tool-less `pr-reviewer` role (#1037).** Reviews a unified diff provided
  inline and emits a structured, cite-the-line JSON review (path + line +
  severity + detail + how-to-fix advice + optional one-click suggestion),
  designed for CI to post as inline PR comments. No repo, no shell, no tools —
  pure reasoning over the given diff.
- **Grammar-constrained structured output — `output_schema` on a role (#1039).**
  A role manifest can declare an `output_schema` (JSON Schema); the internal
  runtime passes it to LMStudio as `response_format: json_schema` (strict), so
  the model is grammar-constrained to emit exactly that shape — the structural
  cure for local-model JSON malformation, vs post-hoc repair. Backward-compatible:
  roles without `output_schema` behave exactly as before.
- **`pr-reviewer` findings carry `advice` + `suggestion` (#1044).** Each finding
  has `advice` (prose how-to-fix, always present) and `suggestion` (the exact
  literal replacement line for a clean one-line fix, or `null` — rendered as a
  one-click GitHub suggestion). Keeps fix-guidance on every finding while
  reserving the one-click path for fixes that actually apply cleanly.

### Fixed
- **`output_schema` nullable fields use `anyOf`, not a type union (#1040).**
  LMStudio's grammar compiler rejects `"type": ["string","null"]` (`ValueError:
  'type' must be a string`); nullable fields are now expressed as
  `anyOf: [{"type":"string"},{"type":"null"}]`. A builtin-role strict-safety
  test now guards the rule. Caught dogfooding the live `pr-reviewer` dispatch.
- **Capability-aware verification boundary for `code-reviewer` + `test-designer`
  (#1035, #400).** The post-dispatch verification rule no longer holds these
  roles to a code-mutation check they aren't expected to satisfy.

## [1.9.0] - 2026-06-23

The dispatch-to-PR loop's engagement-context cure goes from foundation to
finale: the loop can now key cautions to the code they fired on, rank what's
relevant to the dispatch, budget what it injects, and **measure** whether the
injected memory changed behavior. Plus dispatch ergonomics for substantial briefs.

### Added
- **Lessons sovereignty verbs — `darkmux lessons edit/remove/export/import/recall`
  (#1003).** Full operator curation of the engagement-context lessons store
  (`add`/`list` shipped in 1.8.0): in-place edit, delete, a self-describing JSON
  export/import roundtrip (idempotent, order-independent), and read-only recall.
- **Loop-lab engagement-context A/B — `darkmux lab loop --ab` (#1004).** Run the
  same workload twice, once with the injected lessons/cautions and once without,
  and report the verdict shift — the empirical proof of whether institutional
  memory changes loop behavior. `--inject-from-mission <id>` scopes the cautions.
- **`crew dispatch --message-from-file <path>` (#386).** Pass a substantial brief
  from a file instead of the command line. The message now flows to the runtime
  via a bind-mounted file rather than `docker run` argv, so a large brief can't
  hit ARG_MAX or show up in `ps`.
- **Proportional injected-context budget (#1011).** The coder brief's injected
  context (cautions + lessons + corrections) is budgeted as a fraction of the
  model's context window with per-authority floors, replacing three flat counts.
  Tunable via `runtime.injected_context_fraction` / `DARKMUX_INJECTED_CONTEXT_FRACTION`.

### Changed
- **Staleness-aware cautions (#1001 + #1002).** Detector firings now capture a
  BLAKE3 hash of the file they fired on; at retrieval, a caution about a file
  whose content has since changed is ranked **down** as stale. Cautions and
  lessons about a file the dispatch will touch rank **above** engagement-level
  ones (file-in-play precision).
- **Prior-sprint output is capped in the brief (#146).** Each dependent sprint's
  injected upstream output is bounded (default ~8000 chars, `DARKMUX_SPRINT_CONTEXT_MAX_CHARS`)
  so a long parent reply can't crowd a small model's window.

### Internal
- Test coverage for the fleet routing completion-matching path (#842).

## [1.8.0] - 2026-06-23

The dispatch-to-PR loop learns from its own failures, gains a closing ceremony,
and the live observability viewer stops asserting state it can't see and starts
showing what it actually observes.

> **Cross-machine schema note.** `FLOW_SCHEMA` bumped **1.13.0 → 1.14.0**: the
> dispatch lifecycle now emits a `Stage::Debrief` value (the NASA-vocabulary
> rename of the old `retrospect` stage). A single machine is unaffected. In a
> **mixed-version fleet**, upgrade every machine together — an older binary does
> not recognize the `debrief` stage value in records written by a 1.8.0 peer.

### Added
- **Engagement-context layer — the doom-loop cure (#994).** The dispatch-to-PR
  loop now closes the detect → distill → inject → don't-repeat loop. Detector
  firings capture the engagement-context files they touched (#995); the index
  derives **cautions** from the flow stream (#996); those cautions surface in
  the next coder brief so a known failure is not silently re-walked (#997); and
  a durable SQLite **lessons** store backs operator-authored conventions —
  `darkmux lessons add/list` — which inject into the brief alongside the
  auto-derived cautions (#998). Two tiers: per-repo and global.
- **Mission debrief ceremony — `darkmux mission debrief <id>` (#1000).** A
  closing read on a finished mission: sprint/mission status, the diffs and flow
  history it produced, and a distiller skill (`darkmux-mission-debrief`) that
  turns the run into reusable lessons. `mission close` now nudges toward it.

### Changed
- **NASA vocabulary, end to end (#999).** The engagement-context store and verb
  are now **lessons** (was `knowledge`); the dispatch lifecycle's closing stage
  is **`Debrief`** (was `Retrospect`), bumping `FLOW_SCHEMA` to 1.14.0 (see the
  cross-machine note above). A vestigial index table was dropped.

### Fixed
- **Viewer derives liveness from the flow stream when Redis presence is down
  (#1007).** With the presence substrate unreachable, running/ended state now
  falls back to recent flow activity instead of showing an empty fleet.
- **Per-dispatch drill-down scopes to the latest attempt (#1013).** A re-run no
  longer blends the prior attempt's subsystem trace into the current one.
- **Operator-state resolves to the user scope, not a project `Auto`-scope
  (#1012).** `lessons add` in a repo no longer silently creates a project-local
  `.darkmux/` that shadows the user's missions and lessons.
- **doctor tags eureka rules by declared runtime, not a substring match (#1010).**
  OpenClaw-only rules are suppressed without `--openclaw` by a `RuleKind::runtime()`
  classification rather than matching the string "openclaw".
- **Observability viewer shows observed state, not asserted fiction.** The
  session CPU chart is relabeled **container CPU** — tool work, not the
  inference that runs off-container in LMStudio (#814); the utility card and
  machine spec line render the model's **observed** residency
  (resident / registered-not-loaded / not-configured / not-reported) instead of
  a hardcoded "resident" (#1008); and the spec line reports RAM in GiB so a
  128 GB machine reads **128 GB**, not 137 (#1020).

## [1.7.0] - 2026-06-22

Loop-engineering tooling and correctness: a bench for measuring how a dispatch
loop behaves, and a fix for the wrong-diagnosis-stuck failure mode.

### Added
- **Loop lab — `darkmux lab loop <workload>` (#986).** A single-run
  loop-engineering bench. Run one dispatch under a chosen harness config and get
  back a verdict for how the loop behaved: `productive`, `struggled` (a loop
  detector fired and the harness bounded it), `inert-false-pass` (the model made
  no tool calls yet verify reports pass because the baseline passes regardless),
  or `failed`. Two loop-variation axes: caps (`--max-turns` / `--max-tokens` /
  `--timeout`) and compaction (`--compact-threshold-tokens` /
  `--compact-threshold-ratio` / `--compact-strategy` / `--bail-after-compactions`
  / `--context-window`); the model axis comes from `--profile` /
  `--profiles-file`. `--json` for programmatic use. The report reads the run's
  trajectory, metrics, and sandbox hashes; no new infrastructure.

### Changed
- **Prior reviewer corrections read as findings-to-verify, not directives
  (#453).** In the dispatch-to-PR loop a confident-but-wrong reviewer diagnosis
  could anchor the next coder into a no-progress loop. Corrections injected into
  a follow-up coder brief, and the code-reviewer and coder role prompts, now
  frame a prior finding as something to verify against the live workspace before
  applying: a concrete change (a renamed field, a command) gets a quick check; a
  diagnosis (a race condition, a failing test) gets reproduced first. A
  correction that does not hold is re-diagnosed, bounded by the existing
  escalation contract. The #849 carry-forward is unchanged.

### Tests
- **Coverage pass (#842).** Closed the genuine remaining gaps in the fleet
  queue-claim decode path (`parse_xreadgroup_response` protocol-shape errors,
  `extract_field` edge cases), the docker-run argv builder (compaction-strategy
  mapping, allowed-tools block-all vs allow-all, the feedback-templates guard),
  and `build_work_job` (the cross-machine WorkJob constructor, previously
  untested). Test-only; no behavior change.

## [1.6.0] - 2026-06-21

Dispatch-to-PR loop correctness, and the lab made fit for profile development.

### Added
- **Corrections persist into the next coder brief (#849).** A correction the
  reviewer records at the gate (`flow note --source adjudication`) is now
  injected into the next dispatch's brief for the same mission — a correction
  made once is carried forward, not re-derived (the doom-loop fix). Injected as
  provenance-framed context (the count + each correction surfaced at dispatch
  time), never a silent rule. Plus a codified recheck-vs-rethink escalation
  policy in the agent docs.

### Fixed
- **`lab run --profiles-file` now reaches the dispatch's model resolution
  (#984).** The flag resolved the profile for lab run's own bookkeeping, but the
  dispatch re-resolved its model from `env > default` — silently using the wrong
  model, which blocked profile development. `config_path` is now threaded end to
  end; `lab tune` / `lab characterize` inherit the fix. No behavior change off
  the lab path.

## [1.5.0] - 2026-06-21

Dispatch-to-PR loop robustness. The headline is the verifier-fabrication
backstop: when a coder's verifier command (e.g. `cargo test`) *failed to run* —
never executed — `mission ship --merge` now holds the auto-merge for human
review instead of trusting a SIGNOFF that may rest on a command that never ran.

### Added
- **Verifier-fabrication gate (#799).** `mission run` parses the dispatch
  envelope's `failed_tool_invocations` (stamped by the runtime in 1.4.x), emits
  a per-run `mission.run.verification` flow record, and prints a gate banner
  naming any verifier that failed to run. `mission ship --merge` reads the
  latest run's record back and **holds** the auto-merge (new exit code `3` — PR
  stays open, worktree intact, never torn down) when the latest run had
  failures. Soft everywhere: never auto-fails, never auto-ships, only holds for
  human review. New flow action `mission.run.verification`; `FLOW_SCHEMA` is
  unchanged (additive action, not a shape change).

### Changed
- **Single source of truth for the `docker run` argv (#847).** The four
  arg-builder helpers (volume mounts, runtime injection, cache mount, compaction
  flags) are no longer duplicated between dead helpers and an inline copy in
  `build_docker_run_argv` — the helpers are the one impl and `build_docker_run_argv`
  delegates to them. Eliminates the divergence trap behind earlier dispatch
  regressions (same bug-class as the 1.4.1 hotfix). No behavior change — the
  emitted argv is byte-identical.

## [1.4.1] - 2026-06-21

Hotfix. The internal-runtime dispatch (`darkmux crew dispatch`, `darkmux
mission run`) was broken in 1.3.x–1.4.0: it invoked `docker docker run` and
exited 125, so the local-AI dispatch-to-PR loop could not start. `--runtime
openclaw` was unaffected. `brew upgrade darkmux` restores it; no schema or
config-surface change.

### Fixed
- **Internal-runtime dispatch ran `docker docker run` (exit 125) (#975).**
  `build_docker_run_argv` returns the full command with the program name at
  `argv[0]` (`["docker", "run", "--rm", …]`), but the consumer pushed the whole
  vector as arguments to `Command::new("docker")`, duplicating the program.
  Split it (program = `argv[0]`, args = `argv[1..]`). Regressed in #848 and
  shipped silently because the tests only asserted the argv vector, never the
  constructed `Command` — the dispatch-argv coverage gap #842 flagged. Added a
  regression test that inspects the real `Command`.

## [1.4.0] - 2026-06-19

Completes the milestone-1.0 hardening pass. The `--json` machine-readable
output convention is now consistent across the read commands the frontier
orchestrator parses (the additive feature that makes this a minor), plus three
batches of correctness/safety polish from the swarm code review. No schema or
config-surface change; `brew upgrade darkmux` is a drop-in.

### Added
- **`--json` parity across the read commands (#907).** `status`, `profiles`,
  `model status`, `recommendations show`, and `role list`/`show` now accept
  `--json`, emitting machine-readable output for the frontier orchestrator
  instead of ANSI-styled text. Each serializes its existing domain shape;
  `role list --json` carries the full (untruncated) description.

### Fixed
- **Serve-daemon request-rate hardening (#925).** A per-route request timeout,
  a cap on concurrent SSE streams, and a bounded per-line read on the flow file,
  so a slow or abusive client can't exhaust the daemon.
- **Runtime nit-batch (#905).** XML tool-call promotion now fails soft per block
  (one malformed `<tool_call>` no longer drops the whole turn's recovered calls);
  the `TIMED OUT` marker only fires when the `timeout` wrapper actually ran (a
  user command exiting 124 isn't mislabeled); a failed non-JSON dispatch prints
  a summary instead of vanishing behind a bare exit code. Plus doc corrections
  (first-close-wins think-block scan; Bash isn't workspace-validated).
- **Lab / flow / profiles / hardware / crew nit-batch (#906).** Escalation
  hand-off targets are validated before the index rebuild (a clear, role-named
  error instead of an opaque deferred-FK abort that rolled back the whole
  rebuild); loaded-context sufficiency compares in `u64` (no truncation); an
  all-`.` `setupContent` key is rejected up front; `doctor` treats a TOCTOU
  file deletion as Pass, not a spurious Warn; Linux `physical_cores` counts
  physical cores (not logical); manifest reads have a 1 MiB cap; `lab register`
  warns that a fixture's `verify_command` runs on the host shell.
- **CLI / dispatch nit-batch (#907).** `mission migrate --apply` refuses to
  clobber an existing destination; `mission run`/`ship`/`abort` work for repos
  at non-ASCII / special-char paths (git C-quoted porcelain decode); docker
  image refs are validated before reaching docker; `external pull --url`
  allowlists `http(s)`; the default daemon port is single-sourced (correct for
  IPv6 / port-less addresses).

## [1.3.4] - 2026-06-19

The third milestone-1.0 safety-net cluster — fleet-substrate + correctness
fixes. No schema or config-surface change; `brew upgrade darkmux` is a drop-in.

### Fixed
- **Memory-headroom estimate tolerates more size formats (#904).** `eureka`'s
  `parse_size_gb` dropped `"18.45 GiB"`, `"18.45GB"` (no space), and comma
  sizes to `0`, undercounting the working set so the `MemoryHeadroomTight`
  warning under-fired (a tight system read as fine). It now parses binary
  (`GiB`/`MiB`/`TiB`) and no-space forms, and reports `Skipped` (naming the
  model) when a size truly can't be parsed instead of silently undercounting.
- **`notebook list` exits 0 when the dir is absent (#895).** A fresh user (or
  `notebook list && …` chaining) no longer sees a false error exit for a
  read-only "nothing to list".
- **Malformed work entries are XACKed, not leaked into the PEL forever (#903).**
  A claimed-but-unparseable fleet work entry (missing `record`, bad JSON, or a
  non-array fields slot) is now dropped from the consumer's pending-entries
  list via a new `Malformed` claim outcome, instead of being mistaken for a
  connection error and left pending indefinitely.
- **Presence reconciler closes two edge races (#902).** A failed close-edge
  write now releases its dedup claim so a peer can still record it (no lost
  `machine.offline`/`session.end` bracket), and the first tick after a
  `read_live` outage rebaselines instead of re-firing long-gone machines as
  fresh disappearances. (Also fixed a latent test-isolation flake surfaced
  along the way.)

### Changed
- **Doc-only: the fleet work-queue `schema` tag is documented as provenance,
  not a compat gate (#882).** Cross-version compatibility is enforced by serde
  shape (`deny_unknown_fields` + required-field deser), as the canonical
  `WORK_JOB_SCHEMA_VERSION` doc already states; the publish-side over-claim is
  corrected to match. No behavior change.

## [1.3.3] - 2026-06-19

A crash-path-hygiene patch — the second cluster of the milestone-1.0
safety-net drain. Four fixes that stop dispatches from corrupting operator
config or leaking resources on crash/error paths. No schema or config-surface
change; `brew upgrade darkmux` is a drop-in.

### Fixed
- **Atomic writes to `openclaw.json` (#901).** `apply_runtime` and the
  `doctor --fix` path wrote the operator's runtime config with a bare
  `fs::write` (truncate-then-stream); a crash / ENOSPC / power-loss mid-write
  could leave the operator's whole hand-authored config (`agents.list[]`,
  channel routing) empty or truncated. Both now write to a sibling temp and
  `rename(2)` onto the file, so a crash leaves the old config intact.
- **Lab-registry temp name is collision-free across threads (#898).** The
  atomic-save temp was process-unique only (`json.tmp.{pid}`); since `save()`
  is `pub(crate)`, two threads racing it could tear the temp before the rename.
  It's now process- and call-unique (`json.tmp.{pid}.{counter}`).
- **Dispatch tears down the watchdog and kills the container on a wait error
  (#889).** If `wait_with_output` itself failed, the dispatch returned without
  signaling the watchdog or killing the container — leaking a watchdog thread
  (which then fired a spurious kill) and potentially orphaning a running
  container until its deadline. The error path now stops the watchdog/sampler
  and best-effort `docker kill`s by the deterministic container name.
- **Auto dispatch workspaces are reclaimed on error/panic (#888).** A
  no-`--workdir` dispatch allocates a throwaway scratch tree in `/tmp`; it was
  never cleaned, so repeated failed dispatches accumulated trees (slow
  disk/inode exhaustion). An RAII guard now reclaims the auto-workspace on an
  error/panic exit before the container completes. An operator `--workdir` is
  never touched, and the bookkeeping dir (trajectory/metrics) is always
  retained so failed dispatches stay debuggable.

## [1.3.2] - 2026-06-19

A robustness patch — the first cluster of the milestone-1.0 safety-net drain.
Five agent-loop / runtime correctness fixes, no schema or config-surface change;
`brew upgrade darkmux` is a drop-in.

### Fixed
- **Hard-kill watchdog survives a poisoned deadline mutex (#890).** The inactivity
  deadline is shared between the trajectory tailer and the host watchdog; a panic
  in the tailer while holding the lock poisoned the mutex, and the watchdog's
  `.lock().unwrap()` then panicked on its next tick — silently disabling the
  hard kill so a stuck dispatch could hang forever. All deadline lock sites now
  recover a poisoned lock, making the safety-net thread the most panic-resilient
  consumer rather than the least.
- **Error-path metrics no longer mislabel infra failures as turn-cap hits (#884).**
  The loop-error branch hardcoded `max_turns_reached: true`, so every
  infrastructure failure looked like a turn-cap termination, corrupting the
  three-way result discrimination downstream consumers branch on. It now reports
  `false`, matching the success path's derivation.
- **Compaction reports the true summary size (#885).** `summary_chars` was read
  from a fixed `messages` index (assuming the preserved head was exactly two
  messages); the compaction functions now return the inserted summary's actual
  char count, so the observability field can't silently report an unrelated
  message's length.
- **Failure-cascade detector framing corrected (#886).** The per-`(tool, args)`-
  signature failure counter was named `consecutive_failures` and described as
  "consecutive / in a row" across the runtime, the host flow message, and the
  analyze-run skill doc — none accurate. Renamed to `failure_count` and reworded
  to the real per-signature semantics. Behavior unchanged.
- **`mission propose` JSON extraction handles malformed model output (#896).**
  `extract_json_block` now prefers a ` ```json `-tagged opener over a bare fence
  (so a bare code block before the real JSON can't capture the wrong region) and
  emits a distinct "unterminated fenced block" error on truncated output instead
  of a misleading "no block found".

(Also: #887 — the inactivity soft-warning's inability to fire mid-stream — was
confirmed working-as-intended and documented; the host hard kill covers
within-turn hangs. No behavior change.)

## [1.3.1] - 2026-06-18

A security-hardening patch. Drains the milestone-1.0 security cluster — five
fixes that close workspace-escape, traversal, and denial-of-service surfaces
across the runtime, lab, serve daemon, and crew/flow subsystems — and finishes
the daemon colorization started in 1.3.0. No schema or config-surface change;
`brew upgrade darkmux` is a drop-in.

### Fixed
- **Runtime refuses writes through a final-component symlink (#883).** A coder
  dispatch could previously be steered into writing through a symlink whose final
  path component pointed outside the mounted workspace. `resolve_write` now
  `lstat`s the final component and refuses a symlink target, closing the escape.
- **Lab validates the sandbox-seed path and stops following symlinks (#897).**
  `coding_task` now rejects seed-key paths that escape the sandbox base
  (canonicalized + `starts_with` containment on both sides) and copies seed
  directories with a no-follow walk, so a symlinked seed entry can't read or write
  outside the run sandbox.
- **Serve daemon bounds the per-day flow-file read (#900).** `/flow/:date` now
  streams the file and keeps only the newest 10,000 records in a ring buffer
  instead of loading an unbounded file into memory, removing a memory-exhaustion
  vector. (Broader request-rate limiting is tracked in #925.)
- **`crew sync` requires `--yes` to write `openclaw.json` (#893).** A bare
  `crew sync` now previews the pending changes and bails with a re-run pointer
  rather than silently mutating operator-owned `openclaw.json`; `--dry-run`
  previews without the gate. Restores the preview-then-confirm sovereignty
  contract.
- **Audit re-seed requires a schema header (#899).** `flow integrity-check` only
  re-seeds the hash chain from a single-line file when that line is the schema
  header; a non-schema single line now bails instead of silently anchoring the
  chain to arbitrary content. The "tamper-evident" phrasing across the code, docs,
  and README is scoped to the detection property the `integrity-check` verb
  actually provides.
- **Colorized the remaining daemon runtime output (#922).** The presence,
  reconciler, fleet-runner, and routing error/warning lines now render through the
  shared style module (TTY- and `NO_COLOR`-gated), completing the daemon
  colorization begun in 1.3.0 (#918).

## [1.3.0] - 2026-06-17

Hardens the serve daemon and the crew index. The headline is **serve daemon
authentication** (#881), which closes the last unauthenticated exposure when the
daemon binds beyond loopback — alongside a fix for a daemon shutdown hang and a
cluster of crew-index correctness repairs.

### Added
- **Serve daemon authentication (#881).** The flow daemon can require a bearer
  token: remote reads and `/diff` are gated while loopback stays open (the local
  viewer is unaffected), and `/health` is always exempt. The token lives in the
  macOS Keychain (`darkmux-serve-token`) or `DARKMUX_SERVE_TOKEN` — never plaintext
  config — and `fleet status --deep` forwards the shared token to peers. `darkmux
  doctor` and the startup banner report the auth posture.
- **Colorized daemon runtime output (#918).** The serve and fleet-runner runtime
  error/warning lines now render red/yellow through the shared style module
  (TTY- and `NO_COLOR`-gated), matching `doctor` and the startup banner.

### Changed
- **BREAKING (narrow): `darkmux serve` refuses a non-loopback `--bind` unless a
  token is configured (#881).** The default install is unchanged — loopback bind,
  no token, the viewer works as today. Only the previously-allowed "bind to a
  non-loopback address with no authentication" setup is now refused (it exposed
  flow records, machine specs, mission state, and live `git diff` to any reachable
  peer). Set a serve token to bind beyond loopback. No action needed for default
  or loopback users.

### Fixed
- **Serve daemon shutdown hang (#918).** The force-exit watchdog ran as a tokio
  task that was cancelled when the runtime dropped, so a wedged background thread
  (e.g. a Redis worker pointed at an unreachable endpoint) could hang the daemon
  after "clean shutdown" printed. The watchdog now runs on a dedicated OS thread
  and guarantees the process exits within the grace window.
- **Crew index self-heals across schema changes (#914).** `darkmux role list`/`show`
  and `crew list`/`show` rebuild the local index on demand, and a schema-drifted
  index (e.g. the mission/sprint timestamp columns) no longer crashes the rebuild
  or silently serves stale data. No operator action — the index auto-rebuilds.
- **Crew index correctness cluster (#894, #891, #892).** `role show` no longer
  errors when a hand-off target row is missing; drift detection catches content
  edits that don't advance mtime; manifest ids strip exactly one `.json`, and
  `load_skills` keys on the authoritative body id so a misnamed user skill
  overrides the builtin.
- **Activity lane brackets `session.end`-only sessions as ended (#856),** so an
  idle machine's bar no longer stretches to the playhead; adds the first
  viewer-lifecycle e2e regression gate.

[1.14.1]: https://github.com/kstrat2001/darkmux/releases/tag/v1.14.1
[1.14.0]: https://github.com/kstrat2001/darkmux/releases/tag/v1.14.0
[1.13.1]: https://github.com/kstrat2001/darkmux/releases/tag/v1.13.1
[1.13.0]: https://github.com/kstrat2001/darkmux/releases/tag/v1.13.0
[1.12.0]: https://github.com/kstrat2001/darkmux/releases/tag/v1.12.0
[1.11.2]: https://github.com/kstrat2001/darkmux/releases/tag/v1.11.2
[1.11.1]: https://github.com/kstrat2001/darkmux/releases/tag/v1.11.1
[1.11.0]: https://github.com/kstrat2001/darkmux/releases/tag/v1.11.0
[1.10.0]: https://github.com/kstrat2001/darkmux/releases/tag/v1.10.0
[1.9.0]: https://github.com/kstrat2001/darkmux/releases/tag/v1.9.0
[1.8.0]: https://github.com/kstrat2001/darkmux/releases/tag/v1.8.0
[1.7.0]: https://github.com/kstrat2001/darkmux/releases/tag/v1.7.0
[1.6.0]: https://github.com/kstrat2001/darkmux/releases/tag/v1.6.0
[1.5.0]: https://github.com/kstrat2001/darkmux/releases/tag/v1.5.0
[1.4.1]: https://github.com/kstrat2001/darkmux/releases/tag/v1.4.1
[1.4.0]: https://github.com/kstrat2001/darkmux/releases/tag/v1.4.0
[1.3.4]: https://github.com/kstrat2001/darkmux/releases/tag/v1.3.4
[1.3.3]: https://github.com/kstrat2001/darkmux/releases/tag/v1.3.3
[1.3.2]: https://github.com/kstrat2001/darkmux/releases/tag/v1.3.2
[1.3.1]: https://github.com/kstrat2001/darkmux/releases/tag/v1.3.1
[1.3.0]: https://github.com/kstrat2001/darkmux/releases/tag/v1.3.0

## [1.2.0] - 2026-06-15

The stability + security hardening release. A multi-agent code review swept the
whole codebase; this release lands the remediation — closing a path-traversal
write primitive, an audit-record loss gap, a config-precedence bypass, and two
runtime panics — alongside dispatch-boundary hardening and richer CLI output.

### Added
- **Colorized dispatch/lab telemetry + tabular CLI verbs (#776).** Run and lab
  telemetry render in color, and the tabular verbs align cleanly for at-a-glance
  reading.
- **`mission ship` is commit-identity-aware (#834).** It honors
  `conventions.json` `commit_author` and enforces a separation-of-duties guard.
- **Dispatch-boundary hardening.** Queue-originated `WorkJob.image` is validated
  at the queue boundary (#838) and `WorkJob.workdir` is base-restricted under
  `~/.darkmux/worktrees` (#840); the dispatch `docker run` invocation is hardened
  (#839).

### Fixed
- **Path traversal from untrusted model output (#867).** Model-supplied
  `mission.id` / `sprint.id` are validated with `fleet::validate_identifier`
  before any path construction, closing a constrained arbitrary-`.json`-write
  primitive in `mission propose`.
- **Audit-record silent loss (#877).** A dropped `AuditFileSink` write now leaves
  a durable breadcrumb in the local sink and `doctor` surfaces the dropped-write
  count, instead of a record vanishing under the best-effort `TeeSink`.
- **Config-precedence bypass (#875).** Production `DARKMUX_*` reads
  (`redis.stream`/`maxlen`, `audit.dir`/`enabled`, `default_role`, CORS origins)
  now route through `config_access`, so `config.json`-only operators get their
  settings honored.
- **Runtime panic on multibyte input (#873).** The compaction slot cap clamps to
  a char boundary before truncating, so a non-ASCII objective no longer panics
  `apply_slot_caps`.
- **Lab harness panic on non-ASCII (#869).** `detect_claim_verify_mismatch`
  builds its excerpt in a consistent index space, so a non-ASCII window around a
  matched claim phrase no longer panics after the dispatch ran.
- **`requires_fixture` honesty (#871).** The matcher is documented as literal
  `name@version` and loudly rejects semver operators that would silently never
  match.
- **Stale `prompt_tokens` (#854).** A stale token count is detected and a local
  estimate substituted for the compaction trigger, fixing a suppressed
  compaction + phantom context drop.
- **`mission ship` from inside a worktree (#844, #846).** Post-merge
  sprint-complete + teardown no longer silently drift when run from the worktree
  layout; the viewer counts `session.end` as a dispatch terminal (#856).
- **Config tier no longer leaks into tests (#811).** Test builds neutralize the
  config tier by construction, so test flow records never reach the operator's
  real Redis stream and default-assertion tests don't flake on a populated
  `config.json`.

### Documentation
- Research-grounded `ROADMAP.md` with themed post-1.0 milestones (M4 loop-depth
  lead) and verified per-theme citations (#850, #853).
- Orchestrator-first getting-started, post-1.0 framing, screenshot refresh, and
  an em-dash cleanup pass across the public docs (#858, #859, #860, #861, #862,
  #863).

[1.2.0]: https://github.com/kstrat2001/darkmux/releases/tag/v1.2.0

## [1.1.0] - 2026-06-14

The work-level observability release: missions become a first-class lens —
across the fleet, in the CLI, and on the dashboard — so you can see how a
mission progresses (sprints + the run→qa→gate→ship cycle), not just what each
machine is doing.

### Added
- **Missions lens in the viewer (#827).** A `fleet | missions` toggle adds a
  work-centric view alongside the machine-centric fleet view — "all machines as
  one" at the work level. A missions index lists every mission with sprint
  progress + cross-machine token rollup; the detail renders the durable sprint
  plan with each sprint's status and a **run → qa → gate → ship cycle strip**,
  click-through to the per-machine run (#828, #832, #833).
- **`darkmux mission status` (#829).** The global mission-control read,
  completing the `<noun> status` family (`flow status`, `model status`): every
  mission grouped by status with sprint progress, the drift that needs
  attention (a Closed mission with a non-terminal sprint; an open mission whose
  sprints are all done), and copy-pasteable, state-accurate reconcile commands.
  Read-only; `--json` for the orchestrator / CI (#830, #831).

### Fixed
- **Live-diff no longer flickers/reloads (#826).** The session-view diff panel
  was rebuilt on every live record (~1/sec during a run), destroying its DOM
  and scroll; it now paints into a stable mount, repainting only on real
  changes with scroll preserved.

### Internal
- A bundled maintainer skill, `darkmux-point-release`, standardizes this release
  ceremony (not shipped to brew installs).

[1.1.0]: https://github.com/kstrat2001/darkmux/releases/tag/v1.1.0

## [1.0.0] - 2026-06-13

darkmux 1.0 — semver stability begins. The release that closes the loop:
darkmux now runs the full local dispatch-to-PR cycle, shows the work (and the
savings) live, and was used to build itself — the observability features in
this release were shipped through `mission run`, and the savings figure on
darkmux.com is this release's own development telemetry.

### Added
- **`mission run` / `mission ship` / `mission abort` — the local dispatch-to-PR
  loop (#782).** `run` creates an isolated git worktree, dispatches the coder
  (sprint-bound, internal runtime), runs the local `code-reviewer` QA against
  the diff, and STOPS at a sign-off gate; `ship` commits, pushes, opens the PR,
  and (opt-in, green-gated) squash-merges — never auto-merge (#786, #787, #788).
- **Verbatim spec fidelity (#815).** `mission propose --ticket <ID>` stamps the
  operator's unabridged input onto the mission; every coder brief carries it
  under an authority-stamped provenance block, so exact strings and constraints
  survive the mission-compiler's summarization (#820).
- **Repo-level shipping conventions (#816).** `<repo>/.darkmux/conventions.json`
  — branch/commit-subject/PR-title templates with `{ticket}`/`{sprint}`/
  `{mission}`/`{subject}` vars, a PR body template, and PR labels. Ship pushes
  the worktree's actual branch, so mid-flight conventions edits can't drift
  (#821).
- **Per-turn token telemetry (#795).** The runtime tailer emits a
  `telemetry.tokens` flow record per model turn (FLOW_SCHEMA 1.13) — the
  dashboard's savings odometer climbs live DURING a dispatch (#800).
- **The savings hero (#783, #803).** "Tokens off the meter" headline with a
  token-class breakdown — generated / fresh input / re-read input — that
  teaches the agent-loop economics (a typical day: ~90% of input is re-read
  context). Tokens only, never currency (#791–#793, #804, #805).
- **Orchestrator notes (#807, #817, #819).** A real channel for the frontier
  orchestrator's voice: `darkmux flow note --source orchestrator` renders as
  the card's conclusion with a history modal; gate/ship print ready-to-paste
  scaffolds (session-id pre-filled) splitting upbeat dashboard notes from
  session-scoped technical adjudications; ship soft-warns when a gated sprint
  ships with a noteless trail (#808, #812, #818, #819, #822).
- **Live diff (#756).** `GET /diff/:session_id` serves the running git diff of
  a mission-run worktree (path-contained, ref-validated, size-bounded); the
  session view renders it live — watch the agent's code form in real time. The
  endpoint was built end-to-end by the local coder through `mission run`
  (#801, #802).
- **Activity-driven live headline (#789).** The viewer's headline tracks the
  live session (mission-scoped → clickable) and reads an affirmative fleet
  status when idle (#790).
- **CLI styling pass (#772–#776).** Semantic color across doctor / scan /
  model-status / profiles / dispatch telemetry, tty-gated (#777–#781).
- **Runtime image on GHCR (#759).** The `darkmux-runtime` image publishes on
  release and pulls on demand — `brew install darkmux` alone can dispatch
  (#764, #765).
- **darkmux.com refresh.** Homebrew-first install docs, copy-to-clipboard on
  all snippets, and the live savings-hero screenshot under the why-headline —
  real work, real telemetry, not a mockup (#763, #766–#770, #784/#823).

### Fixed
- **Saturated Redis streams no longer drop the live tail (#809).** Day reads
  and fleet completion-waits now read newest-first (`XREVRANGE`); at the
  `MAXLEN` cap the oldest records age out instead of the newest vanishing
  (#810).
- **Live-tail idempotency (#794).** SSE re-delivery is identity-deduped so
  cumulative readouts can't inflate and "reset" on refresh (#796).
- Activity-timeline rightmost bar no longer clips past the track (#797);
  savings hero is always visible and compact on mobile (#792, #793).

[1.0.0]: https://github.com/kstrat2001/darkmux/releases/tag/v1.0.0

## [0.9.0] - 2026-06-11

First tagged release. 0.9.0 exercises the full Homebrew + release pipeline ahead
of 1.0; it captures the work merged on `main` since the changelog was seeded.

### Added
- **`config.json` configuration subsystem (#661).** `darkmux init` writes a
  self-documenting `~/.darkmux/config.json` with the common knobs visible (not
  hidden as code-defaults). Every setting resolves with one precedence —
  `env(DARKMUX_*) > config.json > built-in default` — surfaced by `darkmux doctor`.
  Off-by-default integrations are `enabled`-gated blocks; the Redis password is the
  only carve-out (macOS Keychain, never plaintext) (#662–#679).
- **Daemon-hosted observability viewer + playback catalog (#557, #691).**
  `darkmux serve` serves the viewer at `GET /` with a live SSE tail; a rolling 24h
  live window driven by presence heartbeats; a `/flow-days` catalog with a day
  picker; first-class event search; an expandable recent-runs list and an
  unscoped-records section (#582–#584, #682, #710, #715, #723–#729, #731, #748).
- **Presence-driven live fleet view (#638).** A machine shows in the live fleet
  when it's heartbeating — records or not — and consistently across live and
  playback (#651, #653).
- **In-sandbox compile via binary injection (#703).** `crew dispatch --image <any
  Linux image>` injects darkmux's static runtime binary into that image, so the
  coder/test-designer roles can run the inner verify loop (`cargo check`/`test`,
  etc.) in-sandbox. darkmux ships no per-language images — bring the agent, you
  bring the environment (#705–#708).
- **`darkmux flow tail` verb (#695)** — follow flow records live from the CLI (#740).
- **Google Antigravity orchestrator support** with zero-config auto-detection, plus
  unified orchestrator naming (#734, #735, #738).
- **`mission_id` / `sprint_id` stamped on crew-dispatch flow records (#716).**
- **`SECURITY.md` + a `cargo-audit` CI job** (daily + dependency-gated) (#744).
- **Homebrew distribution (#618).** The `kstrat2001/homebrew-darkmux` tap is live
  with a formula auto-synced from `main`; docs lead with `brew install` (#650, #652,
  #654). (Stable bottled release lands with this tag.)
- `doctor` proactively surfaces the Docker runtime requirement (#680) and warns when
  `OPENAI_BASE_URL` would silently defeat `darkmux swap` (#5) (#681, #753).
- Capability-based model selection scaffolding: capability vectors on `ProfileModel`,
  a `select_model` scorer, and a two-value `role_family` axis (#588, #599, #592).
- Machine-level `internal.utility` model — one global utility/compactor per machine,
  loaded alongside workers on `swap`, with a `doctor` loaded-guard and a
  pre-compaction loaded-check (#593, #594, #602).
- `lab doctor` fixture-cleanliness check — flags stray run-artifact dirs left in a
  fixture source (#610).
- The viewer respects `prefers-reduced-motion` (drops the infinite live-badge pulse) (#238, #751).

### Changed
- **OpenClaw is now opt-in, not the default.** `swap` patches openclaw config only
  under an explicit `--runtime openclaw`; `crew dispatch` / `lab run` default to the
  internal Docker-bounded runtime (#606, #607).
- **The fleet executor is now the `runner`** (was `worker`) — a single overloaded
  term retired; `lab-runner` → `lab-manager` to resolve the collision (#595, #659,
  #660, #688).
- **`DARKMUX_LMSTUDIO_URL` is now the base URL** — callers append `/v1/...`
  (semantic break) (#673).
- **The profiles registry is configured as `profiles`** — `DARKMUX_PROFILES` env and
  the `--profiles-file` flag (renamed from the misleading `--config`/`DARKMUX_CONFIG`,
  then from `--profiles`) now that a real `config.json` exists (#677, #739).
- **`swap --recommended`** replaces the reserved `"recommended"` profile name (#700, #702).
- **`profiles.json` gains `schema_version` + forward-compatible extras** so an older
  binary tolerates a newer file (#694, #712).
- Viewer output-encoding hardening — record-derived fields are escaped at the
  template edge and clicks run through one delegated handler (no inline handlers);
  container-written trajectory fields are bounded at ingest (#237, #743, #749).
- `swap` treats a profile's `n_ctx` as a minimum, not an exact size (#600).
- `crew dispatch` resolves and logs the `--profile` override rather than silently
  using the registry default (#608).
- Fleet work-routing collapsed to a single `darkmux:work` stream (first-available
  claims); per-tier routing retired (#604).
- The internal runtime writes its bookkeeping (`.darkmux-runtime/`) to a mounted
  out-dir, never inside the workspace it operates on (#611).
- One canonical `RUN_ARTIFACT_DIRS` shared by the lab clone, the content hash, and
  the workspace-delta view; per-run clones are pruned clean by construction (#609).
- The frontier-orchestrator label generalized from `frontier-claude` to `frontier`,
  with richer telemetry formatting (#738).

### Removed (breaking, pre-1.0)
- `ModelRole` — `default_model` is the canonical worker (#601).
- Machine-tier across the stack: `Role.tier`, `FlowRecord.machine_tier`,
  `WorkJob.target_tier`, and the `{inference/hub/client}` taxonomy (#587, #604, #605).
- `ProfileRuntime` camelCase serde aliases — fields are snake_case only (#699, #709).
- Run-manifest keys normalized to snake_case (#698, #719).
- Dead fixture-manifest fields `hash_include` / `hash_exclude` (never consumed) (#610).

### Fixed
- `DarkmuxPaths.profiles` pointed at `profiles.yaml` instead of `profiles.json` (#585).
- Atomic line append in `LocalFileSink` — fixes concurrent-write tearing and a
  crew-dispatch flake (#667).
- Internal dispatch is bookended with a terminal record; killed runs are recognized
  as `dispatch.error` rather than reading as still-running (#717, #718, #720, #721).
- The live SSE stream re-targets the new day file on UTC date rollover (#730, #731).
- The runtime returns a non-zero exit status on `EscalationTriggered` (#737).
- Lab fixture content-hash drift from stray `coverage/` and `.darkmux-agent/` dirs —
  now excluded from the hash and pruned from per-run clones (#609).
