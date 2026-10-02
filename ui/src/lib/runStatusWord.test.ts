import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { runStatusLabel } from "../lenses/runs/format";
import type { Run } from "../types/generated/Run";
import { relayedFromText } from "./relayWords";
import type { AbandonReason } from "../types/generated/AbandonReason";
import type { RunStatus } from "../types/generated/RunStatus";

// `darkmux run list` reads the same file (`src/run_list.rs`): one word per
// status on the board, the run page and the console.
const dir = path.dirname(fileURLToPath(import.meta.url));
const shared = JSON.parse(readFileSync(path.join(dir, "../../../tests/fixtures/run-status-words.json"), "utf8")) as {
  statuses: Array<{ status: RunStatus; abandoned_reason?: AbandonReason; not_reporting?: boolean; word: string }>;
  relay: { asked_on_machine: string; text: string };
};

describe("the words shared with `darkmux run list`", () => {
  it.each(shared.statuses.map((c) => [`${c.status} ${c.abandoned_reason ?? ""} ${c.not_reporting ? "not reporting" : ""}`, c] as const))("%s", (_n, c) => {
    // Through the board's own path: the row carries everything the word needs.
    const row: Run = { id: "r", kind: "dispatch", status: c.status, tracked: true, receive_key: 0, abandoned_reason: c.abandoned_reason, not_reporting: c.not_reporting };
    expect(runStatusLabel(row)).toBe(c.word);
  });
  it("covers every run status", () => {
    expect(new Set(shared.statuses.map((c) => c.status))).toEqual(
      new Set<RunStatus>(["planned", "running", "complete", "degraded", "error", "escalated", "abandoned", "unparseable"]),
    );
  });
  it("words a relayed run the same", () => {
    expect(relayedFromText({ asked_on_machine: shared.relay.asked_on_machine })).toBe(shared.relay.text);
  });
});
