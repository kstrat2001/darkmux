import { useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { LiveStatusBadge, PlaybackModeBadge } from "./LiveStatusBadge";
import { CatalogPanel } from "../lenses/catalog/CatalogPanel";
import { AboutDialog } from "./AboutDialog";
import { openModalEl } from "../lib/dialogManager";
import { isLiveRoute, type Route } from "../lib/route";
import { todayUTC } from "../lib/flow";
import { injectedMeta } from "../lib/injectedMeta";
// (#2022) The mark, from the SAME file the site and the icon generator read —
// one source, so a future change to the identity cannot leave a stale copy
// behind in the bundle.
//
// Imported as a URL, NOT `?raw` + `dangerouslySetInnerHTML`. The first cut
// did the latter and `no-danger.test.ts` failed it, correctly: that guard is
// absolute by design, and "my string is trusted" is exactly the argument every
// XSS bug is made of. Vite inlines an asset under `assetsInlineLimit` as a
// data URI, so this stays self-contained — no network fetch, and no route the
// daemon would have to serve — while React keeps its escape-by-construction
// guarantee intact.
//
// The SMALL variant, not the full mark: the masthead sets it near 24px, where
// the four-channel diagram measured as a smudge when the icon set was built.
// Two-in/one-out still reads at that size.
import markUrl from "../brand/mark-trapezoid-out.svg";
import { getSource } from "../lib/source";
import type { LiveTailStatus } from "../hooks/useLiveTail";
import type { MachineSpecs } from "../types/handwritten";

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
 * **Now wired to the about modal (#1640).** Legacy's populated chip is also
 * the trigger for `#imodalbg` (viewer.html:1132, the build/status snapshot
 * dialog) — this component's `verbadge` is a real `data-act="about"` button
 * (only when it has content: an empty chip has nothing to show a dialog
 * about, matching legacy's own `if(vb&&verMeta)` gate) rendering
 * `AboutDialog`, restored now that the shared dialog/focus machinery exists
 * to hold it.
 *
 * **The catalog trigger (legacy's `#srcbadge`, "today"/a specific date,
 * doubling as the playback-catalog toggle) is represented by the EXISTING,
 * already-tested `<CatalogPanel>` component moved in here from `App.tsx`,
 * not reproduced as a second, separate element.** `CatalogPanel`'s own
 * toggle button already provides the real interaction (open/close,
 * outside-click + Escape dismissal, live/mission/day rows) with a full test
 * suite (`CatalogPanel.test.tsx`) pinned to that button's ACCESSIBLE NAME
 * ("browse history", via `aria-label` — see `CatalogPanel.tsx`'s own doc).
 * This component passes `label={srcbadgeText(route, replayDate)}` to override the
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
 * **On a static build (#1801, `getSource().kind === "static"`), the badge is TEXT, not a
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
export function Masthead({
  route,
  liveStatus,
  specs = null,
  replayDate = null,
}: {
  route: Route;
  liveStatus: LiveTailStatus;
  /** Passed through to `AboutDialog` for its "machine"/"hardware" rows —
   *  optional (defaults to `null`) so every existing caller/test that
   *  doesn't pass it is unaffected; only a live route ever reads it. */
  specs?: MachineSpecs | null;
  /** The day a daemon dispatch/mission page belongs to, once the shell has
   * derived it from the records; `null` until then (the chip reads "RESULT"
   * meanwhile) and on every other route. */
  replayDate?: string | null;
}) {
  const queryClient = useQueryClient();
  const [spinning, setSpinning] = useState(false);
  const live = isLiveRoute(route);

  const verMeta = injectedMeta("darkmux-version");
  const schemaMeta = injectedMeta("darkmux-flow-schema");
  // (#2107) The inline `v<semver> (<sha>) · schema <n>` text is GONE — it
  // was 200-225px of the masthead's own width budget (see `styles.css`'s
  // `.masthead__ver` sub-560px doc, which already hides the whole chip on
  // phones for exactly that reason), and the new global machine pill needs
  // that room on desktop too. The full detail survives in TWO places: the
  // `title` attribute below (a hover tooltip on the bare ⓘ) and — now
  // reachable from every viewport, phones included, where this affordance
  // itself stays hidden — the machine drawer's own header line
  // (`MachineDrawer.tsx`), which reads the SAME `injectedMeta` values.
  const verText = verMeta ? "ⓘ" : "";
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
          {/* `aria-hidden` + no alt text: the wordmark beside it already
              names the product, so announcing the mark would read the name
              twice to a screen reader. */}
          {/* Empty alt, not a description: the wordmark beside it already
              names the product, so alt text would read the name twice. */}
          <img className="masthead__mark" src={markUrl} alt="" width="24" height="24" />
          <b>darkmux</b>
        </a>
      </span>
      {verText ? (
        <button
          type="button"
          className="masthead__ver"
          id="verbadge"
          data-act="about"
          title={verTitle}
          onClick={() => openModalEl("imodalbg")}
        >
          {verText}
        </button>
      ) : (
        <span className="masthead__ver" id="verbadge" />
      )}
      <AboutDialog route={route} liveStatus={liveStatus} specs={specs} />
      {getSource().kind === "static" ? (
        // (#1801) No `<CatalogPanel>` here — see this component's own doc
        // for why a static build gets inert text instead of a button that
        // would 404 on click.
        <span className="chip masthead__srcbadge" title={/^\d{4}-\d{2}-\d{2}$/.test(srcbadgeText(route, replayDate)) ? "the first recorded day in this replay" : undefined}>
          {srcbadgeText(route, replayDate)}
        </span>
      ) : (
        <CatalogPanel label={srcbadgeText(route, replayDate)} />
      )}
      {/* (#1801) A static build shows the PLAYBACK badge on every lens, not
          just the playback route. `isLiveRoute()` now returns false for the
          whole build (see its own doc), so keying the fallback on
          `route.kind === "playback"` alone would leave `#lens=runs` on the
          demo with NO mode badge at all — a page that is neither live nor
          visibly playback. Legacy's `setBadges(mode, date)` shows the play
          badge on every lens, mode being global there. */}
      {live ? (
        <LiveStatusBadge status={liveStatus} />
      ) : getSource().kind === "static" || route.kind === "playback" || replayDate !== null ? (
        <PlaybackModeBadge />
      ) : null}
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

/** The masthead's source chip, per route. A live route reads `TODAY`; a
 * playback names its day; a dispatch or mission page names the day the
 * shell derived from its records (`replayDate`) and reads `RESULT` until
 * that is known — a finished run is a result, and the date is what the
 * demo shows on the same routes. */
function srcbadgeText(route: Route, replayDate: string | null = null): string {
  // (#1800) `"Flow · "+date` verbatim from legacy's `play` arm, pre-uppercased
  // per this component's own module doc. The previous version rendered a bare
  // ISO date and said why: the "Flow · " prefix was "a borrowed live-mode
  // phrase" for a route with no real playback pipeline behind it. That reason
  // has EXPIRED — the pipeline exists now, `goldens/playback-date.txt` reads
  // `FLOW · 2026-08-07`, and the prefix is the honest label rather than a
  // borrowed one. A same-day `#<date>` hash still reads "TODAY", matching
  // legacy's `dl` (its own boot treats today's date as live, not playback).
  // `?? todayUTC()` is not defensive noise: `route.date` became `string | null`
  // (#1801, a static build knows its date only after the flow file resolves)
  // and a template literal accepts null SILENTLY — `FLOW · null` would render
  // and typecheck. Unreachable today because `App.tsx` passes `displayRoute`,
  // whose date is already resolved; this keeps the invariant in the code
  // rather than in that one caller's habits.
  // (#2072) A static build replays one recorded file on every route; naming
  // its day only on the playback route left the runs/machine/console tabs
  // saying `TODAY` and mission/dispatch saying `REPLAY` for the same data.
  // Checked BEFORE the playback branch on purpose: that branch's date comes
  // from the fetched records and is unresolved on first paint, so the
  // landing route flashed `TODAY` (and stayed there if the file was slow)
  // while every other tab had the date from the meta at once.
  // (operator, 2026-08-28) The chip is the bare date on every width: the
  // `FLOW · ` prefix read as noise on desktop and was already hidden on
  // phones, so dropping it is also what makes the two consistent.
  const source = getSource();
  if (source.kind === "static" && source.date) return source.date;
  if (route.kind === "playback") {
    const date = route.date ?? todayUTC();
    // (operator, 2026-08-28) A playback names its day even when that day
    // is today: `TODAY` beside `▶ PLAYBACK` read as a different chip from
    // the demo's dated one. `TODAY` is the LIVE view's word.
    return date;
  }
  // A dispatch or mission page names its day once the shell has derived it
  // from the records; until then it is what the page shows: a RESULT
  // (operator, 2026-08-28: "instead of replay doesn't result seem better?").
  if (route.kind === "dispatch" || route.kind === "mission") return replayDate ?? "RESULT";
  return "TODAY";
}
