import { MachineIcon } from "./MachineIcon";

/**
 * The per-activity glyphs the event log shows inline on each row so the
 * operator can scan "what is the model doing" without reading every label
 * (`ICON` + `ACT_ICON`, viewer.html:948-968).
 *
 * **Why this went missing and nothing caught it.** `MachineIcon`'s own doc
 * names the mechanism exactly: an inline SVG contributes no text to
 * `innerText`, so the parity goldens matched byte-for-byte with the icons
 * absent. Purely visual elements are structurally invisible to a
 * text-extraction harness — the same class of gap, found the same way, by
 * the operator looking at the screen. The tests below therefore assert on
 * the `data-act-icon` attribute rather than on rendered text.
 */
type IconKey =
  | "tool"
  | "brain"
  | "turn"
  | "play"
  | "flag"
  | "compress"
  | "route"
  | "note"
  | "pulse"
  | "machine"
  | "checkpoint";

const S = { fill: "none", stroke: "currentColor", strokeLinecap: "round", strokeLinejoin: "round" } as const;

const GLYPH: Record<IconKey, React.ReactNode> = {
  // Wrench — a tool call (the model reaching for its belt).
  tool: <path d="M14.7 6.3a4 4 0 0 0-5 5L4 17v3h3l5.7-5.7a4 4 0 0 0 5-5l-2.6 2.6-2.1-.5-.5-2.1z" />,
  // Brain — reasoning.
  brain: (
    <>
      <path d="M9.5 4A2.5 2.5 0 0 0 7 6.5 2.5 2.5 0 0 0 5 9a2.5 2.5 0 0 0 .5 4.9A2.5 2.5 0 0 0 8 18a2 2 0 0 0 4 0V4.5A2 2 0 0 0 9.5 4z" />
      <path d="M14.5 4A2.5 2.5 0 0 1 17 6.5 2.5 2.5 0 0 1 19 9a2.5 2.5 0 0 1-.5 4.9A2.5 2.5 0 0 1 16 18a2 2 0 0 1-4 0" />
    </>
  ),
  // Refresh/loop — a turn boundary.
  turn: <path d="M20 11a8 8 0 0 0-14-4.5L4 8M4 4v4h4M4 13a8 8 0 0 0 14 4.5L20 16M20 20v-4h-4" />,
  play: <path d="M7 5l12 7-12 7z" />,
  flag: <path d="M5 21V4M5 4h11l-2 4 2 4H5" />,
  compress: <path d="M8 4v4H4M16 4v4h4M8 20v-4H4M16 20v-4h4" />,
  route: (
    <>
      <circle cx="6" cy="19" r="2" />
      <circle cx="18" cy="5" r="2" />
      <circle cx="6" cy="5" r="2" />
      <path d="M6 7v4a4 4 0 0 0 4 4h6M6 17V7" />
    </>
  ),
  // Speech bubble — an orchestrator note.
  note: <path d="M20 15a2 2 0 0 1-2 2H8l-4 4V6a2 2 0 0 1 2-2h12a2 2 0 0 1 2 2z" />,
  // Pulse — telemetry (host / lms / runtime / tokens / detector).
  pulse: <path d="M3 12h4l2-6 4 12 2-6h6" />,
  // Milestone — a reasoning checkpoint (#1221). Newer than the legacy map,
  // which predates checkpoints entirely, so restoring parity alone would
  // leave the one activity a long crawl emits most often glyph-less: this
  // run produced ten of them.
  checkpoint: (
    <>
      <path d="M12 3v18" />
      <path d="M12 5h7l-1.6 2.5L19 10h-7z" />
    </>
  ),
  // Reuses the shared chip outline rather than declaring a second copy.
  machine: null,
};

/** Activity label (`activityOf`) → glyph key. viewer.html:968, verbatim —
 * anything unmapped renders no glyph, leaving a plain row. */
export const ACT_ICON: Record<string, IconKey> = {
  reasoning: "brain",
  "tool call": "tool",
  turn: "turn",
  heartbeat: "turn",
  "dispatch start": "play",
  "dispatch end": "flag",
  "dispatch error": "flag",
  feedback: "note",
  routing: "route",
  compaction: "compress",
  checkpoint: "checkpoint",
  note: "note",
  "machine online": "machine",
  "machine offline": "machine",
  detector: "pulse",
  runtime: "pulse",
  tokens: "pulse",
  lms: "pulse",
  "host telemetry": "pulse",
  telemetry: "pulse",
};

/** Renders the glyph for one activity label, or nothing when unmapped.
 * `data-act-icon` carries the glyph key so tests can assert on it — see the
 * module doc for why asserting on text cannot work here. */
export function ActivityIcon({ act }: { act: string }) {
  const key = ACT_ICON[act];
  if (!key) return null;
  return (
    <span className={`aico act-${act.replace(/ /g, "-")}`} title={act} data-act-icon={key} aria-hidden="true">
      {key === "machine" ? (
        <MachineIcon />
      ) : (
        <svg viewBox="0 0 24 24" strokeWidth={key === "brain" ? 1.7 : 1.8} {...S}>
          {GLYPH[key]}
        </svg>
      )}
    </span>
  );
}
