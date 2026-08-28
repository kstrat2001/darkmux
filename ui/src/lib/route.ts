/**
 * Hash-route parser — a straight TypeScript port of `viewer.html`'s own
 * `catalogQuery`/`labQuery`/`consoleQuery`/`machineQuery` functions (see that
 * file's own comments around line 3210 onward). The grammar and its
 * precedence order are PRESERVED VERBATIM — the CLI prints these deep links
 * (`panel_deep_link`, `mission status`'s reconcile commands, etc.) and every
 * bookmark/phone-shortcut minted against the legacy viewer must keep
 * resolving once `/next` takes over.
 *
 * Precedence when the hash carries multiple intents (same order `boot()`
 * checks them in): `lens=fleet` > `lens=runs`/`lens=lab` > `lens=machine` >
 * `lens=console` > `mission=`/`dispatch=` (alias `session=`) > a static-build's
 * `darkmux-flow-src` meta (#1801 — see below) > a bare `#<date>` (or
 * `?date=`) playback pin > default fleet. `lens=fleet` (#1920) is just the
 * EXPLICIT name for the same default the bottom of this function already
 * falls back to with no hash at all — naming it here doesn't change what
 * it resolves to, only whether typing/sharing it dead-ends. `lens` itself
 * is lowercased before every comparison below (#1920), matching `kind`'s
 * existing degrade-gracefully behavior. A `lens` value this build doesn't
 * recognize even after lowercasing (not
 * `fleet`/`runs`/`lab`/`machine`/`console`, and no bare `mission=`/
 * `dispatch=`/date present either) falls through to `unknown` — a visible
 * "lens not ported yet" placeholder naming the raw hash, never a blank
 * page (the overnight runbook's render-sanity contract).
 *
 * (Packet 4) The bare-date form is `targetDate()`'s convenience in
 * `viewer.html` (type/bookmark `#2026-08-07` directly) — a DIFFERENT
 * mechanism from the catalog panel's own day-row click, which does a full
 * navigation to `/play/<date>` (a separate server route this app doesn't own
 * and doesn't try to). This was `unknown` through Packet 1–3 (out of scope
 * until a lens packet owned the catalog); this packet is that lens packet,
 * so it's recognized as its own `playback` route now — see
 * `lenses/catalog/PlaybackLens.tsx` for what it renders (not the full
 * historical-playback view yet, a follow-up; see that file's doc).
 */

import { injectedPlaybackDate } from "./injectedMeta";
import { getSource } from "./source";
import { sanitizeOptParams } from "../lenses/console/panels";

export const RUNS_KINDS = ["all", "mission", "dispatch", "lab"] as const;
export type RunsKind = (typeof RUNS_KINDS)[number];

/** The console lens's panel allowlist — a straight port of `viewer.html`'s
 * `PANELS` id list (`crates/darkmux-serve/src/panel.rs::PANEL_IDS` is the
 * server-side twin, hard-capped at 8 BASE VERBS by its own doctrine
 * assertion). Kept here, next to `RUNS_KINDS`, because both exist for the
 * SAME reason: `parseRoute` needs a closed set to validate a hash param
 * against before trusting it. Console-lens-only concerns (tab labels, which
 * panels are manual-only, the declared option space) stay in
 * `lenses/console/panels.ts` — this list is the routing-grammar's business,
 * not the lens's.
 *
 * (#1911) `mission-status-all` dropped out of this list — it is no longer a
 * base verb, exactly mirroring `panel.rs`'s own layer-2 guard ("flags are
 * opts, ids are verbs": its old argv `["mission","status","--all"]` bakes a
 * flag into base argv, which the Rust-side allowlist no longer tolerates as
 * a direct entry). It survives only as a one-release alias — see
 * `PANEL_ALIASES` below, the client twin of `panel.rs`'s `resolve_alias`.
 * `run-list` joins in its place, the CLI twin of the RUNS lens's union
 * (`src/run_list.rs`). */
export const PANEL_IDS = [
  "mission-status",
  "role-list",
  "machine-status",
  "config-list",
  "flow-status",
  "lab-fixture-list",
  "run-list",
  "doctor",
] as const;
export type PanelId = (typeof PANEL_IDS)[number];

/** One-release compatibility alias (#1911): the client twin of `panel.rs`'s
 * `resolve_alias`. A `panel=mission-status-all` deep link — the CLI has
 * printed these (`panel_deep_link` in `src/mission_status.rs`) and an
 * operator may have bookmarked one — resolves to the BASE id `panelId`
 * plus the FORCED opt selections that reproduce the old entry's exact
 * behavior. `parseRoute` applies the forced opts LAST (after any `opt.*`
 * the hash also carried), matching the server's own "the alias's whole
 * point is a fixed, non-negotiable selection" ordering. `hashSync`'s
 * `canonicalHash` then rewrites the address bar to the canonical
 * `panel=mission-status&opt.all=all` form — the SAME upgrade path
 * `#lens=lab` → `#lens=runs&kind=lab` already uses. Dropped entirely once
 * every emitter has migrated, per the pre-1.0 no-compat-baggage posture. */
export const PANEL_ALIASES: Readonly<Record<string, { panelId: PanelId; opts: Readonly<Record<string, string>> }>> = {
  "mission-status-all": { panelId: "mission-status", opts: { all: "all" } },
};

export type Route =
  | { kind: "fleet" }
  /** `machine` — added for #1809 (finishing #1508 step 4): pins the runs
   * lens to ONE machine, composable with `runsKind`/`run` (independent
   * params on the same hash — a pinned kind filter, or a pinned lab-run
   * drill-in, are both real reachable states). `null` is "every machine",
   * the pre-existing behavior — every hash this port already emits
   * (`#lens=runs`, `#lens=runs&kind=lab`, …) still parses to `machine:
   * null` and renders identically to before this field existed.
   *
   * An open string, same precedent as `machine.uid` below and
   * `dispatch.dispatchId` further down — machine uids are arbitrary
   * hardware-derived identifiers with no closed set to validate against
   * here (an unresolvable pin degrades gracefully: `RunsBoard` just shows
   * zero rows for a uid nothing is filed under, same posture `MachineLens`
   * already takes for a stale `machine.uid`). This is the runs-lens half of
   * `MachineLens`'s `RUNS ON <MACHINE>` list moving out into a real lens —
   * see `MachineLens.tsx`'s own doc and #1508 step 2's commit message
   * (`d2041ae3`), which named this the deliberately-interim piece step 4
   * was always going to replace. */
  | { kind: "runs"; runsKind: RunsKind; run: string | null; machine: string | null }
  /** `uid` — widened this packet (the drill-in packet) to carry a SPECIFIC
   * machine uid: `null` for the nav-tab/deep-link entry (`goMachine` in
   * legacy — always "the local machine"), a real uid for a fleet-card
   * drill (`drillMachine(uid, false)` — local OR remote). Legacy itself has
   * NO deep-link form for the remote-uid case (`syncLabHash`'s `inMachine`
   * branch writes only `lens=machine`, never the drilled uid — a real gap,
   * not a design this port narrows further) — the `uid=` param below is a
   * genuine, deliberate WIDENING beyond legacy's own address-bar behavior,
   * so a fleet-card drill into a remote machine is bookmarkable/pasteable
   * (the hard deep-link requirement this packet's brief sets), where legacy
   * would silently drop you back to the local machine on reload. `uid` is
   * an open string (not from `PANEL_IDS`/`RUNS_KINDS`'s closed sets) —
   * machine uids are arbitrary hardware-derived identifiers, matching
   * `dispatch.dispatchId`'s existing open-string precedent below. An
   * unrecognized/stale uid degrades gracefully (see `MachineLens`'s own
   * doc) rather than needing its own validation here. */
  | { kind: "machine"; uid: string | null }
  /** `panelId` is `""` for "no explicit panel requested" AND for "an
   * unrecognized id" — both parse the same way, matching legacy's
   * `consoleQuery()`: `PANELS.some(x=>x.id===id) ? id : ""`. The router
   * does the same allowlist check `consoleQuery` does; the lens component
   * owns deciding what "" RENDERS as — `ConsolePanel.tsx` resolves it to
   * `panels.ts::DEFAULT_PANEL_ID` (`"run-list"`).
   *
   * (#1904/#1905 step 3) A client-rendered `ActivityPanel` briefly stood in
   * for `""` — a `/runs`-fed view with no CLI command behind it, genuinely
   * different from any `PanelId`, which meant `mission-status`'s own
   * address-bar collapse (see below) had to be removed so it stayed
   * reachable as itself. #1905 step 3 deleted `ActivityPanel` (a THIRD
   * client-side renderer of `/runs`, the same drift #1905 exists to
   * prevent) in favor of `run-list` (#1910), a real CLI panel that reads
   * the identical union — so `""` is back to meaning "the default CLI
   * panel," the same relationship `mission-status` had pre-#1904. Every
   * `PanelId` (`run-list` included) stays independently addressable by its
   * own explicit `panel=<id>` regardless of which one is the default —
   * `canonicalHash` does not collapse an explicit choice into `""`.
   *
   * `opts` (#1911) — the panel's own resolved `opt.<name>` selections
   * (`{}` when `panelId` is `""`, or a panel declares no options). Only
   * entries `sanitizeOptParams` recognized for THIS panel ever land here —
   * see that function's own doc. Always present (never optional) so every
   * consumer (`ConsolePanel`, `hashSync.canonicalHash`) can read it
   * unconditionally rather than defaulting to `{}` at every call site. */
  | { kind: "console"; panelId: PanelId | ""; opts: Readonly<Record<string, string>> }
  /** `#dispatch=<id>` — the detail view for ONE dispatch: one role's one
   * model execution (`CLAUDE.md` contract 8, the work-unit vocabulary).
   * Named for the `RunKind` it opens, which is that contract's conformance
   * rule — every other surface already called this thing a dispatch
   * (`RUNS_KINDS`, `RunKind::Dispatch`, `darkmux dispatch <role>`); only the
   * route said "session".
   *
   * `session=` is accepted as a ONE-RELEASE ALIAS (see `parseRoute`), the
   * same shape `PANEL_ALIASES` below uses, because this module's header
   * requires every bookmark and printed deep link to keep resolving. The
   * `dispatchId` is still the flow `session_id` on the wire — that FIELD
   * keeps its name (renaming it strands every archive, and #1974 demotes
   * "session" to an internal join key rather than deleting it). An open
   * string, same precedent as `machine.uid` above. */
  | { kind: "dispatch"; dispatchId: string }
  /** `#mission=<id>` — the mission-graph lens (#1868). A FULL NAVIGATION in
   * the LEGACY viewer (`location.href = "/mission/<id>/graph"`, a separate
   * document with its own vendored React Flow bundle); this port instead
   * renders `MissionGraphLens` IN-PLACE — no navigation, the hash IS the
   * route. Renamed from `mission-redirect` (#1868 — the earlier packets'
   * placeholder name, back when this route only recognized the shape and
   * deferred rendering to the standalone page) now that it renders for
   * real. */
  | { kind: "mission"; missionId: string }
  /** A bare `#<date>` hash (or `?date=<date>`, its query-string form) —
   * `viewer.html`'s `targetDate()` fallback. Distinct from `fleet`: legacy's
   * `boot()` computes `live = date===todayUTC()`, which is false for any
   * OTHER date, forcing the playback fetch branch instead of the live
   * window — a genuinely different render, not just a different label on
   * the same one. See `route.ts`'s own module doc for the precedence this
   * sits at (lowest, below every `lens=`/`mission=`/`session=` form).
   *
   * (#1801) `date` is `string | null` — `null` ONLY when `isStaticBuild()`
   * forced this route (see below): a static demo build has no server-
   * assigned date the way `/play/<date>`'s injected meta does, and no daemon
   * to ask `/flow/<date>` for one either. Legacy's own flowSrc branch has the
   * identical gap — `let date=injectedDate||targetDate()` defaults to today,
   * then gets overwritten by `RAW[0].ts` ONLY once the file has actually
   * loaded (viewer.html:3902) — so the real date is knowable only after a
   * fetch this synchronous parser can't perform. `null` names that honestly;
   * a consumer that needs a display date derives one from the loaded records
   * via `lib/flow.ts::firstRecordDate`, the same derivation legacy performs,
   * once they exist (see `App.tsx`'s `displayRoute`). */
  | { kind: "playback"; date: string | null }
  | { kind: "unknown"; hash: string };

/** (Packet 5) Should the App-level live tail (`hooks/useLiveTail.ts` — SSE +
 * reconcile backstop feeding the FLEET-wide rolling window) be running for
 * this route? Mirrors legacy's `wantsPlayback` (viewer.html:3853:
 * `injectedMode==="play" || !!flowSrc || !!cq`, where `cq` is a mission/
 * session catalog query) — `playback`/`session`/`mission` are all requests
 * for a SPECIFIC slice, not the rolling live window, so `boot()` never
 * started `startLiveTail` for any of them. `fleet`/`runs`/`machine`/
 * `console`/`unknown` are the live routes — every one of them renders the
 * SAME rolling `useFlowWindow` this app has no separate playback data
 * pipeline for yet (see `PlaybackLens`'s own module doc).
 *
 * `mission` stays excluded even though `MissionGraphLens` (#1868) genuinely
 * IS live (it runs its own SSE subscription): the App-level tail this flag
 * gates is scoped to the FLEET-WIDE two-day window `useFlowWindow` builds,
 * which the mission lens doesn't need — it mounts its OWN
 * `useLiveTail(true)` call, the same shared primitive, independently (see
 * `MissionGraphLens.tsx`'s own doc). Two mounts of the SAME hook against the
 * SAME `queryKeys.flowTail(date)` slot would be redundant, not wrong, but
 * this app never does both at once: the App-level copy is gated off here
 * specifically so only the lens's own copy runs while this route is active. */
export function isLiveRoute(route: Route): boolean {
  // (#1801) A daemon-less build is NEVER live, on any lens. Legacy's gate is
  // GLOBAL — `wantsPlayback = injectedMode==="play" || !!flowSrc || !!cq`
  // (viewer.html:3880) — and `startLiveTail(date); startLivePoll();` runs only
  // under `if(mode==="live")` (viewer.html:3956), so a static build never
  // opens an SSE stream or a presence poll no matter which lens is showing.
  //
  // This gate was keyed on route KIND alone, and `parseRoute` resolves `lens=`
  // BEFORE the static-build branch — so `#lens=runs` on the demo parsed to
  // `{kind:"runs"}` and every live consumer opened. Measured on the served
  // demo: an EventSource to `/flow/<today>/stream`, `/fleet/machines/live`
  // polled every 5s indefinitely, and the mode badge reading `◌ RECONNECTING`
  // — a page asserting there is something to reconnect TO, on a marketing
  // site with no daemon anywhere near it.
  if (getSource().kind === "static") return false;
  return route.kind !== "playback" && route.kind !== "dispatch" && route.kind !== "mission";
}

/** Should the event-log column (`components/EventLogColumn.tsx` — the
 * per-record stream + its search/filter/follow chrome and the `#detail`
 * selected-event panel) render for this route?
 *
 * QA CORRECTION (2026-08-09, this packet): the packet brief this function
 * was written against claimed the column "shows on fleet, console, and the
 * catalog/replay overlay; hidden on runs and machine" — that is WRONG,
 * verified two ways against the actual legacy source:
 *
 * 1. Reading `renderCrumb()` (viewer.html:2521): `document.body.classList
 *    .toggle("runs-mode", inRuns||inConsole)` — CONSOLE sets `runs-mode`
 *    too, not just runs. `machine-mode` is its own separate toggle
 *    (line 2523). The CSS (viewer.html:106/258) hides `.log`/`.split`/
 *    `.scrub` on BOTH `body.runs-mode` and `body.machine-mode`.
 * 2. A throwaway Playwright probe against the recorded corpus
 *    (`getComputedStyle('.log').display` per lens, via the harness's own
 *    `installCorpusRoutes`/`waitSettled`) measured it directly rather than
 *    trusting the read: `fleet` → `flex` (visible); `console` → `none`;
 *    `runs` → `none`; `machine` → `none`; back to `fleet` → `flex` again.
 *
 * So the real rule is the INVERSE of the brief for `console`: the column is
 * hidden on `runs`, `console`, AND `machine` (every `state.level` legacy
 * sets `runs-mode`/`machine-mode` for), and shown everywhere else — `fleet`,
 * a session drill-in (`state.level==="subsystem"`), a bare-date playback
 * (stays at the `fleet` level, per `targetDate()`'s boot path never
 * reassigning `state.level`). `unknown` has no legacy analog; defaults to
 * shown (the fleet-like default) rather than inventing a hide rule with no
 * source to verify it against.
 *
 * `mission` (#1868) is a NEW exclusion this packet adds, with no legacy
 * analog to verify against (legacy's `#mission=<id>` was always a full
 * navigation away, past render — see `route.ts`'s own `mission` doc). Once
 * `MissionGraphLens` renders in-place, it owns its OWN events pane (fed by
 * `EventLogColumn` — same component, second call site — mission-scoped
 * records rather than the fleet window), so the App-level column must not
 * ALSO render alongside it; that would be two event logs on one page,
 * disagreeing about scope. */
export function showsEventLog(route: Route): boolean {
  // (#1066) `runs`/`console`/`machine` no longer hide it. Those three were
  // parity with `viewer.html`'s `runs-mode`/`machine-mode` — measured, and
  // correct while that viewer still served users. It was DELETED in #1865,
  // so the rule was matching a thing that no longer exists, against an
  // operator asking for the opposite: "the events panel being a collapsible
  // mainstay on all tabs." A pane the operator can collapse is strictly more
  // capable than one the route hides for them.
  //
  // `mission` STAYS excluded, and this is not the same kind of rule.
  // `MissionGraphLens` mounts its OWN instance of `EventLogColumn`, fed
  // mission-scoped records; showing the App-level column too would put two
  // event logs on one page disagreeing about scope (#1868). That is
  // structural, not parity — it would still hold if legacy had never
  // existed.
  return route.kind !== "mission";
}

const DATE_RE = /^\d{4}-\d{2}-\d{2}$/;

function hashParams(): URLSearchParams {
  return new URLSearchParams((location.hash || "").replace(/^#/, ""));
}

/** Parse the CURRENT `location.hash` into a [[Route]]. Pure function of
 * `location.hash` (and, matching the legacy grammar, `location.search` as a
 * fallback source for the same param names) — call it fresh on every
 * `hashchange`, never cache across navigations. */
export function parseRoute(): Route {
  const search = new URLSearchParams(location.search);
  const hash = hashParams();
  const get = (name: string): string =>
    (search.get(name) || hash.get(name) || "").trim();

  // (#1920) Lowercased the same way `kind` already is below — `lens` is a
  // closed-set param (`runs`/`lab`/`machine`/`console`/`fleet`) exactly
  // like `kind` is, and a hand-typed or autocapitalized `#lens=RUNS` used
  // to compare with strict `===` against every branch, never matching, and
  // falling all the way to `unknown`. `kind`'s own degrade-gracefully
  // behavior is the target this brings `lens` in line with, not a new
  // invention.
  const lens = get("lens").toLowerCase();

  // (#1920) `fleet` is the bare-root default (no hash at all falls
  // through to `{kind:"fleet"}` at the bottom of this function) but had no
  // EXPLICIT `lens=` form of its own — an operator typing or sharing
  // `#lens=fleet` (a wholly plausible guess: it's the one lens every other
  // `lens=` value sits beside, and the nav tab's own hash-write for the
  // fleet tab is `""`, not `"lens=fleet"`, so nothing in this app ever
  // MINTS that hash, but a person composing one by hand doesn't know
  // that) hit the `unknown` placeholder instead of the lens that was
  // already the default. Named explicitly here, same as every other real
  // lens above and below it, so the fleet lens is addressable by name, not
  // only by omission.
  if (lens === "fleet") {
    return { kind: "fleet" };
  }

  if (lens === "runs" || lens === "lab") {
    const rawKind = (get("kind") || (lens === "lab" ? "lab" : "all")).toLowerCase();
    const runsKind = (RUNS_KINDS as readonly string[]).includes(rawKind)
      ? (rawKind as RunsKind)
      : "all";
    const run = search.has("run") ? search.get("run") : hash.has("run") ? hash.get("run") : null;
    const machine = get("machine");
    return { kind: "runs", runsKind, run: run === null ? null : run.trim(), machine: machine ? machine : null };
  }

  if (lens === "machine") {
    const uid = get("uid");
    return { kind: "machine", uid: uid ? uid : null };
  }

  if (lens === "console") {
    const rawPanel = get("panel");
    const alias = PANEL_ALIASES[rawPanel];
    const panelId: PanelId | "" = alias
      ? alias.panelId
      : (PANEL_IDS as readonly string[]).includes(rawPanel)
        ? (rawPanel as PanelId)
        : "";
    // (#1911) `opt.<name>` — read from BOTH the hash and the query string,
    // same dual-source posture `get()` already gives every other param;
    // hash wins on a name present in both. Validated against `panelId`'s
    // OWN table (an id this build doesn't recognize has no table to
    // validate against, so it gets no opts at all — matching "an
    // unrecognized `panel` parses the same as absent").
    // (#1920) Hash first, then search fills only what the hash did not
    // set — so a non-empty SEARCH value wins, matching `get()` above
    // (`search.get(name) || hash.get(name)`) and therefore every other
    // NAMED param: `lens`, `panel`, `machine`, `uid`, `mission`,
    // `session`. The first draft appended hash last and let it overwrite
    // unconditionally, so `opt.kind=` and `panel=` on one page obeyed
    // opposite rules.
    //
    // Note this repo does have a deliberate hash-wins case, pinned by
    // "an in-hash date wins over a co-present ?date= query param" — but
    // that is the BARE `#<date>` form, a positional hash shape rather
    // than a named param, so it is a different rule for a different
    // thing, not a precedent for this one.
    const rawOpts: Record<string, string> = {};
    for (const [k, v] of hash.entries()) if (k.startsWith("opt.")) rawOpts[k.slice(4)] = v;
    for (const [k, v] of search.entries()) if (k.startsWith("opt.") && v !== "") rawOpts[k.slice(4)] = v;
    let opts: Readonly<Record<string, string>> = panelId ? sanitizeOptParams(panelId, rawOpts) : {};
    // The alias's forced selection wins over anything a stray `opt.*`
    // param claimed — mirrors `panel.rs::resolve_alias`'s own doc: "the
    // alias's whole point is a fixed, non-negotiable selection".
    if (alias) opts = { ...opts, ...alias.opts };
    return { kind: "console", panelId, opts };
  }

  const mission = get("mission");
  if (mission) {
    return { kind: "mission", missionId: mission };
  }

  // (#1974) `dispatch=` is canonical; `session=` is the one-release alias.
  // Canonical wins when both are present — a hash carrying both is already
  // malformed, and preferring the new spelling makes `canonicalHash`'s
  // rewrite idempotent rather than oscillating.
  const dispatch = get("dispatch") || get("session");
  if (dispatch) {
    return { kind: "dispatch", dispatchId: dispatch };
  }

  const raw = (location.hash || "").replace(/^#/, "");
  if (lens) {
    // A `lens=` value this build doesn't recognize (typo, a future lens the
    // legacy viewer grew after this port, etc.) — name it rather than
    // silently falling back to fleet, which would hide a broken bookmark.
    return { kind: "unknown", hash: raw };
  }

  // (#1801) `darkmux-flow-src` (the static demo's committed .jsonl) forces
  // playback UNCONDITIONALLY — checked here, above even an explicit bare
  // `#<date>` hash, because it mirrors legacy's own precedence exactly:
  // `wantsPlayback = injectedMode==="play" || !!flowSrc || !!cq`
  // (viewer.html:3880) forces the playback branch regardless of what `date`
  // holds, and the flowSrc RECORD-LOADING branch itself (3897-3906) reads
  // the committed file unconditionally too — a hash like `#2026-08-01` on a
  // static build has no daemon behind it to serve THAT day, so legacy
  // renders the one file it has and lets the file's own first record
  // relabel the date (`firstRecordDate`, `lib/flow.ts`), never treats the
  // hash as a request for a day that doesn't exist. Only `lens=`/`mission=`/
  // `session=` (already checked above) can still preempt this — matching
  // this port's existing precedence order (see this file's own module doc);
  // legacy's stricter `cq` suppression of mission/session under flowSrc
  // (`(flowSrc||lq||mq||nq!=null) ? null : catalogQuery()`) is NOT ported —
  // narrower scope, named here rather than silently dropped.
  if (getSource().kind === "static") {
    return { kind: "playback", date: null };
  }

  if (!raw) {
    // (Packet 4) `?date=<date>` — the query-string form of the same
    // `targetDate()` fallback the bare hash below reads, checked here only
    // when the hash itself is empty (matching legacy's own precedence: an
    // in-hash date wins when both are present, since the hash check below
    // runs first when `raw` is non-empty).
    const qDate = search.get("date");
    if (qDate && DATE_RE.test(qDate)) {
      return { kind: "playback", date: qDate };
    }
    // (the flip) `GET /play/<date>` carries its date in an INJECTED META TAG,
    // not in the URL the client can read as a route: the server responds to
    // the path with `inject_mode_meta(html, "play", Some(date))`, and the
    // browser's `location` shows `/play/2026-08-07` with no hash and no query.
    // Legacy reads those metas at boot (`injectedMode`/`injectedDate`,
    // viewer.html:3836+) — this port read only version/schema, so before the
    // flip it had no way to know and no reason to: `/play/:date` served
    // LEGACY, and `/next` was live-only by construction.
    //
    // That stops being true the moment `/play/:date` serves this app. Without
    // this branch the flip would render the LIVE fleet view at a playback URL
    // — today's numbers under yesterday's address, silently, which is the
    // exact failure class the whole #1800 P2 gate was about. Checked LAST so
    // an explicit hash or `?date=` still wins; a page served in live mode
    // injects no date at all and falls through unchanged.
    const injected = injectedPlaybackDate();
    if (injected) {
      return { kind: "playback", date: injected };
    }
    return { kind: "fleet" };
  }
  // (Packet 4) A bare `#<date>` hash — `targetDate()`'s convenience form
  // (type/bookmark a date directly). A garbage non-date, non-`lens=` hash
  // falls through to the final `fleet` below, matching legacy's own silent
  // fallback verbatim (targetDate() defaults to today for anything it can't
  // parse, which is the same live view as no hash at all) — only a
  // genuinely date-shaped hash gets its own route.
  if (DATE_RE.test(raw)) {
    return { kind: "playback", date: raw };
  }
  return { kind: "fleet" };
}
