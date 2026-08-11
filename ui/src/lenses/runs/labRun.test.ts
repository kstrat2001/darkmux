import { describe, it, expect } from "vitest";
import {
  computeLabPipeline,
  labStageMeta,
  labPipelineLines,
  labShortId,
  labFeedTs,
  labFeedRowLines,
  labFeedLines,
  labCliHint,
  labBadgeText,
  LAB_FEED_CAP,
} from "./labRun";
import type { LabRunEvent } from "../../types/handwritten";

describe("computeLabPipeline", () => {
  it("folds step-result events into per-step-id payloads, in first-seen order", () => {
    const events: LabRunEvent[] = [
      { ts: "2026-01-01T00:00:00Z", action: "step result", payload: { step_id: "bundle", items_out: 5 } },
      { ts: "2026-01-01T00:00:01Z", action: "step result", payload: { step_id: "probe", draws_total: 10, draws_done: 3 } },
      { ts: "2026-01-01T00:00:02Z", action: "step result", payload: { step_id: "bundle", items_out: 8 } },
    ];
    const pipe = computeLabPipeline(events);
    expect(pipe.order).toEqual(["bundle", "probe"]);
    // Later payload for the same step_id overwrites the earlier one, but
    // the ORDER position is set on first sight only.
    expect(pipe.steps.bundle).toEqual({ step_id: "bundle", items_out: 8 });
  });

  it("tallies review-ruling events by pass, separately from the step order", () => {
    const events: LabRunEvent[] = [
      { ts: "t", action: "step result", payload: { step_id: "review-ruling", pass: 1, ruling: "confirmed" } },
      { ts: "t", action: "step result", payload: { step_id: "review-ruling", pass: 1, ruling: "confirmed" } },
      { ts: "t", action: "step result", payload: { step_id: "review-ruling", pass: 2, ruling: "needs_check" } },
    ];
    const pipe = computeLabPipeline(events);
    expect(pipe.order).toEqual([]);
    expect(pipe.rulingTally).toEqual({ 1: { confirmed: 2 }, 2: { needs_check: 1 } });
  });

  it("ignores events with no payload or a non-step-result action", () => {
    const events: LabRunEvent[] = [
      { ts: "t", action: "step result" },
      { ts: "t", action: "telemetry" },
    ];
    expect(computeLabPipeline(events)).toEqual({ steps: {}, order: [], rulingTally: { 1: {}, 2: {} } });
  });
});

describe("labStageMeta", () => {
  it("reports not started for a null payload", () => {
    expect(labStageMeta(null)).toBe("not started");
  });

  it("prefers draws over items over a bare model", () => {
    expect(labStageMeta({ draws_total: 10, draws_done: 4, model: "darkmux:foo" })).toBe("4/10 draws · foo");
    expect(labStageMeta({ items_in: 3, items_out: 2 })).toBe("3 → 2");
    expect(labStageMeta({ model: "darkmux:bar" })).toBe("bar");
  });

  it("appends wall_ms when present, and falls back to 'done' with nothing else to say", () => {
    expect(labStageMeta({ wall_ms: 120 })).toBe("120ms");
    expect(labStageMeta({})).toBe("done");
  });
});

describe("labPipelineLines", () => {
  it("emits a name/meta line pair per stage in arrival order, plus a trailing synthesis stage", () => {
    const pipe = computeLabPipeline([
      { ts: "t", action: "step result", payload: { step_id: "bundle", items_out: 5 } },
    ]);
    const lines = labPipelineLines(pipe, { crew: "c", mode: "m", confirmed: 3, needs_check: 1, archived: 0 });
    // Only `items_out` was seeded above (no `items_in`) — the em dash marks
    // the absent side, matching `labStageMeta`'s own `itemsIn ?? "—"`.
    expect(lines).toEqual(["bundle", "— → 5", "synthesis", "confirmed 3 · needs_check 1 · archived 0"]);
  });

  it("falls back to a single 'pipeline' stage with nothing started yet", () => {
    const pipe = computeLabPipeline([]);
    const lines = labPipelineLines(pipe, null);
    expect(lines[0]).toBe("pipeline");
    expect(lines[1]).toBe("not started");
  });

  it("shows the provisional ruling tally when no terminal envelope has landed", () => {
    const pipe = computeLabPipeline([
      { ts: "t", action: "step result", payload: { step_id: "review-ruling", pass: 1, ruling: "confirmed" } },
    ]);
    const lines = labPipelineLines(pipe, null);
    expect(lines.slice(-2)).toEqual(["synthesis", "(provisional, from rulings so far) pass1 confirmed:1 · pass2 —"]);
  });
});

describe("labShortId / labFeedTs", () => {
  it("truncates a long id with an ellipsis at 18 chars", () => {
    expect(labShortId("a".repeat(30))).toBe("a".repeat(18) + "…");
    expect(labShortId("short")).toBe("short");
    expect(labShortId(undefined)).toBe("");
  });

  it("strips the RFC3339 T/Z for a compact feed timestamp", () => {
    expect(labFeedTs("2026-08-08T12:00:00Z")).toBe("2026-08-08 12:00:00");
  });
});

describe("labFeedRowLines", () => {
  it("renders a host-telemetry row as ts/host/cpu-mem-gpu", () => {
    const r: LabRunEvent = { ts: "t", category: "telemetry", source: "process", payload: { cpu: 12, mem: 40, gpu: 0 } };
    expect(labFeedRowLines(r)).toEqual(["t", "host", "cpu 12% · mem 40% · gpu 0%"]);
  });

  it("renders a review-ruling row naming stage/bundle/ruling/seconds", () => {
    const r: LabRunEvent = {
      ts: "t",
      action: "step result",
      payload: { step_id: "review-ruling", stage: "judge", pass: 1, bundle_id: "short-id", ruling: "confirmed", seconds: 2.3456 },
    };
    expect(labFeedRowLines(r)).toEqual(["t", "judge", "short-id pass1 → confirmed (2.3s)"]);
  });

  it("truncates a long bundle_id at 18 chars with an ellipsis (labShortId)", () => {
    const r: LabRunEvent = {
      ts: "t",
      action: "step result",
      payload: { step_id: "review-ruling", stage: "judge", pass: 1, bundle_id: "someVerb@some/path.ts", ruling: "confirmed", seconds: 2.3456 },
    };
    expect(labFeedRowLines(r)).toEqual(["t", "judge", "someVerb@some/path… pass1 → confirmed (2.3s)"]);
  });

  it("renders a plain stage-completion step-result row", () => {
    const r: LabRunEvent = { ts: "t", action: "step result", payload: { step_id: "probe", wall_ms: 500 } };
    expect(labFeedRowLines(r)).toEqual(["t", "probe", "500ms"]);
  });

  it("falls back to a bare action line for anything else", () => {
    const r: LabRunEvent = { ts: "t", action: "task started" };
    expect(labFeedRowLines(r)).toEqual(["t", "", "task started"]);
  });
});

describe("labFeedLines", () => {
  it("is empty for no events", () => {
    expect(labFeedLines([])).toEqual([]);
  });

  it("renders newest-first, flattened", () => {
    const events: LabRunEvent[] = [
      { ts: "1", action: "task started" },
      { ts: "2", action: "task ended" },
    ];
    expect(labFeedLines(events)).toEqual(["2", "", "task ended", "1", "", "task started"]);
  });

  it("caps at LAB_FEED_CAP, keeping the newest", () => {
    const events: LabRunEvent[] = Array.from({ length: LAB_FEED_CAP + 10 }, (_, i) => ({
      ts: String(i),
      action: "task started",
    }));
    const lines = labFeedLines(events);
    // 3 lines per event.
    expect(lines.length).toBe(LAB_FEED_CAP * 3);
    // Newest (highest ts) first.
    expect(lines[0]).toBe(String(LAB_FEED_CAP + 9));
  });
});

describe("labCliHint", () => {
  it("prefers the envelope's crew/mode over the scores fallback", () => {
    expect(
      labCliHint({ crew: "reviewer", mode: "auto", confirmed: 0, needs_check: 0, archived: 0 }, { crew: "other", exec_mode: "manual" }),
    ).toBe("darkmux lab eval --funnel --roster-profile reviewer --exec-mode auto --workdirs <workdirs-root> <cases-dir>");
  });

  it("falls back to scores, then to the literal placeholders", () => {
    expect(labCliHint(null, { crew: "reviewer", exec_mode: "manual" })).toBe(
      "darkmux lab eval --funnel --roster-profile reviewer --exec-mode manual --workdirs <workdirs-root> <cases-dir>",
    );
    expect(labCliHint(null, null)).toBe(
      "darkmux lab eval --funnel --roster-profile <crew> --exec-mode auto --workdirs <workdirs-root> <cases-dir>",
    );
  });
});

describe("labBadgeText", () => {
  it("names finished vs live", () => {
    expect(labBadgeText(true)).toBe("finished");
    expect(labBadgeText(false)).toBe("● live");
  });
});
