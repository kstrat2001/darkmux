import { scopeCenter } from "../../lib/scopeCenter";
import { WorkStatus } from "../../components/WorkStatus";
import { Shimmer } from "../../components/Placeholder";
import { LampDot } from "../../components/LampDot";
import { LampForm } from "../../lib/lamp";
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { useCountUp } from "../../hooks/useCountUp";
import { parseNumericLike } from "../../lib/numericLike";
import { useQuery } from "@tanstack/react-query";
import { fetchJson, type FetchResult } from "../../lib/fetcher";
import { queryKeys, PRESENCE_POLL_MS } from "../../lib/queryKeys";
import { useSessionLiveness } from "../../hooks/useSessionLiveness";
import { flowToRenderModel } from "../../lib/flow";
import { NO_PRESENCE, isRunning, judgementAt, lifecycleAt, type Presence } from "../../lib/lifecycle";
import { useLifecyclePolicy } from "../../hooks/useLifecyclePolicy";
import { sessionRouteRecords, sessionRun } from "../../lib/runRef";
import { CATEGORY, ingest, isBookendStart, recordsAsOf, type NormRecord } from "../../lib/ingest";
import { useNowMs } from "../../lib/clock";
import { clkhm } from "../../lib/format";
import { getSource } from "../../lib/source";
import { useDay } from "../../hooks/useDay";
import { injectedPlaybackDate } from "../../lib/injectedMeta";

import { livenessState } from "../../components/LivenessPulse";
import { TokenScope } from "../../components/TokenScope";
import { usePlaybackClock } from "../../lib/pageClockRate";
import { WALL_CLOCK } from "../../lib/restHand";
import { liveStateLabel, reasonForLine, toolReadout, type LiveStateReading } from "../../lib/tokenRate";
import { leftTrimWidth } from "../../lib/leftTrim";
import { REPLAY_GRANULARITY_NOTE, useLiveOverlay } from "../../lib/liveChannel";
import { scopeStateOf, type ScopeState } from "../../lib/scopeMorph";
import type { RunRelay } from "../../types/generated/RunRelay";
import { useRelayOrigin } from "../../hooks/useRelayOrigin";
import { CLEAN_DETECTORS, runRegions } from "../session/sessionRun";
import type { BriefEntry, SessionRunView } from "../session/sessionRun";
import type { FlowRecordsResponse } from "../../types/generated/FlowRecordsResponse";

/**
 * `#dispatch=<id>` — `viewer.html`'s `catalogQuery()` session branch, and
 * `drillSession()`'s destination (the "open →" link on a machine page's run
 * row). Fetches `/flow-dispatch/<id>` the same way `boot()` does, runs it
 * through `flowToRenderModel` (`lib/flow.ts`) the same way `boot()`'s own
 * `cq` branch does, then renders `runRegions()`'s (`lenses/session/
 * sessionRun.ts`) derivation — the real port of legacy's
 * `renderSubsystem()`. See `sessionRun.ts`'s own module doc for exactly what
 * this covers (validated BYTE-FOR-BYTE against the one real recorded golden
 * this repo has for legacy's own render, `goldens/session-task-list.txt`)
 * and what it deliberately does not (the two SVG chart regions).
 *
 * DOM shape follows the same "one `<div>`/text-run per visible line"
 * convention `MachineLens`/`LabRunDetail` already establish — see
 * `MachineLens.tsx`'s `Lines` component doc for why this port represents
 * content as line arrays rather than leaning on legacy's CSS-flex-dependent
 * `innerText` line-break behavior.
 *
 * An EMPTY response is a genuine no-data state (this corpus's own
 * `flow-dispatch-task-list.json` fixture is non-empty, so the parity spec
 * exercises the populated branch — the empty branch is honest but
 * unexercised by this corpus).
 */
/** (#2878) A MODEL/SYSTEM metric tile's value (`.mv`), counting up/down
 * when it changes on a live run — TURNS, COMPACTIONS, a host CPU/RAM/GPU
 * percentage. `sessionRun.ts` hands this component an already-FORMATTED
 * string (comma grouping, `%`, rounding all baked in), so
 * `parseNumericLike` is the bridge: it recognizes a plain number (with
 * that same comma/`%` shape) and reproduces it exactly at every
 * intermediate frame. A value that ISN'T one plain number — a duration
 * like "10:15", a model name, "—" — renders exactly as it always did, no
 * animation, because there is nothing here safe to interpolate. */
/** The MODEL hero's state lamps (#2890; the TOK/S tile's before it): one
 *  per state, grey when off, exactly one lit in its state's color. Seeing every state at once is what makes the
 *  current one legible (operator: "is this resting? can't tell"). The rest
 *  lamp carries its countdown while lit. */
const SCOPE_LAMPS: Array<{ state: LiveStateReading["state"]; label: string }> = [
  { state: "generating", label: "gen" },
  { state: "prompt", label: "prompt" },
  { state: "tools", label: "tools" },
  { state: "rest", label: "rest" },
  // (#2911) "stalled", the word the fleet card's line and this row's own
  // status text (`liveStateLabel`) already use; the lamp was the one place
  // that said "stall".
  { state: "stalled", label: "stalled" },
];
export function ScopeLamps({
  reading,
  noSignal = false,
  finished = false,
}: {
  reading: { state: LiveStateReading["state"] | null; restSecondsLeft?: number; restReason?: string; toolName?: string; writing?: true; writingSeconds?: number; thinking?: boolean; compacting?: true; compactingSeconds?: number };
  /** (#2886 pass 3) `state: null` is ALSO what a disconnection-downgraded
   *  stall reads as (`liveStateWhileConnected`) — visually identical
   *  (every lamp off) but a different fact, so the aria text says which. */
  noSignal?: boolean;
  /** (#2890) A finished run keeps its lamp row (every lamp off) under the
   *  calm scope; the status text says it finished. */
  finished?: boolean;
}) {
  // `state: null` is no live execution (a mission between model steps):
  // every lamp is off.
  const aria =
    reading.state === null
      ? finished
        ? "finished"
        : noSignal
          ? "disconnected — page lost the daemon"
          : "no model working"
      : reading.state === "generating"
        ? reading.thinking
          ? "generating, thinking"
          : "generating"
        : liveStateLabel({
            state: reading.state,
            restSecondsLeft: reading.restSecondsLeft,
            restReason: reading.restReason,
            toolName: reading.toolName,
            writing: reading.writing,
            writingSeconds: reading.writingSeconds,
            compacting: reading.compacting,
            compactingSeconds: reading.compactingSeconds,
          } as LiveStateReading);
  return (
    <div className="scope-lamps" role="status" aria-label={`run state: ${aria}`}>
      {SCOPE_LAMPS.map(({ state, label }) => {
        const on = reading.state === state;
        // (#2890, operator) While the model reasons rather than writing
        // visible text, the lit GEN lamp reads "think" in the same shimmer
        // as the ring and the fleet card's "think tok/s". A state's name,
        // not live data; the dot stays GEN's green.
        const thinking = on && state === "generating" && reading.thinking === true;
        return (
          <span key={state} className="scope-lamp" data-state={state} data-on={on ? "true" : "false"} data-thinking={thinking ? "true" : undefined}>
            <LampDot form={on ? LampForm.Filled : LampForm.Off} />
            {/* Only the lit lamp shows its word; the rest read as dots, so
                the row stays one line in a narrow tile (operator, 2026-09-24:
                "the lights are too big, taking too much space"). The text
                stays in the DOM for the status role. */}
            <span className="scope-lamp__label">{thinking ? "think" : label}</span>
          </span>
        );
      })}
    </div>
  );
}

/** The readout line under the lamps (`.modelbox__note`): "no signal", a
 *  finished average's qualifier, TOOL GEN, compacting, why REST rests, or
 *  (#2963) the action of a read/write/edit darkmux is running and, when
 *  known, the running call's file. A
 *  tool's line puts the file in its own box that trims from the LEFT, so a
 *  long path loses its leading folders and keeps its file name; the whole
 *  line is the hover. One line either way (`styles.css`). */
function ScopeNote({ note, tool }: { note: string | null; tool?: { action: string; path?: string } }) {
  if (!note) return null;
  if (!tool?.path) return <div className="modelbox__note">{note}</div>;
  return (
    <div className="modelbox__note modelbox__note--tool" title={note}>
      <span className="modelbox__note-act">{`${tool.action} · `}</span>
      <TrimmedPath path={tool.path} />
    </div>
  );
}

/** (#2963) The file box of a tool's readout line, trimmed from the left.
 *  When the path overflows, the box is narrowed to the "…" plus whole
 *  characters (`leftTrimWidth`), so the "…" sits right after "write · "
 *  with the same spacing an untrimmed path has. Re-fit when the line's slot
 *  changes size. The width only ever shrinks inside the one-line slot, so
 *  nothing around it moves. */
function TrimmedPath({ path }: { path: string }) {
  const box = useRef<HTMLSpanElement>(null);
  useLayoutEffect(() => {
    const el = box.current;
    const slot = el?.parentElement;
    if (!el || !slot) return;
    const fit = () => {
      el.style.maxWidth = "";
      const text = el.firstElementChild as HTMLElement | null;
      if (!text) return;
      const probe = document.createElement("span");
      probe.textContent = "…";
      el.appendChild(probe);
      const ellipsis = probe.getBoundingClientRect().width;
      probe.remove();
      const width = leftTrimWidth({ available: el.clientWidth, full: text.getBoundingClientRect().width, chars: [...(text.textContent ?? "")].length, ellipsis });
      if (width !== null) el.style.maxWidth = `${width}px`;
    };
    fit();
    if (typeof ResizeObserver === "undefined") return;
    const ro = new ResizeObserver(fit);
    ro.observe(slot);
    return () => ro.disconnect();
  }, [path]);
  return (
    <span className="modelbox__note-path" ref={box}>
      <bdi>{path}</bdi>
    </span>
  );
}

/** (#2890) What the MODEL hero's scope shows, from the one view derivation:
 *  the live reading while the run is going, the calm finished state with its
 *  average once it has ended, or nothing when the unit did no model work.
 *  Exported for its tests; the same function serves live and playback. */
export function modelScopeHero(view: Pick<SessionRunView, "liveTokScope" | "finishedTokRate">): {
  state: ScopeState;
  tokensPerSec: number | null;
  toolName?: string;
  /** (#2889) TOOLS while the model writes the call — see `TokenScope`'s
   *  `toolWriting`. */
  toolWriting?: boolean;
  /** (#2890) Generating while the model reasons: the scope tints violet to
   *  pink and the rate shimmers; the words and number are unchanged. */
  thinking?: boolean;
  centerLabel: string | null;
  centerUnit: string | null;
  centerCarried: boolean;
  /** (#2915) The tube shows a utility job (compacting), not the work model. */
  utility?: true;
  /** (#2961) REST only: when the rest ends, on the page clock `clockMs` is
   *  read on. The scope phases its seconds hand and counts its number from
   *  it (`lib/restHand.ts`). */
  restEndMs?: number;
  /** (#2961) The page clock this reading was derived at. */
  clockMs?: number;
  lamps: { state: LiveStateReading["state"] | null; restSecondsLeft?: number; restReason?: string; toolName?: string; writing?: true; writingSeconds?: number; thinking?: boolean; compacting?: true; compactingSeconds?: number };
  note: string | null;
  /** (#2963) Present when `note` is a tool's line: the action and, when the
   *  running call's file is known, the file, so the line can trim the file
   *  from the left (`ScopeNote`). */
  noteTool?: { action: string; path?: string };
} | null {
  const live = view.liveTokScope;
  if (live) {
    const state = scopeStateOf({ state: live.state, noSignal: live.noSignal });
    const generating = state === "generating";
    // (#2889) The model generating a tool call: a wrench over "tool gen".
    // PROMPT: the brain.
    const writing = state === "tools" && live.writing === true;
    // (#2915) The execution is compacting: PROMPT stays lit, the tube reads
    // "compacting" with the utility treatment.
    const compacting = state === "prompt" && live.compacting === true;
    // (#2963) darkmux running a read/write/edit whose file is known: the
    // action and the file on the readout line ("write · src/a.ts").
    const toolLine =
      state === "tools" && !writing
        ? toolReadout({ state: "tools", toolName: live.toolName, ...(live.toolPath !== undefined ? { toolPath: live.toolPath } : {}) })
        : null;
    return {
      state,
      tokensPerSec: generating ? live.tokensPerSec : 0,
      toolName: state === "tools" ? live.toolName : undefined,
      toolWriting: state === "tools" ? writing : undefined,
      thinking: generating && live.thinking === true,
      ...(state === "rest" && live.restEndMs !== undefined ? { restEndMs: live.restEndMs, clockMs: live.clockMs } : {}),
      // (#2890) The center is the SAME for every scope in the app: see
      // `lib/scopeCenter.ts`.
      ...scopeCenter({
        state,
        tokensPerSec: live.tokensPerSec,
        carried: live.carried,
        restSecondsLeft: live.restSecondsLeft,
        writing,
        writingSeconds: live.writingSeconds,
        thinking: live.thinking === true,
        compacting,
        // (#2911) A live scope means the run is in flight: `state: null`
        // (a mission between model steps) reads "no model working" in the
        // tube, the same phrase the lamps' status gives it.
        inFlight: true,
      }),
      lamps: {
        state: live.state,
        restSecondsLeft: live.restSecondsLeft,
        ...(state === "rest" && live.restReason !== undefined ? { restReason: live.restReason } : {}),
        toolName: state === "tools" ? live.toolName : undefined,
        writing: live.writing,
        writingSeconds: live.writingSeconds,
        thinking: generating && live.thinking === true,
        ...(compacting ? { compacting: true as const, compactingSeconds: live.compactingSeconds } : {}),
      },
      // (#2926) While the model generates a tool call, the readout line
      // under the lamps says which tool and for how long ("tool gen · write
      // · 18s"): LM Studio streams nothing for the arguments, so there is no
      // rate, and the lit lamp and the tube center carry no live data. The
      // seconds tick with the page's clock (the playhead in playback).
      note:
        state === "nosignal"
          ? "disconnected"
          : writing
            ? liveStateLabel({ state: "tools", toolName: live.toolName, writing: true, writingSeconds: live.writingSeconds })
            : compacting
              ? // (#2915) "compacting · Ns" in the same readout slot, counting
                // with the page's clock; the tube center carries no timer.
                liveStateLabel({ state: "prompt", compacting: true, compactingSeconds: live.compactingSeconds })
              : state === "rest" && live.restReason !== undefined
                ? // (#2950) Why it rests, from the rest's own record, in the
                  // same slot: "thermal · serious". The words only: the
                  // countdown is already the tube's center (TOOL GEN's
                  // seconds are not, which is why its line carries them).
                  // Nothing when the record names no reason. Trimmed to the
                  // line (the lamp status keeps it whole).
                  reasonForLine(live.restReason)
                : toolLine
                  ? toolLine.path
                    ? `${toolLine.action} · ${toolLine.path}`
                    : // (#2963) The running call's file unknown: the action
                      // alone, never an earlier call's file.
                      toolLine.action
                  : null,
      ...(toolLine ? { noteTool: toolLine } : {}),
    };
  }
  const fin = view.finishedTokRate;
  if (fin) {
    return {
      state: "finished",
      // (#2890 operator review) The average drives the echo's wave, so the
      // shape matches the number; "—" (no fully billed turn) stays flat.
      tokensPerSec: Number.isFinite(Number(fin.average)) ? Number(fin.average) : 0,
      centerLabel: fin.average,
      centerUnit: "avg tok/s",
      centerCarried: false,
      lamps: { state: null },
      note: fin.sub,
    };
  }
  return null;
}

function AnimatedMetricValue({ value }: { value: string }) {
  const parsed = parseNumericLike(value);
  // `useCountUp` is called unconditionally either way — only the target it
  // tweens toward (a number, or `null` for "nothing to animate") depends
  // on `parsed`, so this never violates the rules of hooks.
  const tweened = useCountUp(parsed ? parsed.n : null, (n) => (n === null ? "" : parsed!.render(n)));
  return <>{parsed ? tweened : value}</>;
}

/** (#2000) `.brief-grid`'s `repeat(auto-fit, minmax(240px, 1fr))` resolves
 *  to an odd column count at common desktop widths (measured: 3 at
 *  1280/1440px — see `styles.css`'s own comment on `.track.brief-grid`).
 *  With row-major auto-flow and each `BriefEntry` as its OWN grid item, an
 *  odd column count means every row starts on the opposite parity from the
 *  last, so a label and its value straddle the column boundary and the
 *  brief renders wrong facts (`model` reading a filesystem path — see the
 *  issue's own reproduction).
 *
 *  The fix: give `sessionRun.ts`'s `pushKv` label/value pairs a SINGLE grid
 *  item to sit in, so a pair can never be split across a row boundary no
 *  matter the column count. Grouped here (view layer) rather than in
 *  `sessionRun.ts` (pure logic layer) because the grouping is a rendering
 *  concern, not a data concern — `BriefEntry[]`'s flat shape is what
 *  `sessionRun.test.ts`'s pure-logic assertions and the golden's `innerText`
 *  read already pin, and both stay valid unchanged: each entry still renders
 *  as its own block element, in the same order, just inside one extra
 *  wrapper per pair. `pushKv` (`sessionRun.ts`) always emits `label` then
 *  `value` together or neither — so every `"label"` is immediately followed
 *  by a `"value"` in practice — but a defensive fallback keeps a stray
 *  unpaired entry (a `"label"` with no following `"value"`, or a `"note"`)
 *  rendering exactly as before, alone in its own grid item. */
type BriefGroup =
  | { kind: "pair"; key: number; label: BriefEntry; value: BriefEntry }
  | { kind: "single"; key: number; entry: BriefEntry };

function groupBriefEntries(entries: BriefEntry[]): BriefGroup[] {
  const groups: BriefGroup[] = [];
  for (let i = 0; i < entries.length; i++) {
    const entry = entries[i];
    const next = entries[i + 1];
    if (entry.kind === "label" && next?.kind === "value") {
      groups.push({ kind: "pair", key: i, label: entry, value: next });
      i++; // consumed the value too
    } else {
      groups.push({ kind: "single", key: i, entry });
    }
  }
  return groups;
}

/** One label/value pair in the pending info card — real label, shimmered
 *  value. Mirrors the shape `groupBriefEntries`'s `"pair"` case renders once
 *  data lands (`.brief-pair` > `.brief-label` + `.brief-value`), so nothing
 *  shifts when the real value replaces the shimmer (#2068's CLS lesson). */
function PendingBriefPair({ label }: { label: string }) {
  return (
    <div className="brief-pair">
      <div className="brief-label">{label}</div>
      <div className="brief-value">
        <Shimmer minWidth="8em" />
      </div>
    </div>
  );
}

/** One MODEL/SYSTEM tile in the pending state — real label, shimmered value,
 *  same `.met`/`.mv`/`.ml` shape the loaded grid renders (see the `view.metrics`
 *  map in the main render below). */
function PendingTile({ label }: { label: string }) {
  return (
    <div className="met">
      <div className="mv">
        <Shimmer minWidth="3em" />
      </div>
      <div className="ml">{label}</div>
    </div>
  );
}

/**
 * (#2862) The pending state for `#dispatch=<session_id>` — the session id is
 * already known from the URL (the caller passed it in), so this draws the
 * REAL header, the info card's labels (route/runtime/model/workspace/timing
 * — the five `pushKv` calls in `sessionRun.ts` that are always present,
 * unlike `image`/`mission` which are conditional), and the MODEL/SYSTEM tile
 * grids with their labels. Only the values — which depend on records nobody
 * has fetched yet — shimmer.
 *
 * The MODEL tiles shown (TURNS, TOKENS IN, TOKENS OUT, CONTEXT) and the
 * SYSTEM tile shown (WALL CLOCK) are the set `sessionRun.ts` always produces
 * for a model-bearing run before any telemetry has arrived (`CONTEXT` is
 * literally `ctxLabel`'s own pre-data default, `!effNctx ? "CONTEXT" : ...`).
 * A `procedural.shell`-only run's real page never shows a MODEL section at
 * all (`hasModelWork` gates it) — this skeleton cannot know that in advance
 * (nothing has been fetched), so it draws the common case, same as guessing
 * six rows for the runs list. That is an inherent skeleton approximation,
 * not a regression: the alternative is the bare "loading…" line this issue
 * replaces.
 */
function SessionPendingHeader({ sessionId }: { sessionId: string }) {
  // `.session-ph`, deliberately NOT `.session-run`/`.session-run__header`/
  // `.pill` — a long list of existing specs use those bare classes (no
  // `[data-state="data"]` qualifier) as their "real session data has
  // landed" signal (`SessionReplay.test.tsx`, its transition sibling,
  // `App.test.tsx`'s scrubber suite). See `.session-ph`'s own doc in
  // styles.css for the collision this avoids and the CSS it stands in for.
  return (
    <div className="session-ph" data-state="pending" role="status" aria-label={`Loading session ${sessionId}`}>
      <h2 className="session-ph__header">
        <Shimmer as="span" className="pill" minWidth="5em" minHeight="1.3em" />{" "}
        <Shimmer as="span" minWidth="6em" />{" "}
        <span className="session-ph__meta">
          ({sessionId} on <Shimmer as="span" minWidth="5em" />)
        </span>
      </h2>
      <div className="track brief-grid">
        <PendingBriefPair label="route" />
        <PendingBriefPair label="model" />
        <PendingBriefPair label="workspace" />
        <PendingBriefPair label="timing" />
      </div>
      <section className="runsec" data-head="model">
        <div className="metrics" data-scope="model" role="group" aria-label="model metrics">
          <PendingTile label="TURNS" />
          <PendingTile label="TOKENS IN" />
          <PendingTile label="TOKENS OUT" />
          <PendingTile label="CONTEXT" />
        </div>
      </section>
      <section className="runsec" data-head="system">
        <div className="metrics" data-scope="system" role="group" aria-label="system metrics">
          <PendingTile label="WALL CLOCK" />
        </div>
      </section>
    </div>
  );
}

function BriefEntryContent({ entry }: { entry: BriefEntry }) {
  return entry.href ? (
    // A real anchor, so it is keyboard-reachable and middle-clickable like
    // any other link. Same text either way — the golden reads `innerText`,
    // which an <a> does not change.
    <a className="brief-link" href={entry.href}>
      {entry.text}
    </a>
  ) : (
    <>{entry.text}</>
  );
}

/** (#3017) ", asked on <machine>" for relayed work, else nothing. */
function askedOnSuffix(relay: RunRelay | null): string {
  return relay ? `, asked on ${relay.asked_on_machine}` : "";
}

export function SessionReplay({
  sessionId,
  missionId = null,
  playhead = null,
  connected = true,
  lastContactMs = null,
  viewerUid,
}: {
  sessionId: string;
  /** The mission whose run on this session the page shows, when the route
   *  names one (`#dispatch=<sid>&dispatch.mission=<id>`): the session's
   *  other missions' records are left out, so every region reads that run. */
  missionId?: string | null;
  playhead?: number | null;
  /** (#2886 pass 3, "STALL while disconnected") Whether the page has a
   *  working connection to the daemon — derived by `App.tsx` from the
   *  header's own liveness read (`useLiveTail`'s `LiveTailStatus`), the same
   *  value `Masthead`/`MachineDrawer` already render, ALSO folding in
   *  `isLiveRoute`: a static/demo build's `useLiveTail` never runs at all
   *  (`isLiveRoute` returns `false` for `getSource().kind === "static"`) and
   *  sits at `"reconnecting"` forever, which is not the same fact as a real
   *  daemon connection dropping — `App.tsx` computes `!isLiveRoute(route) ||
   *  liveStatus === "live"` before passing this down, so a static build
   *  never falsely reads "no signal". Defaults to `true` so a bare
   *  `<SessionReplay sessionId=... />` (every existing test) keeps behaving
   *  as before. */
  connected?: boolean;
  /** (#2886 pass 4, do-it — fresh-reviewer finding 5, "half-open connection
   *  race") `App.tsx`'s `lastContactRef.current`. `null` (the default) on
   *  every call that doesn't pass it, which skips the half-open check
   *  entirely — see `runRegions`'s own doc. */
  lastContactMs?: number | null;
  /** The page's own machine identity (`App.tsx`'s `localUid`): the route line says
   *  "this machine" only for a run that ran on it. Absent or `null` when not
   *  known (`runRegions` defaults it). */
  viewerUid?: string | null;
}) {
  // (#1972) POLLS while the session is live. Without this the page fetched
  // its records ONCE, which is the defect a live dogfood run exposed: the
  // wall clock advanced (it reads the browser clock), while turns, tokens,
  // signals and `lastBeatMs` all froze at page load — so the pulse went quiet
  // five seconds in and could never beat, on the one page whose entire
  // purpose is watching a run happen.
  //
  // This is the FOURTH live view found fetching once (#1966 was the third).
  // Liveness comes from presence rather than from these records: asking the
  // records whether to keep asking for records is circular, and presence is
  // already the fleet's source of truth for session membership. The gate also
  // stops a replay or a finished run polling forever.
  //
  // (#2011) `shouldPoll` — not bare presence — because a presence-gated
  // interval that simply switches off at the drop never fetches the terminal
  // record, and this page then reports `RUNNING` forever with a clock still
  // counting. `useSessionLiveness` owns that window; it also owns the SAME
  // query key this component reads, so the event log and the stage cannot
  // disagree about when the run ended.
  // The mission this run belongs to, learned from the run's own records
  // below. It reaches the liveness hook one render after those records land:
  // a mission's run-grain session never beats itself, so without it the page
  // is never live, fetches once, and freezes on its first read.
  const [livenessMissionId, setLivenessMissionId] = useState<string | null>(null);
  const { isLive, shouldPoll, endedByPresence } = useSessionLiveness(sessionId, livenessMissionId);
  // Presence, as the lifecycle's additive input: it holds this run open
  // against the staleness clock, never against a record that closed it. It
  // is a fact about NOW, so a parked playhead judges without it
  // (`judgementAt`, below: the rule the event log beside this page reads).
  const livePresence = useMemo<Presence>(() => (isLive ? new Set([sessionId]) : NO_PRESENCE), [isLive, sessionId]);
  const policy = useLifecyclePolicy();
  const relay = useRelayOrigin(sessionId);

  // (#2065) A static build has no `/flow-dispatch/<id>` to reach — the demo's
  // dispatch-row tap 404'd here. Read the committed file instead (the same
  // `queryKeys.staticFlowSrc` slot the playback lens and `useRouteRecords`
  // fill, so this is cache reuse) and slice this session out of it, shaped
  // like the daemon's response so nothing below has to know. The day as
  // INGESTED, not shaped: `/flow-dispatch` hands back no synthesized rows,
  // and `flowToRenderModel` synthesizes the per-session runtime telemetry
  // row itself — slicing the shaped day would add a second copy the daemon
  // path never has.
  const source = getSource();
  const flowSrc = source.flow;
  const query = useQuery({
    queryKey: queryKeys.flowSession(sessionId),
    queryFn: () => fetchJson<FlowRecordsResponse>(`/flow-dispatch/${encodeURIComponent(sessionId)}`),
    enabled: flowSrc === null,
    refetchInterval: shouldPoll ? PRESENCE_POLL_MS : false,
  });
  // (#2086) The static day comes from the one resolver (the shell already
  // holds it for the transport; same cache slot, no second download).
  const day = useDay(null);
  const staticSlice: NormRecord[] | null = useMemo(() => {
    if (flowSrc === null || day.ingested === null) return null;
    return day.ingested.filter((r) => r.session_id === sessionId);
  }, [flowSrc, day.ingested, sessionId]);
  const daemonSlice: NormRecord[] | null = useMemo(
    () => (query.data?.ok ? ingest(query.data.data.records) : null),
    [query.data],
  );
  const session: FetchResult<{ count: number }> | undefined =
    flowSrc === null
      ? query.data
      : staticSlice === null
        ? undefined
        : { ok: true, data: { count: staticSlice.length } };

  // (#2759) A run's OWN top-level session (the run-grain `run.start`/
  // `run.complete`/`mission.grow` trio a mission mints for itself)
  // carries no model telemetry — every turn, token and context record lives
  // on the mission's INNER role-execution sessions instead. This session's
  // OWN fetch can never see those; only a mission-wide fetch can. So: look
  // at what THIS session's own records already show, and only reach for the
  // wider set when they are silent — the common case (opening a specialist's
  // own dispatch directly) already has real telemetry and never pays for the
  // extra fetch.
  //
  // Checked before `flowToRenderModel`: `category` is a first-class wire
  // field on a real telemetry record, not something that pass invents (it
  // only fills in a DEFAULT when the field is absent) — so this reads
  // reliably before that pass runs.
  const ownRaw = flowSrc === null ? daemonSlice : staticSlice;
  const ownMissionId = useMemo(() => {
    if (!ownRaw) return null;
    if (missionId !== null) return missionId;
    const start = ownRaw.find((r) => r.session_id === sessionId && isBookendStart(r.action));
    return start?.mission_id ?? null;
  }, [ownRaw, sessionId, missionId]);
  useEffect(() => setLivenessMissionId(ownMissionId), [ownMissionId]);
  const ownHasTelemetry = useMemo(
    () => (ownRaw ? ownRaw.some((r) => r.session_id === sessionId && r.category === CATEGORY.Telemetry) : false),
    [ownRaw, sessionId],
  );
  const missionQuery = useQuery({
    queryKey: queryKeys.flowMission(ownMissionId ?? ""),
    queryFn: () => fetchJson<FlowRecordsResponse>(`/flow-mission/${encodeURIComponent(ownMissionId ?? "")}`),
    enabled: flowSrc === null && ownMissionId != null && !ownHasTelemetry,
    refetchInterval: shouldPoll ? PRESENCE_POLL_MS : false,
  });
  // `/flow-mission/<id>` holds every record carrying this mission's id,
  // including the run's own bookends, so it is not simply appended to
  // `ownRaw`: that would double-count this session's dispatch records, which
  // for a plain sum (TOKENS IN/OUT) is silently wrong. See `enrichedRaw`
  // below for what is kept from `ownRaw`.
  //
  // Static builds get the same enrichment from the day's own committed file
  // (below, `staticMissionSlice`) rather than this query, which never runs
  // there (`enabled: flowSrc === null`).
  const missionRaw = useMemo(() => (missionQuery.data?.ok ? ingest(missionQuery.data.data.records) : null), [missionQuery.data]);
  const staticMissionSlice = useMemo(() => {
    if (flowSrc === null || day.ingested === null || ownHasTelemetry || ownMissionId == null) return null;
    const recs = day.ingested.filter((r) => r.mission_id === ownMissionId);
    return recs.length ? recs : null;
  }, [flowSrc, day.ingested, ownHasTelemetry, ownMissionId]);
  // A union of the two, each record once. Neither side covers the other: the
  // session fetch carries host samples the daemon attaches by time window
  // (no mission_id), and the two queries refresh separately, so the run's
  // terminal record can reach the session fetch before the mission fetch has
  // it. Both are served from the same JSONL by the same daemon, so a record
  // present in both serializes identically.
  const missionSlice = missionRaw ?? staticMissionSlice;
  const enrichedRaw = useMemo(() => {
    if (!missionSlice || !ownRaw) return missionSlice ?? ownRaw;
    const seen = new Set(missionSlice.map((r) => JSON.stringify(r)));
    return [...missionSlice, ...ownRaw.filter((r) => !seen.has(JSON.stringify(r)))];
  }, [missionSlice, ownRaw]);

  // (#1972) HOISTED ABOVE EVERY EARLY RETURN, deliberately. React counts
  // hooks per render, so calling `useNowMs` after the loading/error/empty
  // guards below meant the first render called fewer hooks than the second —
  // "change in the order of Hooks", caught immediately by the existing suite
  // when this was first written the obvious way.
  //
  // `base` is the derivation against record time; `useNowMs` subscribes only
  // when that says the run is live, and `view` below re-derives against the
  // ticking clock. Two derivations per second while live, none when not —
  // `runRegions` is pure over a bounded record set, so the second pass costs
  // a fraction of a millisecond, and it is what makes the elapsed counter
  // advance during a STALL rather than freezing at the newest record's
  // timestamp.
  // (#2071) The shell's transport hands this lens the playhead it renders
  // at: the run's turns, tokens and status derive from the records up to
  // that instant, so scrubbing a run detail replays the run rather than
  // narrowing only the event log beside a finished stage. `null` (a live
  // daemon route, no transport) renders the whole slice as before.
  // (#2759) `enrichedRaw` is `ownRaw` (this session's own fetch) unless a
  // mission-wide fetch found MORE — see that computation's own doc above.
  const all = useMemo(() => (enrichedRaw ? sessionRouteRecords(enrichedRaw, sessionId, missionId) : enrichedRaw), [enrichedRaw, sessionId, missionId]);
  const records = all && playhead !== null ? recordsAsOf(all, playhead) : all;
  const data = records ? flowToRenderModel(records) : [];
  const hasRecords = !!records && records.length > 0;
  // Gated on PLAYBACK too, not just on the run's own liveness. A recorded
  // session that never emitted a terminal record still reads as `live`, and
  // in a static/playback build there is no wall clock it could sensibly
  // advance against — its elapsed time is a fact about when it was recorded.
  // Ticking there would also make the parity corpus non-deterministic, which
  // is this project's own clock rule: no fixture may mix a fixed timestamp
  // with a clock-relative assertion.
  // A run with no terminal record is not automatically LIVE. One that died in
  // January has no `dispatch.complete` either, and ticking its counter up to
  // now would read `17:51:54 so far` and climbing — abandonment rendered as
  // liveness. Whether it is still running is the run's lifecycle
  // (`lib/lifecycle.ts`) as of the page clock, the same answer the fleet
  // card and the timeline give: silent past the daemon's staleness window,
  // it has stopped with no ending recorded.
  //
  // (#2011) `endedByPresence` stops the clock sooner. Presence watching this
  // session disappear is direct evidence the run stopped, so the counter
  // should not climb toward the staleness window before it admits that. It
  // is deliberately NOT `!isLive` — a session presence never listed at all
  // (a replay, a January run, or a machine with Redis switched off, where
  // `/fleet/dispatches/live` returns an empty set for everything) is not
  // evidence of anything. Only the observed transition counts. It decides
  // the clock and the pulse (the activity axis), never the run's status.
  // (Playback parity, Change A) `clockNow` — `playhead ?? wallNow` — is the
  // ONE "now" every render-time derivation below reads, in both modes. This
  // used to be `Date.now()` unconditionally (finding #1's `quietMs`, and
  // the `ticking`/`base` split below): correct at the live edge, but a
  // replay probed mid-scrub compared a RECORDED instant against the
  // browser's real wall clock, which is what made a mid-generation replay
  // read "stale"/"no recent activity" — the run had gone quiet relative to
  // NOW, when the question is whether it was quiet as of the PLAYHEAD.
  // `wallNow` is a plain per-render `Date.now()` read (not a hook) — it
  // does not itself drive a re-render; see `ticking`/`useNowMs` below for
  // what does, at the live edge only.
  const wallNow = Date.now();
  const { asOf: clockNow, presence } = judgementAt(playhead, wallNow, livePresence);
  const pageRun = hasRecords ? sessionRun(data, sessionId, clockNow) : null;
  const plausiblyRunning =
    pageRun !== null && isRunning(lifecycleAt(pageRun, clockNow, policy, presence)) && !endedByPresence;
  // (#2757) `playhead === null` — a non-null playhead means the operator has
  // actively parked the shell's transport away from the live edge (`App.tsx`'s
  // `isPlayheadReady`: `transport.scrubbed && transport.t < transport.tMax`;
  // at the live edge `playhead` is `null`). This gate now decides ONLY
  // whether the shared 1s clock subscribes (a pure perf/re-render concern —
  // there is no reason to re-render every second while scrubbed, since the
  // transport itself re-renders this component on every tick it advances).
  // It no longer decides whether `runRegions` gets a moving "now" — that is
  // `clockNow` above, unconditionally, in both modes (Change A). Before this
  // split, the SAME boolean gated both, which is what made a scrubbed
  // replay's "so far" clock freeze between records (finding #2): `view`
  // below fell back to `base` — `runRegions` with NO override, i.e. the
  // newest CUT record's own timestamp — instead of the playhead, so the
  // reading only advanced when a new record happened to arrive.
  const ticking = plausiblyRunning && source.kind !== "static" && injectedPlaybackDate() == null && playhead === null;
  const nowMs = useNowMs(ticking);
  // (#2961) The playhead's clock, for REST's seconds hand.
  const playbackClock = usePlaybackClock();
  // (#2928) The live channel, on the same gate as the clock: the live edge
  // of a live route only, so a scrubbed or played-back run shows its
  // durable 2 s heartbeats and nothing else.
  const liveOverlay = useLiveOverlay(ticking);
  // The override actually fed to `runRegions`: the playhead when scrubbed
  // (unconditionally — a playhead means a replay, and a replay's clock is
  // never "no override", full stop); otherwise the ticking clock's own
  // snapshot while plausibly running, or the wall clock once it is not (so a
  // run gone silent reads as stopped, as it does on every other surface).
  // `undefined` is record time only (`runRegions` clamps to the newest
  // record): for a static or injected-date build, which has no wall clock to
  // judge a recording against, and (#2011) once presence saw the run go,
  // where the clock stops at the run's last sign of life.
  const frozenAtRecords = source.kind === "static" || injectedPlaybackDate() != null || endedByPresence;
  const clockOverride: number | undefined = playhead ?? (ticking ? nowMs : frozenAtRecords ? undefined : wallNow);

  if (!session) {
    return <SessionPendingHeader sessionId={sessionId} />;
  }

  if (!session.ok) {
    return (
      <div data-state="error" role="alert">
        <div className="stagehdr">session replay</div>
        <div className="none">
          couldn't reach /flow-dispatch/{sessionId}
          {session.status !== null ? ` (HTTP ${session.status})` : ""}: {session.message}
        </div>
      </div>
    );
  }

  const count = session.data.count;

  if (count === 0) {
    return (
      <div data-state="empty">
        <div className="stagehdr">session replay</div>
        <div className="none">no records found for session {sessionId}.</div>
      </div>
    );
  }


  // (#2071) The playhead can sit BEFORE this run's first record (rewind on
  // a day the run started partway into): the cut slice is empty, and the
  // header below would have nothing to read — measured as "the
  // dispatch lens stopped rendering" through the error boundary. Say what
  // is true instead: at this instant the run has not started.
  if (!hasRecords) {
    return (
      <div data-state="before-start" role="status" aria-label={`Session ${sessionId} not started yet`}>
        <div className="stagehdr">session replay</div>
        <div className="none">
          {sessionId} has not started yet at this point of the day{playhead !== null ? ` (${clkhm(playhead)})` : ""}. Scrub forward to see it.
        </div>
      </div>
    );
  }
  // (Playback parity, Change A) Run ONCE, with `clockOverride`, in both
  // modes — no more `ticking ? ... : base` branch selecting between a
  // moving clock and a frozen one keyed on live/playback.
  //
  // (#2886 pass 4, do-it — fresh-reviewer finding 6) A SCRUBBED view
  // (`playhead !== null`) is looking at a moment in the past, not the live
  // edge — the page's CURRENT connection status says nothing about whether
  // that past moment's heartbeats went quiet, so it must never drive a
  // scrubbed render's STALL/no-signal read. (An earlier version of this
  // comment claimed a scrubbed/finished view "has no `liveTokScope` to
  // affect either way" — wrong: a scrubbed view of a run that was STILL
  // RUNNING as of the playhead has one, and the live `connected` value was
  // leaking into it.) `effectiveConnected` is `true` whenever scrubbed,
  // regardless of the real live status, and the half-open evidence
  // (`lastContactMs`) is withheld the same way — it is equally a fact about
  // "right now", not about the playhead's moment.
  const effectiveConnected = connected || playhead !== null;
  const effectiveLastContactMs = playhead !== null ? null : lastContactMs;
  const view = runRegions(data, sessionId, clockOverride, effectiveConnected, effectiveLastContactMs, ticking ? liveOverlay : null, presence, policy, viewerUid);
  // `animate: plausiblyRunning`, not `ticking` — `ticking` is now purely the
  // "should the shared clock subscribe" perf gate (see its own doc above)
  // and is unconditionally `false` in playback (`playhead === null` fails
  // whenever scrubbed), which is exactly finding #1: the pill read "stale"
  // in playback regardless of whether the run was actually still going as
  // of the playhead. `plausiblyRunning` is computed from `clockNow` above,
  // so it answers the SAME question live and replayed.
  const liveness = livenessState({ done: view.ended, animate: plausiblyRunning, lastBeatMs: view.lastBeatMs, nowMs: clockNow });
  const scopeHero = modelScopeHero(view);

  return (
    <div data-state="data" className="session-run">
      <h2 className="session-run__header">
        {/* (#1972) Whitespace here is load-bearing: the parity golden compares
            `#stage` innerText byte-for-byte, and the pulse contributes NO text
            of its own. So exactly one space separates the pill from `RUN ·` —
            the `{" "}` below — and there must be none before `RUN`. The first
            version added a second and CI caught `RUNNING  RUN ·`, which is
            invisible on screen and unmissable to the golden. */}
        <WorkStatus
          status={view.header.status}
          label={view.header.pillLabel}
          live={liveness.state}
          className="pill"
          title={liveness.label}
        />{" "}
        {/* (#1974) No noun. This view's subject is ONE ROLE EXECUTION — one
            role, one model, its turns, tokens and signals. `RUN` was the one
            word contract 8 says it definitely is not: `run` is the umbrella
            over mission/dispatch/lab, never a grain. `STEP` would be wrong
            too, since a step contains 0..N role executions (a `dispatch.map`
            step holds one per item). `DISPATCH` names the run KIND, not what
            is on screen.
            The role already names the thing, so the noun is dropped rather
            than replaced with a differently-wrong one. */}
        {view.header.role}{" "}
        <span className="session-run__meta">
          ({view.header.sid} on {view.header.machineName}
          {askedOnSuffix(relay)})
        </span>
      </h2>

      {view.briefLines.length > 0 && (
        <div className="track brief-grid">
          {/* (#2000) A label+value PAIR is now one grid item (`.brief-pair`),
              so a pair can never straddle a column boundary regardless of how
              many columns `.brief-grid` resolves to — see `groupBriefEntries`'s
              own doc above for the mechanism this fixes. Each entry still
              renders as its own block element, in the same order and with the
              same text as before — `goldens/session-task-list.txt` (which
              reads `innerText`) is unaffected by an extra wrapper `<div>`. */}
          {groupBriefEntries(view.briefLines).map((group) =>
            group.kind === "pair" ? (
              <div key={group.key} className="brief-pair">
                <div className={`brief-${group.label.kind}`}>
                  <BriefEntryContent entry={group.label} />
                </div>
                <div className={`brief-${group.value.kind}`}>
                  <BriefEntryContent entry={group.value} />
                </div>
              </div>
            ) : (
              <div key={group.key} className={`brief-${group.entry.kind}`}>
                <BriefEntryContent entry={group.entry} />
              </div>
            ),
          )}
        </div>
      )}

      {view.disclosures.map((d) => (
        // (#1973) The payload the brief summarizes, reachable. `<details>`
        // rather than a JS toggle: it is keyboard-operable and
        // find-in-page-searchable for free, and the text is in the DOM whether
        // or not it is open — which is what the golden asserts, since an
        // assertion on the summary line alone would pass against the very bug
        // this fixes.
        <details className="disclosure" key={d.id} data-act={`disclose-${d.id}`}>
          <summary className="disclosure__sum">
            {d.label} · {d.chars} chars{d.truncated ? " · truncated" : ""}
          </summary>
          <pre className="disclosure__body">{d.text}</pre>
        </details>
      ))}

      {/* (#2863) Three sections, one header style: MODEL (its tiles, then
          which model did the work), SYSTEM (the machine around it), SIGNALS.
          The model row used to sit BELOW the system tiles, so the page read
          model, system, model again; it belongs with the model's numbers.
          Headers are CSS-generated from `data-head` (`.runsec::before`), so
          the parity goldens, which read innerText, never see them. The
          `.metrics[data-scope]` grids keep their scope attribute as a hook. */}
      {(view.metricScope.model.length > 0 || view.hasModelWork) && (
        <section className="runsec" data-head="model">
          {/* (#2890) ONE container for the whole MODEL section (hairline
              dividers, no per-metric cards): the scope is the hero on the
              left with its lamp row under it, the figures fill a collection
              grid on the right, and the loaded models sit below a hairline.
              Phone: scope on top, grid, models last (`styles.css`). A
              FINISHED run keeps the scope as the hero, calm and dimmed, with
              its average in the center, rather than a TOK/S tile. One
              derivation (`runRegions`), no live/playback branch. */}
          <div className="modelbox">
            {(view.metricScope.model.length > 0 || scopeHero) && (
              <div className="modelbox__main">
                {scopeHero && (
                  <div
                    className="modelbox__hero"
                    data-testid="run-token-scope"
                    // (#2928) A replayed, still-running instant is drawn from
                    // the durable heartbeats, one every 2 s; the live edge
                    // follows the live channel. Said on hover, never by
                    // changing the box.
                    title={!ticking && view.liveTokScope ? REPLAY_GRANULARITY_NOTE : undefined}
                  >
                    <TokenScope
                      state={scopeHero.state}
                      // A stale reading from the LAST generating stretch must
                      // not still drive the wave once the state has moved on.
                      tokensPerSec={scopeHero.tokensPerSec}
                      toolName={scopeHero.toolName}
                      toolWriting={scopeHero.toolWriting}
                      thinking={scopeHero.thinking}
                      utility={scopeHero.utility === true}
                      size="tile"
                      centerLabel={scopeHero.centerLabel}
                      centerUnit={scopeHero.centerUnit}
                      // (#2885) Dims the number when it's carried forward from
                      // an earlier turn rather than the current one's own two
                      // most recent heartbeats.
                      centerCarried={scopeHero.centerCarried}
                      // (#2961) REST's seconds hand follows the page clock:
                      // the playhead in playback (at the transport's speed
                      // while it plays, still while paused), the wall clock
                      // at a live edge, still when the view is frozen.
                      restEndMs={scopeHero.restEndMs}
                      clock={
                        playhead !== null
                          ? (playbackClock ?? { kind: "frozen", tMs: playhead })
                          : ticking
                            ? WALL_CLOCK
                            : { kind: "frozen", tMs: scopeHero.clockMs ?? clockNow }
                      }
                    />
                    {/* The lamps and, under them, a quiet readout line: "no
                        signal" when the page lost its connection (distinct
                        from a genuinely idle run, which has nothing wrong to
                        name), the finished average's qualifier when it is
                        partial or a fallback, or (#2926) the tool being
                        generated and its elapsed seconds. The readout takes
                        NO height of its own: it sits in the hero's bottom
                        padding (`.modelbox__status` in `styles.css`), so the
                        MODEL section is the same size in every state. */}
                    <div className="modelbox__status">
                      <ScopeLamps
                        reading={scopeHero.lamps}
                        noSignal={scopeHero.state === "nosignal"}
                        finished={scopeHero.state === "finished"}
                      />
                      <ScopeNote note={scopeHero.note} tool={scopeHero.noteTool} />
                    </div>
                  </div>
                )}
                {view.metricScope.model.length > 0 && (
                  <div className="metrics modelbox__figs" data-scope="model" role="group" aria-label="model metrics">
                    {view.metricScope.model.map((i) => view.metrics[i]).filter(Boolean).map((m, i) => (
                      <div className="met" key={i} title={m.hintTitle} data-subhint={m.sub ? undefined : m.hint}>
                        <div className="mv"><AnimatedMetricValue value={m.value} />{m.unit ? <span className="munit">{m.unit}</span> : null}</div>
                        <div className="ml" data-hint={m.hint}>{m.label}</div>
                        {m.bar && (
                          <div className="mbar" aria-hidden="true">
                            <i className="mbar__peak" style={{ width: `${m.bar.peakPct}%` }} />
                            <i className="mbar__now" style={{ width: `${m.bar.nowPct}%` }} />
                          </div>
                        )}
                        {m.sub && <div className="msub">{m.sub}</div>}
                      </div>
                    ))}
                  </div>
                )}
              </div>
            )}
            {view.showModelCard && (
              <div className="track modelbox__models">
                <div className="lbl">{view.modelTrackLabel}</div>
                {/* (#2863) One row per model, the one that ran first and marked:
                    the name, its size, and what it was here for. The text lines
                    remain for the cases with no per-model structure (an
                    endpoint, no telemetry yet). */}
                {view.modelEntries
                  ? view.modelEntries.map((m, i) => (
                      <div className={`modelrow${m.ran ? " modelrow--ran" : ""}`} key={i}>
                        <span className="modelrow__name">{m.name}</span>
                        <span className="modelrow__size">{m.gb != null ? `${m.gb} GB` : "?"}</span>
                        {m.ran != null && (
                          <span className={`modelrow__tag${m.ran ? " modelrow__tag--ran" : ""}`}>
                            {m.ran ? "ran this run" : "also loaded"}
                          </span>
                        )}
                      </div>
                    ))
                  : view.modelTrackLines.map((line, i) => <div key={i}>{line}</div>)}
              </div>
            )}
          </div>
        </section>
      )}

      {view.metricScope.system.length > 0 && (
        <section className="runsec" data-head="system">
          <div className="metrics" data-scope="system" role="group" aria-label="system metrics">
            {view.metricScope.system.map((i) => view.metrics[i]).filter(Boolean).map((m, i) => (
            <div className="met" key={i} title={m.hintTitle} data-subhint={m.sub ? undefined : m.hint}>
              <div className="mv"><AnimatedMetricValue value={m.value} />{m.unit ? <span className="munit">{m.unit}</span> : null}</div>
              <div className="ml" data-hint={m.hint}>{m.label}</div>
              {m.sub && <div className="msub">{m.sub}</div>}
            </div>
            ))}
          </div>
        </section>
      )}

      {/* (#1973) SIGNALS — grouped by kind, severity-coded, run-relative
          times. Was a flat list of grey strings with a `⚠` in front of every
          entry, including the ones that report a successful RECOVERY. */}
      <section className="runsec" data-head={view.signalsLabel} aria-label={view.signalsLabel}>
      <div className="track signals">
        {view.signalGroups.length === 0 ? (
          // (#2863) State as shape: the shared outcome pill, then one cell
          // per detector that looked and found nothing. The check marks are
          // CSS, so the text stays the detectors' names.
          <>
            <div className="sigclean">
              {/* Its own class, not `.pill`: `.pill` on this page means the run's
                  status in the header, and e2e specs address it that way. */}
              <WorkStatus status="complete" label="clean" className="sigpill" />
              <span className="sigclean__note">no detector flagged this run</span>
            </div>
            <div className="sigchecks">
              {CLEAN_DETECTORS.map((d) => {
                // (#2887 F2) `repetition` under a run-level policy of `off`
                // was never measured — rendering it as a plain checkmark
                // claims the detector looked and found nothing, which is a
                // different (and false) fact from "it didn't run".
                const off = d === "repetition" && view.repetitionOff;
                // (#2887 N2) `repetition` on a run whose records predate
                // FLOW_SCHEMA_VERSION 1.56.0 (or carry no `flow_schema` at
                // all) — the gate's own findings only started reaching the
                // flow stream at that version, so a clean-looking record
                // set here may simply be evidence the OLD forwarder
                // dropped. Same visual family as "(off)" (a dash, not a
                // checkmark — see `.sigcheck--off`), distinct wording: this
                // is "we don't know", not "it didn't run".
                const notRecorded = d === "repetition" && !off && !view.repetitionRecorded;
                const unmeasured = off || notRecorded;
                return (
                  <div className={`sigcheck${unmeasured ? " sigcheck--off" : ""}`} key={d}>
                    {off ? `${d} (off)` : notRecorded ? `${d} (not recorded)` : d}
                  </div>
                );
              })}
            </div>
          </>
        ) : (
          view.signalGroups.map((g) => (
            <div className={`signal signal--${g.severity}`} key={g.kind} data-severity={g.severity}>
              <div className="signal__head">
                {/* NOT `aria-hidden`. Severity was carried by this glyph, a
                    class and a `data-` attribute — the latter two invisible to
                    assistive tech — so hiding the glyph left a screen-reader
                    user no way at all to tell a struggle from a recovery,
                    which is the entire distinction this redesign exists to
                    draw. */}
                <span className="signal__glyph" role="img" aria-label={g.severity === "warn" ? "warning" : "recovered"}>
                  {g.severity === "warn" ? "⚠" : "✓"}
                </span>
                <span className="signal__kind">{g.kind}</span>
                {/* Count only when it IS a count. `×1` is noise on every row. */}
                {g.count > 1 && <span className="signal__count">×{g.count}</span>}
              </div>
              {g.signals.map((sig, i) => (
                <div className="signal__row" key={i}>
                  {sig.offsetLabel && <span className="signal__at">{sig.offsetLabel}</span>}
                  <span className="signal__detail">{sig.detail}</span>
                  {sig.fix ? <span className="signal__fix">fix: {sig.fix}</span> : null}
                </div>
              ))}
            </div>
          ))
        )}
      </div>
      </section>
    </div>
  );
}
