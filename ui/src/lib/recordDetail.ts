import type { FlowRecord } from "../types/handwritten";

/**
 * The event-log row's trailing preview (`renderLog()`'s `detail`,
 * viewer.html:2487-2503) — the part that says WHAT a record did, not merely
 * what kind of record it is.
 *
 * The port rendered `time · activity · machine · session` and stopped there,
 * so every tool call in the log read "tool call" with no name, no arguments
 * and no result size. The stream was never the problem: a `dispatch.tool`
 * record carries `tool_name`, the full `args`, `result_chars` and `ok`. For a
 * `create_finding` call those args ARE the finding — file, line, evidence and
 * reasoning — which is why a crawl's findings were invisible in a viewer that
 * was already receiving all of them.
 *
 * Kept as plain string-building rather than JSX so it can be unit-tested
 * directly. Escaping is React's job here; legacy had to `esc()` by hand
 * because it built HTML.
 */

const MAX_VALUE = 60;
const MAX_RAW = 80;

function clip(s: string, max: number): string {
  return s.length > max ? s.slice(0, max) + "…" : s;
}

/**
 * `prettyArgs()` (viewer.html:2454-2467) — flatten a tool call's JSON
 * arguments to `k=v k=v`, each value clipped, so a row shows the search
 * pattern / path / command itself.
 *
 * Non-JSON or non-object args fall back to the clipped raw string, which is
 * what keeps a truncated tool call (the per-call cap can cut one mid
 * arguments, producing `args_chars: 0`) legible rather than blank.
 */
export function prettyArgs(args: unknown): string {
  if (typeof args !== "string") return "";
  let parsed: unknown;
  try {
    parsed = JSON.parse(args);
  } catch {
    return clip(args, MAX_RAW);
  }
  if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
    return Object.entries(parsed as Record<string, unknown>)
      .map(([k, v]) => `${k}=${clip(typeof v === "string" ? v : JSON.stringify(v) ?? "", MAX_VALUE)}`)
      .join(" ");
  }
  return clip(args, MAX_RAW);
}

function firstLine(s: string, max: number): string {
  return clip(s.slice(0, max).replace(/\n/g, " "), max);
}

/** The row's trailing preview, or `""` when this record kind has none.
 * Branch-for-branch the legacy set. */
export function recordDetail(r: FlowRecord): string {
  const f = (r.fields || r.payload) as Record<string, unknown> | undefined;
  const a = r.action || "";

  if (a === "dispatch.reasoning" && typeof f?.reasoning_text === "string") {
    return `"${firstLine(f.reasoning_text, 60)}..."`;
  }
  if (a === "dispatch.tool" && f) {
    // `args` absent on pre-1.16 records — fall back to its size, so an old
    // record degrades to what it can say rather than rendering a bare arrow.
    const args = f.args != null ? prettyArgs(f.args) : `${f.args_chars ?? 0}ch`;
    // (#2008) Three outcomes, not two. A command that RAN and reported a
    // non-zero exit — a red test, a lint finding — is the tool working, and
    // marking it ❌ told the operator the instrument was broken. It shows its
    // exit code instead; only a tool that could not run gets the cross.
    //
    // `outcome` is absent on pre-1.22 records, where `ok === false` still
    // carried the old conflated meaning. Those degrade to the cross, which is
    // what they meant when written — the honest reading of an old record, not
    // a retroactive reinterpretation of it.
    let suffix = "";
    if (typeof f.outcome === "string") {
      if (f.outcome === "reported") suffix = ` exit ${f.exit_code ?? "?"}`;
      else if (f.outcome === "failed") suffix = " ❌";
    } else if (f.ok === false) {
      suffix = " ❌";
    }
    return `${String(f.tool_name ?? "")} ${args} → ${f.result_chars ?? 0}ch${suffix}`;
  }
  if (a === "dispatch.turn" && f) {
    return `turn ${f.turn_seq ?? 0} (${String(f.finish_reason ?? "")})`;
  }
  // `reasoning` is a tier-decision-only top-level field absent from
  // `FlowRecord`'s typed surface, so it is read defensively rather than added
  // to the type for one branch.
  const reasoning = (r as unknown as { reasoning?: unknown }).reasoning;
  if (a === "tier-decision" && typeof reasoning === "string") {
    return `"${firstLine(reasoning, 60)}..."`;
  }
  if (a === "dispatch.start" || a === "dispatch start") {
    return `start (prompt: ${f?.prompt_chars ?? 0}ch)`;
  }
  return "";
}

/** (#2863) What one event row shows: a kind chip, the object the event was
 * about, and (for a tool) how it came out. Everything else about the record
 * (arguments, output, full reasoning) belongs to the detail pane, so a row
 * says what happened in a line a person can scan.
 *
 * Built on the same fields `recordDetail` reads, with the same #2008
 * three-way outcome: a command that ran and reported a non-zero exit is the
 * tool working (`reported`), not a failure. */
export interface RecordObject {
  /** The chip: a tool's name, or "reasoning". Absent for other actions,
   * which keep their activity word. */
  chip?: string;
  kind?: "tool" | "think";
  text: string;
  /** Monospace: a command or pattern, where exact characters matter. */
  mono: boolean;
  outcome?: "ok" | "reported" | "failed";
}

const CONTAINER_ROOT = /^\/workspace\//;

/** `cd /workspace && npm test 2>&1` → `npm test`: the working-directory hop
 * and the stream redirect are how the runtime runs a command, not what the
 * command is. */
function commandText(cmd: string): string {
  return cmd
    // Only the runtime's own hop into the sandbox root. A `cd` the model
    // wrote itself is part of the command: `cd /tmp && rm -rf *` is not
    // `rm -rf *` (#2863 review).
    .replace(/^cd\s+\/workspace\/?\s*&&\s*/, "")
    .replace(/\s*2>&1\s*$/, "")
    .split("\n")[0]
    .trim();
}

/** The one field that names a tool call's object, read out of arguments
 * that did not parse: the host clips long arguments, so a large edit's are
 * cut mid-JSON (and may be double-encoded too).
 *
 * Only the call's OWN top-level keys count. A key nested inside a value (a
 * written file's content holding `"command": "rm -rf build"`) is not the
 * call's command (#2863 review). So: peel whole-document encoding layers (a
 * leading `"` means the arguments are a JSON string of JSON), then walk the
 * object tracking strings and depth, and read only complete `"key":"value"`
 * pairs at depth 1. Among the keys found, `keys` order decides. */
function fieldFromRaw(raw: string, keys: readonly string[]): { key: string; value: string } | null {
  let t = raw.trim();
  for (let i = 0; i < 3 && t.startsWith('"'); i++) {
    t = t.slice(1).replace(/"?…?$/, "").replace(/\\(["\\/])/g, "$1");
  }
  const found = new Map<string, string>();
  let depth = 0;
  let i = 0;
  // Reads a JSON string starting at `t[at] === '"'`; null if cut short.
  const readString = (at: number): { value: string; end: number } | null => {
    let v = "";
    for (let j = at + 1; j < t.length; j++) {
      const c = t[j];
      if (c === "\\") {
        if (j + 1 >= t.length) return null;
        v += t[j + 1];
        j++;
      } else if (c === '"') return { value: v, end: j + 1 };
      else v += c;
    }
    return null;
  };
  while (i < t.length) {
    const c = t[i];
    if (c === '"') {
      const k = readString(i);
      if (!k) break;
      i = k.end;
      // A string at depth 1 followed by `:` is one of the call's own keys.
      const rest = t.slice(i).match(/^\s*:\s*/);
      if (depth === 1 && rest) {
        i += rest[0].length;
        if (t[i] === '"') {
          const v = readString(i);
          if (!v) break;
          if (keys.includes(k.value) && !found.has(k.value)) found.set(k.value, v.value);
          i = v.end;
        }
      }
      continue;
    }
    if (c === "{" || c === "[") depth++;
    else if (c === "}" || c === "]") depth--;
    i++;
  }
  for (const key of keys) {
    const value = found.get(key);
    if (value !== undefined) return { key, value };
  }
  return null;
}

function unquote(s: string): string {
  const t = s.trim();
  return t.startsWith('"') ? t.replace(/^"+|"+$/g, "") : t;
}

export function recordObject(r: FlowRecord): RecordObject {
  const f = (r.fields || r.payload) as Record<string, unknown> | undefined;
  const a = r.action || "";

  if (a === "dispatch.tool" && f) {
    let args: Record<string, unknown> | null = null;
    if (typeof f.args === "string") {
      try {
        let parsed: unknown = JSON.parse(f.args);
        // Some tools' arguments arrive double-encoded, a JSON string of JSON
        // (measured on `edit`): one parse yields a string, so parse again.
        if (typeof parsed === "string") parsed = JSON.parse(parsed);
        if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) args = parsed as Record<string, unknown>;
      } catch {
        args = null;
      }
    }
    let text = "";
    let mono = false;
    if (args && typeof args.command === "string") {
      text = commandText(args.command);
      mono = true;
    } else if (args && typeof args.pattern === "string") {
      text = args.pattern;
      mono = true;
    } else if (args && typeof args.path === "string") {
      text = args.path.replace(CONTAINER_ROOT, "");
    } else {
      const raw = typeof f.args === "string" && !args ? fieldFromRaw(f.args, ["command", "pattern", "path"]) : null;
      if (raw?.key === "path") {
        text = raw.value.replace(CONTAINER_ROOT, "");
      } else if (raw) {
        text = raw.key === "command" ? commandText(raw.value) : raw.value;
        mono = true;
      } else {
        text = f.args != null ? prettyArgs(f.args) : `${f.args_chars ?? 0}ch`;
        mono = true;
      }
    }
    let outcome: RecordObject["outcome"];
    if (typeof f.outcome === "string") {
      outcome = f.outcome === "reported" ? "reported" : f.outcome === "failed" ? "failed" : "ok";
    } else if (f.ok === false) {
      outcome = "failed";
    } else if (f.ok === true) {
      outcome = "ok";
    }
    const o: RecordObject = { chip: String(f.tool_name ?? "tool"), kind: "tool", text, mono };
    if (outcome) o.outcome = outcome;
    return o;
  }
  if (a === "dispatch.reasoning" && typeof f?.reasoning_text === "string") {
    const first = unquote(f.reasoning_text).split("\n").find((l) => l.trim()) ?? "";
    return { chip: "reasoning", kind: "think", text: first.trim() || "(no reasoning text)", mono: false };
  }
  return { text: recordDetail(r), mono: false };
}
