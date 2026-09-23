import { describe, expect, it } from "vitest";
import { escapeBidiControls, prettyArgs, recordDetail, recordObject } from "./recordDetail";
import type { FlowRecord } from "../types/handwritten";

/** A `dispatch.tool` record shaped exactly like the ones a live crawl emits
 * (captured from `~/.darkmux/flows/2026-08-25.jsonl`, run
 * `crawl-error-discard-deep-1787669136-1`). */
function toolRec(payload: Record<string, unknown>): FlowRecord {
  return { ts: "2026-08-25T14:45:41Z", action: "dispatch.tool", fields: payload } as unknown as FlowRecord;
}

describe("dispatch.tool outcome (#2008)", () => {
  const rec = (fields: Record<string, unknown>) =>
    ({ action: "dispatch.tool", fields: { tool_name: "bash", args: "{}", result_chars: 12, ...fields } }) as never;

  it("shows the exit code for a command that RAN and reported non-zero", () => {
    // A red test is the tool working. Marking it ❌ told the operator the
    // instrument was broken on exactly the workflow darkmux is built for.
    const out = recordDetail(rec({ ok: true, outcome: "reported", exit_code: 1 }));
    expect(out).toContain("exit 1");
    expect(out).not.toContain("❌");
  });

  it("keeps the cross for a tool that could not run", () => {
    const out = recordDetail(rec({ ok: false, outcome: "failed", failure_reason: "command not found" }));
    expect(out).toContain("❌");
  });

  it("leaves a pre-1.22 record reading the way it meant when written", () => {
    // No `outcome` key: `ok:false` carried the old conflated meaning, so the
    // cross is the honest reading of that record rather than a retroactive
    // reinterpretation of what the writer knew.
    const out = recordDetail(rec({ ok: false }));
    expect(out).toContain("❌");
  });
});

describe("prettyArgs", () => {
  it("flattens a tool call's JSON arguments to k=v pairs", () => {
    expect(prettyArgs('{"path":"/workspace/src","pattern":"let _ =","max_results":100}')).toBe(
      "path=/workspace/src pattern=let _ = max_results=100",
    );
  });

  it("clips a long value rather than letting one argument fill the row", () => {
    const long = "x".repeat(200);
    const out = prettyArgs(JSON.stringify({ why: long }));
    expect(out.length).toBeLessThan(80);
    expect(out.endsWith("…")).toBe(true);
  });

  it("falls back to the clipped raw string when the args are not JSON", () => {
    expect(prettyArgs("{not json")).toBe("{not json");
  });
});

describe("recordDetail", () => {
  it("names the tool, its arguments and the result size", () => {
    const d = recordDetail(
      toolRec({ tool_name: "search", args: '{"pattern":"let _ ="}', result_chars: 4816, ok: true }),
    );
    expect(d).toBe("search pattern=let _ = → 4816ch");
  });

  it("surfaces a create_finding's evidence — the finding itself, which is why a crawl's results were invisible", () => {
    const d = recordDetail(
      toolRec({
        tool_name: "create_finding",
        args: JSON.stringify({
          file: "/workspace/crates/darkmux-flow/src/lib.rs",
          line: 147,
          evidence: "let _ = std::fs::create_dir_all(&dir);",
        }),
        result_chars: 133,
        ok: true,
      }),
    );
    expect(d).toContain("create_finding");
    expect(d).toContain("lib.rs");
    expect(d).toContain("line=147");
    expect(d).toContain("create_dir_all");
  });

  it("marks a failed call", () => {
    expect(recordDetail(toolRec({ tool_name: "read", args: "{}", result_chars: 0, ok: false }))).toContain("❌");
  });

  it("degrades a pre-1.16 record with no args to its size rather than a bare arrow", () => {
    expect(recordDetail(toolRec({ tool_name: "read", args_chars: 76, result_chars: 1553 }))).toBe("read 76ch → 1553ch");
  });

  it("reads `payload` as well as `fields` (records that never went through flowToRenderModel)", () => {
    const r = {
      ts: "2026-08-25T14:45:41Z",
      action: "dispatch.tool",
      payload: { tool_name: "bash", args: '{"command":"ls"}', result_chars: 12, ok: true },
    } as unknown as FlowRecord;
    expect(recordDetail(r)).toBe("bash command=ls → 12ch");
  });

  it("gives a turn its finish reason — the `length` finishes a checkpointing turn produces", () => {
    const r = {
      ts: "2026-08-25T14:45:41Z",
      action: "dispatch.turn",
      fields: { turn_seq: 7, finish_reason: "length" },
    } as unknown as FlowRecord;
    expect(recordDetail(r)).toBe("turn 7 (length)");
  });

  it("returns nothing for a record kind with no preview", () => {
    const r = { ts: "2026-08-25T14:45:41Z", action: "dispatch.turn.heartbeat", fields: {} } as unknown as FlowRecord;
    expect(recordDetail(r)).toBe("");
  });
});

// ── (#2863) A row's object: what a person scans for, not a report ──────────
// (#2863 review, finding 7, security — Trojan-Source class) A model-written
// string can carry bidi/zero-width control characters that REORDER what a
// row visually displays without changing what actually runs.
describe("escapeBidiControls (#2863 review, finding 7)", () => {
  it("replaces a RIGHT-TO-LEFT OVERRIDE with a visible, unambiguous escape", () => {
    expect(escapeBidiControls("echo ok‮txt.exe‬")).toBe("echo ok⟨U+202E⟩txt.exe⟨U+202C⟩");
  });

  it("replaces a zero-width space, which is otherwise invisible", () => {
    expect(escapeBidiControls("rm​ -rf")).toBe("rm⟨U+200B⟩ -rf");
  });

  it("leaves ordinary text untouched", () => {
    expect(escapeBidiControls("npm test")).toBe("npm test");
  });
});

describe("recordObject", () => {
  const tool = (fields: Record<string, unknown>) =>
    ({ action: "dispatch.tool", fields: { result_chars: 10, ...fields } }) as never;

  it("names a file tool by its file, with the container prefix dropped", () => {
    const o = recordObject(tool({ tool_name: "edit", args: '{"path":"/workspace/test/refreshTokenService.test.js","edits":[]}', ok: true, outcome: "ok" }));
    expect(o).toEqual({ chip: "edit", kind: "tool", text: "test/refreshTokenService.test.js", mono: false, outcome: "ok" });
  });

  it("reads arguments that arrive double-encoded (a JSON string of JSON)", () => {
    // Measured on a live run: `edit`'s args are `"{\"path\":…}"`, so one
    // parse yields a STRING and the row showed the raw JSON.
    const args = JSON.stringify(JSON.stringify({ path: "/workspace/test/a.test.js", edits: [] }));
    expect(recordObject(tool({ tool_name: "edit", args })).text).toBe("test/a.test.js");
  });

  it("reads the object out of arguments the flow record cut short", () => {
    // Measured: the host clips long arguments, so a big edit's args are not
    // valid JSON (double-encoded AND truncated) and a parse fails.
    const args = JSON.stringify(JSON.stringify({ path: "/workspace/test/a.test.js", edits: [{ old_string: "x".repeat(50) }] })).slice(0, 70) + "…";
    expect(recordObject(tool({ tool_name: "edit", args })).text).toBe("test/a.test.js");
    const cmd = '{"command":"cd /workspace && npm test 2>&1","timeout_se…';
    expect(recordObject(tool({ tool_name: "bash", args: cmd })).text).toBe("npm test");
  });

  it("reads only the call's OWN top-level keys from cut-short arguments, never a key inside a value", () => {
    // (#2863 review) A truncated write of a file whose CONTENT holds a
    // `"command"` key rendered that command as the row, a command that never
    // ran. Both encodings, both key orders.
    const content = JSON.stringify({ version: "2.0.0", tasks: [{ command: "rm -rf build", label: "clean" }] });
    const pathFirst = JSON.stringify({ path: "/workspace/.vscode/tasks.json", content }).slice(0, 95) + "…";
    const contentFirst = JSON.stringify({ content, path: "/workspace/.vscode/tasks.json" });
    for (const raw of [pathFirst, contentFirst.slice(0, -1) + "…"]) {
      for (const args of [raw, JSON.stringify(raw)]) {
        const o = recordObject(tool({ tool_name: "write", args }));
        expect(o.text, args).not.toContain("rm -rf");
        expect(o.text, args).toBe(".vscode/tasks.json");
      }
    }
  });

  // (#2863 review, finding 5) A `write` whose `content` is long enough that
  // the host's per-call args cap (~7018 chars, measured) is exhausted before
  // reaching `path` — `content` precedes `path` in the call's own key order,
  // so `fieldFromRaw` never even SEES `path`, not just fails to parse it.
  // The runtime's own result names the path it wrote
  // (`runtime/src/tools/mod.rs`'s `write`: "Wrote N bytes to <path>"), so
  // that is the fallback before showing raw JSON.
  it("names a write's path from the result when content ate the whole args cap", () => {
    const content = "x".repeat(7018);
    const args = JSON.stringify({ content, path: "/workspace/test/refreshTokenService.gaps.test.js" }).slice(0, 7018) + "…";
    const o = recordObject(
      tool({
        tool_name: "write",
        args,
        result: "Wrote 6650 bytes to /workspace/test/refreshTokenService.gaps.test.js",
      }),
    );
    expect(o.text).toBe("test/refreshTokenService.gaps.test.js");
    expect(o.mono).toBe(false);
  });

  it("falls back to raw JSON when neither the args nor the result name a path", () => {
    const args = JSON.stringify({ content: "x".repeat(7018) }).slice(0, 7018) + "…";
    const o = recordObject(tool({ tool_name: "write", args, result: "ok" }));
    expect(o.text).not.toBe("");
    expect(o.mono).toBe(true);
  });

  // (#2863 review, finding 6, security) `commandText` used to silently drop
  // every line after the first (`.split("\n")[0]`) — a command carrying a
  // second, unrelated line rendered identically to a single, innocuous one,
  // with no marker that anything was cut. The row now says how many lines
  // were hidden, so a reader can tell "one command" from "more than one"
  // without opening the detail pane.
  it("marks a multi-line command instead of silently showing only its first line", () => {
    const o = recordObject(tool({ tool_name: "bash", args: JSON.stringify({ command: "echo ok\nrm -rf /workspace" }) }));
    expect(o.text).toBe("echo ok ⏎ +1 more line");
  });

  it("a padded first line does not defeat the multi-line marker", () => {
    const cmd = "echo " + "y".repeat(200) + "\nrm -rf /workspace";
    const o = recordObject(tool({ tool_name: "bash", args: JSON.stringify({ command: cmd }) }));
    expect(o.text).toMatch(/⏎ \+1 more line$/);
  });

  it("keeps a cd the model wrote itself; only the runtime's own /workspace hop is dropped", () => {
    // (#2863 review) Stripping any `cd X &&` rendered `cd /tmp && rm -rf *`
    // as `rm -rf *`, a different command.
    const cmd = (c: string) => recordObject(tool({ tool_name: "bash", args: JSON.stringify({ command: c }) })).text;
    expect(cmd("cd /tmp && rm -rf *")).toBe("cd /tmp && rm -rf *");
    expect(cmd("cd crates/a && cargo test 2>&1")).toBe("cd crates/a && cargo test");
    expect(cmd("cd /workspace/ && npm test")).toBe("npm test");
    expect(cmd("cd /workspace/sub && npm test")).toBe("cd /workspace/sub && npm test");
  });

  it("a reasoning record with no text says so rather than showing a bare chip", () => {
    const r = { action: "dispatch.reasoning", fields: { reasoning_text: "\n\n" } } as never;
    expect(recordObject(r).text).toBe("(no reasoning text)");
  });

  it("names a command by the command, without the cd prefix or the redirect", () => {
    const o = recordObject(tool({ tool_name: "bash", args: '{"command":"cd /workspace && npm test 2>&1","timeout_seconds":30}', outcome: "ok" }));
    expect(o.text).toBe("npm test");
    expect(o.mono).toBe(true);
  });

  it("names a search by its pattern", () => {
    expect(recordObject(tool({ tool_name: "search", args: '{"path":"/workspace/src","pattern":"let _ ="}' })).text).toBe("let _ =");
  });

  it("carries the three outcomes #2008 distinguishes", () => {
    expect(recordObject(tool({ tool_name: "bash", args: "{}", outcome: "reported", exit_code: 1 })).outcome).toBe("reported");
    expect(recordObject(tool({ tool_name: "bash", args: "{}", outcome: "failed" })).outcome).toBe("failed");
    // pre-1.22: `ok:false` meant failed when it was written
    expect(recordObject(tool({ tool_name: "bash", args: "{}", ok: false })).outcome).toBe("failed");
    expect(recordObject(tool({ tool_name: "bash", args: "{}", ok: true })).outcome).toBe("ok");
  });

  it("falls back to the arguments when no field names the object", () => {
    expect(recordObject(tool({ tool_name: "fetch", args: '{"url":"http://x"}' })).text).toBe("url=http://x");
  });

  it("(#2863 review, finding 7) escapes a bidi override in a command so it cannot reorder the row", () => {
    const o = recordObject(tool({ tool_name: "bash", args: JSON.stringify({ command: "echo ok‮txt.exe" }) }));
    expect(o.text).toBe("echo ok⟨U+202E⟩txt.exe");
    expect(o.text).not.toContain("‮");
  });

  it("(#2863 review, finding 7) escapes a bidi override in a reasoning row", () => {
    const r = { action: "dispatch.reasoning", fields: { reasoning_text: "Let me ‮esrever siht‬ read." } } as never;
    expect(recordObject(r).text).not.toContain("‮");
    expect(recordObject(r).text).toContain("⟨U+202E⟩");
  });

  it("a reasoning row is its first line, without the JSON-string quotes it sometimes carries", () => {
    const r = { action: "dispatch.reasoning", fields: { reasoning_text: '"Let me analyze the implementation.\n\nMore."' } } as never;
    expect(recordObject(r)).toEqual({ chip: "reasoning", kind: "think", text: "Let me analyze the implementation.", mono: false });
  });

  it("anything else keeps its existing one-line detail and no chip", () => {
    const r = { action: "dispatch start", fields: { prompt_chars: 3204 } } as never;
    expect(recordObject(r)).toEqual({ text: "start (prompt: 3204ch)", mono: false });
  });
});
