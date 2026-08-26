// `clk()` (`lib/format.ts`) reads the PROCESS timezone via
// `toLocaleTimeString`, with no override — the parity harness's own
// Playwright context pins `timezoneId: 'UTC'` (see `extract.spec.ts`), but
// this vitest suite has no such pin by default. The BYTE-PARITY test below
// compares directly against a golden captured under that UTC pin, so this
// one file forces the SAME timezone for the process — set before any
// `clk()` call happens (module import order doesn't matter here since
// `clk()` only runs at test-execution time, but this stays at the top for
// visibility). The OTHER describe block ("pure-logic unit coverage") is
// deliberately NOT written to depend on this — see its own tests, none of
// which assert a literal `clk()`-derived clock string (following this
// codebase's existing convention — see `timeline.test.ts`'s own doc for why
// a TZ-dependent literal is the wrong pattern for a non-golden unit test).
process.env.TZ = "UTC";

import { describe, it, expect } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { runRegions } from "./sessionRun";
import { flowToRenderModel } from "../../lib/flow";
import type { FlowRecord } from "../../types/handwritten";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
// `ui/src/lenses/session/` -> repo root is four levels up.
const REPO_ROOT = path.resolve(__dirname, "../../../..");

function readCorpus(name: string): FlowRecord[] {
  const raw = JSON.parse(readFileSync(path.join(REPO_ROOT, "tests/parity/corpus", name), "utf8"));
  return raw.records as FlowRecord[];
}

/** The `=== stage ===` section of a legacy golden, in the SAME normalized
 * form the parity harness's own extraction produces (trimmed lines, no
 * leading `=== stage ===` marker) — reused here as an independent
 * differential check that this module's derivation matches the REAL
 * recorded legacy output, not a hand-rolled approximation of the source. */
function stageSectionOf(goldenText: string): string[] {
  const lines = goldenText.split("\n");
  const start = lines.findIndex((l) => l.trim() === "=== stage ===");
  const rest = lines.slice(start + 1);
  const end = rest.findIndex((l) => l.trim().startsWith("==="));
  const stage = end === -1 ? rest : rest.slice(0, end);
  // Trailing blank lines are a golden-file artifact (the file ends with a
  // newline); drop them so the comparison is content-only.
  while (stage.length && stage[stage.length - 1] === "") stage.pop();
  return stage;
}

/** Flattens a `SessionRunView` into the SAME line sequence
 * `SessionReplay.tsx` renders (one `<div>`/text-run per element) — see that
 * component's own doc for the exact DOM shape this mirrors. */
function flattenView(view: ReturnType<typeof runRegions>): string[] {
  const lines: string[] = [];
  lines.push(`${view.header.pillLabel} RUN · ${view.header.role} (${view.header.sid} on ${view.header.machineName})`);
  lines.push(...view.briefLines.map((e) => e.text));
  for (const m of view.metrics) {
    lines.push(m.value, m.label);
  }
  lines.push(view.modelTrackLabel, ...view.modelTrackLines);
  // (#1973) Mirrors the SIGNALS block's DOM: label, then either the clean
  // pair or, per group, a head line and one line per signal. Note this mirror
  // is NOT enforced against the component (#1978) — the rendered assertions
  // live in `SessionReplay.test.tsx`. What this pins is the DERIVATION.
  lines.push(view.signalsLabel);
  if (view.signalGroups.length === 0) {
    lines.push("✓ clean", "no behavioral flags (cycle, tool-failure, reasoning-loop, edit-drift)");
  } else {
    for (const g of view.signalGroups) {
      lines.push(`${g.severity === "warn" ? "⚠" : "✓"}${g.kind}${g.count > 1 ? `×${g.count}` : ""}`);
      for (const sig of g.signals) {
        lines.push(`${sig.offsetLabel}${sig.detail}${sig.fix ? `fix: ${sig.fix}` : ""}`);
      }
    }
  }
  return lines;
}

describe("runRegions — byte parity against the real recorded legacy golden", () => {
  it("matches goldens/session-task-list.txt's #stage section for the real flow-session-task-list.json corpus", () => {
    const records = readCorpus("flow-session-task-list.json");
    const data = flowToRenderModel(records);
    const view = runRegions(data, "task-list");

    const golden = readFileSync(path.join(REPO_ROOT, "tests/parity/goldens/session-task-list.txt"), "utf8");
    const expected = stageSectionOf(golden);

    expect(flattenView(view)).toEqual(expected);
  });
});

describe("runRegions — pure-logic unit coverage beyond the one recorded corpus", () => {
  const BASE_TS = "2026-01-01T00:00:00Z";

  it("a clean, completed local dispatch: 'complete' pill, full brief, real metrics", () => {
    const data: FlowRecord[] = [
      {
        ts: BASE_TS,
        session_id: "s1",
        action: "dispatch.start",
        handle: "darkmux/coder",
        model: "darkmux:qwen3-coder",
        payload: { runtime: "internal", image: "darkmux-runtime:latest", workspace: "/tmp/wt", prompt_chars: 500 },
      },
      {
        ts: "2026-01-01T00:05:00Z",
        session_id: "s1",
        action: "dispatch.turn",
        payload: { turn_seq: 3 },
      },
      {
        ts: "2026-01-01T00:05:30Z",
        session_id: "s1",
        category: "telemetry",
        source: "tokens",
        payload: { prompt_tokens: 1000, completion_tokens: 200 },
      },
      {
        ts: "2026-01-01T00:10:00Z",
        session_id: "s1",
        action: "dispatch.complete",
        payload: { prompt_tokens: 1000, completion_tokens: 200 },
      },
    ];
    const view = runRegions(flowToRenderModel(data), "s1");
    expect(view.header.pillLabel).toBe("COMPLETE");
    expect(view.header.pillCls).toBe("done");
    expect(view.header.role).toBe("CODER");
    expect(view.briefLines.map((e) => e.text)).toContain("route");
    expect(view.briefLines.map((e) => e.text)).toContain("LMStudio · local · this machine");
    expect(view.briefLines.map((e) => e.text)).toContain("runtime");
    expect(view.briefLines.map((e) => e.text)).toContain("internal container");
    expect(view.briefLines.map((e) => e.text)).toContain("image");
    expect(view.briefLines.map((e) => e.text)).toContain("darkmux-runtime:latest");
    expect(view.briefLines.map((e) => e.text)).toContain("model");
    expect(view.briefLines.map((e) => e.text)).toContain("darkmux:qwen3-coder");
    expect(view.briefLines.map((e) => e.text)).toContain("workspace");
    expect(view.briefLines.map((e) => e.text)).toContain("/tmp/wt");
    expect(view.briefLines.map((e) => e.text)).toContain("prompt");
    expect(view.briefLines.map((e) => e.text)).toContain("500 chars");
    expect(view.metrics.find((m) => m.label === "TURNS")?.value).toBe("3");
    // `fmtC` — the COMPACT formatter (`lib/format.ts`), same as legacy's own
    // `fmtC(tokIn)` on the metric tile (NOT `fmtN`'s comma-grouped form).
    expect(view.metrics.find((m) => m.label === "TOKENS IN")?.value).toBe("1k");
    expect(view.metrics.find((m) => m.label === "TOKENS OUT")?.value).toBe("200");
    expect(view.metrics.find((m) => m.label === "WALL CLOCK")?.value).toBe("10:00");
  });

  it("an errored (non-killed) dispatch names the exit code and reads red", () => {
    const data: FlowRecord[] = [
      { ts: BASE_TS, session_id: "s1", action: "dispatch.start", handle: "coder" },
      { ts: "2026-01-01T00:01:00Z", session_id: "s1", action: "dispatch.error", payload: { exit_code: 1 } },
    ];
    const view = runRegions(flowToRenderModel(data), "s1");
    expect(view.header.pillLabel).toBe("ERRORED");
    expect(view.header.pillCls).toBe("err");
    expect(view.metrics.find((m) => m.label === "WALL CLOCK")?.value).toBe("1:00 · errored (exit 1)");
  });

  it("a watchdog-killed dispatch (exit 137) reads 'killed', not 'errored'", () => {
    const data: FlowRecord[] = [
      { ts: BASE_TS, session_id: "s1", action: "dispatch.start", handle: "coder" },
      { ts: "2026-01-01T00:01:00Z", session_id: "s1", action: "dispatch.error", payload: { exit_code: 137 } },
    ];
    const view = runRegions(flowToRenderModel(data), "s1");
    expect(view.header.pillLabel).toBe("KILLED");
    expect(view.metrics.find((m) => m.label === "WALL CLOCK")?.value).toBe("1:00 · killed (timeout)");
  });

  it("a remote (endpoint-served) run names the endpoint and omits the local model track", () => {
    const data: FlowRecord[] = [
      { ts: BASE_TS, session_id: "s1", action: "dispatch.start", handle: "reviewer", payload: { endpoint: "azure:my-host/gpt-4o" } },
      { ts: "2026-01-01T00:01:00Z", session_id: "s1", action: "dispatch.complete", payload: {} },
    ];
    const view = runRegions(flowToRenderModel(data), "s1");
    expect(view.briefLines.map((e) => e.text)).toContain("Azure OpenAI · my-host/gpt-4o · off-fleet");
    expect(view.modelTrackLabel).toBe("model (remote)");
    expect(view.modelTrackLines[0]).toMatch(/served off-fleet — no local model/);
  });

  it("a jit-model-swap (more than one local model loaded in one run) surfaces as a warning detection", () => {
    const data: FlowRecord[] = [
      { ts: BASE_TS, session_id: "s1", action: "dispatch.start", handle: "coder" },
      { ts: "2026-01-01T00:00:10Z", session_id: "s1", category: "telemetry", source: "lms", fields: { event: "load", model: "a", gb: 10 } },
      { ts: "2026-01-01T00:02:00Z", session_id: "s1", category: "telemetry", source: "lms", fields: { event: "load", model: "b", gb: 20 } },
      { ts: "2026-01-01T00:05:00Z", session_id: "s1", action: "dispatch.complete", payload: {} },
    ];
    const view = runRegions(flowToRenderModel(data), "s1");
    expect(view.signalsLabel).toBe("signals (1)");
    expect(view.signalGroups).toHaveLength(1);
    expect(view.signalGroups[0].kind).toBe("jit-model-swap");
    expect(view.signalGroups[0].severity).toBe("warn");
    expect(view.signalGroups[0].signals[0].detail).toMatch(/2 models loaded in one run \(a → b\)/);
    expect(view.signalGroups[0].signals[0].fix).toMatch(/^pin one model/);
    // Synthesized from the load track, so it has no record and no moment —
    // it must say so rather than borrow one.
    expect(view.signalGroups[0].signals[0].atMs).toBeNull();
    expect(view.signalGroups[0].signals[0].offsetLabel).toBe("");
    expect(view.modelTrackLines).toEqual(["a · 10GB", "b · 20GB"]);
  });

  it("a real detector finding carries its severity/kind/detail, without a fix line", () => {
    const data: FlowRecord[] = [
      { ts: BASE_TS, session_id: "s1", action: "dispatch.start", handle: "coder" },
      {
        ts: "2026-01-01T00:00:30Z",
        session_id: "s1",
        category: "telemetry",
        source: "detector",
        fields: { severity: "warn", kind: "cycle", detail: "repeated identical tool call 4x" },
      },
      { ts: "2026-01-01T00:01:00Z", session_id: "s1", action: "dispatch.complete", payload: {} },
    ];
    const view = runRegions(flowToRenderModel(data), "s1");
    expect(view.signalGroups).toEqual([
      {
        kind: "cycle",
        severity: "warn",
        count: 1,
        signals: [
          {
            kind: "cycle",
            severity: "warn",
            detail: "repeated identical tool call 4x",
            // 30s after `dispatch.start` — run-relative, because "how far in
            // did this start" is the question, and a wall-clock stamp makes
            // the reader compute it.
            atMs: Date.parse("2026-01-01T00:00:30Z"),
            offsetLabel: "+0:30",
          },
        ],
      },
    ]);
  });

  it("(#1973) an `info` signal is NOT rendered as a warning — a recovery is not a struggle", () => {
    // `dispatch_internal` emits `severity: "info"` for `intra-turn-stall`,
    // which reports that a stall RECOVERED. The old viewer dropped severity
    // entirely and prefixed every entry with `⚠`, so a successful recovery
    // looked exactly like a doom loop.
    const data: FlowRecord[] = [
      { ts: BASE_TS, session_id: "s1", action: "dispatch.start", handle: "coder" },
      {
        ts: "2026-01-01T00:00:30Z",
        session_id: "s1",
        category: "telemetry",
        source: "detector",
        fields: { severity: "info", kind: "intra-turn-stall", detail: "stall recovered after 45s" },
      },
    ];
    const view = runRegions(flowToRenderModel(data), "s1");
    expect(view.signalGroups[0].severity).toBe("info");
  });

  it("(#1973) an UNRECOGNIZED severity degrades to warn, never to info", () => {
    // A severity this build does not know is more likely to matter than not.
    // Degrading it to `info` is how a new detector ships invisible.
    const data: FlowRecord[] = [
      { ts: BASE_TS, session_id: "s1", action: "dispatch.start", handle: "coder" },
      {
        ts: "2026-01-01T00:00:05Z",
        session_id: "s1",
        category: "telemetry",
        source: "detector",
        fields: { severity: "catastrophic", kind: "future-detector", detail: "something new" },
      },
    ];
    const view = runRegions(flowToRenderModel(data), "s1");
    expect(view.signalGroups[0].severity).toBe("warn");
  });

  it("(#1973) groups repeats by kind with a count, and orders warn before info", () => {
    // Eleven cycle detections should be ONE row that says 11, not eleven rows
    // that bury everything else — and a recovery must never outrank a
    // struggle, whatever order the records happen to arrive in.
    const det = (ts: string, kind: string, severity: string) => ({
      ts,
      session_id: "s1",
      category: "telemetry" as const,
      source: "detector",
      fields: { severity, kind, detail: `${kind} at ${ts}` },
    });
    const data: FlowRecord[] = [
      { ts: BASE_TS, session_id: "s1", action: "dispatch.start", handle: "coder" },
      // The `info` one arrives LAST in time, so recency alone would float it.
      det("2026-01-01T00:00:10Z", "cycle", "warn"),
      det("2026-01-01T00:00:20Z", "cycle", "warn"),
      det("2026-01-01T00:00:50Z", "intra-turn-stall", "info"),
    ];
    const view = runRegions(flowToRenderModel(data), "s1");
    expect(view.signalsLabel).toBe("signals (3)");
    expect(view.signalGroups.map((g) => [g.kind, g.count, g.severity])).toEqual([
      ["cycle", 2, "warn"],
      ["intra-turn-stall", 1, "info"],
    ]);
    // Newest first WITHIN a group.
    expect(view.signalGroups[0].signals.map((x) => x.offsetLabel)).toEqual(["+0:20", "+0:10"]);
  });

  it("no dispatch.start at all falls back to the first record on the session, and reads 'no start'", () => {
    const data: FlowRecord[] = [
      { ts: BASE_TS, session_id: "s1", action: "step start", handle: "fetch-render" },
      { ts: "2026-01-01T00:00:05Z", session_id: "s1", action: "step complete" },
    ];
    const view = runRegions(flowToRenderModel(data), "s1");
    expect(view.header.role).toBe("FETCH-RENDER");
    // Still "running" — no terminal edge exists at all.
    expect(view.header.pillLabel).toBe("RUNNING");
  });

  it("the context metric names peak/window once a context-telemetry stream exists", () => {
    const data: FlowRecord[] = [
      { ts: BASE_TS, session_id: "s1", action: "dispatch.start", handle: "coder" },
      { ts: "2026-01-01T00:01:00Z", session_id: "s1", category: "telemetry", source: "context", fields: { max: 100000, used: 20000 } },
      { ts: "2026-01-01T00:02:00Z", session_id: "s1", category: "telemetry", source: "context", fields: { max: 100000, used: 45000 } },
      { ts: "2026-01-01T00:03:00Z", session_id: "s1", action: "dispatch.complete", payload: {} },
    ];
    const view = runRegions(flowToRenderModel(data), "s1");
    const ctx = view.metrics.find((m) => m.label.startsWith("CTX"));
    expect(ctx?.label).toBe("CTX PEAK 45K / 100K WINDOW");
    expect(ctx?.value).toBe("45K"); // done -> headline is peak, not the last sample
  });

  // (#1945 review) The mission drill-out was entirely unpinned: deleting the
  // `href:` line left 967/967 green, and parity cannot reach it either — that
  // corpus's only start record is `step start`, so `d` is null and the brief
  // renders route+timing with no mission row at all.
  it("a dispatch carrying a mission_id gets a clickable drill-out to the mission", () => {
    const data = [
      {
        ts: "2026-01-01T00:00:00Z",
        session_id: "s2",
        action: "dispatch.start",
        mission_id: "m-42",
        phase_id: "p1",
        payload: { role: "coder" },
      },
    ] as never[];
    const view = runRegions(flowToRenderModel(data), "s2");

    const label = view.briefLines.find((e) => e.text === "mission");
    expect(label?.kind).toBe("label");

    const value = view.briefLines.find((e) => e.text.startsWith("m-42"));
    expect(value?.kind).toBe("value");
    expect(value?.text).toBe("m-42 · phase p1");
    // The route the viewer actually understands (`lib/route.ts` -> kind
    // "mission"), with the id encoded the same way CatalogPanel encodes it.
    expect(value?.href).toBe("#mission=m-42");
  });

  it("a dispatch with no mission_id renders no mission row", () => {
    const data = [
      {
        ts: "2026-01-01T00:00:00Z",
        session_id: "s3",
        action: "dispatch.start",
        payload: { role: "coder" },
      },
    ] as never[];
    const view = runRegions(flowToRenderModel(data), "s3");
    expect(view.briefLines.some((e) => e.text === "mission")).toBe(false);
    expect(view.briefLines.some((e) => e.href?.startsWith("#mission="))).toBe(false);
  });

});
