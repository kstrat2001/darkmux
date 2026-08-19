import { T } from "../lib/flow";
import { clk, relAgoFrom } from "../lib/format";
import { orchNotes } from "../lenses/fleet/hybridNote";
import type { FlowRecord } from "../types/handwritten";
import { Dialog } from "./Dialog";

/**
 * `openNotes()` (viewer.html:1606-1610) — every orchestrator note in the
 * current window, newest first. `data` is `FleetLens`'s `scopedData`
 * (#1869: filtered to `ts <= playhead`, not the raw `flowWindow.data` —
 * see `savings.ts`'s module doc) — `orchNotes` is the SAME function
 * `hybridNote.ts`'s `hasHistory` flag reads, one source, not a second
 * derivation, so the notes history dialog can never disagree with which
 * note the hero itself picked.
 *
 * `nowMs` is real wall-clock `Date.now()`, passed through to `relAgoFrom`
 * for the "Ns ago" readout. Legacy's own `relAgo(t)` is `relAgoFrom(state.t,
 * t)` — relative to the PLAYHEAD, not the clock, so a scrubbed replay's
 * notes read "3m ago" relative to where the transport sits, not "3 days
 * ago" relative to real now. This port still reads wall-clock here (a
 * pre-existing divergence from before #1869, not introduced by it, and
 * orthogonal to the acceptance criteria that packet's transport had to
 * meet — the notes-history dialog's relative-age readout on a replay is a
 * follow-up, not fixed here).
 */
export function NotesDialog({ data, nowMs }: { data: FlowRecord[]; nowMs: number }) {
  const notes = orchNotes(data).slice().reverse();
  return (
    <Dialog id="nmodalbg" titleId="notes-title" title="orchestrator notes" wide>
      <div id="notesbody">
        {notes.length ? (
          notes.map((n, i) => <NoteRow key={i} note={n} nowMs={nowMs} />)
        ) : (
          <div className="dialog__none">no orchestrator notes in this window</div>
        )}
      </div>
    </Dialog>
  );
}

function NoteRow({ note, nowMs }: { note: FlowRecord; nowMs: number }) {
  const t = T(note.ts);
  return (
    <div className="dialog__nrow">
      <div className="dialog__nts">
        {clk(t)} · {relAgoFrom(nowMs, t)}
      </div>
      <div className="dialog__ntext">{note.handle ?? ""}</div>
    </div>
  );
}
