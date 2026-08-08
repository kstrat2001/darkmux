import { useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../../lib/fetcher";
import { queryKeys, MACHINE_MEM_POLL_MS } from "../../lib/queryKeys";
import { useFlowWindow } from "../../hooks/useFlowWindow";
import { useLiveMachines } from "../../hooks/useLiveMachines";
import { useLiveSessionIds } from "../../hooks/useLiveSessionIds";
import { buildMachineRuns, localMachineUid, looseRecords, nameOf, RECENT_CAP } from "../../lib/flow";
import { ramGiB } from "../../lib/format";
import { utilityLines, healthLines } from "./memoryLedgerLines";
import { machineRunLines } from "./runLines";
import type { MachineSpecs, MachineResources } from "../../types/handwritten";

/** One `<div>` per line. `innerText` puts each block-level sibling on its
 * own line regardless of stylesheet — see `runLines.ts`'s module doc for
 * why this port represents content as line arrays rather than leaning on
 * legacy's CSS-flex-dependent `innerText` line-break behavior. */
function Lines({ lines }: { lines: string[] }) {
  return (
    <>
      {lines.map((line, i) => (
        <div key={i}>{line}</div>
      ))}
    </>
  );
}

/**
 * The unified machine page — `renderMachine()` (viewer.html:1796-1991).
 * ONLY the nav-tab/deep-link entry (`goMachine`, always "the local
 * machine") is ported; the fleet-card drill-in to a REMOTE machine
 * (`drillMachine(uid, false)`) has no route in this scaffold's `Route` type
 * yet (`lib/route.ts`'s `{kind:"machine"}` carries no uid) — so
 * `isLocalMach` is always `true` here, unlike legacy's
 * `state.machineIsLocal || isLocalMachine(state.machine)` OR-gate. A future
 * packet that wires a remote drill-in needs to widen the route and restore
 * that OR — ledgered rather than silently narrowed (see the packet
 * report's deviations section).
 *
 * Data sources (read the code, not the plan's assumption): `/machine/specs`
 * + `/machine/resources` (the plan's named pair) PLUS `/flow/<today>` +
 * `/flow/<yesterday>` (the runs-on-this-machine list is derived from RAW
 * flow records, not `/runs` — see `lib/flow.ts`'s module doc) PLUS
 * `/fleet/machines/live` + `/fleet/sessions/live` (machine presence +
 * session liveness fallback).
 */
export function MachineLens() {
  const nowMs = Date.now();
  const [recentAll, setRecentAll] = useState(false);

  const flowWindow = useFlowWindow(nowMs);
  const liveMachines = useLiveMachines();
  const liveSessionIds = useLiveSessionIds();

  const specsQuery = useQuery({
    queryKey: queryKeys.machineSpecs(),
    queryFn: () => fetchJson<MachineSpecs>("/machine/specs"),
  });
  const resourcesQuery = useQuery({
    queryKey: queryKeys.machineResources(),
    queryFn: () => fetchJson<MachineResources>("/machine/resources"),
    refetchInterval: MACHINE_MEM_POLL_MS,
  });

  const specs = specsQuery.data?.ok ? specsQuery.data.data : null;
  const resourcesErrored = resourcesQuery.data ? !resourcesQuery.data.ok : false;
  const resources = resourcesQuery.data?.ok ? resourcesQuery.data.data : null;

  const localUid = useMemo(
    () => localMachineUid(flowWindow.data, liveMachines, specs?.machine_id ?? null),
    [flowWindow.data, liveMachines, specs],
  );
  const isLocalSpecs = !!(specs && localUid != null && nameOf(flowWindow.data, liveMachines, localUid) === specs.machine_id);
  // This route is always "the local machine" (see the module doc above) —
  // `isLocalMach` in legacy also ORs in the explicit nav-tab/deep-link
  // intent, which this port's single always-local route makes trivially
  // true regardless of whether `isLocalSpecs` has confirmed yet.
  const isLocalMach = true;

  const label = localUid != null ? nameOf(flowWindow.data, liveMachines, localUid) : "this machine";
  const spec = isLocalSpecs && specs?.cpu_brand ? specs.cpu_brand + (specs.ram_total_bytes ? ` · ${ramGiB(specs.ram_total_bytes)} GB` : "") : "";

  const runs = useMemo(() => {
    if (localUid == null) return [];
    return buildMachineRuns(flowWindow.data, liveMachines, liveSessionIds, flowWindow.tMax, nowMs, localUid);
  }, [flowWindow.data, flowWindow.tMax, liveMachines, liveSessionIds, localUid, nowMs]);

  const loose = useMemo(() => (localUid != null ? looseRecords(flowWindow.data, localUid) : []), [flowWindow.data, localUid]);

  const total = runs.length;
  const shown = recentAll ? runs : runs.slice(0, RECENT_CAP);

  return (
    <div className="machine-lens">
      <div className="machine-lens__hdr">
        fleet › machine · {label}
        {spec ? ` — ${spec}` : ""}
      </div>

      <div className="machine-lens__util">
        <Lines lines={utilityLines(specs, isLocalSpecs)} />
      </div>

      {/* `data-state` is the parity harness's post-fetch content marker —
          the React-port twin of legacy's `.memcard` selector (see
          `tests/parity/extract.spec.ts`'s machine-lens comment): "loaded"
          appears ONLY once `/machine/resources` has resolved with real
          data, never during the loading/error placeholder text branches. */}
      <div className="machine-lens__health" data-state={resources ? "loaded" : resourcesErrored ? "error" : "loading"}>
        <Lines
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
