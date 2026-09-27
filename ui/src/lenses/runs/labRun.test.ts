import { describe, it, expect } from "vitest";
import {
  computeLabPipeline,
  labStageMeta,
  labPipelineLines,
  labShortId,
  labFeedTs,
  labFeedRowLines,
  labFeedLines,
  labFeedCountText,
  labFeedStatusSuffix,
  labCliHint,
  labBadgeText,
  LAB_FEED_CAP,
} from "./labRun";
import { norm, normAll, type RawRecord } from "../../testing/records";

describe("computeLabPipeline", () => {
  it("folds step-result events into per-step-id payloads, in first-seen order", () => {
    const events: RawRecord[] = [
      { ts: "2026-01-01T00:00:00Z", action: "step.result", payload: { step_id: "bundle", items_out: 5 } },
      { ts: "2026-01-01T00:00:01Z", action: "step.result", payload: { step_id: "probe", draws_total: 10, draws_done: 3 } },
      { ts: "2026-01-01T00:00:02Z", action: "step.result", payload: { step_id: "bundle", items_out: 8 } },
    ];
    const pipe = computeLabPipeline(normAll(events));
    expect(pipe.order).toEqual(["bundle", "probe"]);
    // Later payload for the same step_id overwrites the earlier one, but
    // the ORDER position is set on first sight only.
    expect(pipe.steps.bundle).toEqual({ step_id: "bundle", items_out: 8 });
  });

  it("tallies review-ruling events by pass, separately from the step order", () => {
    const events: RawRecord[] = [
      { ts: "t", action: "step.result", payload: { step_id: "review-ruling", pass: 1, ruling: "confirmed" } },
      { ts: "t", action: "step.result", payload: { step_id: "review-ruling", pass: 1, ruling: "confirmed" } },
      { ts: "t", action: "step.result", payload: { step_id: "review-ruling", pass: 2, ruling: "needs_check" } },
    ];
    const pipe = computeLabPipeline(normAll(events));
    expect(pipe.order).toEqual([]);
    expect(pipe.rulingTally).toEqual({ 1: { confirmed: 2 }, 2: { needs_check: 1 } });
  });

  it("ignores events with no payload or a non-step-result action", () => {
    const events: RawRecord[] = [
      { ts: "t", action: "step.result" },
      { ts: "t", action: "telemetry" },
    ];
    expect(computeLabPipeline(normAll(events))).toEqual({ steps: {}, order: [], rulingTally: { 1: {}, 2: {} } });
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
    const pipe = computeLabPipeline(normAll([
      { ts: "t", action: "step.result", payload: { step_id: "bundle", items_out: 5 } },
    ]));
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
    const pipe = computeLabPipeline(normAll([
      { ts: "t", action: "step.result", payload: { step_id: "review-ruling", pass: 1, ruling: "confirmed" } },
    ]));
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
    const r: RawRecord = { ts: "t", category: "telemetry", source: "process", payload: { cpu: 12, mem: 40, gpu: 0 } };
    expect(labFeedRowLines(norm(r))).toEqual(["t", "host", "cpu 12% · mem 40% · gpu 0%"]);
  });

  it("renders a review-ruling row naming stage/bundle/ruling/seconds", () => {
    const r: RawRecord = {
      ts: "t",
      action: "step.result",
      payload: { step_id: "review-ruling", stage: "judge", pass: 1, bundle_id: "short-id", ruling: "confirmed", seconds: 2.3456 },
    };
    expect(labFeedRowLines(norm(r))).toEqual(["t", "judge", "short-id pass1 → confirmed (2.3s)"]);
  });

  it("truncates a long bundle_id at 18 chars with an ellipsis (labShortId)", () => {
    const r: RawRecord = {
      ts: "t",
      action: "step.result",
      payload: { step_id: "review-ruling", stage: "judge", pass: 1, bundle_id: "someVerb@some/path.ts", ruling: "confirmed", seconds: 2.3456 },
    };
    expect(labFeedRowLines(norm(r))).toEqual(["t", "judge", "someVerb@some/path… pass1 → confirmed (2.3s)"]);
  });

  it("renders a plain stage-completion step-result row", () => {
    const r: RawRecord = { ts: "t", action: "step.result", payload: { step_id: "probe", wall_ms: 500 } };
    expect(labFeedRowLines(norm(r))).toEqual(["t", "probe", "500ms"]);
  });

  it("falls back to a bare action line for anything else", () => {
    const r: RawRecord = { ts: "t", action: "task started" };
    expect(labFeedRowLines(norm(r))).toEqual(["t", "", "task started"]);
  });
});

describe("labFeedLines", () => {
  it("is empty for no events", () => {
    expect(labFeedLines([])).toEqual([]);
  });

  it("renders newest-first, flattened", () => {
    const events: RawRecord[] = [
      { ts: "1", action: "task started" },
      { ts: "2", action: "task ended" },
    ];
    expect(labFeedLines(normAll(events))).toEqual(["2", "", "task ended", "1", "", "task started"]);
  });

  it("caps at LAB_FEED_CAP, keeping the newest", () => {
    const events: RawRecord[] = Array.from({ length: LAB_FEED_CAP + 10 }, (_, i) => ({
      ts: String(i),
      action: "task started",
    }));
    const lines = labFeedLines(normAll(events));
    // 3 lines per event.
    expect(lines.length).toBe(LAB_FEED_CAP * 3);
    // Newest (highest ts) first.
    expect(lines[0]).toBe(String(LAB_FEED_CAP + 9));
  });
});

describe("labFeedCountText", () => {
  it("names the raw total under the cap", () => {
    expect(labFeedCountText(0)).toBe("0 records");
    expect(labFeedCountText(1)).toBe("1 record");
    expect(labFeedCountText(10)).toBe("10 records");
    expect(labFeedCountText(LAB_FEED_CAP)).toBe(`${LAB_FEED_CAP} records`);
  });

  it("discloses the truncation once the raw total exceeds the cap, matching legacy's #1640 wording", () => {
    expect(labFeedCountText(LAB_FEED_CAP + 1)).toBe(`newest ${LAB_FEED_CAP} of ${LAB_FEED_CAP + 1}`);
    expect(labFeedCountText(900)).toBe(`newest ${LAB_FEED_CAP} of 900`);
  });
});

describe("labFeedStatusSuffix", () => {
  it("is playback once finished, regardless of the unreachable flag", () => {
    expect(labFeedStatusSuffix(true, false)).toBe(" (playback)");
    expect(labFeedStatusSuffix(true, true)).toBe(" (playback)");
  });

  it("is the live-polling suffix when not finished and reachable", () => {
    expect(labFeedStatusSuffix(false, false)).toBe(" — live, polling");
  });

  it("names the daemon-unreachable state when not finished and the poll has failed repeatedly", () => {
    expect(labFeedStatusSuffix(false, true)).toBe(" — daemon unreachable, retrying");
  });
});

describe("labCliHint", () => {
  // (4.0) The hint used to print `--funnel --roster-profile … --exec-mode …`,
  // flags `lab eval` no longer has (clap rejects them). It is rebuilt from
  // what `scores.json` actually records: `role`, `mode`, and the profile.
  it("a strict run names its role, cases dir, and profile", () => {
    expect(labCliHint({ role: "coder", mode: "strict", provenance: { profile: "fast" } })).toBe(
      "darkmux lab eval coder --cases-dir <cases-dir> --profile fast",
    );
  });

  it("an experimental mode keeps the default role and adds its own flags", () => {
    expect(labCliHint({ role: "pr-reviewer", mode: "agentic" })).toBe(
      "darkmux lab eval --cases-dir <cases-dir> --agentic --workdirs <workdirs-root>",
    );
    expect(labCliHint({ role: "pr-reviewer", mode: "dialectic" })).toBe(
      "darkmux lab eval --cases-dir <cases-dir> --dialectic --workdirs <workdirs-root>",
    );
    expect(labCliHint({ role: "pr-reviewer", mode: "freeform" })).toBe(
      "darkmux lab eval --cases-dir <cases-dir> --freeform",
    );
  });

  it("with no scores yet, placeholders only — and never a removed flag", () => {
    const hint = labCliHint(null);
    expect(hint).toBe("darkmux lab eval <role> --cases-dir <cases-dir>");
    for (const gone of ["--funnel", "--roster-profile", "--exec-mode", "--k", "--bundler"]) {
      expect(hint).not.toContain(gone);
    }
  });
});

describe("labBadgeText", () => {
  it("names finished vs live", () => {
    expect(labBadgeText(true)).toBe("finished");
    expect(labBadgeText(false)).toBe("running");
  });

  it("names the daemon-unreachable state only while live, never overriding finished", () => {
    expect(labBadgeText(false, true)).toBe("⚠ daemon unreachable — retrying");
    expect(labBadgeText(true, true)).toBe("finished");
  });
});
