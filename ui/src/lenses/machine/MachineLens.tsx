import { useMemo } from "react";
import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../../lib/fetcher";
import { queryKeys, MACHINE_MEM_POLL_MS } from "../../lib/queryKeys";
import { useFlowWindow } from "../../hooks/useFlowWindow";
import { useLiveMachines } from "../../hooks/useLiveMachines";
import { localMachineUid, looseRecords, nameOf } from "../../lib/flow";
import { specOf } from "../fleet/cards";
import { utilityLines, healthLines } from "./memoryLedgerLines";
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

/** Classify a health line by CONTENT PATTERN so the region can be styled
 * without the builders having to emit structure.
 *
 * Rules over per-case templates, the same call `RecordView` makes: the
 * health region's length and composition vary by machine state, so
 * positional selectors would be guesswork that happens to fit whatever data
 * was on screen when they were written. These four markers are all authored
 * deliberately in `memoryLedgerLines.ts` — `↳` for hints, `⚠` for warnings,
 * an uppercased state string, and ` · ` joining meta fields — so they are
 * load-bearing content, not incidental formatting.
 *
 * This does NOT re-pair the flattened key/value lines ("pressure" then
 * "RED"). CSS can't, and neither can a per-line classifier; that needs the
 * builders to emit pairs. See the stylesheet's machine-lens header. */
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
 * easy to render and carries the same guarantee by construction.
 *
 * `classify` is opt-in per region: the util/run/loose blocks have
 * deterministic array shapes and are styled positionally in CSS, so only the
 * variable-length health region pays for pattern matching. Adding a class
 * changes no `innerText`, so the parity goldens are unaffected either way. */
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
  const resources = isLocalMach && resourcesQuery.data?.ok ? resourcesQuery.data.data : null;

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
        <Lines
          classify
          lines={healthLines({
            isLocalMach,
            machineName: label,
            resources,
            resourcesErrored,
          })}
        />
      </div>

      {/* (#1809, finishing #1508 step 4) The `RUNS ON <MACHINE>` list —
          #1508 step 2's own commit named it "deliberately interim". This is
          the replacement: a link into the Runs lens, pinned to this
          machine, carrying the run count so the page still answers "how
          much is there" without owning the list itself anymore. Rendered
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
