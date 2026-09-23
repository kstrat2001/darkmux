/**
 * A flow record rendered FOR A PERSON, not for a parser.
 *
 * The panel used to print `JSON.stringify(record, null, 2)`. That is the
 * operator's own doctrine inverted — "record exhaustively, display
 * selectively; defaults must answer a question, not enumerate a store" — and
 * it reads as thirty lines of syntax around six lines of meaning.
 *
 * Deliberately NOT per-action templates: the stream carries 26 distinct
 * actions and 26 templates is a maintenance surface nobody will keep true.
 * Instead the rules key off FIELD NAME PATTERNS and VALUE TYPES, which cover
 * every action with about eight rules and stay correct for actions that do
 * not exist yet.
 *
 * ## What the survey of 801 real records decided
 *
 * Roughly half the envelope never varies within a session, so rendering it at
 * the same weight as the meaningful fields is noise wearing signal's clothes:
 *
 *   machine_uid   1 distinct value  (and among the longest fields on screen)
 *   level/tier/stage/orchestrator/model/_type/version   <= 2 distinct
 *   ts 394 · session_id 26 · action 22 · handle 22 · source 11   <- signal
 *
 * So constants collapse behind one line, and the fields that actually differ
 * between two clicks get the space.
 *
 * Structure is also flat: depth 3, ~13 keys, and FIVE arrays across 2,794
 * records with a maximum length of two. A collapsible tree would be a
 * navigation tool for a structure that needs no navigating; the real
 * collapse problem is a single 46KB string in an otherwise 463-byte median
 * record, which is what `LongText` handles.
 */
import { useState } from "react";
import { escapeBidiControls } from "../lib/recordDetail";

/** Envelope fields measured as effectively constant within a session. Not a
 *  guess — see this module's header for the distinct-value counts. */
const CONSTANT_FIELDS = new Set([
  "level", "tier", "stage", "machine_uid", "orchestrator", "model",
  "_type", "version", "darkmux_version", "schema_version", "phase_id",
]);

/** Rendered as the headline rather than as rows. */
const HEADLINE_FIELDS = new Set(["action", "handle", "ts"]);

const MAX_INLINE = 160;

function grouped(n: number): string {
  return n.toLocaleString("en-US");
}

/** Middle-truncate. These ids share long PREFIXES
 *  (`crew-dispatch-coder-1786251936375019-0` vs `…-11db-0-step`), so cutting
 *  the tail removes exactly the part that distinguishes two of them. */
function midTruncate(s: string, max = 28): string {
  if (s.length <= max) return s;
  const keep = Math.floor((max - 1) / 2);
  return `${s.slice(0, keep)}…${s.slice(-keep)}`;
}

function relTime(iso: string): string | null {
  const t = Date.parse(iso);
  if (Number.isNaN(t)) return null;
  const s = Math.floor((Date.now() - t) / 1000);
  if (s < 0) return null;
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}

function clockOf(iso: string): string {
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? iso : d.toLocaleTimeString([], { hour12: false });
}

/** Below this, a `*_ms` NUMBER is a DURATION (milliseconds elapsed); at or
 * above it, it's an EPOCH millisecond TIMESTAMP — the field name doesn't
 * change, but what it measures flips from "how long" to "when". 1e12ms is
 * 2001-09-09, comfortably below any real record this panel renders and
 * comfortably above any real duration one carries (`sampler_cost_ms: 10`,
 * `wall_ms` in the tens of thousands) — so the split is unambiguous against
 * both shapes actually seen. Below the fix, `sampled_at_ms: 1790121627281`
 * (a `machine.telemetry` sample's own capture time) rendered as
 * `1790174634.5s` — the duration formula run on an epoch value. */
const EPOCH_MS_THRESHOLD = 1e12;

/** An epoch-ms `_ms` field, rendered in the SAME clock format the pane uses
 * for its own header time (`clockOf`, same `toLocaleTimeString([], {
 * hour12: false })` call), prefixed with the short date when the value
 * isn't today — same same-day rule `clkrange` (`lib/format.ts`) uses for
 * the fleet timeline. */
function clockOfEpochMs(ms: number): string {
  const d = new Date(ms);
  if (Number.isNaN(d.getTime())) return String(ms);
  const sameDay = d.toDateString() === new Date().toDateString();
  const time = d.toLocaleTimeString([], { hour12: false });
  if (sameDay) return time;
  return `${d.toLocaleDateString([], { month: "short", day: "numeric" })} ${time}`;
}

/** A long value that would otherwise dominate the panel. The 46KB outlier is
 *  precisely when the panel matters most, so it truncates rather than either
 *  flooding the column or hiding the content behind a click.
 *
 * (#2863 review, finding 6) A CHAR-count truncation alone is defeatable: a
 * short second line (a second command an operator never asked to run, an
 * injected instruction) sits well under `MAX_INLINE` and rendered in full
 * with no marker at all — `.rv__str` carries no `white-space: pre-wrap`, so
 * the newline collapses to a space and the second line visually disappears
 * into the first. Padding line 1 out past the cutoff does not change how
 * many LINES there are, so the marker counts lines, not just characters —
 * nothing a model-controlled string can pad away. */
function LongText({ text }: { text: string }) {
  const [open, setOpen] = useState(false);
  const nl = text.indexOf("\n");
  const multiline = nl !== -1;
  if (!multiline && text.length <= MAX_INLINE) return <span className="rv__str">{text}</span>;
  const firstLine = multiline ? text.slice(0, nl) : text;
  const collapsed = firstLine.length > MAX_INLINE ? firstLine.slice(0, MAX_INLINE) : firstLine;
  const lineCount = multiline ? text.split("\n").length : 1;
  const hiddenChars = text.length - collapsed.length;
  const label = multiline
    ? `+${grouped(lineCount - 1)} more line${lineCount - 1 === 1 ? "" : "s"}`
    : `+${grouped(hiddenChars)} more`;
  return (
    <span className="rv__str">
      {open ? <span style={{ whiteSpace: "pre-wrap" }}>{text}</span> : `${collapsed}…`}{" "}
      <button className="rv__more" onClick={() => setOpen(!open)}>
        {open ? "less" : label}
      </button>
    </span>
  );
}

/** The label for one element of an array-of-objects group — a meaningful
 *  key when the element carries one (`cpu_clusters[0].name === "Super"`),
 *  else the index. Checked in this order because `name` is the common case
 *  across the real telemetry payloads surveyed; `kind` covers the other
 *  shape seen elsewhere in the envelope (a discriminated union member). */
function arrayItemLabel(item: unknown, i: number): string {
  if (item && typeof item === "object" && !Array.isArray(item)) {
    const obj = item as Record<string, unknown>;
    for (const key of ["name", "kind"]) {
      const v = obj[key];
      if (typeof v === "string" && v) return v;
    }
  }
  return String(i);
}

/** Format by NAME PATTERN then TYPE — the rule set that replaces 26 schemas. */
function Value({ name, value }: { name: string; value: unknown }) {
  if (value === null || value === undefined) return <span className="rv__nil">—</span>;
  if (typeof value === "boolean") return <span className={`rv__bool${value ? " on" : ""}`}>{String(value)}</span>;

  // (real `machine.telemetry` payload, `cpu_clusters`) An array whose
  // members are themselves objects used to fall through to the generic
  // string branch below, and `String()` on an array of objects is
  // `"[object Object],[object Object]"` — Array.prototype.toString joining
  // each element's own (useless) Object.prototype.toString. The pane
  // already renders nested OBJECTS as their own group (`Group`, below this
  // function) — reused here, one sub-group per element, rather than a
  // second rendering scheme. An array whose members are all primitives is
  // UNCHANGED: it falls through to the same `String(value)` join the panel
  // has always used for it.
  if (Array.isArray(value)) {
    const hasObjectMember = value.some((v) => v !== null && typeof v === "object");
    if (hasObjectMember) {
      return (
        <div className="rv__group rv__arr">
          {value.map((item, i) => (
            <Group
              key={i}
              name={arrayItemLabel(item, i)}
              obj={(item && typeof item === "object" && !Array.isArray(item) ? item : { value: item }) as Record<string, unknown>}
            />
          ))}
        </div>
      );
    }
  }

  if (typeof value === "number") {
    if (/_ms$/.test(name)) {
      // (operator, 2026-09-23) `sampled_at_ms` is an EPOCH timestamp, not a
      // duration — see `EPOCH_MS_THRESHOLD`'s doc. Below it, unchanged.
      if (value >= EPOCH_MS_THRESHOLD) {
        return <span className="rv__time">{clockOfEpochMs(value)}</span>;
      }
      return <span className="rv__num">{value < 1000 ? `${value}ms` : `${(value / 1000).toFixed(1)}s`}</span>;
    }
    if (/tokens?$|_count$|^turn/.test(name)) return <span className="rv__num">{grouped(value)}</span>;
    return <span className="rv__num">{grouped(value)}</span>;
  }

  // (#2863 review, finding 7, security — Trojan-Source class) A raw field
  // value can be model-written (a tool's command/path, a result, a stray
  // top-level field on a hand-shaped record) and can carry bidi override or
  // zero-width control characters that reorder what this panel visually
  // shows. Escaped once, at the point every string value in this panel
  // funnels through, rather than per-branch below.
  const s = escapeBidiControls(String(value));
  if (/(^|_)ts$/.test(name)) {
    const rel = relTime(s);
    return <span className="rv__time">{clockOf(s)}{rel ? <span className="rv__dim"> · {rel}</span> : null}</span>;
  }
  if (/(_id|_uid)$/.test(name)) return <span className="rv__id" title={s}>{midTruncate(s)}</span>;
  if (/^(level|status|finish_reason|result|outcome)$/.test(name)) return <span className="rv__chip">{s}</span>;
  return <LongText text={s} />;
}

function Row({ name, value }: { name: string; value: unknown }) {
  return (
    <div className="rv__row">
      <span className="rv__key">{name.replace(/_/g, " ")}</span>
      <span className="rv__val"><Value name={name} value={value} /></span>
    </div>
  );
}

/** Nested objects (in practice: `payload`) render as their own group rather
 *  than as an indented brace block. Depth beyond this is 3% of records and
 *  falls through to the same generic treatment one level down. */
function Group({ name, obj }: { name: string; obj: Record<string, unknown> }) {
  return (
    <div className="rv__group">
      <div className="rv__grouphd">{name.replace(/_/g, " ")}</div>
      {Object.entries(obj).map(([k, v]) =>
        v && typeof v === "object" && !Array.isArray(v)
          ? <Group key={k} name={k} obj={v as Record<string, unknown>} />
          : <Row key={k} name={k} value={v} />,
      )}
    </div>
  );
}

export function RecordView({ record }: { record: Record<string, unknown> }) {
  const [showConstants, setShowConstants] = useState(false);
  const [showRaw, setShowRaw] = useState(false);

  const entries = Object.entries(record);
  const constants = entries.filter(([k]) => CONSTANT_FIELDS.has(k));
  // (operator, 2026-09-23) A top-level ARRAY field used to match neither
  // filter below — excluded from `context` by `typeof v === "object"`
  // (true for arrays too) and from `groups` by `!Array.isArray(v)` — so it
  // rendered nowhere at all, silently. `Array.isArray(v)` widens `context`
  // back to include it; `Row` -> `Value` now renders an array of either
  // shape correctly (see `Value`'s own array branch above).
  const context = entries.filter(
    ([k, v]) => !CONSTANT_FIELDS.has(k) && !HEADLINE_FIELDS.has(k) && (!(v && typeof v === "object") || Array.isArray(v)),
  );
  const groups = entries.filter(([k, v]) => !CONSTANT_FIELDS.has(k) && v && typeof v === "object" && !Array.isArray(v));

  const ts = typeof record.ts === "string" ? record.ts : null;

  return (
    <div className="rv">
      <div className="rv__head">
        <span className="rv__action">{String(record.action ?? "record")}</span>
        {record.handle ? <span className="rv__handle">{String(record.handle)}</span> : null}
      </div>
      {ts ? (
        <div className="rv__when">
          {clockOf(ts)}
          {relTime(ts) ? <span className="rv__dim"> · {relTime(ts)}</span> : null}
        </div>
      ) : null}

      {/* The payload is the CONTENT; the envelope above is packaging. It
          renders first among the detail so the answer precedes the metadata. */}
      {groups.map(([k, v]) => <Group key={k} name={k} obj={v as Record<string, unknown>} />)}

      {context.length ? (
        <div className="rv__ctx">
          {context.map(([k, v]) => <Row key={k} name={k} value={v} />)}
        </div>
      ) : null}

      {constants.length ? (
        <div className="rv__constants">
          <button className="rv__toggle" onClick={() => setShowConstants(!showConstants)}>
            {showConstants ? "hide" : `${constants.length} unchanging fields`}
          </button>
          {showConstants ? constants.map(([k, v]) => <Row key={k} name={k} value={v} />) : null}
        </div>
      ) : null}

      <button className="rv__toggle rv__rawtoggle" onClick={() => setShowRaw(!showRaw)}>
        {showRaw ? "hide raw" : "raw JSON"}
      </button>
      {/* (#2863 review round 2, finding 5) A SEPARATE render path from
          `Value()` above — `JSON.stringify` bypasses every field-level
          escape, so a bidi override anywhere in the record reached this
          view raw. Escaping the WHOLE serialized text is safe here: this
          is a debug dump for a person to read, not re-parsed JSON. */}
      {showRaw ? <pre className="eventlog__detailpre">{escapeBidiControls(JSON.stringify(record, null, 2))}</pre> : null}
    </div>
  );
}
