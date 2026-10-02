/**
 * One machine card of the fleet lens: the name row, the hardware line, the
 * status line (a lamp and a word, or the running execution's reading), the
 * count line, the serves line and the tube. A card shows only facts about its
 * own machine, so the fleet reads the same from any serving machine.
 */
import type { KeyboardEvent, MouseEvent } from "react";
import { scopeCenter } from "../../lib/scopeCenter";
import { HubBadge } from "../../components/HubBadge";
import { MachineIcon } from "../../components/MachineIcon";
import { TokenScope } from "../../components/TokenScope";
import { UtilityGlyph } from "../../components/UtilityGlyph";
import { ServesLine } from "../../components/ServesLine";
import { LampDot } from "../../components/LampDot";
import { LampForm } from "../../lib/lamp";
import { WALL_CLOCK } from "../../lib/restHand";
import type { PageClock } from "../../lib/restHand";
import { REPLAY_GRANULARITY_NOTE } from "../../lib/liveChannel";
import { scopeStateOf } from "../../lib/scopeMorph";
import { dispatchHash } from "../../lib/route";
import { onIntentClick } from "../../lib/clickIntent";
import type { ExecutionTokenReading, LiveState } from "../../lib/tokenRate";
import { executionCountText, specDimLabel, type CardFace, type FleetCard } from "./cards";
import { CardStatus, STATUS_WORD, lampOf, secondLineOf, statusReason } from "./cardStatus";
import { readingLabel, readingTitle } from "./readingLabel";

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
function machineDrillHash(machineKey: string): string {
  // (#2929) A machine KEY (`lib/machineKey.ts`), never the hardware uid: the
  // address bar lands in screenshots and shared links.
  return `lens=runs&machine=${encodeURIComponent(machineKey)}`;
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
function machineRunsHash(machineKey: string, runningSessionIds: string[]): string | null {
  if (runningSessionIds.length === 0) return null;
  if (runningSessionIds.length === 1) return dispatchHash(runningSessionIds[0], null);
  return machineDrillHash(machineKey);
}

/** The props of a card's tube wrapper. With 2+ executions the tube itself is
 * the control (W3): a tap or Enter / Space shows the next one, wrapping. It
 * keeps focus (the element is never remounted), and `onCycle` stops the event
 * so the tap does not also open the machine, as the running count's does
 * (#1903). Otherwise it is decoration, with the replay note on hover (#2928:
 * replay has only the 2 s heartbeats). */
function tubeProps(f: {
  paged: boolean;
  position: number;
  count: number;
  role: string | null;
  onCycle: (e: { stopPropagation: () => void }) => void;
  replayNote: string | undefined;
}) {
  if (!f.paged) return { title: f.replayNote };
  return {
    role: "button",
    tabIndex: 0,
    "aria-label": `execution ${f.position} of ${f.count}${f.role ? `, ${f.role}` : ""}: show the next one`,
    title: f.role ? `${f.role} · tap for the next execution` : "tap for the next execution",
    onClick: f.onCycle,
    onKeyDown: (e: KeyboardEvent<HTMLDivElement>) => {
      if (e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        f.onCycle(e);
      }
    },
  };
}

/** The card's name row: the machine icon, its name, what its own card
 * declares (HUB) and the utility strip. One row, so a badge never changes
 * the card's height. What the machine serves has its own line under the count.
 *
 * A fleet card shows only facts about its own machine. A relationship with
 * the machine serving the viewer (a grant, a radio permission) is not a card
 * fact: the same fleet must read the same from any server. Relationships live
 * in the console, which runs commands on the serving machine. */
function CardNameRow({ card, utilityQuietKnown }: { card: FleetCard; utilityQuietKnown: boolean }) {
  return (
    <div className="name">
      <span className="mico">
        <MachineIcon />
      </span>
      {/* (#2890) Its own box, so a long name ellipsizes beside the tube (a
          flex row's bare text cannot); the full name is the tooltip. */}
      <span className="mach-name" title={card.name}>
        {card.name}
      </span>
      {/* (#3022) What the machine's own card declares. */}
      <HubBadge declared={card.hub} />
      {/* (#2915) The utility strip: a fixed box at the end of the name row,
          always present, so a job starting or ending never changes the
          card's layout. See `UtilityGlyph`. */}
      <UtilityGlyph strip={card.utility} noSignal={!utilityQuietKnown} />
    </div>
  );
}

/** The card's subtitle line: the hardware; when no hardware is known,
 * "hardware not reported" in the dim style. Never a status. One line, so the
 * card keeps its height. */
function CardSpec({ card }: { card: FleetCard }) {
  return (
    <div className="spec" title={card.spec || undefined}>
      {card.spec ? (
        card.spec
      ) : (
        <span className="specdim">{specDimLabel(card)}</span>
      )}
    </div>
  );
}


/** The status line. With a reading it IS the reading (a lit lamp and "42 tok/s"
 * take the word's place, so the card keeps one height in every state);
 * otherwise a lamp and the word, whose reason is the tooltip. */
function StatusLine({
  status,
  note,
  reading,
}: {
  status: CardStatus;
  note: string | null;
  reading: { exec: ExecutionTokenReading; state: LiveState } | null;
}) {
  if (reading) {
    const { exec, state } = reading;
    const label = readingLabel(exec, state);
    return (
      <div
        className="stat mach-scope__rate"
        data-tone={state}
        data-carried={exec.carried ? "true" : "false"}
        data-thinking={state === "generating" && exec.thinking === true ? "true" : undefined}
        title={readingTitle(exec, state)}
      >
        <LampDot form={LampForm.Filled} />
        {label.kind === "text" ? (
          label.text
        ) : (
          <>
            <span className="mach-scope__why mach-scope__why--full">{label.full}</span>
            <span className="mach-scope__why mach-scope__why--word">{label.word}</span>
          </>
        )}
      </div>
    );
  }
  return (
    <div className="stat" data-lamp={lampOf(status)} title={statusReason(note, status)}>
      <LampDot form={lampOf(status)} />
      {STATUS_WORD[status]}
    </div>
  );
}

/** The count line: "N running" (a link to the run while something runs),
 * "2 running · 1/2" with several executions, or "—" / "not streaming" while
 * there is no count to claim. */
function CountLine({
  card,
  face,
  status,
  machineKey,
  executions,
  position,
}: {
  card: FleetCard;
  face: CardFace;
  status: CardStatus;
  machineKey: string;
  executions: number;
  position: number;
}) {
  const secondLine = secondLineOf(status);
  if (secondLine !== null) return <div className="runs runs--quiet">{secondLine}</div>;
  if (!face.countShown) return <div className="runs runs--quiet">—</div>;
  const runsHash = machineRunsHash(machineKey, card.runningSessionIds);
  const countText = executionCountText({ runsCount: card.runsCount, executions, position });
  if (!runsHash) return <div className="runs">{countText}</div>;
  const activate = (e: { stopPropagation: () => void }) => {
    e.stopPropagation();
    location.hash = runsHash;
  };
  // The selection guard must not swallow the stopPropagation above, or the
  // outer card would still drill on a selection.
  const onCountClick = (e: MouseEvent<HTMLDivElement>) => {
    e.stopPropagation();
    onIntentClick(() => activate(e))(e);
  };
  return (
    <div
      className="runs runs--live"
      role="button"
      tabIndex={0}
      aria-label={`open the ${card.runsCount} running ${card.runsCount === 1 ? "dispatch" : "dispatches"} on ${card.name}`}
      onClick={onCountClick}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          activate(e);
        }
      }}
    >
      {countText}
    </div>
  );
}

/** The tube: a live execution's scope, the idle ring, the powered-off screen of
 * an offline machine, or no-signal static. Every card carries the tube's box,
 * so no card changes size when its data arrives or its machine comes and goes. */
function CardTube({
  card,
  face,
  selected,
  position,
  count,
  onCycle,
  clock,
}: {
  card: FleetCard;
  face: CardFace;
  selected: ExecutionTokenReading | null;
  position: number;
  count: number;
  onCycle: (e: { stopPropagation: () => void }) => void;
  clock: CardClock;
}) {
  if (face.tube === "reading" && selected) {
    const scopeState = scopeStateOf({ state: selected.state, noSignal: selected.state === null });
    return (
      <div
        className="mach-scope"
        data-testid="fleet-token-scope"
        {...tubeProps({
          paged: count >= 2,
          position,
          count,
          role: selected.role,
          onCycle,
          replayNote: clock.playhead != null || !clock.livePolling ? REPLAY_GRANULARITY_NOTE : undefined,
        })}
      >
        <TokenScope
          // A stale rate from the last generating stretch must not still drive
          // the wave once the state has moved on. The RAW nullable reading: a
          // `null` is "not yet measured", never coerced into a confident zero.
          tokensPerSec={selected.state === "generating" ? selected.tokensPerSec : 0}
          state={scopeState}
          toolName={selected.toolName}
          toolWriting={selected.writing === true}
          thinking={selected.state === "generating" && selected.thinking === true}
          {...scopeCenter({
            state: scopeState,
            tokensPerSec: selected.tokensPerSec,
            carried: selected.carried,
            restSecondsLeft: selected.restSecondsLeft,
            writing: selected.writing === true,
            writingSeconds: selected.writingSeconds,
            thinking: selected.thinking === true,
            compacting: selected.compacting === true,
          })}
          // (#2961) REST's seconds hand follows the page clock.
          restEndMs={selected.state === "rest" ? selected.restEndMs : undefined}
          clock={clock.playhead != null ? (clock.playbackClock ?? { kind: "frozen", tMs: clock.playheadT }) : WALL_CLOCK}
          size="card"
        />
      </div>
    );
  }
  if (face.tube === "nosignal") {
    return (
      <div className="mach-scope" data-testid="fleet-token-scope">
        <TokenScope tokensPerSec={0} state="nosignal" size="card" {...scopeCenter({ state: "nosignal", tokensPerSec: 0 })} />
      </div>
    );
  }
  if (face.tube === "off") {
    return (
      <div className="mach-scope" data-testid="fleet-token-scope">
        {/* Plain markup, not a `TokenScope`, so an offline card runs no canvas at all. */}
        <div className="token-scope-bezel token-scope-bezel--card" data-state="off" aria-hidden="true">
          <div className="token-scope-screen" />
        </div>
      </div>
    );
  }
  if (face.tube === "idle") {
    return (
      <div className="mach-scope" data-testid="fleet-token-scope">
        {/* (#2911) A card reading "dispatch in flight" says "no model working"
            in the tube, not "idle": the two words contradicted each other. */}
        <TokenScope tokensPerSec={0} state="idle" size="card" {...scopeCenter({ state: "idle", tokensPerSec: 0, inFlight: card.active })} />
      </div>
    );
  }
  return null;
}

/** What the tube needs of the page's clock. */
export interface CardClock {
  playhead: number | null;
  livePolling: boolean;
  playbackClock: PageClock | null;
  playheadT: number;
}

/** One card. `selectedIdx` is the execution the tube shows (-1 when none);
 * the fleet lens owns the pager's pick, this only draws it. */
export function MachineCard({
  card,
  face,
  selectedIdx,
  machineKey,
  clock,
  onShowExecution,
}: {
  card: FleetCard;
  face: CardFace;
  selectedIdx: number;
  /** The card's machine as the address bar names it (`lib/machineKey.ts`). */
  machineKey: string;
  clock: CardClock;
  onShowExecution: (uid: string, sessionId: string) => void;
}) {
  const execs = card.executions;
  const selected = selectedIdx >= 0 ? execs[selectedIdx] : null;
  // (#2955 review) The shown execution has no live state: the page lost the
  // daemon. The line says "disconnected", the tube shows static, and the
  // card stays active (its machine IS running).
  const disconnected = face.tube === "reading" && selected != null && selected.state === null;
  const status = disconnected ? CardStatus.Disconnected : face.status;
  const liveState = face.tube === "reading" && selected ? selected.state : null;
  const reading = selected && liveState ? { exec: selected, state: liveState } : null;
  // (W3) A tap on the tube shows the next execution, wrapping.
  const cycle = (e: { stopPropagation: () => void }) => {
    e.stopPropagation();
    if (execs.length < 2 || selectedIdx < 0) return;
    onShowExecution(card.uid, execs[(selectedIdx + 1) % execs.length].sessionId);
  };
  const drill = () => {
    location.hash = machineDrillHash(machineKey);
  };
  const cls = ["mach", face.active ? "active" : "", status === CardStatus.Offline ? "absent" : "", face.noSignal || disconnected ? "nosignal" : ""]
    .filter(Boolean)
    .join(" ");
  return (
    // Every card drills to the runs lens pinned to that machine (see
    // `machineDrillHash`). `data-act`/`data-arg` are DOM inspection hooks for
    // the e2e specs; the click goes through `onClick`. The explicit
    // `aria-label` keeps the card's accessible name deterministic, since the
    // nested running-count button (its own tap target, #1903) would otherwise
    // fold its name in.
    <div
      data-flip-key={card.uid}
      className={cls}
      data-act="machine"
      data-arg={machineKey}
      role="button"
      tabIndex={0}
      aria-label={card.name}
      onClick={onIntentClick(drill)}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          drill();
        }
      }}
    >
      <CardNameRow card={card} utilityQuietKnown={face.utilityQuietKnown} />
      <CardSpec card={card} />
      <div className="mach-body mach-body--scope">
        <StatusLine status={status} note={card.note} reading={reading} />
        <CountLine card={card} face={face} status={status} machineKey={machineKey} executions={execs.length} position={selectedIdx + 1} />
        <ServesLine machine={card.name} profiles={card.servesProfiles} radio={card.servesRadio} />
        <CardTube card={card} face={face} selected={selected} position={selectedIdx + 1} count={execs.length} onCycle={cycle} clock={clock} />
      </div>
    </div>
  );
}
