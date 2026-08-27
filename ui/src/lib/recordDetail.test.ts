import { describe, expect, it } from "vitest";
import { prettyArgs, recordDetail } from "./recordDetail";
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

  it("surfaces a report_finding's evidence — the finding itself, which is why a crawl's results were invisible", () => {
    const d = recordDetail(
      toolRec({
        tool_name: "report_finding",
        args: JSON.stringify({
          file: "/workspace/crates/darkmux-flow/src/lib.rs",
          line: 147,
          evidence: "let _ = std::fs::create_dir_all(&dir);",
        }),
        result_chars: 133,
        ok: true,
      }),
    );
    expect(d).toContain("report_finding");
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
