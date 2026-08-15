import { T } from "../lib/flow";
import { clk, relAgoFrom } from "../lib/format";
import { orchNotes } from "../lenses/fleet/hybridNote";
import type { FlowRecord } from "../types/handwritten";
import { Dialog } from "./Dialog";

/**
 * `openNotes()` (viewer.html:1606-1610) — every orchestrator note in the
 * current window, newest first. `data`/`nowMs` are the same
 * `flowWindow.data`/wall-clock `FleetLens` already has (`orchNotes` is the
 * SAME function `hybridNote.ts`'s `hasHistory` flag reads — one source, not
 * a second derivation); `relAgoFrom` matches legacy's `relAgo(t)` =
 * `relAgoFrom(state.t, t)`, `state.t` being "now" in live mode.
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
