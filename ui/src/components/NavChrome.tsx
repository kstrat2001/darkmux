import type { Route } from "../lib/route";

const TABS = [
  { act: "fleet", label: "fleet" },
  { act: "console", label: "console" },
  { act: "runs", label: "runs" },
  { act: "machine", label: "machine" },
] as const;

type TabAct = (typeof TABS)[number]["act"];

/** Legacy: `viewer.html:2504-2508`'s `lf`/`lm`/`ll`/`lmm` `.on`-class
 * assignment, folded into a switch over [[Route]]. `fleet` is "on" whenever
 * the operator is NOT inside runs/machine/console (legacy:
 * `(inMission||inRuns||inMachine||inConsole)?"":" on"`) — which includes
 * the `session` drill-in (legacy's `state.level==="subsystem"` state also
 * leaves fleet lit, since a session drills IN from fleet, not from a lens
 * tab) AND `mission`.
 *
 * QA correction (2026-08-09, pre-#1868): an earlier version of this comment
 * mapped the mission route to the CONSOLE tab, reasoning from `inMission` in
 * the `.on`-class assignment above. QA MEASURED live against the real corpus
 * that this was wrong for the path this port actually exercised at the
 * time: legacy's `inMission` (`state.level==="mission"`) is only ever
 * reached through `renderMissionStatic()`, its DAEMON-LESS static-build
 * fallback for `#mission=<id>` — a code path this app never ran (this app
 * always has a daemon). `fleet` was the faithful placeholder mapping then,
 * matching legacy's real live-daemon behavior (a full navigation before
 * `inMission` was ever computed).
 *
 * That reasoning still holds post-#1868, for a different concrete reason:
 * `MissionGraphLens` (#1868) now genuinely RENDERS for this route, but it is
 * reached FROM the fleet hero (a mission card) or the runs board, not from
 * any `.lenstabs` tab — there is no dedicated "mission" tab to light, and
 * the lens's own header (not `NavChrome`) is its in-page navigation.
 * `fleet` stays the honest placeholder mapping: an operator arriving here
 * came from fleet-adjacent surfaces, and no OTHER tab claims to be "where
 * you are" either.
 *
 * `unknown` lights no tab — there's no legacy analog (an unrecognized
 * `lens=` silently falls back to fleet there; this port renders it as a
 * visibly distinct placeholder instead, a documented deviation — see
 * route.ts's module doc), so highlighting a tab the operator didn't
 * navigate to would be misleading. */
function isActive(route: Route, tab: TabAct): boolean {
  switch (route.kind) {
    case "fleet":
    case "session":
    case "mission":
      return tab === "fleet";
    case "runs":
      return tab === "runs";
    // (#1809) A fleet-card drill (`route.uid != null`) is the SAME shape as
    // the session drill above it — arriving IN from fleet, not from a lens
    // tab — so it keeps FLEET lit, matching that case's own reasoning. Only
    // the tab click / bare `#lens=machine` deep-link (`uid == null`, always
    // "the local machine" — see `route.ts`'s own doc on the widened
    // variant) lights MACHINE. Before this, the stage's own `fleet ›
    // machine · <name>` back-link said "child of fleet" while the tab bar
    // said "sibling of fleet" for the exact same page — the contradiction
    // #1809 traced back to #1508 step 2 unifying the two views without
    // revisiting the nav.
    case "machine":
      return route.uid == null ? tab === "machine" : tab === "fleet";
    case "console":
      return tab === "console";
    case "unknown":
      return false;
    case "playback":
      // (merge of packets 1.5 + 4) A bare-date playback view is a
      // time-scoped FLEET rendering in legacy (`live=false`, same hero) —
      // fleet is the honest tab.
      return tab === "fleet";
  }
}

function targetHash(tab: TabAct): string {
  switch (tab) {
    case "fleet":
      return "";
    case "console":
      return "lens=console";
    case "runs":
      return "lens=runs";
    case "machine":
      return "lens=machine";
  }
}

/**
 * The lens-tab bar — `/next`'s port of legacy's `.lenstabs`
 * (`viewer.html:816`: `<a id="lens-fleet">fleet` · `<a id="lens-console">console`
 * · `<a id="lens-runs">runs` · `<a id="lens-machine">machine`, in that exact
 * DOM order). Order is NOT reordered at any width — the phone-width
 * "broadest-scope-first" reflow (`tests/e2e/chrome-order.spec.js`) is about
 * the OUTER chrome regions (`#meta` above this bar, this bar above `#crumb`),
 * not about the tabs' own order within the bar; see `styles.css`'s matching
 * media query for that half.
 *
 * Navigates by WRITING `location.hash` directly (not `history.replaceState`
 * — that's the separate write-BACK half's job, see `lib/hashSync.ts`'s
 * module doc). `useHashRoute`'s `hashchange` listener is what actually
 * re-renders the app, so a tab click is indistinguishable from the operator
 * typing/pasting a new hash by hand — the deep-link equivalence the port
 * plan's Rule 2 requires (the CLI prints these same hashes as deep links).
 *
 * Every tab renders regardless of whether its lens has landed yet
 * (`console`: Packet 6, not yet merged as of this packet) — the
 * render-sanity contract is "navigating to an unported lens shows
 * [[LensPlaceholder]]", never "the tab doesn't exist" (operator-authored
 * perfection posture: a stuck feature gets a visible placeholder, not a
 * missing affordance).
 */
export function NavChrome({ route }: { route: Route }) {
  return (
    <nav className="app-shell__navtabs" aria-label="lens navigation">
      {TABS.map(({ act, label }) => {
        const active = isActive(route, act);
        return (
          <a
            key={act}
            id={`lens-${act}`}
            className={`nav-tab${active ? " on" : ""}`}
            data-act={act}
            href={"#" + targetHash(act)}
            aria-current={active ? "page" : undefined}
            onClick={(e) => {
              e.preventDefault();
              location.hash = targetHash(act);
            }}
          >
            {label}
          </a>
        );
      })}
    </nav>
  );
}
