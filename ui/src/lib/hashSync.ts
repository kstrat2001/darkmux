import { useEffect } from "react";
import type { Route } from "./route";

/**
 * Hash write-back — the `/next` port of legacy's `syncLabHash()`
 * (`viewer.html:3279`). Reflects the CURRENT lens state into
 * `location.hash` via `history.replaceState` (never `pushState` — a lens
 * state change must not spam browser history, matching legacy's own
 * comment: "lens hops must not spam history") so the address bar is
 * always copyable/bookmarkable. Legacy's own reasoning, unchanged here:
 * "the operator bookmarks the viewer on a phone via tailscale-serve —
 * tab-only state isn't addressable" (#1247/#1639).
 *
 * Three pieces, matching legacy's own split between `labQuery()`/pure state
 * and `syncLabHash()`/the write:
 *
 * - `canonicalHash` (pure): given a [[Route]], returns the CANONICAL hash
 *   string (no leading `#`) legacy would write for that state. `null`
 *   means "don't touch the address bar" — the out-of-scope routes below.
 *   A route object fully determines its canonical hash — no extra
 *   out-of-URL state is needed, because `RunsBoard`'s kind filter (the one
 *   piece of lens state that lives OUTSIDE `Route` between navigations) is
 *   written back directly via `writeHash` at the moment it changes (see
 *   below), not routed back through this route-keyed path.
 * - `writeHash` (imperative): the actual `replaceState` call, with the
 *   no-change guard matching `syncLabHash`'s own `if(next===raw)return;`.
 *   Exported standalone so a lens can call it directly at the moment its
 *   OWN out-of-route state changes — `RunsBoard.tsx`'s `selectKind` is the
 *   one user today: a kind-chip click changes no `Route` (no `hashchange`
 *   fires), so the route-keyed effect below would never see it.
 * - `useSyncHash` (effect): runs `canonicalHash` on every route change and
 *   writes it back — this is what performs the legacy `#lens=lab` →
 *   `#lens=runs&kind=lab` upgrade, since arriving on the alias already
 *   parses to the canonical `Route` (`route.runsKind === "lab"`) and this
 *   effect just names it in the address bar.
 *
 * Scope: only the params THIS packet's lenses actually drive
 * (`lens`/`kind`/`panel`/`session`) are written. `run` (the lab-run-detail
 * deep link) and `mission` (the mission-graph full-navigation) are both
 * genuinely out of scope this packet — see `route.ts`'s own module doc.
 *
 * QA correction (2026-08-09): an earlier version of this doc claimed a
 * `run` param survives untouched. It does NOT — `canonicalHash`'s `"runs"`
 * branch never reads `route.run`, so booting on `#lens=runs&run=/x/y`
 * gets REWRITTEN to the bare `#lens=runs` the moment `useSyncHash`'s
 * effect fires (which includes the very first render, mount included) —
 * the `run` param is silently dropped, not preserved. QA measured that
 * this is actually the FAITHFUL analog of legacy's own behavior, not a
 * deviation from it: legacy's `syncLabHash` only WRITES `run` when
 * `state.level==="lab-run"` — i.e. when `drillLabRun` actually resolved
 * the dir and the operator is genuinely looking at a lab-run detail pane;
 * for an unresolvable `run` (the dir doesn't match anything, or — as
 * here — the drill-down code path doesn't exist at all) legacy's `else`
 * branch deletes the param too. Since this port never implements
 * `state.level==="lab-run"` at all (lab-run detail is out of scope — see
 * `route.ts`'s module doc), EVERY `run=` this port receives is
 * structurally in legacy's "unresolvable" bucket, so always dropping it
 * reproduces legacy's actual behavior for the only case this port can
 * ever hit — it isn't an omission, it's the faithful mapping. Revisit
 * this comment (and add `run` to `canonicalHash`'s `"runs"` branch) the
 * day a lab-run-detail packet lands.
 *
 * `mission-redirect` and `unknown` routes are likewise never
 * canonicalized: legacy does a full navigation away for the former
 * (nothing left to write back to), and rewriting an `unknown` hash would
 * silently "fix" what should stay a visible, debuggable broken bookmark
 * (`LensPlaceholder` names the raw hash for exactly this reason — see
 * that component's doc).
 */
export function canonicalHash(route: Route): string | null {
  switch (route.kind) {
    case "fleet":
      return "";
    case "runs": {
      const p = new URLSearchParams();
      p.set("lens", "runs");
      if (route.runsKind && route.runsKind !== "all") p.set("kind", route.runsKind);
      return p.toString();
    }
    case "machine": {
      const p = new URLSearchParams();
      p.set("lens", "machine");
      return p.toString();
    }
    case "console": {
      const p = new URLSearchParams();
      p.set("lens", "console");
      // Legacy: `pid=state.panelId||"mission-status"; if(pid==="mission-status")
      // p.delete("panel")` — the default panel is never written explicitly.
      if (route.panelId && route.panelId !== "mission-status") p.set("panel", route.panelId);
      return p.toString();
    }
    case "session": {
      const p = new URLSearchParams();
      p.set("session", route.sessionId);
      return p.toString();
    }
    case "mission-redirect":
    case "unknown":
      return null;
  }
}

/** Imperative half — the actual `replaceState` write, with the same
 * no-change guard `useSyncHash`'s effect uses. Exported standalone for a
 * lens to call directly at the moment its own out-of-route state changes
 * (see the module doc — today's one caller is `RunsBoard`'s kind chips). */
export function writeHash(next: string | null): void {
  if (next === null) return;
  const current = (location.hash || "").replace(/^#/, "");
  if (next === current) return;
  const url = next ? "#" + next : location.pathname + location.search;
  history.replaceState(null, "", url);
}

/** Effect half — call once, at the App root, with the CURRENT route. Fires
 * on every route change (tab click, deep-link boot, or a `hashchange` from
 * the operator editing the bar by hand) and writes the canonical form back. */
export function useSyncHash(route: Route): void {
  useEffect(() => {
    writeHash(canonicalHash(route));
    // `route` is a referentially-stable snapshot from `useHashRoute` (only
    // changes identity when the hash actually moved) — safe as a direct dep.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [route]);
}
