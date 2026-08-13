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
 * checks them in): `lens=runs`/`lens=lab` > `lens=machine` > `lens=console` >
 * `mission=`/`session=` > a bare `#<date>` (or `?date=`) playback pin >
 * default fleet. A `lens` value this build doesn't recognize (not
 * `runs`/`lab`/`machine`/`console`, and no bare `mission=`/`session=`/date
 * present either) falls through to `unknown` — a visible "lens not ported
 * yet" placeholder naming the raw hash, never a blank page (the overnight
 * runbook's render-sanity contract).
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

export const RUNS_KINDS = ["all", "mission", "dispatch", "lab"] as const;
export type RunsKind = (typeof RUNS_KINDS)[number];

/** The console lens's panel allowlist — a straight port of `viewer.html`'s
 * `PANELS` id list (`crates/darkmux-serve/src/panel.rs::PANEL_IDS` is the
 * server-side twin). Kept here, next to `RUNS_KINDS`, because both exist for
 * the SAME reason: `parseRoute` needs a closed set to validate a hash param
 * against before trusting it. Console-lens-only concerns (tab labels, which
 * panels are manual-only) stay in `lenses/console/panels.ts` — this list is
 * the routing-grammar's business, not the lens's. */
export const PANEL_IDS = [
  "mission-status",
  "mission-status-all",
  "machine-status",
  "flow-status",
  "role-list",
  "config-list",
  "lab-fixture-list",
  "doctor",
] as const;
export type PanelId = (typeof PANEL_IDS)[number];

export type Route =
  | { kind: "fleet" }
  | { kind: "runs"; runsKind: RunsKind; run: string | null }
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
   * `session.sessionId`'s existing open-string precedent below. An
   * unrecognized/stale uid degrades gracefully (see `MachineLens`'s own
   * doc) rather than needing its own validation here. */
  | { kind: "machine"; uid: string | null }
  /** `panelId` is `""` for "no explicit panel requested" AND for "an
   * unrecognized id" — both fall back to the console lens's own default
   * (`mission-status`), matching legacy's `consoleQuery()`:
   * `PANELS.some(x=>x.id===id) ? id : ""`. The router does the same
   * allowlist check `consoleQuery` does; the lens component owns applying
   * the "" -> default fallback (mirroring `state.panelId` starting at
   * `"mission-status"` and `boot()`'s `if(nq) state.panelId=nq` — a falsy
   * `nq` leaves the existing default untouched rather than overwriting it). */
  | { kind: "console"; panelId: PanelId | "" }
  | { kind: "session"; sessionId: string }
  /** `#mission=<id>` is a FULL NAVIGATION in the legacy viewer
   * (`location.href = "/mission/<id>/graph"`) — a separate asset with its own
   * vendored React Flow bundle. Out of scope for this packet (see
   * `tests/parity/README.md`'s lens inventory); the router recognizes the
   * hash shape and reports it distinctly from `unknown` so a lens packet can
   * wire the redirect without re-deriving the grammar. */
  | { kind: "mission-redirect"; missionId: string }
  /** A bare `#<date>` hash (or `?date=<date>`, its query-string form) —
   * `viewer.html`'s `targetDate()` fallback. Distinct from `fleet`: legacy's
   * `boot()` computes `live = date===todayUTC()`, which is false for any
   * OTHER date, forcing the playback fetch branch instead of the live
   * window — a genuinely different render, not just a different label on
   * the same one. See `route.ts`'s own module doc for the precedence this
   * sits at (lowest, below every `lens=`/`mission=`/`session=` form). */
  | { kind: "playback"; date: string }
  | { kind: "unknown"; hash: string };

/** (Packet 5) Should the live tail (`hooks/useLiveTail.ts` — SSE + reconcile
 * backstop) be running for this route? Mirrors legacy's `wantsPlayback`
 * (viewer.html:3853: `injectedMode==="play" || !!flowSrc || !!cq`, where
 * `cq` is a mission/session catalog query) — `playback`/`session`/
 * `mission-redirect` are all requests for a SPECIFIC historical slice, not
 * the rolling live window, so `boot()` never starts `startLiveTail` for any
 * of them. `fleet`/`runs`/`machine`/`console`/`unknown` are the live routes
 * — every one of them renders the SAME rolling `useFlowWindow` this app has
 * no separate playback data pipeline for yet (see `PlaybackLens`'s own
 * module doc). */
export function isLiveRoute(route: Route): boolean {
  return route.kind !== "playback" && route.kind !== "session" && route.kind !== "mission-redirect";
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
 * reassigning `state.level`), and the daemon-less static mission summary
 * (`state.level==="mission"`, reached here as `mission-redirect` — moot in
 * practice since a real daemon always navigates away before render, but
 * named for completeness). `unknown` has no legacy analog; defaults to
 * shown (the fleet-like default) rather than inventing a hide rule with no
 * source to verify it against. */
export function showsEventLog(route: Route): boolean {
  return route.kind !== "runs" && route.kind !== "console" && route.kind !== "machine";
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

  const lens = get("lens");

  if (lens === "runs" || lens === "lab") {
    const rawKind = (get("kind") || (lens === "lab" ? "lab" : "all")).toLowerCase();
    const runsKind = (RUNS_KINDS as readonly string[]).includes(rawKind)
      ? (rawKind as RunsKind)
      : "all";
    const run = search.has("run") ? search.get("run") : hash.has("run") ? hash.get("run") : null;
    return { kind: "runs", runsKind, run: run === null ? null : run.trim() };
  }

  if (lens === "machine") {
    const uid = get("uid");
    return { kind: "machine", uid: uid ? uid : null };
  }

  if (lens === "console") {
    const rawPanel = get("panel");
    const panelId = (PANEL_IDS as readonly string[]).includes(rawPanel) ? (rawPanel as PanelId) : "";
    return { kind: "console", panelId };
  }

  const mission = get("mission");
  if (mission) {
    return { kind: "mission-redirect", missionId: mission };
  }

  const session = get("session");
  if (session) {
    return { kind: "session", sessionId: session };
  }

  const raw = (location.hash || "").replace(/^#/, "");
  if (lens) {
    // A `lens=` value this build doesn't recognize (typo, a future lens the
    // legacy viewer grew after this port, etc.) — name it rather than
    // silently falling back to fleet, which would hide a broken bookmark.
    return { kind: "unknown", hash: raw };
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
