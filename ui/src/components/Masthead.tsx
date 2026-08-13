import { useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { LiveStatusBadge, PlaybackModeBadge } from "./LiveStatusBadge";
import { CatalogPanel } from "../lenses/catalog/CatalogPanel";
import { isLiveRoute, type Route } from "../lib/route";
import { todayUTC } from "../lib/flow";
import { injectedMeta } from "../lib/injectedMeta";
import { isStaticBuild } from "../lib/staticSource";
import type { LiveTailStatus } from "../hooks/useLiveTail";

/**
 * The masthead (`.top`, viewer.html:802-816) — brand, build-identifier chip,
 * the playback-catalog trigger, the live/mode badge, the manual-refetch
 * control, and the topnav links. Mounted once at `App.tsx`'s root, ABOVE
 * `.app-shell__crumbbar` (matching legacy's DOM order: `.top` precedes
 * `.crumbbar`).
 *
 * **The version/build-hash chip is the one deliberately volatile piece of
 * this component** — `darkmux-version` carries a git SHA that changes on
 * every commit and a semver that changes every release (see
 * `injectedMeta`'s own doc). This component does not try to hide that from
 * a REAL running daemon (an operator SHOULD see which build is serving
 * their page) — the volatility is handled at the PARITY-HARNESS layer
 * instead: `tests/parity/lib/extract-lens.js`'s `extractTopbarText`
 * normalizes any populated verbadge text to a fixed placeholder before it
 * ever reaches a committed golden, so a real release never makes that
 * golden flaky. See that function's own doc for why normalizing (not
 * excluding the region) was the chosen fix.
 *
 * **Deliberately NOT wired to an about-modal.** Legacy's populated chip is
 * also the trigger for `#imodalbg` (viewer.html:1132, the build/status
 * snapshot dialog) — out of scope for this packet (not itemized in the
 * masthead's carried-over piece list), so the chip here is informational
 * only (a `title` attribute repeats the same text on hover, same as
 * legacy's `vb.title`), not `role="button"`. Named here as a follow-up
 * rather than half-wiring a modal with no content behind it.
 *
 * **The catalog trigger (legacy's `#srcbadge`, "today"/a specific date,
 * doubling as the playback-catalog toggle) is represented by the EXISTING,
 * already-tested `<CatalogPanel>` component moved in here from `App.tsx`,
 * not reproduced as a second, separate element.** `CatalogPanel`'s own
 * toggle button already provides the real interaction (open/close,
 * outside-click + Escape dismissal, live/mission/day rows) with a full test
 * suite (`CatalogPanel.test.tsx`) pinned to that button's ACCESSIBLE NAME
 * ("browse history", via `aria-label` — see `CatalogPanel.tsx`'s own doc).
 * This component passes `label={srcbadgeText(route)}` to override the
 * button's VISIBLE text to legacy's actual `#srcbadge` content ("TODAY" in
 * live mode, matching `setBadges()`'s `dl` — viewer.html:3432/3439) —
 * literally pre-uppercased, the SAME "uppercase the STRING directly, don't
 * lean on a CSS rule this port doesn't reproduce" discipline `App.tsx`'s
 * `routeChrome` already uses for the fleet `#logscope` value (see that
 * function's own comment) — so the accessible name and the on-page text are
 * deliberately decoupled: a screen reader always hears "browse history", a
 * sighted operator reading the masthead sees "TODAY" (or a date), matching
 * BOTH `next-parity.spec.ts`'s strict byte comparison against
 * `goldens/fleet.txt`/`goldens/machine.txt` (which now includes the
 * masthead — see `extract-lens.js`'s `extractTopbarText`) AND
 * `CatalogPanel.test.tsx`'s own accessible-name-based queries, which never
 * render through this component and so never see the override.
 *
 * **On a static build (#1801, `isStaticBuild()`), the badge is TEXT, not a
 * `<CatalogPanel>`.** Legacy's own gate is `if(!flowSrc && mode!=="no-daemon"){
 * sb.dataset.act="catalog"; ... }` (viewer.html:3936) — `#srcbadge` becomes
 * the history-browser trigger ONLY when a real daemon is behind the page;
 * the static demo's badge stays inert. `CatalogPanel`'s toggle fetches
 * `/flow-days` + `/flow-missions`, neither of which the static demo ships a
 * fixture for (out of scope per #1801's brief — only `-flow-src`/`-runs-src`/
 * `-lab-runs-src` have consumers), so mounting the real toggle there would
 * render a working-looking button that 404s on click. The plain `<span>`
 * below carries the same VISIBLE text (`srcbadgeText`) with no click
 * handler and no fetch — the honest equivalent of legacy's un-upgraded
 * `#srcbadge`.
 *
 * `#modebadge` (`<LiveStatusBadge>`) needs no equivalent split: its
 * lowercase JS-rendered text ("● live"/"◌ reconnecting") is matched to
 * legacy's CSS-uppercased visual (`.pb{text-transform:uppercase}`,
 * viewer.html:400ish) by a real `text-transform: uppercase` rule on
 * `#modebadge.pb` in `styles.css` instead — CSS, not a literal string, is
 * the right tool THERE because `next-parity-live.spec.ts`'s own assertions
 * (`toContainText("live")`/`toContainText("reconnecting")`) read
 * `textContent`, not the rendered/`innerText` value, so they're
 * unaffected by a CSS transform (verified with a throwaway Playwright probe
 * — `text-transform:uppercase` on an element does NOT change what
 * `toContainText` matches against) — unlike `#srcbadge`, which has no
 * shared component with its own case-sensitive test suite to protect.
 *
 * Moving `<CatalogPanel>`'s MOUNT POINT here is a pure relocation beyond
 * the `label` prop: its internal markup/tests are otherwise untouched, and
 * `position:absolute`-based dropdown positioning is anchor-relative, not
 * page-relative, so the move doesn't affect it.
 */
export function Masthead({ route, liveStatus }: { route: Route; liveStatus: LiveTailStatus }) {
  const queryClient = useQueryClient();
  const [spinning, setSpinning] = useState(false);
  const live = isLiveRoute(route);

  const verMeta = injectedMeta("darkmux-version");
  const schemaMeta = injectedMeta("darkmux-flow-schema");
  const verText = verMeta ? `v${verMeta}${schemaMeta ? ` · schema ${schemaMeta}` : ""}  ⓘ` : "";
  const verTitle = verMeta ? `darkmux ${verMeta}${schemaMeta ? ` · flow schema ${schemaMeta}` : ""} — about` : undefined;

  // `refreshbtn` — viewer.html:809/3439-ish (`refetchLive()`). No single
  // legacy-equivalent "refetch exactly the live window" hook is exposed
  // from `useLiveTail` today (it owns SSE + a 20s reconcile backstop
  // internally, not a manual trigger) — invalidating every active query is
  // the real, working "force everything to refetch now" action a manual
  // tap wants, and is TanStack Query's own idiom for it. `spinning` mirrors
  // `.rfbtn.spin`'s CSS animation class, cleared once the fetch settles.
  function refetchNow() {
    setSpinning(true);
    void queryClient.invalidateQueries().finally(() => setSpinning(false));
  }

  return (
    // `top` is a SECOND class, purely an extraction hook — the parity
    // harness's `extractTopbarText` selects `.top` (legacy's own class name
    // for this element) against BOTH apps, the same cross-app-selector-reuse
    // convention `#crumb`/`#meta`/`#logscope`/`#stage` already establish.
    // `masthead`/`masthead__*` (below and in `styles.css`) are this port's
    // own BEM-style styling hook, unrelated to the extractor.
    <header className="masthead top">
      <span className="masthead__brand">
        {/* (operator) Just the name. "· observability" labelled the category
            of the thing you are already looking at — decoration in a view
            whose stated goal is less noise. Changed in the LEGACY viewer too,
            so the parity goldens rebaseline from a source that genuinely
            changed rather than being edited to match the port. */}
        <a href="https://darkmux.com/" target="_blank" rel="noopener">
          <b>darkmux</b>
        </a>
      </span>
      {verText ? (
        <span className="masthead__ver" id="verbadge" title={verTitle}>
          {verText}
        </span>
      ) : (
        <span className="masthead__ver" id="verbadge" />
      )}
      {isStaticBuild() ? (
        // (#1801) No `<CatalogPanel>` here — see this component's own doc
        // for why a static build gets inert text instead of a button that
        // would 404 on click.
        <span className="masthead__srcbadge">{srcbadgeText(route)}</span>
      ) : (
        <CatalogPanel label={srcbadgeText(route)} />
      )}
      {live ? <LiveStatusBadge status={liveStatus} /> : route.kind === "playback" ? <PlaybackModeBadge /> : null}
      {/* (operator: "a reload button next to 'live' is absurd") — and it is:
          a refresh control beside a badge reading `● LIVE` contradicts
          itself. If the view is live there is nothing to refresh; if you
          need to refresh, it is not live. The one state where a manual retry
          genuinely helps is `◌ RECONNECTING`, so the button and the badge
          are now mutually exclusive by MEANING rather than by screen width
          (the earlier mobile-only hide was the same instinct, argued from
          the wrong premise). */}
      {live && liveStatus !== "live" ? (
        <button
          type="button"
          className={`masthead__refresh${spinning ? " spin" : ""}`}
          onClick={refetchNow}
          title="Refetch now"
          aria-label="Refetch now"
        >
          ⟳
        </button>
      ) : null}
      <nav className="masthead__nav">
        <a href="https://darkmux.com/" target="_blank" rel="noopener">
          home
        </a>
        <a href="https://darkmux.com/guide/" target="_blank" rel="noopener">
          guide
        </a>
        <a href="https://darklyenergized.substack.com" target="_blank" rel="noopener">
          articles
        </a>
        <a href="https://github.com/kstrat2001/darkmux" target="_blank" rel="noopener">
          github
        </a>
      </nav>
    </header>
  );
}

/** `setBadges()`'s `sb.textContent` assignment — viewer.html:3432/3439.
 * Legacy's full mode matrix is `live`/`play`/`empty`/`no-daemon`; this app
 * only ever occupies the `live` (rolling window) shape for `fleet`/`runs`/
 * `machine`/`console`/`unknown` (`isLiveRoute`'s own doc), so those all
 * collapse to legacy's `dl` — `"today"` whenever the live window's date is
 * today, which for a rolling live window is always (there's no code path
 * in this app that keeps `useLiveTail` running against a non-today date —
 * see `useLiveTail.ts`'s own rollover handling). `playback` has a REAL date
 * (`route.date`) to show, matching legacy's `mode==="play"` branch's date
 * component (dropping the "Flow · " prefix — not byte-tested for this
 * route and this app has no real playback data pipeline behind it yet, see
 * `PlaybackLens`'s own doc, so a literal date reads more honestly than a
 * borrowed live-mode phrase). `session`/`mission-redirect` have no natural
 * "source date" in this app (no historical fetch pipeline backs them
 * either) — "REPLAY" names what they are without inventing a fake date.
 * Pre-uppercased (see this component's own module doc for why: matches
 * `App.tsx`'s `routeChrome` precedent for the fleet `#logscope` value)
 * except the literal ISO date, which has no case to begin with. */
function srcbadgeText(route: Route): string {
  // (#1800) `"Flow · "+date` verbatim from legacy's `play` arm, pre-uppercased
  // per this component's own module doc. The previous version rendered a bare
  // ISO date and said why: the "Flow · " prefix was "a borrowed live-mode
  // phrase" for a route with no real playback pipeline behind it. That reason
  // has EXPIRED — the pipeline exists now, `goldens/playback-date.txt` reads
  // `FLOW · 2026-08-07`, and the prefix is the honest label rather than a
  // borrowed one. A same-day `#<date>` hash still reads "TODAY", matching
  // legacy's `dl` (its own boot treats today's date as live, not playback).
  if (route.kind === "playback") return route.date === todayUTC() ? "TODAY" : `FLOW · ${route.date}`;
  if (route.kind === "session" || route.kind === "mission-redirect") return "REPLAY";
  return "TODAY";
}
