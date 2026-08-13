import { useMemo, useState, type KeyboardEvent } from "react";
import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../../lib/fetcher";
import { queryKeys, MACHINE_MEM_POLL_MS } from "../../lib/queryKeys";
import { useFlowWindow } from "../../hooks/useFlowWindow";
import { useLiveMachines } from "../../hooks/useLiveMachines";
import { useLiveSessionIds } from "../../hooks/useLiveSessionIds";
import { buildMachineRuns, localMachineUid, looseRecords, nameOf, RECENT_CAP } from "../../lib/flow";
import { specOf } from "../fleet/cards";
import { utilityLines, healthLines } from "./memoryLedgerLines";
import { machineRunLines } from "./runLines";
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

/** Enter/Space activates a `role="button"` `<div>` the same way a native
 * `<button>` would — this element has a click handler and no other keyboard
 * path to it (matching `RunsBoard.tsx`'s own `onActivateKeyDown`, ported
 * here rather than shared since the two modules have no other coupling). */
function onActivateKeyDown(onActivate: () => void) {
  return (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      onActivate();
    }
  };
}

/** One `<div>` per line. `innerText` puts each block-level sibling on its
 * own line regardless of stylesheet — see `runLines.ts`'s module doc for
 * why this port represents content as line arrays rather than leaning on
 * legacy's CSS-flex-dependent `innerText` line-break behavior.
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
 * The unified machine page — `renderMachine()` (viewer.html:1796-1991).
 * Reached by THREE entry points, same as legacy (viewer.html:1827-1829):
 * the nav tab / `#lens=machine` deep-link (`uid: null` — always "the local
 * machine"), and a fleet-card drill (`uid: <uid>` — local OR remote,
 * `drillMachine(uid, false)`). `isLocalMach` restores legacy's
 * `state.machineIsLocal || isLocalMachine(state.machine)` OR-gate now that
 * `lib/route.ts`'s widened `{kind:"machine",uid}` gives a fleet-card drill
 * somewhere to carry its uid (this OR was narrowed to an unconditional
 * `true` before that widening existed — see the drill-in packet's report
 * for the restore).
 *
 * Data sources (read the code, not the plan's assumption): `/machine/specs`
 * + `/machine/resources` (the plan's named pair — resources is fetched ONLY
 * for the local machine, matching legacy's `pollMachineMem` guard: a
 * remote machine's page never reads THIS daemon's own probe under the
 * wrong name) PLUS `/flow/<today>` + `/flow/<yesterday>` (the
 * runs-on-this-machine list is derived from RAW flow records, not `/runs`
 * — see `lib/flow.ts`'s module doc) PLUS `/fleet/machines/live` +
 * `/fleet/sessions/live` (machine presence + session liveness fallback,
 * ALSO the source of a remote machine's own `specs` string — `specOf()`,
 * viewer.html:1124-1129 — since a remote machine's hardware line comes
 * from its presence beat, not this daemon's local `/machine/specs` probe).
 */
export function MachineLens({ uid: routeUid }: { uid: string | null }) {
  const nowMs = Date.now();
  const [recentAll, setRecentAll] = useState(false);

  const flowWindow = useFlowWindow(nowMs);
  const liveMachines = useLiveMachines();
  const liveSessionIds = useLiveSessionIds();

  const specsQuery = useQuery({
    queryKey: queryKeys.machineSpecs(),
    queryFn: () => fetchJson<MachineSpecs>("/machine/specs"),
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
  const isLocalSpecs = !!(specs && targetUid != null && nameOf(flowWindow.data, liveMachines, targetUid) === specs.machine_id);
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
    enabled: isLocalMach,
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

  const runs = useMemo(() => {
    if (targetUid == null) return [];
    return buildMachineRuns(flowWindow.data, liveMachines, liveSessionIds, flowWindow.tMax, nowMs, targetUid);
  }, [flowWindow.data, flowWindow.tMax, liveMachines, liveSessionIds, targetUid, nowMs]);

  const loose = useMemo(() => (targetUid != null ? looseRecords(flowWindow.data, targetUid) : []), [flowWindow.data, targetUid]);

  const total = runs.length;
  const shown = recentAll ? runs : runs.slice(0, RECENT_CAP);

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

      <div className="machine-lens__runshdr">RUNS ON {label.toUpperCase()}</div>
      <div className="machine-lens__runs">
        {shown.map((n, i) => (
          <div className="machine-lens__run" key={n.sid}>
            <Lines lines={machineRunLines(n, total - i, flowWindow.tMax)} />
          </div>
        ))}
        {total > RECENT_CAP && (
          <div
            className="machine-lens__runsmore"
            role="button"
            tabIndex={0}
            onClick={() => setRecentAll((v) => !v)}
            onKeyDown={onActivateKeyDown(() => setRecentAll((v) => !v))}
          >
            {recentAll ? "show fewer" : `show all ${total} →`}
          </div>
        )}
        {total === 0 && <div className="hint">no runs recorded for this machine at this point in the timeline</div>}
      </div>

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
