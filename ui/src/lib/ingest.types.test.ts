// Compile-time proof that a record's tagged fields cannot be matched as
// strings. Every `@ts-expect-error` below is a shape that used to typecheck
// (and one that a source scan missed): if a change to `ingest.ts` lets any of
// them compile again, `tsc` fails on the now-unused directive. The runtime
// assertions only keep vitest from reporting an empty file.
import { describe, expect, it } from "vitest";
import { ACTION, CATEGORY, LEVEL, STAGE, TIER, tagText, type NormRecord } from "./ingest";
import { norm } from "../testing/records";

const r: NormRecord = norm({ ts: "2026-09-27T10:00:00Z", action: "dispatch.start", level: "info", category: "work", stage: "dispatch", tier: "local" });

describe("the tagged fields are opaque to string logic", () => {
  it("rejects a literal on every field and in every comparing shape", () => {
    const hits: boolean[] = [];
    // @ts-expect-error a literal compared to the action
    hits.push(r.action === "dispatch.start");
    // @ts-expect-error a literal compared to the level
    hits.push(r.level === "info");
    // @ts-expect-error a literal compared to the category
    hits.push(r.category !== "telemetry");
    // @ts-expect-error a literal compared to the stage
    hits.push(r.stage == "dispatch");
    // @ts-expect-error a literal compared to the tier
    hits.push("local" === r.tier);
    // @ts-expect-error element access is the same field
    hits.push(r["action"] === "dispatch.start");
    const { action, category } = r;
    // @ts-expect-error a destructured field is the same field
    hits.push(action === "dispatch.start");
    // @ts-expect-error nor the destructured category
    hits.push(category === "work");
    // @ts-expect-error `.includes` on a string list
    hits.push(["dispatch.start"].includes(r.action));
    // @ts-expect-error `.has` on a string set
    hits.push(new Set(["dispatch.start"]).has(r.action));
    // @ts-expect-error a regex test
    hits.push(/^dispatch\./.test(r.action));
    // @ts-expect-error a string method
    hits.push(!!r.action?.startsWith("dispatch.")); // eslint-disable-line @typescript-eslint/no-unsafe-call
    // @ts-expect-error a string method on the category
    hits.push(!!r.category?.endsWith("metry")); // eslint-disable-line @typescript-eslint/no-unsafe-call
    const byName: Record<string, boolean> = { "dispatch.start": true };
    // @ts-expect-error the field as a map key
    hits.push(!!byName[r.action!]);
    // A comparison split across lines is still one comparison.
    hits.push(
      // @ts-expect-error
      r.action ===
        "dispatch.start",
    );
    switch (r.action) {
      // @ts-expect-error a literal case
      case "dispatch.start":
        hits.push(true);
    }
    expect(hits.length).toBeGreaterThan(0);
  });

  it("accepts the constants, and the text only through tagText", () => {
    expect(r.action === ACTION.DispatchStart).toBe(true);
    expect(r.level === LEVEL.Info && r.category === CATEGORY.Work && r.stage === STAGE.Dispatch && r.tier === TIER.Local).toBe(true);
    expect(tagText(r.action)).toBe("dispatch.start");
    // @ts-expect-error fields of different kinds never compare
    expect(r.level === CATEGORY.Work).toBe(false);
  });
});
