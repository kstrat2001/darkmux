import { useEffect, useRef, useState, type KeyboardEvent as ReactKeyboardEvent } from "react";
import { useQuery, keepPreviousData } from "@tanstack/react-query";
import { queryKeys, PANEL_CACHE_MS } from "../../lib/queryKeys";
import type { PanelId } from "../../lib/route";
import { PANELS, DEFAULT_PANEL_ID, isManualPanel, panelCols, panelArgv, panelOptGroups, composeArgv, canonicalOptPairs, type PanelOpt } from "./panels";
import { fetchPanel } from "./fetchPanel";
import { canonicalHash, writeHash } from "../../lib/hashSync";
import { panelAgeLabel } from "./format";
import { AnsiText } from "./ansi";
import type { PanelResponse } from "../../types/handwritten";

/**
 * The console lens — `#lens=console&panel=<id>`. Pure port of
 * `viewer.html`'s `renderConsole()`/`loadPanel()`/`setPanel()`/`goConsole()`
 * (search that file for `── CLI panel:`). Renders an allowlisted CLI
 * command's own ANSI output, so the panel cannot drift from the CLI the way
 * a JS re-implementation could (`crates/darkmux-serve/src/panel.rs`'s own
 * module doc: "twin-drift becomes structurally impossible... a panel cannot
 * diverge from the CLI because it IS the CLI").
 *
 * (#1905 step 3) Every `panelId` this component ever holds is a real,
 * allowlisted `PanelId` — there is no client-only view left to special-case.
 * `panelId === ""` (no `panel=` param, or an unrecognized one) resolves to
 * `DEFAULT_PANEL_ID` (`panels.ts` — `"run-list"`) once, at mount; every
 * `panelId` renders via `CliPanelView` below, unconditionally.
 *
 * (#1911, opts-as-command-tokens redesign) An interim shape rendered a
 * SECOND row of pills below the pill row — an "opts bar" — for a panel's
 * declared options. The operator reviewed it live and rejected it: those
 * pills are not siblings of the panel tabs, they belong to the ACTIVE tab's
 * own command, and rendering them as a peer row was the wrong shape. This
 * version deletes that row entirely; a panel's options now render as
 * TOKENS inside the `$ darkmux ...` command line itself
 * (`ChromeCommandLine` below) — the control and the receipt are the same
 * object, so they cannot drift apart, and what you edit is literally the
 * command you would type. See `ChromeCommandLine`'s own doc for the token
 * shapes and the staleness/drift handling this redesign is really about.
 *
 * DOM/CSS shape is a direct port (`.runsbar`/`.runchip` tabs, `.panelwrap` >
 * `.panelchrome` + a `.panelout`/`.panelerr` body) — see `styles.css`'s own
 * "Console lens (Packet 6)" block for why the flex layout is load-bearing
 * for `innerText` byte-parity, not just visual.
 *
 * Fetch/cache mapping onto TanStack Query (the ONE genuinely non-literal
 * translation `CliPanelView` makes, since legacy hand-rolls its own
 * `PANEL_STATE` cache + `loadPanel`):
 * - One `useQuery` keyed by `queryKeys.panel(id, opts)` (#1911: `opts` is
 *   the panel's own current selection, canonicalized into the key the same
 *   way `hashSync.canonicalHash` canonicalizes it into the address bar —
 *   `lenses/console/panels.ts::variantKey` is the ONE function both call) —
 *   switching tabs OR flipping a command token reuses Query's own cache
 *   exactly the way `PANEL_STATE[id]` reuse worked (revisiting an
 *   already-loaded panel/variant shows the cached body immediately, no
 *   re-fetch), with no hand-rolled state object needed.
 * - `placeholderData: keepPreviousData` (this redesign's own addition) —
 *   see `ChromeCommandLine`'s doc for why: flipping a token changes the
 *   query key, and without this the output would blank to "running…" on
 *   every single token flip, which is correct but needlessly jarring for a
 *   fast local fetch. `<CliPanelView key={id} .../>` (below, in
 *   `ConsolePanel`'s own return) is what keeps this SCOPED to
 *   selection changes WITHIN one panel — without the `key`, React would
 *   reuse ONE `useQuery` hook instance across PANEL SWITCHES too, and
 *   `keepPreviousData` would then bleed a DIFFERENT panel's stale content
 *   onto a freshly-selected tab, which is exactly the regression bullet 4
 *   (six panels with no options must render byte-identical to before this
 *   redesign) forbids. Remounting on `id` change means the cache — which
 *   lives in the `QueryClient`, not the component — still serves an
 *   already-fresh cached body instantly on tab-revisit; only the
 *   PLACEHOLDER-CARRYOVER mechanism resets, exactly where it should.
 * - `doctor` (`isManualPanel`) is `enabled: false` PERMANENTLY — selecting
 *   the tab NEVER fetches (matches `MANUAL_PANELS`'s guard on every
 *   auto-fetch call site: `goConsole`/`setPanel`/the visibilitychange
 *   handler). Only the explicit "run"/"re-run" button's `refetch()` call
 *   (which works regardless of `enabled`) ever populates it — same
 *   operator-must-ask contract as legacy (#1286). A manual panel that DID
 *   declare options (none does today — `doctor` declares none) would still
 *   only ever fetch on that explicit click: `enabled: false` blocks
 *   `useQuery`'s own automatic refetch-on-key-change the same way it
 *   already blocks refetch-on-mount, keyed off `auto_refresh` in DATA
 *   (`isManualPanel`), never off the id.
 * - `staleTime: PANEL_CACHE_MS` (`queryKeys.ts`'s already-documented mirror
 *   of the daemon's own `PANEL_CACHE_TTL`) + `refetchOnWindowFocus` stands
 *   in for legacy's own `visibilitychange` listener (re-check only when the
 *   tab regains focus AND the cached copy has passed its TTL, never a
 *   timer) — a deliberate, DOCUMENTED adaptation to Query's idioms rather
 *   than a byte-for-byte port of the DOM listener, since no golden exercises
 *   that specific code path (see the packet report's ledger).
 */
export function ConsolePanel({
  initialPanelId,
  initialOpts,
}: {
  initialPanelId: PanelId | "";
  /** (#1911) The panel's own opt selections carried in on a deep link
   * (`route.opts` — `lib/route.ts`), already sanitized against that
   * panel's own table by `parseRoute`. Seeds this component's per-pill
   * selection memory (below) so a shared link reproduces panel AND
   * variant, not just the panel. */
  initialOpts?: Readonly<Record<string, string>>;
}) {
  // (#1905 step 3) `""` (no `panel=` in the URL, or an unrecognized one —
  // both already collapse to `""` in `parseRoute`) resolves to
  // `DEFAULT_PANEL_ID` HERE, once, at the initial state seed — matching
  // `route.ts`'s own long-standing division of labor: "the router does the
  // same allowlist check `consoleQuery` does; the lens component owns
  // deciding what `""` renders as." A real `PanelId` means the operator
  // (or a deep link) explicitly picked that tab.
  const [panelId, setPanelId] = useState<PanelId>(initialPanelId || DEFAULT_PANEL_ID);

  // (#1911) Per-pill opt selection memory — a `Map` beside `panelId`,
  // deliberately NOT `localStorage` (see the module doc below for why: a
  // filter silently restored from last week is a wrong reading that looks
  // like a quiet system). Session-only: flipping run-list -> flow -> run-list
  // mid-investigation does not discard a `--kind lab` pick; a fresh mount
  // (reload) lands on defaults unless the hash itself says otherwise.
  const [selections, setSelections] = useState<Map<PanelId, Record<string, string>>>(() => {
    const m = new Map<PanelId, Record<string, string>>();
    if (initialPanelId && initialOpts && Object.keys(initialOpts).length > 0) {
      m.set(initialPanelId, { ...initialOpts });
    }
    return m;
  });

  // A fresh deep-link into a DIFFERENT panel while this component is already
  // mounted re-syncs local selection — mirrors `boot()`'s `if(nq)
  // state.panelId=nq` applying on every fresh console entry. Guarded on a
  // TRUTHY `initialPanelId` (unchanged from before this packet): a bare
  // `#lens=console` re-render must not stomp local pill-click navigation
  // back to the default, since `initialOpts`'s object identity changes on
  // every hash-driven re-render even when its own panel value stays "".
  useEffect(() => {
    if (!initialPanelId) return;
    setPanelId(initialPanelId);
    if (initialOpts && Object.keys(initialOpts).length > 0) {
      setSelections((prev) => {
        const next = new Map(prev);
        next.set(initialPanelId, { ...(next.get(initialPanelId) ?? {}), ...initialOpts });
        return next;
      });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [initialPanelId, initialOpts]);

  const activeSelection: Readonly<Record<string, string>> = selections.get(panelId) ?? {};
  const setOpt = (name: string, value: string) => {
    const next = { ...activeSelection, [name]: value };
    setSelections((prev) => {
      const m = new Map(prev);
      m.set(panelId, next);
      return m;
    });
    // (#1911) Write the selection into the URL at the moment it changes.
    //
    // This is the same direct-`writeHash` path `RunsBoard.selectKind` uses,
    // and for the identical reason its own doc gives: a selection lives in
    // component state, not in `Route`, so no `hashchange` fires and the
    // route-keyed `useSyncHash` effect never sees it. `canonicalHash`
    // already knew how to serialize `opt.*`; nothing was calling it.
    //
    // Without this, `parseRoute` reads a selection out of a deep link but
    // nothing ever puts one in: picking `--kind lab` could not be shared,
    // did not survive a reload, and left no history entry. Verified live
    // against the daemon before the fix, which is how it was found.
    //
    // `writeHash` uses `replaceState`, which does NOT fire `hashchange`,
    // so this cannot feed back into the effect above as a render loop.
    writeHash(canonicalHash({ kind: "console", panelId, opts: next }));
  };

  return (
    <>
      <div className="runsbar">
        {PANELS.map((p) => (
          <PanelTab key={p.id} id={p.id} label={p.label} active={p.id === panelId} onSelect={setPanelId} />
        ))}
      </div>
      {/* `key={panelId}` — see the module doc's `placeholderData` paragraph
          for why this is load-bearing, not decorative: it is what keeps
          `keepPreviousData` scoped to selection changes WITHIN one panel. */}
      <CliPanelView key={panelId} id={panelId} opts={activeSelection} onOptChange={setOpt} onPanelSwitch={setPanelId} />
    </>
  );
}

/** viewer.html: one `.runchip[data-act="setpanel"]` entry of the tab bar. */
function PanelTab({ id, label, active, onSelect }: { id: PanelId; label: string; active: boolean; onSelect: (id: PanelId) => void }) {
  return (
    <span
      className={`runchip${active ? " on" : ""}`}
      data-act="setpanel"
      data-arg={id}
      role="button"
      tabIndex={0}
      onClick={() => onSelect(id)}
      onKeyDown={(e: ReactKeyboardEvent<HTMLSpanElement>) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          onSelect(id);
        }
      }}
    >
      {label}
    </span>
  );
}

/** The CLI-panel fetch/chrome/body machinery, extracted from `ConsolePanel`'s
 * own return for readability. (#1905 step 3) This is now the ONLY render
 * branch `ConsolePanel` ever mounts. */
function CliPanelView({
  id,
  opts,
  onOptChange,
  onPanelSwitch,
}: {
  id: PanelId;
  /** (#1911) The panel's current opt selections — the RAW per-pill map
   * from `ConsolePanel`'s own state, which may include an explicitly
   * re-picked default. `queryKeys.panel`/`fetchPanel` both canonicalize
   * (drop defaults, sort) before this reaches a cache key or the wire, so
   * "picked the default" and "never touched it" land in the SAME query. */
  opts: Readonly<Record<string, string>>;
  onOptChange: (name: string, value: string) => void;
  onPanelSwitch: (id: PanelId) => void;
}) {
  const manual = isManualPanel(id);
  const wireOpts = Object.fromEntries(canonicalOptPairs(id, opts));
  const query = useQuery({
    queryKey: queryKeys.panel(id, opts),
    queryFn: () => fetchPanel(id, panelCols(document.querySelector(".panelout")), wireOpts),
    enabled: !manual,
    staleTime: PANEL_CACHE_MS,
    refetchOnWindowFocus: !manual,
    // See the module doc's `placeholderData` paragraph. `keepPreviousData`
    // is TanStack's own name for "when the query key changes, keep
    // rendering the PREVIOUS key's resolved data while the new key loads,
    // and tell me so via `isPlaceholderData`" — exactly the primitive this
    // redesign's staleness treatment is built on, rather than a hand-rolled
    // comparison of what's on screen against the live selection.
    placeholderData: keepPreviousData,
  });

  const loadedBody = query.data?.ok === true ? query.data.data : null;
  const errorMessage = query.data?.ok === false ? query.data.message : null;
  // `isPlaceholderData`: true exactly while `loadedBody`/`errorMessage`
  // describe a PREVIOUS selection (the query key changed and the new
  // key's own fetch hasn't landed yet) — see `ChromeCommandLine`'s doc for
  // the full "why this and not a comparison" reasoning.
  const stale = query.isPlaceholderData;

  return (
    <div className="panelwrap">
      <div className="panelchrome">
        <ChromeCommandLine id={id} opts={opts} onOptChange={onOptChange} loadedBody={loadedBody} stale={stale} manual={manual} />
        <span className="pc-spacer"></span>
        {/* (byte-identical-to-today pin) The old `LoadedChrome`/
            `NotLoadedChrome` split had one asymmetry worth preserving
            exactly: "re-run" (once `loadedBody` has EVER landed) was never
            disabled, even mid-refetch; "run" (nothing loaded yet — this
            covers the daemon-refusal/error case too, matching the old
            `loadedBody ? Loaded : NotLoaded` branch, which treated an
            error response as "not loaded" for button purposes) disabled
            while a fetch is in flight. `stale` doesn't enter this formula
            at all: during a placeholder-serving refetch, `loadedBody` is
            already truthy (the kept-previous data), so the button reads
            "re-run" and stays enabled — the same shape a background
            refetch on an ALREADY-loaded panel already had before this
            redesign. */}
        <button className="pcbtn" data-act="refreshpanel" disabled={!loadedBody && query.isFetching} onClick={() => query.refetch()}>
          {loadedBody ? "re-run" : "run"}
        </button>
      </div>
      <PanelBody
        loading={query.isFetching && !stale}
        stale={stale}
        errorMessage={errorMessage}
        ansiText={loadedBody ? loadedBody.ansi_text || "" : null}
        exitCode={loadedBody ? loadedBody.exit_code : null}
        stderrTail={loadedBody ? loadedBody.stderr_tail || "" : ""}
        onPanelSwitch={onPanelSwitch}
      />
    </div>
  );
}

/** A two-value option whose non-default argv is a single flag (`ALL_OPT`'s
 * own shape: `[{argv:[]},{argv:["--all"]}]`) — the exact case #1911's own
 * spec names for the single-toggle-token collapse. A hypothetical future
 * two-value opt whose non-default argv is NOT flag-shaped (can't happen
 * under `panel.rs`'s own `every_opt_value_argv_is_flag_shaped` guard, but
 * checked here anyway rather than assumed) falls through to the enum
 * (dropdown) token instead. */
function isBooleanFlagToggle(opt: PanelOpt): boolean {
  return opt.values.length === 2 && opt.values[0].argv.length === 0 && opt.values[1].argv.length === 1;
}

function arraysEqual(a: readonly string[], b: readonly string[]): boolean {
  return a.length === b.length && a.every((v, i) => v === b[i]);
}

/**
 * (#1911, opts-as-command-tokens redesign) The `.pc-cmd` command line — the
 * whole reason this redesign exists. The operator's own framing: "the
 * control and the receipt become the same object, so they cannot drift,
 * and what you edit is literally the command you would type." Renders
 * EXACTLY what the earlier opts-bar iteration rendered as a separate row
 * of pills, but now as tokens INSIDE the command text, positioned right
 * after the panel's own base argv, in DECLARATION order — same
 * `panelOptGroups(id)` data source, still generic (no switch on panel id).
 *
 * Two token shapes, both derived from the data:
 * - Enum (`--kind`, 4+ values): `EnumToken` — one button reading
 *   `--kind <value> ▾`, opening a `role="listbox"` menu of every legal
 *   value on activation.
 * - Boolean (`--all`, `isBooleanFlagToggle`): `BooleanToken` — one
 *   `role="switch"` button reading just the flag, dim when off, accented
 *   when on.
 *
 * Six of eight panels declare no options at all (`panelOptGroups(id)` is
 * empty) — for those, this renders the EXACT SAME markup as before this
 * whole opts feature existed: one plain `<span className="pc-cmd">$
 * darkmux {argv}</span>`, no tokens, no interactivity, no layout change.
 * `panels.test.ts`/`ConsolePanel.test.tsx` both pin this byte-for-byte —
 * it is the main regression risk in this redesign, named explicitly in
 * the operator's own brief.
 *
 * **The line shows what WILL run, composed from the current selection**
 * (`composeArgv`), not `response.argv` — a deliberate reversal of the
 * pre-redesign chrome, which echoed the SERVER's resolved argv once a
 * response landed. Two exceptions, both look at `loadedBody`:
 *
 * 1. **Staleness** (`stale`, `CliPanelView`'s own `query.isPlaceholderData`
 *    — see that component's doc for why `keepPreviousData` is what makes
 *    this a real, observable state rather than a same-render impossibility):
 *    when true, `loadedBody`/`errorMessage` describe an OLDER selection
 *    than what the tokens currently show. The tokens themselves stay live
 *    (they always reflect the CURRENT selection) — the STALENESS signal
 *    lives in the captured-info cluster and the output body instead (see
 *    `PanelBody`'s own `stale` handling), because that is the part that is
 *    actually out of date, not the command preview.
 * 2. **Drift** (`loadedBody != null && !stale && !arraysEqual(loadedBody.argv,
 *    composeArgv(id, opts))`): the loaded response genuinely corresponds to
 *    the CURRENT selection (not stale — the query key guarantees this,
 *    see `CliPanelView`'s own doc) yet its `argv` disagrees with what this
 *    client's own `PANEL_OPTS` table computed for the identical selection.
 *    That can only mean the client's table and the server's
 *    (`crates/darkmux-serve/src/panel.rs`) have drifted apart — a real bug,
 *    not a rendering choice — and per the operator's own instruction, "the
 *    RESPONSE wins for display and that is a bug worth surfacing, not
 *    hiding": the line falls back to the literal `response.argv` as plain
 *    text (no tokens — trusting a token's own value-to-argv mapping here
 *    would be trusting the very thing just proven wrong) plus a visible
 *    `.pc-drift` marker naming what happened.
 */
function ChromeCommandLine({
  id,
  opts,
  onOptChange,
  loadedBody,
  stale,
  manual,
}: {
  id: PanelId;
  opts: Readonly<Record<string, string>>;
  onOptChange: (name: string, value: string) => void;
  loadedBody: PanelResponse | null;
  stale: boolean;
  manual: boolean;
}) {
  const groups = panelOptGroups(id);
  const pendingArgv = composeArgv(id, opts);
  const drift = loadedBody != null && !stale && !arraysEqual(loadedBody.argv, pendingArgv);

  if (drift) {
    return (
      <>
        <span className="pc-cmd">$ darkmux {loadedBody!.argv.join(" ")}</span>
        <span className="pc-drift" title="the composed request did not match what the server actually ran">
          ⚠ argv mismatch — showing what actually ran
        </span>
        <CapturedInfo body={loadedBody!} stale={false} />
        {loadedBody!.auto_refresh === false && <span className="pc-manual">· manual-run only</span>}
        <span>· {loadedBody!.gather_ms}ms</span>
      </>
    );
  }

  return (
    <>
      <span className="pc-cmd">
        $ darkmux {panelArgv(id).join(" ")}
        {groups.map((opt) => (
          <span key={opt.name}>
            {" "}
            {isBooleanFlagToggle(opt) ? (
              <BooleanToken opt={opt} value={opts[opt.name] ?? opt.values[0].value} manual={manual} onChange={(v) => onOptChange(opt.name, v)} />
            ) : (
              <EnumToken opt={opt} value={opts[opt.name] ?? opt.values[0].value} manual={manual} onChange={(v) => onOptChange(opt.name, v)} />
            )}
          </span>
        ))}
      </span>
      {loadedBody && (
        <>
          <CapturedInfo body={loadedBody} stale={stale} />
          {loadedBody.auto_refresh === false && <span className="pc-manual">· manual-run only</span>}
          <span>· {loadedBody.gather_ms}ms</span>
        </>
      )}
    </>
  );
}

/** `captured HH:MM:SS [· daemon cache Ns]` — describes the OUTPUT below,
 * a different moment than the command line above it (the operator's own
 * framing, #1911). `stale` (this panel's `query.isPlaceholderData`) adds
 * its own note rather than reusing the age-based `· stale (Ns)` text
 * `panelAgeLabel` already produces — the two are different claims ("this
 * cached copy outlived its own TTL" vs "the selection has moved on since
 * this was fetched") and conflating their wording would say something
 * neither claim actually means. Both reuse the SAME `.pc-stale` CSS class
 * (the existing amber-text idiom) since they're the same KIND of signal —
 * only one fires at a time in practice (a placeholder response is always
 * fresher than its own TTL). */
function CapturedInfo({ body, stale }: { body: PanelResponse; stale: boolean }) {
  const a = panelAgeLabel(body, Date.now());
  return (
    <>
      <span>
        captured {a.hhmmss}
        {a.served}
      </span>
      {stale && <span className="pc-stale">· selection changed, refreshing…</span>}
      {!stale && a.stale && <span className="pc-stale">· stale ({a.ageSec}s)</span>}
    </>
  );
}

/** The enum token — `--kind all ▾`, opening a `role="listbox"` menu of
 * every legal value on activation. `aria-haspopup="listbox"` +
 * `aria-expanded` on the trigger button, per the operator's own
 * accessibility brief. Keyboard: Enter/Space/ArrowDown on the closed
 * button opens the menu and focuses the current value's option; ArrowUp/
 * ArrowDown while open moves REAL DOM focus between options (a roving
 * option, not `aria-activedescendant` — simpler to get right, and no
 * screen reader distinguishes the two for a listbox this short); Enter/
 * Space on a focused option selects it, closes the menu, and returns
 * focus to the trigger button; Escape (button or any option) closes
 * without changing the selection and returns focus to the trigger.
 * Clicking outside the menu closes it the same way (mirrors
 * `CatalogPanel.tsx`'s own dismissal pattern — see that component's doc).
 * 40px minimum touch target on the trigger AND every option row (`.pc-tok`/
 * `.pc-tok-menu-option` in `styles.css`). */
function EnumToken({
  opt,
  value,
  manual: _manual,
  onChange,
}: {
  opt: PanelOpt;
  value: string;
  manual: boolean;
  onChange: (value: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const anchorRef = useRef<HTMLSpanElement>(null);
  const buttonRef = useRef<HTMLSpanElement>(null);
  const flag = `--${opt.name}`;

  function close(refocus: boolean) {
    setOpen(false);
    if (refocus) buttonRef.current?.focus();
  }

  function select(v: string) {
    onChange(v);
    close(true);
  }

  // Focus the current value's option (or the first) the instant the menu
  // opens — an operator who opens the menu with the keyboard lands
  // somewhere real, not on a menu with nothing focused.
  useEffect(() => {
    if (!open || !anchorRef.current) return;
    const options = Array.from(anchorRef.current.querySelectorAll<HTMLElement>('[role="option"]'));
    const current = options.find((o) => o.dataset.value === value) ?? options[0];
    current?.focus();
  }, [open, value]);

  // Same dismissal shape as `CatalogPanel.tsx`: Escape closes unconditionally,
  // a click outside the anchor closes without changing the selection.
  useEffect(() => {
    if (!open) return;
    function handleKeyDown(e: KeyboardEvent) {
      if (e.key === "Escape") close(true);
    }
    function handleClick(e: MouseEvent) {
      if (anchorRef.current && e.target instanceof Node && !anchorRef.current.contains(e.target)) {
        close(false);
      }
    }
    document.addEventListener("keydown", handleKeyDown);
    document.addEventListener("click", handleClick);
    return () => {
      document.removeEventListener("keydown", handleKeyDown);
      document.removeEventListener("click", handleClick);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  function onOptionKeyDown(e: ReactKeyboardEvent<HTMLLIElement>, v: string) {
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      select(v);
      return;
    }
    if (e.key === "Escape") {
      e.preventDefault();
      close(true);
      return;
    }
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      const options = Array.from(anchorRef.current?.querySelectorAll<HTMLElement>('[role="option"]') ?? []);
      const idx = options.indexOf(e.currentTarget);
      const next = e.key === "ArrowDown" ? Math.min(idx + 1, options.length - 1) : Math.max(idx - 1, 0);
      options[next]?.focus();
    }
  }

  return (
    <span className="pc-tok-anchor" ref={anchorRef}>
      {/* `<span role="button">`, not a real `<button>` — the same choice
          `PanelTab` already makes. The layout bug that motivated
          double-checking this (found against the real built port, not
          guessed): CSS flex layout BLOCKIFIES every direct child of a flex
          container regardless of that child's own `display` — `.pc-tok-
          anchor` was briefly `display:inline-flex` (so it could center its
          own child), which blockified this trigger and split "$ darkmux
          run list --kind all ▾ --all" across three lines in real Chromium
          `innerText`, breaking `#stage` byte-parity. The actual fix was
          giving `.pc-tok-anchor` no `display` override at all (plain
          `inline`, `styles.css`) so it never becomes a flex container in
          the first place — kept as a `<span role="button">` regardless of
          that fix, matching `PanelTab`'s own established pattern here
          rather than reintroducing a native `<button>` this codebase has
          already chosen not to use in an inline command context.
          `role="button"` plus the explicit Enter/Space/ArrowDown handling
          below is what makes a `<span>` keyboard-operable in its place. */}
      <span
        ref={buttonRef}
        role="button"
        tabIndex={0}
        className="pc-tok pc-tok-enum"
        aria-haspopup="listbox"
        aria-expanded={open}
        onClick={() => setOpen((o) => !o)}
        onKeyDown={(e: ReactKeyboardEvent<HTMLSpanElement>) => {
          if (!open && (e.key === "ArrowDown" || e.key === "Enter" || e.key === " ")) {
            e.preventDefault();
            setOpen(true);
          }
        }}
      >
        {`${flag} ${value} ▾`}
      </span>
      {open && (
        <ul className="pc-tok-menu" role="listbox" aria-label={flag}>
          {opt.values.map((v) => (
            <li
              key={v.value}
              role="option"
              aria-selected={v.value === value}
              tabIndex={-1}
              data-value={v.value}
              className="pc-tok-menu-option"
              onClick={() => select(v.value)}
              onKeyDown={(e: ReactKeyboardEvent<HTMLLIElement>) => onOptionKeyDown(e, v.value)}
            >
              {v.value}
            </li>
          ))}
        </ul>
      )}
    </span>
  );
}

/** The boolean token — `--all`, `role="switch"`, dim when off, accented
 * (`.on`) when on. Activating it toggles between the opt's default value
 * (`values[0]`, always empty argv) and its non-default value
 * (`values[1]`, always the single flag). `manual` is accepted for
 * signature symmetry with `EnumToken` and because a future manual panel
 * that declares a boolean opt should read the same way here as there —
 * unused today (`_manual` in `EnumToken`, same reason): changing either
 * token NEVER auto-fetches regardless of `manual`, because `CliPanelView`'s
 * `enabled: !manual` on `useQuery` already blocks the auto-refetch
 * `onChange` would otherwise trigger — the gate lives in ONE place, keyed
 * off `auto_refresh` in data, not re-derived per token. */
function BooleanToken({
  opt,
  value,
  onChange,
}: {
  opt: PanelOpt;
  value: string;
  manual: boolean;
  onChange: (value: string) => void;
}) {
  const flag = `--${opt.name}`;
  const on = value === opt.values[1].value;
  function toggle() {
    onChange(on ? opt.values[0].value : opt.values[1].value);
  }
  return (
    // `<span role="switch">`, not a real `<button>` — same `PanelTab`
    // convention `EnumToken`'s own doc explains in full (the real
    // line-splitting bug there was `.pc-tok-anchor`'s own flex-item
    // blockification, not the element type — kept as a span regardless,
    // for consistency with the rest of this file's command tokens).
    <span
      role="switch"
      tabIndex={0}
      className={`pc-tok${on ? " on" : ""}`}
      aria-checked={on}
      onClick={toggle}
      onKeyDown={(e: ReactKeyboardEvent<HTMLSpanElement>) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          toggle();
        }
      }}
    >
      {flag}
    </span>
  );
}

/** viewer.html: `renderConsole()`'s body switch (`.panelout`/`.panelerr`,
 * loading > error > loaded > not-yet-run precedence). The loaded branch is
 * the ONLY one using a real `<pre>` element — see `styles.css`'s module doc
 * for why the tag choice (not just the class) matters.
 *
 * (#1908) The loaded branch used to render `ansi_text` (stdout) alone, so a
 * command that failed with empty stdout was indistinguishable from one that
 * succeeded with nothing to say — same header, same timing, same empty box.
 * The daemon already sends `exit_code` and `stderr_tail`
 * (`crates/darkmux-serve/src/panel.rs`) for exactly this; the viewer just
 * never read them.
 *
 * `failed` treats anything OTHER than a real `0` as not-a-clean-exit — this
 * covers `null` (`Command::status().code()`'s own signal-killed case) and,
 * defensively, a malformed/absent field despite the declared `number |
 * null` type (never leak the literal word "undefined" into the fallback
 * sentence below). `hasStdout`/`hasStderr` are then independent of
 * `failed`: a ZERO exit can still have written a warning to stderr, and
 * that warning is real content, not noise to bury under "no output" (QA
 * finding on this issue — dropping it would be the same silent-failure
 * shape #1908 exists to kill, one branch over). So the three regions
 * compose rather than branch exclusively:
 *   - `hasStdout` → the ANSI-rendered pre, unchanged from before this
 *     packet.
 *   - `hasStderr` (regardless of exit code) → `stderr_tail`, verbatim, in
 *     the SAME `.panelerr` treatment the HTTP-refusal path already uses
 *     (`errorMessage` above) — never reworded, per the daemon's own
 *     "surfaced so a failure isn't silent" comment.
 *   - `failed && !hasStderr` → a plain sentence naming the exit status
 *     instead of falling back to a blank box again. This one line isn't
 *     the daemon's own text (there isn't any to quote), but it still
 *     describes only what actually happened, never invents what the
 *     command might have meant.
 * Only when NONE of the three apply (clean exit, empty stdout, empty
 * stderr) does the plain "no output" empty state render — distinct
 * wording from "not run yet" (never fetched) so the two stay
 * distinguishable.
 *
 * (#1911, opts-as-command-tokens redesign) `loading` is now
 * `query.isFetching && !stale` (`CliPanelView`'s own computation) — a
 * placeholder-serving refetch (a token flip, mid-flight) no longer blanks
 * this to "running…"; it falls through to whatever branch the STALE
 * content itself would already render, with `stale` appending a dimming
 * modifier class so the operator can see at a glance that what's on
 * screen predates the current selection. A genuine first-ever fetch (no
 * prior data to keep) still shows "running…", unchanged — `stale` can
 * only be true when there IS prior data to show instead. */
function PanelBody({
  loading,
  stale,
  errorMessage,
  ansiText,
  exitCode,
  stderrTail,
  onPanelSwitch,
}: {
  loading: boolean;
  stale: boolean;
  errorMessage: string | null;
  ansiText: string | null;
  exitCode: number | null;
  stderrTail: string;
  onPanelSwitch: (id: PanelId) => void;
}) {
  const staleClass = stale ? " pc-body-stale" : "";
  if (loading) return <div className="panelout">running…</div>;
  if (errorMessage) return <div className={`panelerr${staleClass}`}>{errorMessage}</div>;
  if (ansiText !== null) {
    const failed = typeof exitCode !== "number" || exitCode !== 0;
    const hasStdout = ansiText !== "";
    const hasStderr = stderrTail !== "";

    if (!failed && !hasStdout && !hasStderr) return <div className={`panelout${staleClass}`}>no output</div>;

    return (
      <>
        {hasStdout && (
          <pre className={`panelout${staleClass}`}>
            <AnsiText text={ansiText} onPanelSwitch={onPanelSwitch} />
          </pre>
        )}
        {(hasStderr || failed) && (
          <div className={`panelerr${staleClass}`}>
            {hasStderr
              ? stderrTail
              : typeof exitCode === "number"
                ? `command exited with status ${exitCode} and printed nothing to stderr`
                : "command did not exit cleanly (killed) and printed nothing to stderr"}
          </div>
        )}
      </>
    );
  }
  return <div className="panelout">not run yet — this panel probes the machine, so it runs only when you ask.</div>;
}
