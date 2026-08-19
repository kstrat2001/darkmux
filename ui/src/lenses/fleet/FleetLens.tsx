import { useMemo, useState, type CSSProperties } from "react";
import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../../lib/fetcher";
import { queryKeys } from "../../lib/queryKeys";
import { useFlowWindow } from "../../hooks/useFlowWindow";
import { useFleetCoverage, useLiveMachines } from "../../hooks/useLiveMachines";
import { useLiveSessionIds } from "../../hooks/useLiveSessionIds";
import { localMachineUid, machineUids, machPresent, liveSessionSet, LIVE_WINDOW_MS, T } from "../../lib/flow";
import type { FlowRecord } from "../../types/handwritten";
import { fmtN, fmtC } from "../../lib/format";
import { MachineIcon } from "../../components/MachineIcon";
import { tokensOffMeter } from "./savings";
import { hybridNote } from "./hybridNote";
import { NotesDialog } from "../../components/NotesDialog";
import { openModalEl } from "../../lib/dialogManager";
import { buildFleetCard } from "./cards";
import { buildActivityTimeline, ACTIVITY_WINDOW_PRESETS, DEFAULT_ACTIVITY_WINDOW_MIN } from "./timeline";
import type { MachineSpecs } from "../../types/handwritten";

/** `ICON.machine` (viewer.html:935) — the generic processor/chip glyph
 * every fleet card renders, since `MACH_ICON` (the per-machine form-factor
 * lookup) is empty in the live viewer (see that source's own comment: "real
 * machines render with no icon until /machine/specs/<id> wiring lands"). No
 * text content — contributes nothing to the parity extractor's `innerText`,
 * same as legacy's inline SVG. */
/** (#1809) A fleet-card click's destination. The runs lens is taken ONLY for
 * a machine positively known to be remote; local and not-yet-known both go
 * to the residency room.
 *
 * The obvious rule — "local goes to the room, everything else goes to runs"
 * — is wrong in a way that only shows up in the first paint, and it was
 * measured doing exactly that: `localUid` is null until `/machine/specs`
 * resolves, so the LOCAL card was clickable-and-wrong for one frame
 * (`+0ms → lens=runs`, `+100ms → lens=machine`). On loopback that is a
 * blink; over the tailnet phone path it scales with round-trip time, and
 * the wrong destination is SILENT — you land on a populated, plausible runs
 * list rather than anything that says it guessed. `specs` is also gated on
 * `liveMode`, so on a playback route locality never resolves at all and the
 * old rule sent every card, local included, to the runs lens permanently.
 *
 * Inverting the unknown case fixes both, because the two failure modes are
 * not symmetric. A remote machine that lands in the residency room gets an
 * honest dead end — "residency / RAM not reported from here, local-probe
 * only" — which names its own limitation and self-corrects the moment specs
 * confirm locality. A local machine that lands in the runs lens gets a
 * confident, wrong, and unlabeled answer to a question it did not ask. When
 * a guess is unavoidable, guess toward the destination that admits it is
 * guessing. */
function machineDrillHash(uid: string, localUid: string | null): string {
  const knownRemote = localUid != null && uid !== localUid;
  if (knownRemote) return `lens=runs&machine=${encodeURIComponent(uid)}`;
  return `lens=machine&uid=${encodeURIComponent(uid)}`;
}

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
function SavingsHero({
  tokens: t,
  note,
  liveMode,
  data,
  nowMs,
}: {
  tokens: ReturnType<typeof tokensOffMeter>;
  note: ReturnType<typeof hybridNote>;
  liveMode: boolean;
  /** The window this hero's numbers derive from — passed through to
   *  `NotesDialog` so "history →" opens the SAME notes those numbers came
   *  from, not a second, differently-scoped fetch. */
  data: FlowRecord[];
  nowMs: number;
}) {
  const hours = Math.round(LIVE_WINDOW_MS / 3600000);
  return (
    <div className="savings">
      {/* (operator) "tokens · last 24h" rather than "by your fleet · last 24h",
          to match the event pane's "events last 24h". Two panels counting two
          things over the same window should say so the same way; "by your
          fleet" named the SOURCE where its neighbor named the SUBJECT.

          The window suffix is LIVE-ONLY — `const win=...live-mode...?` last
          ${h}h`:''` (viewer.html:1660). A replay's numbers cover the recorded
          day, not the last 24 hours, and the meta bar already states that
          day's range. (#1800 P2: the suffix was unconditional, so a replayed
          day claimed a window it had not been measured over.) */}
      <div className="saveyebrow">tokens{liveMode ? ` · last ${hours}h` : ""}</div>
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
        {/* (#1640) Legacy's real `<a class="hyblink" data-act="notes">history
            →</a>` (viewer.html:1584) — restored now that `NotesDialog`
            exists to open. Previously a plain, deliberately non-interactive
            `<span>` (a trap control would have looked clickable and done
            nothing, before the modal existed to back it). */}
        {note.hasHistory ? (
          <a className="hyblink" data-act="notes" href="#" onClick={(e) => {
            e.preventDefault();
            openModalEl("nmodalbg");
          }}>
            {" "}history →
          </a>
        ) : null}
      </div>
      <NotesDialog data={data} nowMs={nowMs} />
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
function FleetCoverageNotice({ historical = false }: { historical?: boolean }) {
  // A replay has no live coverage to report — see useFleetCoverage's note.
  const coverage = useFleetCoverage(!historical);
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

/** (#1800 P2) `records`/`tMax`/`tMin` OPTIONAL so playback can render this
 * same hero over a historical day. Omitted = the live rolling window, exactly
 * as before, so every existing caller is unchanged.
 *
 * `historical` is legacy's `liveMode`, inverted — and it is NOT merely a
 * presence switch. Legacy branches on it in FOUR places inside `renderFleet()`
 * + `savingsHero()`, and the port had collapsed all four to their live arm
 * because `/next` had no route that reached the other one:
 *
 * | surface | live | replay |
 * |---|---|---|
 * | hero eyebrow | `tokens · last 24h` | `tokens` |
 * | card count | running sessions, "N running" | the day's sessions, "N specialists" |
 * | timeline span | `max(tMax, now) - window` | `tMin..tMax` |
 * | window control | 10m/1h/4h/24h | absent |
 *
 * Presence is the fifth: `liveMachines`/`liveSessionIds` are LIVE endpoints
 * describing NOW, so a replay must neither fetch nor consult them. Asserting
 * today's presence over a past day is exactly the "confidently WRONG:
 * machines read idle, running work reads zero" failure this file's own
 * coverage notice exists to warn about.
 *
 * (#1869) `tMax` was a fixed ceiling (`computeTMax(records)`, the day's true
 * max) AND the de facto playhead until this packet — the two were always
 * the same number, because nothing before `PlaybackLens`'s own transport
 * ever scrubbed. `PlaybackLens` now owns a real `t` (playhead) state and can
 * pass anything from `tMin` up to that ceiling, driven by its `Scrubber`.
 * That makes `tMax`-as-playhead a real conflation instead of a harmless one
 * — measured live: rewinding to a day's start collapsed the activity axis
 * itself (`tlMin..tlMax`) down to a single instant instead of staying fixed
 * while the playhead marker swept back across it, because the SAME number
 * was feeding both roles. This component now takes a separate `playhead`
 * prop (below) and threads TWO numbers where it used to thread one:
 * `flowWindow.tMax` stays the fixed axis ceiling everywhere it already fed
 * `cards.ts`/`timeline.ts`'s ceiling-shaped arguments; `playheadT` (derived
 * below) is the actual bracketing value — `machPresent`, `buildFleetCard`'s
 * `t`, `buildActivityTimeline`'s new `playheadT` argument, and `scopedData`
 * (the token hero has no playhead argument of its own, so its "as of the
 * playhead" gate is applied to the array it's handed instead). See
 * `timeline.ts`'s own module doc for the fuller account of the bug this
 * split fixes. */
export function FleetLens({
  records,
  tMax,
  tMin,
  playhead,
  historical = false,
}: {
  records?: FlowRecord[];
  tMax?: number;
  tMin?: number;
  /** (#1869) The scrub PLAYHEAD — a genuinely separate value from `tMax`
   * once `PlaybackLens` has a real transport. Defaults to `tMax` (the old,
   * pre-transport behavior: playhead == ceiling, always). `tMax` itself
   * stays the FIXED axis ceiling — `PlaybackLens` passes the day's true
   * `computeTMax(records)` there, unmoved by scrubbing, and its scrubbable
   * `t` state here instead. See `timeline.ts`'s own module doc for the bug
   * this split fixes: collapsing both into one number made the activity
   * axis itself shrink as the playhead scrubbed back, instead of staying
   * fixed while a marker sweeps across it. */
  playhead?: number;
  historical?: boolean;
} = {}) {
  const nowMs = Date.now();
  const liveMode = !historical;
  const [windowMinutes, setWindowMinutes] = useState(DEFAULT_ACTIVITY_WINDOW_MIN);

  const liveWindow = useFlowWindow(nowMs);
  const flowWindow = records !== undefined
    ? { data: records, tMax: tMax ?? 0, settled: true }
    : liveWindow;
  // The playhead every bracketing derivation below reads — `flowWindow.tMax`
  // when the caller didn't separate the two (live mode; any pre-#1869
  // caller), or the real scrub position when it did.
  const playheadT = playhead ?? flowWindow.tMax;
  // `enabled: false` stops the REQUEST, not just the result: an earlier draft
  // discarded the data while the hook kept polling `/fleet/machines/live`
  // every few seconds behind a replay.
  //
  // It is NOT sufficient on its own, and the QA gate proved it: a disabled
  // TanStack observer still READS the shared cache slot, so as long as ANY
  // enabled observer of the same key exists anywhere in the tree, this one
  // keeps returning live beats and the poll never stops. `App.tsx` held
  // exactly such an observer. Gating the fetch AND the consumer is what makes
  // the property true in the composed app rather than only in this lens's own
  // isolated test.
  const liveMachines = useLiveMachines(liveMode);
  const liveSessionIds = useLiveSessionIds(liveMode);
  // `/machine/specs` is the THIRD live-only endpoint on this screen, and the
  // one that got away in the first pass. It describes the hardware of the
  // machine serving the page RIGHT NOW — `pollMachineSpecs` is the live-only
  // 5s poll, and legacy states outright that "playback mode never starts that
  // poll" (viewer.html:2696), leaving `MACHINE_SPECS` null so `specOf` returns
  // "" and the card reads "hardware not reported". Rendering today's CPU and
  // RAM against a replayed day is the same confidently-wrong claim as
  // rendering today's presence.
  //
  // It also made the parity test genuinely FLAKY rather than merely wrong:
  // whether the specs response landed before the assertion was a race, so a
  // local run passed and CI failed on the identical commit. Gating it removes
  // the race at its source — the request never happens — instead of waiting
  // harder for a value that should not be read.
  const specsQuery = useQuery({
    enabled: liveMode,
    queryKey: queryKeys.machineSpecs(),
    queryFn: () => fetchJson<MachineSpecs>("/machine/specs"),
  });
  const specs = liveMode && specsQuery.data?.ok ? specsQuery.data.data : null;

  // (#1809) Which uid IS this daemon — the SAME derivation `App.tsx`/
  // `MachineLens.tsx` already do off their own copies of `flowWindow`/
  // `liveMachines`/`specs`, used here for exactly one decision: where a
  // fleet-card CLICK routes to (see the card below). `null` on a replay
  // (`specs` is gated off — `localMachineUid(..., null)` returns `null`
  // unconditionally) and on any daemon-less static build (no
  // `/machine/specs` to confirm against) — every card reads as "not
  // confirmed local" in both cases, which is the honest answer: neither has
  // a live residency probe to claim local identity against.
  const localUid = useMemo(
    () => localMachineUid(flowWindow.data, liveMachines, specs?.machine_id ?? null),
    [flowWindow.data, liveMachines, specs],
  );

  // (#1869) The token hero + hybrid note are "as of the playhead" — legacy's
  // own `visible()` gate (`DATA.filter(r=>T(r.ts)<=state.t)`), restored at
  // this call site rather than inside `tokensOffMeter`/`hybridNote`
  // themselves (see `savings.ts`'s module doc for the full reasoning). A
  // no-op in live mode: `playheadT` there is `flowWindow.tMax`, which is
  // `computeTMax(flowWindow.data)` by construction, so every record already
  // satisfies `ts <= playheadT`. In replay, `playheadT` is the scrubbable
  // position `PlaybackLens` passes as its `playhead` prop, so this is what
  // makes scrubbing before a session's completion drop that session's
  // tokens out of "local" and into "unattributed" — the token half of the
  // issue's own acceptance test.
  const scopedData = useMemo(
    () => flowWindow.data.filter((r) => T(r.ts) <= playheadT),
    [flowWindow.data, playheadT],
  );
  const tokens = useMemo(() => tokensOffMeter(scopedData), [scopedData]);
  const note = useMemo(() => hybridNote(scopedData, tokens), [scopedData, tokens]);

  const liveSet = useMemo(
    // The flow-derived liveness FALLBACK inside `liveSessionSet` is itself
    // live-only in legacy (viewer.html:3378). Without `liveMode` a replay
    // would route around the disabled presence hooks above and re-derive
    // "running" from the day's own records — presence-agnostic in name only.
    () => liveSessionSet(flowWindow.data, liveSessionIds, nowMs, liveMode),
    [flowWindow.data, liveSessionIds, nowMs, liveMode],
  );
  const uids = useMemo(() => machineUids(flowWindow.data, liveMachines), [flowWindow.data, liveMachines]);

  const cards = useMemo(
    () =>
      uids.map((m) =>
        buildFleetCard(
          flowWindow.data,
          liveMachines,
          specs,
          liveSet,
          machPresent(flowWindow.data, liveMachines, playheadT, m) === false,
          m,
          liveMode,
          playheadT,
        ),
      ),
    [uids, flowWindow.data, playheadT, liveMachines, specs, liveSet, liveMode],
  );

  const timeline = useMemo(
    () =>
      buildActivityTimeline(
        flowWindow.data,
        liveMachines,
        uids,
        liveSet,
        // The FIXED axis ceiling — never the playhead. See timeline.ts's own
        // doc + this component's `playhead` prop doc for why the two must
        // stay separate arguments once a replay can scrub.
        flowWindow.tMax,
        nowMs,
        windowMinutes,
        liveMode,
        tMin ?? 0,
        playheadT,
      ),
    [flowWindow.data, liveMachines, uids, liveSet, flowWindow.tMax, nowMs, windowMinutes, liveMode, tMin, playheadT],
  );

  return (
    <div className="fleet-lens" data-state={flowWindow.settled ? "loaded" : "loading"}>
      <SavingsHero tokens={tokens} note={note} liveMode={liveMode} data={scopedData} nowMs={nowMs} />
      <FleetCoverageNotice historical={historical} />
      <div className="fleet">
        {cards.map((card) => (
          // `<div class="mach ..." data-act="machine" data-arg="${uid}">`
          // (viewer.html:1711) — the fleet-card drill-in: `ACTIONS.machine`
          // (viewer.html:2991) calls `drillMachine(uid)` for an explicit
          // arg. Ported as a real cross-lens navigation (a literal
          // `location.hash` write, firing `hashchange` so `useHashRoute`
          // actually swaps the rendered component — the SAME mechanism
          // `NavChrome`'s tab clicks use, see that component's own doc for
          // why replaceState alone can't do this), not a
          // `history.replaceState`. `data-act`/`data-arg` themselves carry
          // no behavior here (the click goes through the `onClick` below,
          // not a delegated listener reading these attrs) — they're
          // restored purely as the DOM inspection hook e2e specs drill
          // through (`viewer-lifecycle.spec.js`, `viewer-xss.spec.js`),
          // same contract legacy's markup gave them.
          //
          // (#1809) The DESTINATION now splits by locality, which legacy
          // never did (every drill went to the same page). Residency is
          // local-probe-only by construction (#1286's "observer must not
          // join the observed" — `/machine/resources` always describes
          // THIS daemon's own host, never a remote one's), so a remote
          // machine's page would otherwise be a header plus two
          // "not reported" notices — the runs lens, pinned to that machine,
          // is the honest destination instead. The LOCAL card (confirmed
          // against `localUid`, above) keeps going to the residency room,
          // same as before this packet.
          <div
            key={card.uid}
            className={`mach${card.active && !card.absent ? " active" : ""}${card.absent ? " absent" : ""}`}
            data-act="machine"
            data-arg={card.uid}
            role="button"
            tabIndex={0}
            onClick={() => {
              location.hash = machineDrillHash(card.uid, localUid);
            }}
            onKeyDown={(e) => {
              if (e.key === "Enter" || e.key === " ") {
                e.preventDefault();
                location.hash = machineDrillHash(card.uid, localUid);
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
            <div className="runs">
              {card.runsCount} {card.runsLabel}
            </div>
          </div>
        ))}
      </div>
      {uids.length ? (
        <div className="fleettl" style={{ "--lname-w": `${timeline.labelWidthPx}px` } as CSSProperties}>
          <div className="tlhdr">
            <span>{timeline.headerText}</span>
            {/* `const winCtl=liveMode?...:''` (viewer.html:1764) — LIVE-ONLY.
                A replay shows the full recorded day, so there is no window to
                slide over and the control would be a dead knob. */}
            {liveMode ? (
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
            ) : null}
          </div>
          {timeline.lanes.map((lane) => (
            <div className="lane" key={lane.uid}>
              <div className="lname" title={lane.name}>
                {lane.name}
              </div>
              <div className="tltrack">
                {/* (#1639, drill-in packet) Session drill — click a bar, land
                    on `#session=<sid>`. Legacy's OWN `.sbar` bars are inert
                    (no `data-act`, no click handler anywhere in
                    `viewer.html`'s timeline code); legacy's only session-drill
                    click was `recentRow()`'s "open →" link on the machine
                    page's per-run list, which #1809 removed outright when it
                    replaced that list with a link into the runs lens (see
                    `MachineLens.tsx`'s own doc, and `viewer-session-url.spec.js`'s
                    module doc for the full gap history). Since #1809 nothing
                    ANYWHERE in this port reaches `SessionReplay` by clicking,
                    even though the fetch + render it needs (`/flow-session/<id>`
                    → `runRegions`) has worked since Packet 4.
                    This is a deliberate WIDENING beyond legacy's own address-bar
                    behavior, same precedent as `machineDrillHash`'s `uid=` and
                    the `machine=` runs-lens pin above: the activity lane already
                    names every session on screen (`bar.sid`, carried into
                    `bar.title`), so it is the least-surprising place to attach
                    the click legacy never wired. A real `location.hash` write
                    (not `writeHash`/`replaceState`) — the same mechanism every
                    other cross-lens hop in this file uses — so `hashchange`
                    fires, back/forward/copy-paste all behave, and `useSyncHash`
                    never has to reconcile a route no navigation actually
                    happened for. */}
                {lane.bars.map((bar) => (
                  <div
                    key={bar.sid}
                    className={`sbar ${bar.cls}`}
                    style={{ left: `${bar.leftPct}%`, width: `${bar.widthPct}%` }}
                    title={bar.title}
                    data-act="session"
                    data-arg={bar.sid}
                    role="button"
                    tabIndex={0}
                    onClick={() => {
                      location.hash = `session=${encodeURIComponent(bar.sid)}`;
                    }}
                    onKeyDown={(e) => {
                      if (e.key === "Enter" || e.key === " ") {
                        e.preventDefault();
                        location.hash = `session=${encodeURIComponent(bar.sid)}`;
                      }
                    }}
                  />
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
