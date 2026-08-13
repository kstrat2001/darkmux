import { useMemo } from "react";
import { useHashRoute } from "./lib/useHashRoute";
import { useSyncHash } from "./lib/hashSync";
import { FleetLens } from "./lenses/fleet/FleetLens";
import { LensPlaceholder } from "./components/LensPlaceholder";
import { NavChrome } from "./components/NavChrome";
import { Masthead } from "./components/Masthead";
import { EventLogColumn } from "./components/EventLogColumn";
import { MachineLens } from "./lenses/machine/MachineLens";
import { RunsBoard } from "./lenses/runs/RunsBoard";
import { ConsolePanel } from "./lenses/console/ConsolePanel";
import { MissionReplay } from "./lenses/catalog/MissionReplay";
import { SessionReplay } from "./lenses/catalog/SessionReplay";
import { PlaybackLens } from "./lenses/catalog/PlaybackLens";
import { useFlowWindow } from "./hooks/useFlowWindow";
import { useRouteRecords } from "./hooks/useRouteRecords";
import { useLiveMachines } from "./hooks/useLiveMachines";
import { useLiveTail } from "./hooks/useLiveTail";
import { computeMetaLines, readyParts } from "./lib/metaLine";
import { ReadyHeadline } from "./components/ReadyHeadline";
import { localMachineUid, nameOf } from "./lib/flow";
import { isLiveRoute, showsEventLog } from "./lib/route";
import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "./lib/fetcher";
import { queryKeys } from "./lib/queryKeys";
import type { MachineSpecs } from "./types/handwritten";
import type { Route } from "./lib/route";

/**
 * The app shell. A `switch` over the parsed [[Route]] (see `lib/route.ts` for
 * the hash-grammar port) drives `#stage`; `fleet` (`FleetLens`, Packet 8 —
 * the savings hero + machine cards + activity timeline; supersedes the
 * scaffold's original `FleetStrip` presence-only proof region, still tested
 * standalone in `components/FleetStrip.test.tsx` but no longer mounted
 * here), `runs` (`RunsBoard`, Packet 3), and `machine` (`MachineLens`,
 * Packet 2) are real regions driven by `useQuery`; `session`/`mission-redirect`/`playback`
 * (Packet 4) do REAL fetches/navigation wiring per the catalog+replay
 * lens's own doc comments; every other lens renders [[LensPlaceholder]]
 * naming what still needs to be built, per the render-sanity contract
 * (never a blank page).
 *
 * `#crumb` and `#logscope` are LENS-SPECIFIC (legacy: `renderCrumb()`'s
 * `$("crumb").innerHTML=...` per `state.level`, and each `render*()`
 * function's own `$("logscope").textContent=...`) — the `{crumb, logscope}`
 * pair is computed here (`routeChrome`, below) per route rather than inside
 * each lens component, since `#crumb` is an App-level sibling of `#stage`,
 * not a descendant of it. `#meta` is GLOBAL (legacy's `renderMeta()` runs on
 * every render() regardless of `state.level` — confirmed: `goldens/fleet.txt`
 * and `goldens/machine.txt` carry byte-identical `=== meta ===` sections), so
 * it's computed here unconditionally rather than per-route. The underlying
 * `useFlowWindow`/`useLiveMachines`/`machineSpecs` queries are ALSO used
 * inside `MachineLens` — TanStack Query dedupes by queryKey, so this is
 * cache reuse, not a second network round trip.
 *
 * **(Chrome packet) `#logscope` moved OUT of this file** — it now lives
 * inside `EventLogColumn` (see that component's own doc), the computed
 * `logscope` STRING still comes from `routeChrome` here and is passed down
 * as a prop, but the DOM node only exists when `showsEventLog(route)` is
 * true. Rendering it unconditionally at the App level (the pre-this-packet
 * shape) was the direct cause of the stray uppercase "FLEET" the operator
 * caught floating above the hero — legacy nests the equivalent node INSIDE
 * the (sometimes-hidden) log column's own header, never loose.
 *
 * `CatalogPanel` mounts inside `<Masthead>` now (moved this packet — see
 * that component's own doc for why: legacy's `#catpanel` toggle,
 * `#srcbadge`, lives in `.top`, not the crumbbar) rather than here directly
 * — still global chrome (`viewer.html`'s `#catpanel`, reachable from every
 * lens), not a routed destination itself.
 *
 * Packet 1.5 additions (nav chrome + hash write-back — the scaffold gap
 * both the machine and runs lens packets independently flagged as a hard
 * blocker for the eventual `/next` → `/` flip):
 *
 * - `<NavChrome>` (see that component's own doc) is a new sibling INSIDE a
 *   `.app-shell__crumbbar` wrapper alongside `#crumb`/`#meta` — a pure DOM
 *   restructuring, not a content change: the parity extractor
 *   (`tests/parity/lib/extract-lens.js`) selects `#crumb`/`#meta` BY ID
 *   regardless of parent, so moving their container doesn't touch
 *   byte-parity. `#logscope`/`#stage` are untouched siblings, same as
 *   before.
 * - `useSyncHash` (see `lib/hashSync.ts`) is the `/next` port of legacy's
 *   `syncLabHash()` — reflects the current `Route` back into `location.hash`
 *   via `replaceState` on every route change, so every view is bookmarkable
 *   (matches legacy's own reasoning: the phone dashboard is the first-class
 *   consumer). This is also what performs the legacy `#lens=lab` →
 *   `#lens=runs&kind=lab` upgrade, since arriving on the alias parses to
 *   the canonical `Route` already and the write-back just names it.
 *   `RunsBoard`'s kind chips are the one piece of lens state that changes
 *   WITHOUT a route change (no `hashchange` fires) — that write goes
 *   straight from `RunsBoard.tsx`'s `selectKind` to `hashSync.ts`'s
 *   `writeHash`, not through this route-keyed effect (see that file's own
 *   doc for why).
 */
export function App() {
  const route = useHashRoute();
  const nowMs = Date.now();

  // (Packet 5) The SSE tail + reconcile backstop + date-rollover handler —
  // gated by `isLiveRoute` (see that function's own doc) so a genuinely
  // historical route (`playback`/`session`/`mission-redirect`) doesn't run
  // a live tail behind it, matching legacy's own `wantsPlayback` gate on
  // `startLiveTail`. Feeds `flowWindow` below via the Query cache
  // (`useFlowWindow`'s own doc), not a direct return-value dependency here.
  const liveStatus = useLiveTail(isLiveRoute(route));

  const flowWindow = useFlowWindow(nowMs);
  // (#1800 P1) The event log follows the ROUTE, not the clock. On a
  // `#session=`/`#<date>` route this is that slice; on a live route it is
  // still the rolling window. Before this, every route got the live window,
  // so a session's stage and its event log described different things.
  const routeRecords = useRouteRecords(route, flowWindow);
  const liveMachines = useLiveMachines();
  const specsQuery = useQuery({
    queryKey: queryKeys.machineSpecs(),
    queryFn: () => fetchJson<MachineSpecs>("/machine/specs"),
  });
  const specs = specsQuery.data?.ok ? specsQuery.data.data : null;

  const localUid = useMemo(
    () => localMachineUid(flowWindow.data, liveMachines, specs?.machine_id ?? null),
    [flowWindow.data, liveMachines, specs],
  );
  const localName = localUid != null ? nameOf(flowWindow.data, liveMachines, localUid) : null;
  // (drill-in packet) The MACHINE route's own target — the local machine
  // for `uid: null`, or the drilled uid's own name otherwise. `nameOf` is
  // uid-generic (works for a remote uid too, via its presence beat or flow
  // records — see `lib/flow.ts`), so this is the same lookup `MachineLens`
  // itself does for its header, not a second implementation.
  const targetMachineName =
    route.kind === "machine" ? (route.uid != null ? nameOf(flowWindow.data, liveMachines, route.uid) : localName) : null;

  const ready = useMemo(() => readyParts(flowWindow.data, liveMachines, nowMs), [flowWindow.data, liveMachines, nowMs]);
  const metaLines = useMemo(() => computeMetaLines(flowWindow.data, liveMachines, nowMs), [flowWindow.data, liveMachines, nowMs]);

  // `logscope` is no longer SHOWN — the outer UI owns context (see
  // EventLogColumn's header). It is still computed and still rendered into a
  // HIDDEN span, because legacy's own span keeps its text and `innerText`
  // falls back to `textContent` for an unrendered element: if this port
  // emitted nothing, the two would disagree in the parity extraction. The
  // values are lowercase now for the same reason — CSS `text-transform`
  // never applies to text that is not rendered, so legacy's raw text is what
  // both sides must match. All of this dies with legacy at the flip.
  const { crumb, logscope } = routeChrome(route, targetMachineName);

  useSyncHash(route);

  return (
    <div className="app-shell">
      {/* (Chrome packet) The masthead — brand, build chip, the catalog
          trigger, the live/mode badge, refresh, topnav — moved out of this
          function into its own component; see `Masthead.tsx`'s own doc for
          why `<LiveStatusBadge>`'s live-route gating (the QA note that used
          to live on this line) now lives there instead. Precedes
          `.app-shell__crumbbar`, matching legacy's DOM order (`.top` before
          `.crumbbar`). */}
      <Masthead route={route} liveStatus={liveStatus} />
      <div className="app-shell__crumbbar">
        <NavChrome route={route} />
        <header className="app-shell__crumb" id="crumb">
          {crumb}
        </header>
        <div className="app-shell__meta" id="meta">
          {/* `whiteSpace: "pre"` — the idle headline's literal double space
              before "· last run" (see `metaLine.ts`'s module doc) is an
              artifact of legacy's icon SPAN breaking the whitespace-collapse
              run; default `white-space: normal` would collapse it back to
              one space here, since there's no element in the way. Preserving
              it verbatim is simpler and more robust than reproducing the
              icon-boundary quirk with a real (empty) element. */}
          {ready ? (
            <div><ReadyHeadline n={ready.n} ago={ready.ago} /></div>
          ) : (
            <div style={{ whiteSpace: "pre" }}>{metaLines[0]}</div>
          )}
          <div style={{ whiteSpace: "pre" }}>{metaLines[1]}</div>
        </div>
      </div>
      {/* (Chrome packet) `.wrap` — `#stage` beside the event-log column,
          exactly the legacy DOM shape (`.stage` then `.log`, siblings inside
          `.wrap`). `EventLogColumn` (which now owns `#logscope`, moved out
          of the always-rendered standalone span this file used to have —
          see that component's own doc for why: rendering it loose above the
          stage regardless of lens produced the stray uppercase "FLEET" the
          operator caught) is ALWAYS mounted — `visible={showsEventLog(route)}`
          toggles a CSS `display:none` class on it instead of conditionally
          unmounting (see `EventLogColumn.tsx`'s own `visible` doc for why
          unmounting is wrong: legacy's real `#logscope` stays present, with
          real text, even when its ancestor is hidden — `next-parity.spec.ts`'s
          byte-parity goldens for the machine lens depend on that). A hidden
          flex item doesn't consume row width, so `#stage` still fills the
          row on its own — no separate CSS class needed here. */}
      <div className="app-shell__content">
        <main className="app-shell__stage" id="stage">
          {renderRoute(route)}
        </main>
        <EventLogColumn
          scopeLabel={logscope}
          records={routeRecords.records}
          visible={showsEventLog(route)}
          loading={routeRecords.loading}
          error={routeRecords.error}
        />
      </div>
    </div>
  );
}

/** `renderCrumb()` (viewer.html:2476-2568) + each lens's own
 * `$("logscope").textContent=` assignment, folded into one lookup keyed on
 * [[Route]]. `machine`/`fleet`/`session`/`playback`/`mission-redirect` all
 * have a real, source-cited `logscope` mapping (the last three added this
 * packet, once `EventLogColumn` gave `#logscope` somewhere to render — see
 * that function's own doc); `unknown` stays at the empty `crumb`/`logscope`
 * default (matching legacy's actual default for those levels — see e.g.
 * `goldens/fleet.txt`'s `(empty)` crumb).
 *
 * `console`/`runs` DO carry a logscope ("console"/"runs", matching
 * `goldens/console.txt` and `runs.txt`) even though their column is hidden.
 * Legacy keeps `#logscope` in the DOM and hides only `.log` via CSS, so
 * `innerText` still falls back to `textContent` there — which is exactly why
 * the column is ALWAYS mounted and merely CSS-hidden (see `EventLogColumn`'s
 * `visible` doc). Corrected at the QA gate: the previous wording claimed both
 * the opposite scope AND conditional unmounting, contradicting the code
 * twenty lines below it. None of
 * these are byte-parity targets for `#crumb` (see each component's own doc
 * for why), so inventing crumb text for them would be UX decoration, not a
 * port. */
function routeChrome(route: Route, targetMachineName: string | null): { crumb: string; logscope: string } {
  if (route.kind === "machine") {
    // `$("crumb").innerHTML = state.machine!=null ? escN(state.machine) :
    // "this machine"` (viewer.html:2537); `$("logscope").textContent =
    // m!=null?nameOf(m):"machine"` (viewer.html:1799). `targetMachineName`
    // (computed below in `App()`) resolves the ROUTE's machine — the local
    // one for `uid: null`, or the drilled uid's own name for a fleet-card
    // drill — matching `escN(state.machine)`'s uid-generic lookup, not
    // hardcoded to "this daemon's own name" the way the pre-drill-in
    // single-route version of this function was.
    return { crumb: targetMachineName ?? "this machine", logscope: targetMachineName ?? "machine" };
  }
  if (route.kind === "fleet") {
    // `$("logscope").textContent="fleet"` (viewer.html:1668) — legacy's
    // literal string is lowercase; `goldens/fleet.txt` shows it UPPERCASE
    // because `#logscope` sits inside `.loglist h3`, which carries
    // `text-transform:uppercase` (that whole event-log sidebar isn't ported
    // yet — see `ui/README.md`'s deferred list). This port has no
    // `.app-shell__logscope` CSS rule at all (the machine lens's real
    // machine-name `logscope` above must render mixed-case, unchanged), so
    // — same discipline as `lib/format.ts`'s "uppercase the STRING
    // directly" helpers — the string here is ALREADY uppercase rather than
    // leaning on a CSS rule this port doesn't have.
    return { crumb: "", logscope: "fleet" };
  }
  // (Chrome packet) `#logscope`'s CASE depends on VISIBILITY, not just its
  // raw JS-set value — a real, verified legacy quirk, not an assumption:
  // `.loglist h3{text-transform:uppercase}` only applies while `#logscope`
  // is actually RENDERED (`showsEventLog(route)` true); once an ancestor
  // gets `display:none` (`runs`/`console`/`machine`), the element is "not
  // rendered" per the CSS spec, and `innerText` falls back to raw
  // `textContent` — NO text-transform applied — same finding
  // `showsEventLog`'s own doc cites for TEXT PRESENCE, extended here to
  // CASE. Proven two ways: (1) `goldens/session-task-list.txt`'s logscope
  // reads "TASK-LIST" (uppercased — session drill-in is a VISIBLE-log
  // route) while `goldens/machine.txt`'s reads "MacBook-Pro" (raw,
  // unchanged — machine is a HIDDEN-log route), both from the SAME
  // `.loglist h3` rule; (2) a throwaway Playwright probe read
  // `getComputedStyle('#logscope').textTransform` as "uppercase" in BOTH
  // states while `innerText` only reflected it in the fleet (visible)
  // state — confirming the CSS cascade is identical, only RENDEREDNESS
  // differs. So each branch below pre-uppercases (or doesn't) to match
  // what CSS would visually do for THAT route's fixed visibility — same
  // "uppercase the STRING directly" discipline as the `fleet`/`machine`
  // branches above, just now visibility-aware instead of uniformly assumed
  // visible.
  if (route.kind === "session") {
    // `$("logscope").textContent=sid` (viewer.html:2042,
    // `renderSubsystem()`) — a VISIBLE-log route, so uppercased.
    return { crumb: "", logscope: route.sessionId };
  }
  if (route.kind === "playback") {
    // A bare-date hash never reassigns `state.level` away from its `"fleet"`
    // default (verified: no `state.level=` assignment sits on the
    // `targetDate()`/playback boot path — see `showsEventLog`'s doc for the
    // same read), so legacy's `renderFleet()` sets the same `"fleet"`
    // logscope (viewer.html:1668) it does on a live fleet view — VISIBLE,
    // already uppercase.
    return { crumb: "", logscope: "fleet" };
  }
  if (route.kind === "mission-redirect") {
    // `$("logscope").textContent="mission"` (viewer.html:2730,
    // `renderMissionStatic()`) — the daemon-less static summary this route
    // stands in for. Moot in practice (a real daemon always navigates away
    // before this would paint — see `MissionReplay`'s own doc), named for
    // completeness. A VISIBLE-log route (`mission` isn't in `showsEventLog`'s
    // hidden set), so uppercased.
    return { crumb: "", logscope: "mission" };
  }
  if (route.kind === "console") {
    // `$("logscope").textContent="console"` (viewer.html:4513) — a HIDDEN-
    // log route (`showsEventLog`), so left RAW/lowercase, matching what
    // legacy's own `textContent` fallback would show if inspected the same
    // way (never visually seen either way, but real for DOM fidelity).
    return { crumb: "", logscope: "console" };
  }
  if (route.kind === "runs") {
    // `$("logscope").textContent="runs"` (viewer.html:4676) — HIDDEN, raw.
    // `$("crumb").innerHTML = state.level==="lab-run" ? esc(state.labRunDir||"—")
    // : ""` (viewer.html:2575, `inRuns` branch) — drill-in packet: `route.run`
    // is only ever populated once the operator is genuinely looking at a
    // lab-run-detail pane (see `route.ts`'s widened `run` doc), matching
    // legacy's own gate exactly.
    return { crumb: route.run ?? "", logscope: "runs" };
  }
  return { crumb: "", logscope: "" };
}

function renderRoute(route: Route) {
  switch (route.kind) {
    case "fleet":
      return <FleetLens />;
    case "runs":
      return <RunsBoard initialKind={route.runsKind} initialRun={route.run} />;
    case "machine":
      return <MachineLens uid={route.uid} />;
    case "console":
      return <ConsolePanel initialPanelId={route.panelId} />;
    case "session":
      // Packet 4: a real fetch to /flow-session/<id> — see SessionReplay's
      // own doc for why the RENDER (not the fetch) is still a not-ported
      // notice.
      return <SessionReplay sessionId={route.sessionId} />;
    case "mission-redirect":
      // Packet 4: a real fetch to /flow-mission/<id>, conditionally
      // navigating to /mission/<id>/graph exactly like legacy's boot() does
      // — see MissionReplay's own doc for why this completes Packet 1's
      // deferred placeholder rather than staying inert.
      return <MissionReplay missionId={route.missionId} />;
    case "playback":
      // Packet 4: a bare #<date> hash — see PlaybackLens's own doc for why
      // this is a named not-ported notice rather than a full historical
      // render.
      return <PlaybackLens date={route.date} />;
    case "unknown":
      return <LensPlaceholder label="unrecognized" hash={route.hash} />;
  }
}
