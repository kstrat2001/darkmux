import { useMemo, useState, type CSSProperties } from "react";
import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../../lib/fetcher";
import { queryKeys } from "../../lib/queryKeys";
import { useFlowWindow } from "../../hooks/useFlowWindow";
import { useFleetCoverage, useLiveMachines } from "../../hooks/useLiveMachines";
import { useLiveSessionIds } from "../../hooks/useLiveSessionIds";
import { machineUids, machPresent, liveSessionSet, LIVE_WINDOW_MS } from "../../lib/flow";
import { fmtN, fmtC } from "../../lib/format";
import { MachineIcon } from "../../components/MachineIcon";
import { tokensOffMeter } from "./savings";
import { hybridNote } from "./hybridNote";
import { buildFleetCard } from "./cards";
import { buildActivityTimeline, ACTIVITY_WINDOW_PRESETS, DEFAULT_ACTIVITY_WINDOW_MIN } from "./timeline";
import type { MachineSpecs } from "../../types/handwritten";

/** `ICON.machine` (viewer.html:935) — the generic processor/chip glyph
 * every fleet card renders, since `MACH_ICON` (the per-machine form-factor
 * lookup) is empty in the live viewer (see that source's own comment: "real
 * machines render with no icon until /machine/specs/<id> wiring lands"). No
 * text content — contributes nothing to the parity extractor's `innerText`,
 * same as legacy's inline SVG. */
/** `sc()` — viewer.html:1633. One token-class chip (value over label). */
function Chip({ value, label, cls }: { value: string | number; label: string; cls?: string }) {
  return (
    <div className={`savc${cls ? ` ${cls}` : ""}`}>
      <div className="scv">{value}</div>
      <div className="scl">{label}</div>
    </div>
  );
}

/**
 * `savingsHero()` — viewer.html:1619-1666 (#783, #1186). Always renders,
 * even at zero — a fresh fleet with no dispatches yet shows "0", not a
 * hidden card (showing "0" that then climbs reads as a live odometer;
 * hiding it made it pop in late on the legacy mobile client — see that
 * source's own comment).
 *
 * TOKENS ONLY — class labels, no rates, no currency, no savings formula.
 * The `unknown` chip is the honesty-about-incomplete-data surface: it
 * renders (with its explanatory `title`) only when `t.unknown` is nonzero,
 * and is EXCLUDED from `t.local` rather than folded into it — see
 * `savings.ts`'s module doc for why that exclusion is load-bearing, not
 * incidental.
 */
function SavingsHero({ tokens: t, note }: { tokens: ReturnType<typeof tokensOffMeter>; note: ReturnType<typeof hybridNote> }) {
  const hours = Math.round(LIVE_WINDOW_MS / 3600000);
  return (
    <div className="savings">
      {/* (operator) "tokens · last 24h" rather than "by your fleet · last 24h",
          to match the event pane's "events last 24h". Two panels counting two
          things over the same window should say so the same way; "by your
          fleet" named the SOURCE where its neighbour named the SUBJECT. */}
      <div className="saveyebrow">tokens · last {hours}h</div>
      <div className="savrow">
        <div className="savlead">
          <div className="savnum">{fmtN(t.local)}</div>
          <div className="savlbl">local tokens</div>
        </div>
        <div className="savlead cloud">
          <div className="savnum">{fmtN(t.cloud)}</div>
          <div className="savlbl">cloud tokens</div>
        </div>
        {t.unknown ? (
          <div
            className="savlead unknown"
            title="No dispatch record for these sessions named an endpoint, so darkmux cannot say whether the model ran locally or on a hosted endpoint. They are excluded from the local figure rather than assumed to be free."
          >
            <div className="savnum">{fmtN(t.unknown)}</div>
            <div className="savlbl">unattributed</div>
          </div>
        ) : null}
        <div className="savclasses">
          <Chip value={fmtC(t.completion)} label="generated" cls="gen" />
          <Chip value={fmtC(t.fresh)} label="fresh input" />
          <Chip value={fmtC(t.reread)} label="re-read" />
          {t.uncls ? <Chip value={fmtC(t.uncls)} label="unclassified" cls="uncls" /> : null}
          <Chip value={t.runs} label={`dispatch${t.runs === 1 ? "" : "es"}`} />
        </div>
      </div>
      <div className="hybnote">
        <b className="hybpre">Orchestrator note:</b> {note.text}
        {/* (QA) Legacy's `history →` opens a notes modal that is NOT ported.
            Rendering it as a pointer-cursor accent link on the default view
            would be a trap control: it looks clickable, it is the README
            screenshot, and nothing happens. Kept as a plain marker so the
            information ("there are older notes") survives without promising
            an interaction that does not exist. Restore the link when the
            modal lands. */}
        {note.hasHistory ? <span className="hybmore"> (older notes exist)</span> : null}
      </div>
    </div>
  );
}

/**
 * The fleet default view — `renderFleet()` (viewer.html:1667-1741): the
 * savings hero, one card per machine, and the recent-activity timeline.
 * `/next`'s default (no-hash) route. See `savings.ts`/`hybridNote.ts`/
 * `cards.ts`/`timeline.ts` for the ported pure logic this component
 * composes.
 *
 * Data sources: `/flow/<today>` + `/flow/<yesterday>` (the live window every
 * number here derives from — `useFlowWindow`), `/fleet/machines/live` +
 * `/fleet/sessions/live` (presence), `/machine/specs` (this machine's own
 * hardware string).
 */
/**
 * (#1729) Presence coverage, on the default view.
 *
 * Every machine card and every "N running" count on this screen is derived
 * from presence. When the fleet substrate cannot be read, those surfaces do
 * not go blank — they render CONFIDENTLY WRONG: machines read idle, running
 * work reads zero, and timeline bars lose their run colouring. That is the
 * dead-looking-seats bug (#1483) with a nicer layout.
 *
 * `off` and `ok` say nothing, deliberately: a standalone machine has no
 * fleet substrate by design, and warning it would be the bug.
 *
 * This restores a marker that briefly existed on `FleetStrip` and was lost
 * when this lens replaced it — a regression no test caught, because
 * FleetStrip's own tests kept passing while it stopped being mounted.
 */
function FleetCoverageNotice() {
  const coverage = useFleetCoverage();
  const fleet = coverage?.sources?.fleet;
  if (!fleet || fleet.state === "ok" || fleet.state === "off") return null;
  const stale = fleet.state === "stale";
  return (
    <div className="fleetcov" data-state={fleet.state} role="status">
      <span className="fleetcov__icon">⚠</span>
      <span>
        {stale
          ? `Fleet presence is stale${"age_ms" in fleet ? ` (${Math.round(fleet.age_ms / 1000)}s old)` : ""} — machines and run counts below may have moved on.`
          : "Fleet presence could not be read — machines and run counts below cover THIS MACHINE only, and are not the whole fleet."}
      </span>
    </div>
  );
}

export function FleetLens() {
  const nowMs = Date.now();
  const [windowMinutes, setWindowMinutes] = useState(DEFAULT_ACTIVITY_WINDOW_MIN);

  const flowWindow = useFlowWindow(nowMs);
  const liveMachines = useLiveMachines();
  const liveSessionIds = useLiveSessionIds();
  const specsQuery = useQuery({
    queryKey: queryKeys.machineSpecs(),
    queryFn: () => fetchJson<MachineSpecs>("/machine/specs"),
  });
  const specs = specsQuery.data?.ok ? specsQuery.data.data : null;

  const tokens = useMemo(() => tokensOffMeter(flowWindow.data), [flowWindow.data]);
  const note = useMemo(() => hybridNote(flowWindow.data, tokens), [flowWindow.data, tokens]);

  const liveSet = useMemo(
    () => liveSessionSet(flowWindow.data, liveSessionIds, nowMs),
    [flowWindow.data, liveSessionIds, nowMs],
  );
  const uids = useMemo(() => machineUids(flowWindow.data, liveMachines), [flowWindow.data, liveMachines]);

  const cards = useMemo(
    () =>
      uids.map((m) =>
        buildFleetCard(flowWindow.data, liveMachines, specs, liveSet, machPresent(flowWindow.data, liveMachines, flowWindow.tMax, m) === false, m),
      ),
    [uids, flowWindow.data, flowWindow.tMax, liveMachines, specs, liveSet],
  );

  const timeline = useMemo(
    () => buildActivityTimeline(flowWindow.data, liveMachines, uids, liveSet, flowWindow.tMax, nowMs, windowMinutes),
    [flowWindow.data, liveMachines, uids, liveSet, flowWindow.tMax, nowMs, windowMinutes],
  );

  return (
    <div className="fleet-lens" data-state={flowWindow.settled ? "loaded" : "loading"}>
      <SavingsHero tokens={tokens} note={note} />
      <FleetCoverageNotice />
      <div className="fleet">
        {cards.map((card) => (
          // `<div class="mach ..." data-act="machine" data-arg="${uid}">`
          // (viewer.html:1711) — the fleet-card drill-in this packet wires:
          // `ACTIONS.machine` (viewer.html:2991) calls `drillMachine(uid)`
          // for an explicit arg, local OR remote. Ported as a real
          // cross-lens navigation (a literal `location.hash` write, firing
          // `hashchange` so `useHashRoute` actually swaps the rendered
          // component — the SAME mechanism `NavChrome`'s tab clicks use,
          // see that component's own doc for why replaceState alone can't
          // do this), not a `history.replaceState` (legacy's OWN
          // `syncLabHash` never even names the drilled uid in the address
          // bar at all — see `route.ts`'s widened-route doc for why this
          // port's `uid=` param is a deliberate improvement, not a replay
          // of legacy's own mechanism).
          <div
            key={card.uid}
            className={`mach${card.active && !card.absent ? " active" : ""}${card.absent ? " absent" : ""}`}
            role="button"
            tabIndex={0}
            onClick={() => {
              location.hash = `lens=machine&uid=${encodeURIComponent(card.uid)}`;
            }}
            onKeyDown={(e) => {
              if (e.key === "Enter" || e.key === " ") {
                e.preventDefault();
                location.hash = `lens=machine&uid=${encodeURIComponent(card.uid)}`;
              }
            }}
          >
            <div className="name">
              <span className="mico">
                <MachineIcon />
              </span>
              {card.name}
            </div>
            <div className="spec">{card.spec ? card.spec : <span className="specdim">hardware not reported</span>}</div>
            <div className="stat">
              <span className="dot" />
              {card.stat}
            </div>
            <div className="runs">{card.runsCount} running</div>
          </div>
        ))}
      </div>
      {uids.length ? (
        <div className="fleettl" style={{ "--lname-w": `${timeline.labelWidthPx}px` } as CSSProperties}>
          <div className="tlhdr">
            <span>{timeline.headerText}</span>
            <span className="twin">
              {ACTIVITY_WINDOW_PRESETS.map((p) => (
                <button
                  key={p.minutes}
                  className={`twinb${windowMinutes === p.minutes ? " on" : ""}`}
                  onClick={() => setWindowMinutes(p.minutes)}
                >
                  {p.label}
                </button>
              ))}
            </span>
          </div>
          {timeline.lanes.map((lane) => (
            <div className="lane" key={lane.uid}>
              <div className="lname" title={lane.name}>
                {lane.name}
              </div>
              <div className="tltrack">
                {lane.bars.map((bar) => (
                  <div key={bar.sid} className={`sbar ${bar.cls}`} style={{ left: `${bar.leftPct}%`, width: `${bar.widthPct}%` }} title={bar.title} />
                ))}
                <div className="ph" style={{ left: `${timeline.playheadPct}%` }} />
              </div>
            </div>
          ))}
          <div className="tlaxis">
            <span>{timeline.axis[0]}</span>
            <span>{timeline.axis[1]}</span>
            <span>{timeline.axis[2]}</span>
          </div>
        </div>
      ) : (
        <div className="fleettl">
          <div className="tlempty">waiting for the first flow record…</div>
        </div>
      )}
    </div>
  );
}
