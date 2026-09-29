import { compactDuration } from "./format";
import { ACTION, type NormAction, type NormRecord } from "./ingest";

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
export function recordDetail(r: NormRecord): string {
  // (#2863 review round 2, finding 5) Escaped at the SOURCE, not only at
  // `recordObject()`'s fallback call site — `EventLogColumn.tsx`'s pushed-
  // detail strip calls this function DIRECTLY for its preview text, a
  // second render path that never went through `recordObject()` at all.
  return escapeBidiControls(recordDetailRaw(r));
}

/** The budget records (#2902 step 5), which name their own subject in a row. */
const BUDGET_ACTIONS: ReadonlySet<NormAction> = new Set<NormAction>([ACTION.BudgetWarn, ACTION.BudgetWait, ACTION.BudgetResume, ACTION.BudgetStop]);

function recordDetailRaw(r: NormRecord): string {
  const f = (r.fields || r.payload) as Record<string, unknown> | undefined;
  const a = r.action;

  if (a === ACTION.DispatchReasoning && typeof f?.reasoning_text === "string") {
    return `"${firstLine(f.reasoning_text, 60)}..."`;
  }
  if (a === ACTION.DispatchTool && f) {
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
  if (a === ACTION.DispatchTurn && f) {
    return `turn ${f.turn_seq ?? 0} (${String(f.finish_reason ?? "")})`;
  }
  // `reasoning` is a tier-decision-only top-level field absent from
  // `NormRecord`'s typed surface, so it is read defensively rather than added
  // to the type for one branch.
  const reasoning = (r as unknown as { reasoning?: unknown }).reasoning;
  if (a === ACTION.TierDecision && typeof reasoning === "string") {
    return `"${firstLine(reasoning, 60)}..."`;
  }
  if (a === ACTION.DispatchStart) {
    return `start (prompt: ${f?.prompt_chars ?? 0}ch)`;
  }
  // (#2902 step 5) A budget record says in the row itself what it is about
  // and, for a wait, how long: the run page is one of the three places a
  // wait must say so (with the CLI and `mission status`).
  if (a !== undefined && BUDGET_ACTIONS.has(a) && f) {
    const subject = String(f.endpoint_id ?? f.step ?? "budget");
    if (a === ACTION.BudgetWait) {
      return typeof f.wait_ms === "number" ? `${subject}: waiting ${spanWords(f.wait_ms / 1000)}` : `${subject}: waiting`;
    }
    if (a === ACTION.BudgetStop) {
      return typeof f.reason === "string" ? `${subject}: wait stopped (${f.reason})` : `${subject}: wait stopped`;
    }
    if (a === ACTION.BudgetResume && typeof f.waited_ms === "number") {
      return `${subject}: resumed after ${spanWords(f.waited_ms / 1000)}`;
    }
    if (a === ACTION.BudgetWarn && typeof f.spent === "number" && typeof f.limit === "number") {
      const per = typeof f.period === "string" ? ` per ${f.period}` : "";
      return `${subject}: ${f.spent}/${f.limit} ${String(f.metric ?? "tokens")}${per}`;
    }
  }
  return "";
}

/** `45s`, `14m`, `1h 4m`: a span in words, never clock-shaped (a wait of
 * `14:03` would read as a time of day). */
const spanWords = compactDuration;

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

/** (#2863 review round 2, finding 9, security) Splits ONE command line on
 * top-level shell separators (`;`, `&&`, `||`, `|`) — quote-aware (a
 * separator inside `'...'` or `"..."` is part of the string, not a real
 * separator; a `\"` inside a double-quoted string does not end it) but
 * deliberately not a full shell parser: no backslash handling outside
 * double quotes, no subshells, no here-docs, no `$(...)`/backtick
 * awareness. Good enough to stop `echo <200 chars of padding>; rm -rf
 * /workspace` from hiding its second command behind the row's own
 * fixed-width CSS ellipsis, which nothing in this function can see coming
 * (there is no length at which a row IS or ISN'T going to be truncated —
 * that is a runtime layout fact, not a string fact) — so the split (and
 * the marker built from it) is unconditional rather than gated on length. */
function splitTopLevelCommands(line: string): string[] {
  const segments: string[] = [];
  let cur = "";
  let quote: '"' | "'" | null = null;
  let i = 0;
  while (i < line.length) {
    const c = line[i];
    if (quote) {
      if (quote === '"' && c === "\\" && i + 1 < line.length) {
        cur += c + line[i + 1];
        i += 2;
        continue;
      }
      cur += c;
      if (c === quote) quote = null;
      i++;
      continue;
    }
    if (c === '"' || c === "'") {
      quote = c;
      cur += c;
      i++;
      continue;
    }
    if (c === ";") {
      segments.push(cur);
      cur = "";
      i++;
      continue;
    }
    if ((c === "&" && line[i + 1] === "&") || (c === "|" && line[i + 1] === "|")) {
      segments.push(cur);
      cur = "";
      i += 2;
      continue;
    }
    if (c === "|") {
      segments.push(cur);
      cur = "";
      i++;
      continue;
    }
    cur += c;
    i++;
  }
  segments.push(cur);
  return segments.map((s) => s.trim()).filter((s) => s.length > 0);
}

/** `cd /workspace && npm test 2>&1` → `npm test`: the working-directory hop
 * and the stream redirect are how the runtime runs a command, not what the
 * command is. */
function commandText(cmd: string): string {
  const stripped = cmd
    // Only the runtime's own hop into the sandbox root. A `cd` the model
    // wrote itself is part of the command: `cd /tmp && rm -rf *` is not
    // `rm -rf *` (#2863 review).
    .replace(/^cd\s+\/workspace\/?\s*&&\s*/, "")
    .replace(/\s*2>&1\s*$/, "");
  const lines = stripped.split("\n");
  const first = lines[0].trim();
  // (#2863 review round 3) A trailing marker ("cmd ⏎ +1") only ever showed
  // the FIRST unit (line or top-level command) — readable, but it hid the
  // rest of a compound command that would otherwise have fit on the row.
  // Moved to a PREFIX instead: the FULL first line is kept (compound
  // separators and all — `cd X && Y` shows both halves, same as before
  // #2863 review round 2 ever touched it), with the marker at position 0.
  //
  // (security) This is also why the prefix form, not just a cosmetic
  // move: `.eventlog__recobj`'s CSS `text-overflow: ellipsis` always cuts
  // from the visual END of the row. A trailing marker after a long first
  // unit could itself be the thing that gets cut off — the one case the
  // marker exists to cover. A marker at position 0 cannot be pushed
  // off-screen by anything that comes after it.
  const segments = splitTopLevelCommands(first);
  const markers: string[] = [];
  if (segments.length > 1) markers.push(`⛓+${segments.length - 1}`);
  // (#2863 review, finding 6, security) A command with more than one line
  // used to show only the first, no different from a genuinely single-line
  // one — `echo ok` and `echo ok\nrm -rf /workspace` were indistinguishable.
  // Counts LINES, not characters, so padding line 1 cannot defeat it.
  if (lines.length > 1) markers.push(`⏎+${lines.length - 1}`);
  return markers.length ? `${markers.join(" ")} ${first}` : first;
}

/** (#2863 review, finding 5) A `write` call's `content` can be long enough
 * that the host's per-call args cap is exhausted before the raw args string
 * ever reaches `path` (`content` precedes `path` in the call's own key
 * order) — `fieldFromRaw` cannot find a key that was never included in what
 * it was given. The runtime's own result names the path it wrote
 * (`runtime/src/tools/mod.rs`'s `write`: `"Wrote {n} bytes to {path}"`), so
 * that is read as the fallback before showing raw JSON.
 *
 * (#2863 review round 2, finding 7) Only called for a `write` tool call
 * (see the caller below) — this phrase is specific to `write`'s own result
 * text; an unrelated tool whose OWN result happened to contain it (an
 * `echo` printing that exact string, a log line) must not have its row
 * renamed after a file it never touched. Captures to the END OF THE LINE,
 * not `\S+`'s first word: the runtime's message has nothing after the path
 * (`format!("Wrote {} bytes to {}", content.len(), path.display())`), and
 * `\S+` truncated "my notes.md" to "my". */
function pathFromResult(result: string): string | null {
  const m = result.match(/^Wrote \d+ bytes to (.+)$/);
  return m ? m[1] : null;
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

/** (#2863 review, finding 7, security — Trojan-Source class) A model-written
 * command, pattern, path or reasoning string can carry bidi override
 * (U+202A–U+202E, U+2066–U+2069), zero-width (U+200B–U+200F), or one of
 * three MORE invisible/directional control points found in round 2's
 * review: ARABIC LETTER MARK (U+061C, an implicit-directional signal — same
 * risk class as the explicit overrides), WORD JOINER (U+2060), and ZERO
 * WIDTH NO-BREAK SPACE (U+FEFF, the BOM character — invisible mid-string).
 * Rendered raw, these REORDER or hide what a row visually displays without
 * changing what actually runs — the same technique CVE-class as Trojan
 * Source. Each one is replaced with a visible, unambiguous escape naming
 * its own code point, so the row shows what is actually there. Written with
 * explicit `\u` escapes, not literal characters, so the source itself never
 * embeds an invisible/directional control point. */
const BIDI_CONTROL = /[\u061C\u200B-\u200F\u202A-\u202E\u2060\u2066-\u2069\uFEFF]/g;
export function escapeBidiControls(s: string): string {
  return s.replace(BIDI_CONTROL, (ch) => `⟨U+${ch.codePointAt(0)!.toString(16).toUpperCase().padStart(4, "0")}⟩`);
}

function unquote(s: string): string {
  const t = s.trim();
  return t.startsWith('"') ? t.replace(/^"+|"+$/g, "") : t;
}

/** (#2008) A `dispatch.tool` payload's three-way outcome: `reported` is a
 *  command that ran and exited non-zero (the tool working), `failed` is a
 *  tool that could not run. `outcome` is absent on pre-1.22 records, where
 *  `ok === false` still carried the old conflated meaning and reads as
 *  `failed`. `undefined` when the record says neither. (#2890) Exported so
 *  the run page's TOOL CALLS failed count reads the same rule as the event
 *  log's row, not a second copy of it. */
export function toolOutcome(f: Record<string, unknown>): RecordObject["outcome"] {
  if (typeof f.outcome === "string") {
    return f.outcome === "reported" ? "reported" : f.outcome === "failed" ? "failed" : "ok";
  }
  if (f.ok === false) return "failed";
  if (f.ok === true) return "ok";
  return undefined;
}

/** A `dispatch.tool` record's `args` as an object, or `null` when they are
 *  not a JSON object (absent, or cut short by the per-call cap). */
function parseToolArgs(f: Record<string, unknown>): Record<string, unknown> | null {
  if (typeof f.args !== "string") return null;
  try {
    let parsed: unknown = JSON.parse(f.args);
    // Some tools' arguments arrive double-encoded, a JSON string of JSON
    // (measured on `edit`): one parse yields a string, so parse again.
    if (typeof parsed === "string") parsed = JSON.parse(parsed);
    return parsed && typeof parsed === "object" && !Array.isArray(parsed) ? (parsed as Record<string, unknown>) : null;
  } catch {
    return null;
  }
}

/** (#2963) The file a completed tool call named, from its `dispatch.tool`
 *  record's own fields: the call's `path` argument (parsed, or read out of
 *  arguments the per-call cap cut short), else, for `write`, the path its
 *  result names. The container prefix and a leading `./` are dropped and
 *  bidi and zero-width characters are escaped (`cleanToolPath`): the event
 *  log row's reading (`recordObject`), plus the `./`, so the
 *  run page's readout and the log never name different files. `null` when
 *  the call named none. */
export function toolCallPath(f: Record<string, unknown>): string | null {
  const args = parseToolArgs(f);
  let path: string | null = null;
  if (args) {
    if (typeof args.path === "string") path = args.path;
  } else if (typeof f.args === "string") {
    const raw = fieldFromRaw(f.args, ["path"]);
    if (raw) path = raw.value;
  }
  if (!path && f.tool_name === "write" && typeof f.result === "string") path = pathFromResult(f.result);
  return path ? cleanToolPath(path) : null;
}

/** (#2963) A tool call's path as the viewer shows it: the container prefix
 *  and any leading `./` dropped, bidi and zero-width characters escaped
 *  (`escapeBidiControls`); `null` when nothing is left. Shared
 *  by `toolCallPath` and the turn record's `tool_paths` list, so the two
 *  compare equal for the same file. */
export function cleanToolPath(raw: unknown): string | null {
  if (typeof raw !== "string") return null;
  // (#2963 review, CONSIDER 4) A leading `./` names the same file as none:
  // a listed `./src/a.ts` and a write result's `/workspace/src/a.ts` must
  // compare equal.
  const clean = raw.replace(CONTAINER_ROOT, "").replace(/^(?:\.\/)+/, "");
  return clean ? escapeBidiControls(clean) : null;
}

export function recordObject(r: NormRecord): RecordObject {
  const f = (r.fields || r.payload) as Record<string, unknown> | undefined;
  const a = r.action;

  if (a === ACTION.DispatchTool && f) {
    const args = parseToolArgs(f);
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
      // (#2863 review round 2, finding 7) Gated on the ACTUAL write-tool
      // name (`runtime/src/tools/mod.rs`'s `ToolName::Write => "write"` —
      // one tool, one name, checked rather than assumed) — otherwise any
      // OTHER tool's result matching the same phrase misnamed its row.
      const resultPath = f.tool_name === "write" && typeof f.result === "string" ? pathFromResult(f.result) : null;
      if (raw?.key === "path") {
        text = raw.value.replace(CONTAINER_ROOT, "");
      } else if (raw) {
        text = raw.key === "command" ? commandText(raw.value) : raw.value;
        mono = true;
      } else if (resultPath) {
        text = resultPath.replace(CONTAINER_ROOT, "");
      } else {
        text = f.args != null ? prettyArgs(f.args) : `${f.args_chars ?? 0}ch`;
        mono = true;
      }
    }
    const outcome = toolOutcome(f);
    // (#2863 review, finding 7) `text` above came from a model-controlled
    // command/pattern/path — the one place bidi/zero-width control
    // characters can reach this row.
    const o: RecordObject = { chip: String(f.tool_name ?? "tool"), kind: "tool", text: escapeBidiControls(text), mono };
    if (outcome) o.outcome = outcome;
    return o;
  }
  if (a === ACTION.DispatchReasoning && typeof f?.reasoning_text === "string") {
    const first = unquote(f.reasoning_text).split("\n").find((l) => l.trim()) ?? "";
    return { chip: "reasoning", kind: "think", text: escapeBidiControls(first.trim()) || "(no reasoning text)", mono: false };
  }
  // (#2863 review round 2, finding 5) `recordDetail()` escapes its own
  // output now, so no second pass is needed here — kept un-wrapped rather
  // than double-escaped, which would be harmless but misleading to read.
  return { text: recordDetail(r), mono: false };
}
