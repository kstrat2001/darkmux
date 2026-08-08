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
 * tab) AND `mission-redirect`.
 *
 * QA correction (2026-08-09): an earlier version of this comment mapped
 * `mission-redirect` to the CONSOLE tab, reasoning from `inMission` in the
 * `.on`-class assignment above. QA MEASURED live against the real corpus
 * that this is wrong for the path our port actually exercises: `inMission`
 * (`state.level==="mission"`) is only ever reached through
 * `renderMissionStatic()`, legacy's DAEMON-LESS static-build fallback for
 * `#mission=<id>` — a code path this app never runs (see `route.ts`'s own
 * module doc: this app always has a daemon). On a live daemon (this
 * harness's setup, and every real `/next` deployment), `#mission=<id>`
 * does a FULL NAVIGATION (`location.href = "/mission/<id>/graph"`) before
 * `inMission` is ever computed — nothing about `.lenstabs` is observable
 * mid-redirect, and the `#mission=` hash is typically reached FROM the
 * fleet hero (a mission card), not the console board. `fleet` is the
 * faithful placeholder-state mapping for the live-daemon path; if a future
 * packet reaches `#mission=<id>` some other way (e.g. from within the
 * console board), revisit this mapping against how THAT click actually
 * arrives.
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
    case "mission-redirect":
      return tab === "fleet";
    case "runs":
      return tab === "runs";
    case "machine":
      return tab === "machine";
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
