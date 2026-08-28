import { useMemo, useState, type CSSProperties } from "react";
import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../../lib/fetcher";
import { queryKeys } from "../../lib/queryKeys";
import { useFlowWindow } from "../../hooks/useFlowWindow";
import { useFleetCoverage, useLiveMachines, useStaticFleetBeats } from "../../hooks/useLiveMachines";
import { getSource } from "../../lib/source";
import { useLiveSessionIds } from "../../hooks/useLiveSessionIds";
import { machineUids, machPresent, liveSessionSet, LIVE_WINDOW_MS, T } from "../../lib/flow";
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
/** Every machine card drills to the RUNS lens, pinned to that machine.
 *
 * One destination, because the alternative is only ever valid for ONE
 * machine. The residency room reads `/machine/resources`, and that probe
 * answers for THIS host only — a remote machine's residency is unreadable
 * from here by construction (`MachineLens.tsx` says so in its own doc, and
 * gates `resourcesQuery` off accordingly). So the machine lens can never be
 * a correct destination for a remote card, and making it a CONDITIONAL
 * destination is what made the navigation inconsistent: the same gesture on
 * two cards went to two different kinds of page, one of which could not
 * answer.
 *
 * "What is running here" is a question every card can answer, local or
 * remote, and it is the question the card is already asking on the
 * operator's behalf — it shows a running count and an activity timeline. So
 * the runs list pinned to that machine continues what they were reading,
 * identically for every machine.
 *
 * Collapsing the branch also removes a defect it had to work around:
 * `localUid` is null until `/machine/specs` resolves, so the destination
 * CHANGED under the operator between first paint and +100ms, and #1809 added
 * a guess-toward-the-humbler-destination rule purely to make that flicker
 * harmless. With one destination there is no guess and no wrong frame.
 *
 * The residency room is not orphaned — it keeps the MACHINE tab in the nav
 * chrome, which is a bare `lens=machine` meaning "this machine", the only
 * machine it can actually report on.
 *
 * Operator call, 2026-08-23: "a remote machine's stats can't be read so
 * clicking that card ... would be an unusable result. The machine tab is
 * this machine and that makes sense ... Always going to runs would make it
 * consistent nav regardless of machine." */
function machineDrillHash(uid: string): string {
  return `lens=runs&machine=${encodeURIComponent(uid)}`;
}

/** (#1903) The running COUNT's own tap target — distinct from
 * `machineDrillHash` above (the card BODY's destination, unchanged by this
 * function or its caller). The count is the thing an operator is actually
 * reading on the card ("N running"), and it had no destination of its own:
 * a tap anywhere on the card, count included, fell through to
 * `machineDrillHash` and landed on the residency room, a machine drill the
 * operator did not ask for. See #1903's own issue text: "the running count
 * is the thing the operator is reading, and it has no affordance of its
 * own."
 *
 * `null` (render the plain, non-interactive count, same as before this
 * packet) when there's nothing running to open — `runningSessionIds` is
 * only ever non-empty in LIVE mode (see `FleetCard.runningSessionIds`'s own
 * doc), so this is naturally a no-op in replay, where the count means "the
 * day's whole session set" rather than "currently running" — a different
 * question, one the card body's own drill-in already answers honestly.
 *
 * Exactly one running session goes straight to that run's own session
 * drill (`#dispatch=<sid>`, same mechanism the activity-timeline bars below
 * already use) rather than the runs lens — the single-run case has one
 * obvious destination, and naming it directly saves a hop. Two or more
 * goes to the runs lens pinned to this machine (`lens=runs&machine=<uid>`,
 * the SAME hash `machineDrillHash` already constructs for a confirmed-
 * remote card body) — a list, not a single run, is the honest surface for
 * "several things running here". */
function machineRunsHash(uid: string, runningSessionIds: string[]): string | null {
  if (runningSessionIds.length === 0) return null;
  if (runningSessionIds.length === 1) return `dispatch=${encodeURIComponent(runningSessionIds[0])}`;
  return `lens=runs&machine=${encodeURIComponent(uid)}`;
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
        {/* (#2068) ALWAYS rendered, dimmed at zero. "Unattributed" is the
            state of every dispatch between its start and its completion, so
            mounting this tile only when the figure is non-zero made it
            appear and vanish with each in-flight dispatch — 85px of reflow
            under the hero on a phone, on every event during playback
            (measured CLS 1.21 over 12s). A streaming view must not let
            transient data change its geometry; the zero state is honest and
            the title already explains the figure. */}
        <div
          className={`savlead unknown${t.unknown ? "" : " zero"}`}
          title="No dispatch record for these sessions named an endpoint, so darkmux cannot say whether the model ran locally or on a hosted endpoint. They are excluded from the local figure rather than assumed to be free."
        >
          <div className="savnum">{fmtN(t.unknown)}</div>
          <div className="savlbl">unattributed</div>
        </div>
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
  // (#2067) A static build cannot poll presence, so its cards' hardware line
  // comes from the committed fleet snapshot instead — spec lookup ONLY;
  // presence at the playhead still derives from the records.
  const staticBeats = useStaticFleetBeats();
  const specBeats = getSource().kind === "static" ? staticBeats : liveMachines;
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
  //
  // (#1869 code review) This scopes only what THIS component owns — the
  // hero + timeline + fleet cards below. It does NOT reach the event log:
  // that's App-level chrome, a DOM SIBLING of this whole lens (mounted by
  // `App.tsx` beside `#stage`, not inside it), so it was never in scope for
  // a fix made from in here. That was a real, separate gap (the log kept
  // listing the whole day regardless of where the scrubber sat, while this
  // hero already tracked it) — closed at the App level instead, via
  // `PlaybackLens`'s `onPlayheadChange` reporting the same `playheadT` this
  // line reads up to `App`, which threads it into `EventLogColumn`. See
  // `App.tsx`'s own `eventLogRecords` doc for that half.
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
          specBeats,
        ),
      ),
    [uids, flowWindow.data, playheadT, liveMachines, specs, liveSet, liveMode, specBeats],
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
          // Every card drills to the runs lens pinned to that machine —
          // see `machineDrillHash`'s own doc for why there is one
          // destination and not a locality split. The short version:
          // residency is local-probe-only by construction (#1286's
          // "observer must not join the observed" — `/machine/resources`
          // always describes THIS daemon's own host), so the residency room
          // can only ever answer for one machine, and a destination valid
          // for one machine made the same gesture mean two different things.
          <div
            key={card.uid}
            className={`mach${card.active && !card.absent ? " active" : ""}${card.absent ? " absent" : ""}`}
            data-act="machine"
            data-arg={card.uid}
            role="button"
            tabIndex={0}
            // (#1903 QA fix) Explicit, so the card's computed accessible
            // name is DETERMINISTIC rather than folding in whatever the
            // nested running-count button's own `aria-label` happens to
            // say (per ARIA's presentational-children rule, a `button`
            // descendant's content — including its own name — isn't
            // exposed separately; without this, the outer card's name
            // absorbed the inner one's text, e.g. "MacBook-Pro Apple M5
            // Max dispatch in flight open the 2 running dispatches on
            // MacBook-Pro"). Nesting one interactive control inside
            // another is itself an accepted, documented exception here —
            // not an oversight — because the count needed its own tap
            // target (#1903) without moving or restructuring the card
            // body's own destination, which the issue is explicit must
            // stay unchanged. `stopPropagation` on the inner control (see
            // the running-count block below) keeps the two handlers from
            // double-firing; this `aria-label` is the remaining a11y-tree
            // cleanup that nesting still needs.
            aria-label={card.name}
            onClick={() => {
              location.hash = machineDrillHash(card.uid);
            }}
            onKeyDown={(e) => {
              if (e.key === "Enter" || e.key === " ") {
                e.preventDefault();
                location.hash = machineDrillHash(card.uid);
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
            {/* (#1903) The running count's own tap target — a SIBLING
                affordance to the card body's `machineDrillHash` click
                above, not a replacement for it. `runsHash` is `null`
                (falls through to the old plain, non-interactive count)
                whenever there's nothing running to open — see
                `machineRunsHash`'s own doc. `stopPropagation` on both
                handlers keeps a click/Enter on the count from ALSO firing
                the card body's own handler underneath it (this is a
                nested interactive control by necessity — the issue is
                explicit that the card body's destination must stay
                unchanged, which rules out restructuring the card to avoid
                the nesting). `.runs--live`'s own CSS is what makes it LOOK
                interactive, matching #1900's lesson in the other
                direction: a clickable-but-inert-looking control is as
                dishonest as an inert-looking one that's secretly a broken
                link. */}
            {(() => {
              const runsHash = machineRunsHash(card.uid, card.runningSessionIds);
              if (!runsHash) {
                return (
                  <div className="runs">
                    {card.runsCount} {card.runsLabel}
                  </div>
                );
              }
              const activate = (e: { stopPropagation: () => void }) => {
                e.stopPropagation();
                location.hash = runsHash;
              };
              return (
                <div
                  className="runs runs--live"
                  role="button"
                  tabIndex={0}
                  aria-label={`open the ${card.runsCount} running ${card.runsCount === 1 ? "dispatch" : "dispatches"} on ${card.name}`}
                  onClick={activate}
                  onKeyDown={(e) => {
                    if (e.key === "Enter" || e.key === " ") {
                      e.preventDefault();
                      activate(e);
                    }
                  }}
                >
                  {card.runsCount} {card.runsLabel}
                </div>
              );
            })()}
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
                    on `#dispatch=<sid>`. Legacy's OWN `.sbar` bars are inert
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
                      location.hash = `dispatch=${encodeURIComponent(bar.sid)}`;
                    }}
                    onKeyDown={(e) => {
                      if (e.key === "Enter" || e.key === " ") {
                        e.preventDefault();
                        location.hash = `dispatch=${encodeURIComponent(bar.sid)}`;
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
