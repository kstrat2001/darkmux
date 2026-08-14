import { useEffect, useMemo, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../../lib/fetcher";
import { queryKeys, MACHINE_MEM_POLL_MS } from "../../lib/queryKeys";
import { useFlowWindow } from "../../hooks/useFlowWindow";
import { useLiveMachines } from "../../hooks/useLiveMachines";
import { localMachineUid, looseRecords, nameOf } from "../../lib/flow";
import { specOf } from "../fleet/cards";
import { utilityLines } from "./memoryLedgerLines";
import { MachineHealthRegion } from "./MachineHealthRegion";
import { advanceResidency, residencyChangedThisPoll, type ResidencyRowView, type ResidencyState } from "./machineGauge";
import { isStaticBuild } from "../../lib/staticSource";
import type { MachineSpecs, MachineResources } from "../../types/handwritten";

/** The health region's state vocabulary, verbatim from `/machine/resources`
 * (`machine.state` and each model's `state`), uppercased for display by
 * `memoryLedgerLines.ts`. Legacy carries the same three
 * (`.memstate.green/.amber/.red`, viewer.html). "RED" also arrives on its
 * own from the pressure block, which is why it maps to the same severity. */
const STATE_SEVERITY: Record<string, string> = {
  GREEN: " is-ok",
  AMBER: " is-warn",
  RED: " is-bad",
  UNKNOWN: "",
};

/** `model.owner` uppercased — the darkmux-namespace marker (see the namespace
 * convention in CLAUDE.md), NOT a health state. Legacy styles it as
 * `.memowner`: muted, not colored. */
const OWNER_WORDS = new Set(["DARKMUX", "USER"]);

/** Classify a health line by CONTENT PATTERN — the technique the health
 * region used to style itself BEFORE #1806 Stage 1 gave it real structure
 * (Stage 1's `.memcard`/`.memhdr`/`.memname`/`.memstate` elements carried
 * their own classes at render time; Stage 2/3's `MachineHealthRegion.tsx`
 * — the redesigned gauge/lamp/odometer/row region, `machineGauge.ts` for
 * the math behind it — carries this further still, so nothing in that
 * region calls this anymore). Kept — not deleted — for two reasons: it is
 * still a correct, independently useful pure function (`lineClass.test.ts`
 * exercises it directly, including the hostile-string inverted case), and
 * `Lines`'s `classify` prop below is still real API that a future
 * flat-line-shaped region could opt into. Rules over per-case templates,
 * the same call `RecordView` makes: content varies by machine state, so
 * positional selectors would be guesswork. These four markers were all
 * authored deliberately in `memoryLedgerLines.ts` — `↳` for hints, `⚠` for
 * warnings, an uppercased state string, and ` · ` joining meta fields — so
 * they are load-bearing content, not incidental formatting. */
export function lineClass(line: string): string | undefined {
  if (line.startsWith("↳")) return "mline--hint";
  if (line.startsWith("⚠")) return "mline--warn";
  // Two different uppercase tokens land in this stream and they mean
  // opposite things, so match the VOCABULARY rather than the shape. An
  // earlier cut keyed off "is it uppercase" with an assumed OK/RED
  // vocabulary; the real states are green/amber/red (`/machine/resources`,
  // uppercased for display by `modelLines`), so every healthy model rendered
  // in the warning color and the owner tag rendered as a health state. A
  // status color that lies about status is the same defect as the coverage
  // banner that rendered colorless — hence the closed sets below.
  const state = STATE_SEVERITY[line];
  if (state !== undefined) return `mline--state${state}`;
  if (OWNER_WORDS.has(line)) return "mline--owner";
  if (line.includes(" · ")) return "mline--meta";
  if (/^[a-z][a-z ]{2,18}$/.test(line)) return "mline--label";
  return undefined;
}

/** One `<div>` per line. `innerText` puts each block-level sibling on its
 * own line regardless of stylesheet — this port represents content as line
 * arrays (rendered one `<div>` per element) rather than leaning on legacy's
 * CSS-flex-dependent `innerText` line-break behavior, which depends on
 * every relevant ancestor being `display:flex` (or another block-level
 * layout) — brittle to depend on implicitly when a plain array is just as
 * easy to render and carries the same guarantee by construction. Still used
 * for the `darkmux/utility` node (`utilityLines()` — a fixed four-element
 * array, styled positionally in CSS, per that block's own stylesheet
 * comment); the health region moved to real structure in #1806 Stage 1
 * (then further, in Stage 2/3, to `MachineHealthRegion.tsx`'s gauge/lamp/
 * odometer/row markup) and no longer renders through here.
 *
 * `classify` stays as real, tested API (`lineClass.test.ts`) even though
 * nothing in this file passes `classify` anymore post-Stage-1 — see
 * `lineClass`'s own doc for why it wasn't deleted. Adding a class changes no
 * `innerText`, so the parity goldens are unaffected either way. */
function Lines({ lines, classify = false }: { lines: string[]; classify?: boolean }) {
  return (
    <>
      {lines.map((line, i) => (
        <div key={i} className={classify ? lineClass(line) : undefined}>
          {line}
        </div>
      ))}
    </>
  );
}

/**
 * The machine page — `renderMachine()` (viewer.html:1796-1991), now purely
 * the RESIDENCY ROOM its #1286 doctrine names it as (probe-fed RAM/model
 * health that only THIS daemon's own host can honestly report — see the
 * module-level `CLAUDE.md`'s "observer must not join the observed"
 * section). #1508 step 2 (`d2041ae3`) merged this page with the flow-scoped
 * fleet-card drill and left its runs list as an explicitly interim stand-in
 * ("step 4 of #1508 replaces it with the full Runs lens, machine-pinned").
 * #1809 is that step 4: the `RUNS ON <MACHINE>` list is gone, replaced by a
 * link into `#lens=runs&machine=<uid>` (`RunsBoard.tsx`'s machine pin) —
 * see the render below for the link itself.
 *
 * Reached by TWO entry points now, not three: the nav tab / bare
 * `#lens=machine` deep-link (`uid: null` — always "the local machine"), and
 * a LOCAL fleet-card drill (`uid: <uid>`, `FleetLens.tsx`'s locality split —
 * a REMOTE card now routes to the runs lens instead, since a remote
 * machine's residency is unreadable from here by construction: no
 * `/machine/resources` probe exists for a host that isn't this one). The
 * component itself still resolves an explicit REMOTE uid gracefully
 * (`isLocalMach` below stays real, `resourcesQuery` stays gated off) — a
 * `#lens=machine&uid=<remote>` bookmark minted before this packet, or typed
 * by hand, still degrades honestly rather than crashing; `FleetLens` simply
 * no longer MINTS that link itself. `isLocalMach` restores legacy's
 * `state.machineIsLocal || isLocalMachine(state.machine)` OR-gate now that
 * `lib/route.ts`'s widened `{kind:"machine",uid}` gives a drill somewhere
 * to carry its uid (this OR was narrowed to an unconditional `true` before
 * that widening existed — see the drill-in packet's report for the
 * restore).
 *
 * Data sources: `/machine/specs` + `/machine/resources` (resources is
 * fetched ONLY for the local machine, matching legacy's `pollMachineMem`
 * guard: a remote machine's page never reads THIS daemon's own probe under
 * the wrong name) PLUS `/flow/<today>` + `/flow/<yesterday>` +
 * `/fleet/machines/live` (machine presence — the source of the header's
 * `label`/`spec`, the run-count link below, AND a remote machine's own
 * `specs` string — `specOf()`, viewer.html:1124-1129, since a remote
 * machine's hardware line comes from its presence beat, not this daemon's
 * local `/machine/specs` probe). `/fleet/sessions/live` — fetched by the
 * OLD runs list for its live-vs-ended status labels — is gone along with
 * that list; nothing on this page needs session liveness anymore.
 */
export function MachineLens({ uid: routeUid }: { uid: string | null }) {
  const nowMs = Date.now();

  // (#1801) A daemon-less build has nothing to poll. `App.tsx` gates its own
  // copies of these on `isLiveRoute(route)` and states the rule in place:
  // "Gating one side and not the other is indistinguishable from gating
  // neither." This lens holds BOTH sides — it calls the presence hooks
  // itself rather than receiving them — and was ungated, so `#lens=machine`
  // on the static demo hit `/fleet/machines/live` and `/machine/resources`
  // every 5s for as long as the tab stayed open, against an origin with no
  // daemon behind it. Measured, then measured again after.
  //
  // Gated on the BUILD rather than the route, for the same reason
  // `useFlowWindow` is: on a static build these fetches could only ever fail,
  // so suppressing them changes nothing a consumer can observe.
  const daemonBacked = !isStaticBuild();

  const flowWindow = useFlowWindow(nowMs);
  const liveMachines = useLiveMachines(daemonBacked);

  const specsQuery = useQuery({
    queryKey: queryKeys.machineSpecs(),
    queryFn: () => fetchJson<MachineSpecs>("/machine/specs"),
    enabled: daemonBacked,
  });

  const specs = specsQuery.data?.ok ? specsQuery.data.data : null;

  const localUid = useMemo(
    () => localMachineUid(flowWindow.data, liveMachines, specs?.machine_id ?? null),
    [flowWindow.data, liveMachines, specs],
  );
  // The machine THIS page is showing — the route's explicit uid (a
  // fleet-card drill, local or remote) or, absent one, the local machine
  // (the nav-tab/deep-link entry — legacy's `goMachine`).
  const targetUid = routeUid ?? localUid;
  // Identity is the UID, never the display name. This compared
  // `nameOf(targetUid) === specs.machine_id`, which asks whether the label we
  // happen to show equals the name specs happens to report — and those are
  // different aliases of one machine often enough to matter. A laptop whose
  // window carried records as both `MacBook-Pro` and `MacBook-Pro.local`
  // classified ITSELF as remote on a fleet-card drill: no residency ledger, no
  // utility model, and a note advising the operator to "view the machine page
  // on MacBook-Pro.local directly" — the machine they were already sitting on.
  // The nav tab looked fine throughout, because `routeUid == null` short-
  // circuits to local without ever asking.
  //
  // `localMachineUid` now resolves specs' name through every alias a uid has
  // used (`lib/flow.ts::machineNames`), so both sides of this are uids and the
  // comparison means what it says.
  const isLocalSpecs = targetUid != null && localUid != null && targetUid === localUid;
  // `state.machineIsLocal` — the explicit nav-tab/deep-link intent
  // (`goMachine` always passes `local=true`; a fleet-card drill always
  // passes `local=false`, even when it happens to BE the local machine —
  // `isLocalSpecs` is what self-corrects that case once specs resolve).
  const machineIsLocal = routeUid == null;
  const isLocalMach = machineIsLocal || isLocalSpecs;

  // The resources probe is LOCAL-ONLY data (`/machine/resources` always
  // describes THIS daemon's own host) — legacy's `pollMachineMem` never
  // even fetches it for a remote machine's page (viewer.html:4906-4907:
  // `if(state.level!=="machine"||!machineIsLocalNow())... return`). Gating
  // the query itself (not just the render) matches that: a remote page
  // never issues the request at all.
  const resourcesQuery = useQuery({
    queryKey: queryKeys.machineResources(),
    queryFn: () => fetchJson<MachineResources>("/machine/resources"),
    refetchInterval: MACHINE_MEM_POLL_MS,
    enabled: isLocalMach && daemonBacked,
  });
  const resourcesErrored = isLocalMach && resourcesQuery.data ? !resourcesQuery.data.ok : false;

  // (#1812) `resources` used to be derived FRESH from `resourcesQuery.data`
  // every render — `fetchJson` returns a discriminated result rather than
  // throwing, so a failed poll is a SUCCESSFUL query whose data says
  // `ok:false`, and TanStack Query happily replaces the previous good
  // payload with that error object. The stale banner existed to keep
  // showing the last snapshot through exactly that moment
  // (`STALE_BANNER_TEXT`), but the derivation threw the snapshot away
  // before the banner ever got a reading to sit over. Holding the last GOOD
  // payload in state, updated ONLY on `ok:true`, restores legacy's
  // two-variable shape (`MACHINE_MEM` + `MACHINE_MEM_ERR`, `viewer.html`) in
  // React terms — `resourcesErrored` above still reflects the LATEST poll
  // (drives the banner + lamp), independent of what `resources` shows.
  const [lastGoodResources, setLastGoodResources] = useState<MachineResources | null>(null);
  const resources = lastGoodResources;

  // The residency state machine (docs/design/machine-lens/proposal.md §8 — ghost/NEW rows) advances
  // on the SAME successful-poll cadence as the payload above: a failed poll
  // carries no model list to diff against, so it must neither advance a
  // ghost's retirement clock nor manufacture a spurious departure. Held in
  // a ref (not state) because `advanceResidency` is a state MACHINE, not a
  // value to re-render on its own — the derived `residencyRows`/
  // `residencyChanged` below are the render-facing outputs.
  const residencyRef = useRef<ResidencyState | null>(null);
  const [residencyRows, setResidencyRows] = useState<ResidencyRowView[]>([]);
  const [residencyChanged, setResidencyChanged] = useState(false);

  useEffect(() => {
    if (!(isLocalMach && resourcesQuery.data?.ok)) return;
    const data = resourcesQuery.data.data;
    setLastGoodResources(data);
    const models = Array.isArray(data.models) ? data.models : [];
    const { state, rows } = advanceResidency(residencyRef.current, models, Date.now());
    residencyRef.current = state;
    setResidencyRows(rows);
    setResidencyChanged(residencyChangedThisPoll(rows));
    // Depends on the query's own settle timestamp, not `resourcesQuery.data`
    // itself — the object is a fresh reference every render regardless of
    // whether new data actually arrived, which would re-run this on every
    // paint rather than once per poll.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [isLocalMach, resourcesQuery.dataUpdatedAt]);

  const label = targetUid != null ? nameOf(flowWindow.data, liveMachines, targetUid) : "this machine";
  // `specOf()` (viewer.html:1124-1129, ported in `lenses/fleet/cards.ts` —
  // reused rather than re-derived here) — the local daemon's own
  // `/machine/specs` probe (cpu + RAM) when this page IS that machine;
  // otherwise the machine's own presence-beat `specs` string (a remote
  // machine's hardware line, as broadcast by ITS heartbeat).
  const spec = targetUid != null ? specOf(flowWindow.data, liveMachines, specs, targetUid) : "";

  // (#1809) The link below carries NO COUNT, deliberately.
  //
  // It used to read `${sessionsOn(data, uid).length} runs on <machine> →`,
  // which was the same total the removed runs list always showed — correct
  // while it labeled a list rendered from that same data, and a lie the
  // moment it labeled a link to somewhere else. The two count different
  // things over different windows: `sessionsOn` counts distinct session ids
  // in the 24h flow window (`LIVE_WINDOW_MS`), while the destination lists
  // `/runs` rows over the daemon's 14-day scan window, unioning missions,
  // lab runs and ghosts this page never sees. Measured live on both
  // machines: the link said "0 runs" while the destination listed 282 and
  // 16. Agreement would have been coincidence.
  //
  // Fetching `/runs` here to make the number honest was the alternative and
  // is the wrong trade: this page is the RESIDENCY ROOM (see the module doc)
  // and the whole point of #1809 is that run accounting belongs to the runs
  // lens. Adding a second live-only query, and a second thing to gate on
  // `isStaticBuild`, to label a hyperlink would walk that back. A count that
  // cannot drift because it is not there beats a count that is right today.
  const loose = useMemo(() => (targetUid != null ? looseRecords(flowWindow.data, targetUid) : []), [flowWindow.data, targetUid]);

  return (
    <div className="machine-lens">
      <div className="machine-lens__hdr stagehdr">
        {/* `<a data-act="fleet">fleet</a> › machine · ${label}` — the
            `.stagehdr` back-link (viewer.html:2021), distinct from `#crumb`
            (which carries no such link for the machine page — see
            `App.tsx`'s `routeChrome` doc). A real `<button>` (not an
            anchor with no href) so it never offers a tooltip/status-bar
            URL; navigation is the same direct hash-write NavChrome's tabs
            use for a cross-lens hop. `stagehdr` is a second class, purely
            an e2e inspection hook — every other stage-header region
            (`RunsBoard`, `LabRunDetail`, `SessionReplay`) already carries
            it; this one dropped it when the header grew its own back-
            BUTTON instead of legacy's plain anchor, and `viewer-xss.spec.js`/
            `viewer-lab.spec.js` drill through the class uniformly across
            all of them regardless of which lens they landed on. */}
        <button type="button" className="machine-lens__back" onClick={() => { location.hash = ""; }}>
          fleet
        </button>
        {" › machine · "}
        {label}
        {spec ? ` — ${spec}` : ""}
      </div>

      <div className="machine-lens__util">
        <Lines lines={utilityLines(specs, isLocalSpecs)} />
      </div>

      {/* `data-state` is the parity harness's post-fetch content marker —
          the React-port twin of legacy's `.memcard` selector (see
          `tests/parity/extract.spec.ts`'s machine-lens comment): "loaded"
          appears ONLY once `/machine/resources` has resolved with real
          data, never during the loading/error placeholder text branches.
          A REMOTE page never issues that fetch at all (`enabled:
          isLocalMach` gates the query off — see the resourcesQuery doc
          above), so without its own branch it would sit at "loading"
          forever even though the not-reported placeholder had already
          rendered correctly — a marker documented as a SETTLED signal
          that never settles. "remote" is that page's own settled value,
          distinct from "loaded"/"error" so a future remote parity test
          waiting on this marker doesn't hang (#1770 merge-gate finding). */}
      <div className="machine-lens__health" data-state={!isLocalMach ? "remote" : resources ? "loaded" : resourcesErrored ? "error" : "loading"}>
        <MachineHealthRegion
          isLocalMach={isLocalMach}
          machineName={label}
          resources={resources}
          resourcesErrored={resourcesErrored}
          residencyRows={residencyRows}
          residencyChanged={residencyChanged}
          nowMs={nowMs}
        />
      </div>

      {/* (#1809, finishing #1508 step 4) The `RUNS ON <MACHINE>` list —
          #1508 step 2's own commit named it "deliberately interim". This is
          the replacement: a link into the Runs lens, pinned to this
          machine. It carries NO count, deliberately — see the block above
          `loose` for the measured reason (the two sides counted different
          things over different windows and the link read "0 runs" against a
          destination listing 282). Rendered
          only once a machine is actually resolved (`targetUid != null` —
          mirrors every other `targetUid`-gated region on this page); a page
          that hasn't resolved a target yet has nothing to link to. A real
          `<a>` (not a `role="button"` div, unlike the old expand control it
          replaces) — this is honest cross-lens NAVIGATION, keyboard-
          activatable for free, and consistent with every other cross-lens
          hop in this file (`NavChrome`'s own tabs use the same direct
          hash-write pattern). */}
      {targetUid != null && (
        <a
          className="machine-lens__runslink"
          href={`#lens=runs&machine=${encodeURIComponent(targetUid)}`}
          onClick={(e) => {
            e.preventDefault();
            location.hash = `lens=runs&machine=${encodeURIComponent(targetUid)}`;
          }}
        >
          runs on {label} →
        </a>
      )}

      {loose.length > 0 && (
        <div className="machine-lens__loose">
          <div className="machine-lens__loosehdr">
            UNSCOPED RECORDS · {loose.length} TODAY
          </div>
          <div className="machine-lens__loosenode">
            <div>unscoped records</div>
            <div>
              {loose.length} record{loose.length === 1 ? "" : "s"} without a session
            </div>
            <div>flow notes, ambient telemetry, etc.</div>
            <div>loose</div>
          </div>
        </div>
      )}
    </div>
  );
}
