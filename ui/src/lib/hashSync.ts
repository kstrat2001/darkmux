import { useEffect } from "react";
import type { Route } from "./route";
import { canonicalOptPairs } from "../lenses/console/panels";

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
 * Scope: the params every ported lens drives (`lens`/`kind`/`panel`/
 * `session`/`uid`/`machine`) are written. `mission` (#1868 — the
 * mission-graph lens) stays out of scope for this write-back path too, for
 * a DIFFERENT reason than before this packet: `#mission=<id>` used to be a
 * full navigation away (nothing to write back to); now it renders in-place,
 * but the hash the operator/a caller navigates to (`location.hash =
 * "mission=<id>"`, set directly by `RunsBoard`/`CatalogPanel`) IS already
 * the canonical form — there is no derived/compressible state on top of it
 * the way `runs`' kind/run/machine params fold together, so there is
 * nothing for this function to add.
 *
 * `run` (the lab-run-detail deep link, drill-in packet): now WRITTEN, once
 * `LabRunDetail` (`lenses/runs/LabRunDetail.tsx`) gave `run` a real
 * destination to point at — matching legacy's own `syncLabHash`, which
 * writes `run` only when `state.level==="lab-run"` (a `drillLabRun` that
 * actually resolved a dir). An earlier version of this doc (pre-drill-in)
 * recorded that `run` was dropped on purpose, reasoning that every `run=`
 * this port could receive was structurally unresolvable since the
 * drill-down didn't exist yet — that reasoning no longer applies now that
 * it does; this paragraph replaces it rather than leaving a stale account
 * of a since-closed gap.
 *

 * `mission` and `unknown` routes are likewise never canonicalized: see the
 * module doc above for why `mission` has nothing to add, and rewriting an
 * `unknown` hash would silently "fix" what should stay a visible,
 * debuggable broken bookmark (`LensPlaceholder` names the raw hash for
 * exactly this reason — see that component's doc).
 */
export function canonicalHash(route: Route): string | null {
  switch (route.kind) {
    case "fleet":
      return "";
    case "runs": {
      const p = new URLSearchParams();
      p.set("lens", "runs");
      if (route.runsKind && route.runsKind !== "all") p.set("kind", route.runsKind);
      // (drill-in packet) `run` now HAS a real destination — the lab-run
      // detail view (`LabRunDetail`) — so it earns a place in the canonical
      // hash again, matching legacy's own `state.level==="lab-run"` write
      // (`if(state.level==="lab-run"&&state.labRunDir!=null)p.set("run",...)`
      // in `syncLabHash`). Written whenever `route.run` is set, INDEPENDENT
      // of `runsKind` — legacy's own gate is `state.level==="lab-run"`, not
      // `state.runsKind==="lab"`: a lab row is visible (and clickable) under
      // BOTH kind=all and kind=lab (any OTHER kind filter excludes lab rows
      // entirely — see `runsFiltered`), so the reachable hash forms are
      // `run=` alone (kind=all, no `kind=` param at all) and
      // `kind=lab&run=`, both real. See this file's module doc for why this
      // REVERSES the prior QA correction, now that the drill-in exists to
      // preserve `run` for.
      if (route.run) p.set("run", route.run);
      // (#1809) The machine pin, written whenever set — composable with
      // `kind`/`run` above, independent params on the same hash (matching
      // `route.ts`'s own doc: a pinned kind filter and a pinned lab-run
      // drill-in are both real, simultaneously reachable states).
      if (route.machine) p.set("machine", route.machine);
      return p.toString();
    }
    case "machine": {
      const p = new URLSearchParams();
      p.set("lens", "machine");
      // (drill-in packet) `uid` — see `route.ts`'s own doc on the widened
      // `machine` route: a genuine widening beyond legacy's own address bar
      // (which never named the drilled uid), written only for a REMOTE/
      // explicit drill (`uid` non-null); the local nav-tab/deep-link entry
      // stays exactly `#lens=machine`, unchanged from before this packet.
      if (route.uid) p.set("uid", route.uid);
      return p.toString();
    }
    case "console": {
      const p = new URLSearchParams();
      p.set("lens", "console");
      // (#1904 QA fix) Was `route.panelId && route.panelId !== "mission-status"`
      // — legacy's own `pid==="mission-status" ? delete("panel") : ...`
      // collapse, valid back when "" (no panel param) and "mission-status"
      // were the SAME landing state. #1904 broke that premise: "" now means
      // the client-rendered activity view, a genuinely different thing from
      // the `mission-status` CLI panel. Keeping the collapse made
      // `mission-status` unreachable by URL — this function would silently
      // rewrite `#lens=console&panel=mission-status` back to bare
      // `#lens=console` on the very next re-render, so a reload/bookmark of
      // that exact address landed on the activity view instead. Only the
      // genuinely-empty case omits the param now.
      if (route.panelId) p.set("panel", route.panelId);
      // (#1911) Non-default `opt.<name>` selections, sorted by name — the
      // SAME `canonicalOptPairs` function `queryKeys.panel` keys its cache
      // on, so the two tiers cannot disagree about what "the current
      // variant" means. This is also the alias-upgrade path: a
      // `panel=mission-status-all` deep link already parsed to
      // `{panelId:"mission-status", opts:{all:"all"}}` (see
      // `route.ts::PANEL_ALIASES`), so writing it back here rewrites the
      // address bar to `panel=mission-status&opt.all=all` — the same
      // `#lens=lab` → `#lens=runs&kind=lab` upgrade this function already
      // performs for the runs lens.
      if (route.panelId) {
        for (const [name, value] of canonicalOptPairs(route.panelId, route.opts)) {
          p.set(`opt.${name}`, value);
        }
      }
      return p.toString();
    }
    case "session": {
      const p = new URLSearchParams();
      p.set("session", route.sessionId);
      return p.toString();
    }
    case "mission":
    case "unknown":
      return null;
    case "playback":
      // (merge of packets 1.5 + 4) A bare `#<date>` hash IS its canonical
      // form — legacy's syncLabHash never rewrites the date view, and
      // canonicalizing it here would erase the date from the address bar.
      return null;
  }
}

/** Imperative half — the actual `replaceState` write, with the same
 * no-change guard `useSyncHash`'s effect uses. Exported standalone for a
 * lens to call directly at the moment its own out-of-route state changes
 * (see the module doc — today's one caller was `RunsBoard`'s kind chips;
 * the lab-run drill-in (`openLabRun`/`closeLabRun`) is a second, added when
 * the drill-in packet gave `run` a real destination).
 *
 * `history.replaceState` never fires `hashchange` (that's WHY it was
 * chosen over `pushState`/a direct `location.hash=` assignment — see the
 * module doc's "lens hops must not spam history" note). But `useHashRoute`
 * (`lib/useHashRoute.ts`) is the ONLY thing that turns a URL change into a
 * React re-render, and it subscribes to exactly that event — so a
 * `writeHash`-only navigation (no other App-level state happens to change
 * afterward) left `App.tsx`'s `route` — and everything derived from it,
 * `#crumb` in particular — permanently stale. Caught live: drilling a lab
 * run wrote `run=<dir>` to the address bar correctly, but `#crumb` (which
 * reads `route.run`) stayed blank for the rest of the session, not just a
 * render behind — probed directly against the running harness for 10s of
 * wall-clock with no App-level re-render ever landing to pick the new
 * `location.href` up. Firing a synthetic `hashchange` here closes that
 * gap without reintroducing the history-spam problem `replaceState` exists
 * to avoid: the event has no effect on `history.length` on its own, it
 * only tells `useHashRoute`'s subscriber to re-check `location.href` (via
 * `getSnapshot`'s own `cachedHref` guard) the same way a real hash edit
 * would. Dispatched only when a write actually happened (after the
 * no-change guards below), so this can't loop: `useSyncHash`'s effect
 * re-runs on the resulting route change, calls `writeHash` again with the
 * now-current canonical form, and the `next === current` guard makes that
 * second call a no-op before it reaches this line. */
export function writeHash(next: string | null): void {
  if (next === null) return;
  const current = (location.hash || "").replace(/^#/, "");
  if (next === current) return;
  const url = next ? "#" + next : location.pathname + location.search;
  history.replaceState(null, "", url);
  window.dispatchEvent(new Event("hashchange"));
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
