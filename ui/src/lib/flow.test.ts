import { describe, it, expect, vi, afterEach } from "vitest";
import {
  parseFlowJsonl,
  fetchStaticFlowRecords,
  firstRecordDate,
  machineNames,
  localMachineUid,
  nameOf,
} from "./flow";

/**
 * (#1801) The static-demo record pipeline: `parseFlowJsonl` (the flowSrc
 * branch's own line-by-line parse, viewer.html:3899-3901),
 * `fetchStaticFlowRecords` (the GET + parse, its own silent-empty-on-failure
 * contract), and `firstRecordDate` (the RAW[0].ts date derivation,
 * viewer.html:3902). All three are exercised indirectly by
 * `useRouteRecords.test.tsx`/`PlaybackLens.test.tsx`'s static-mode cases;
 * these cover the pure-function edges those integration tests don't reach on
 * their own (a malformed line, a CRLF file, a schema-header-first file).
 */

describe("parseFlowJsonl", () => {
  it("parses one record per line", () => {
    const text = '{"ts":"2026-08-07T00:00:00Z","action":"a"}\n{"ts":"2026-08-07T00:00:01Z","action":"b"}';
    expect(parseFlowJsonl(text)).toEqual([
      { ts: "2026-08-07T00:00:00Z", action: "a" },
      { ts: "2026-08-07T00:00:01Z", action: "b" },
    ]);
  });

  it("drops blank lines (including a trailing newline) rather than choking on them", () => {
    const text = '{"ts":"2026-08-07T00:00:00Z","action":"a"}\n\n   \n{"ts":"2026-08-07T00:00:01Z","action":"b"}\n';
    expect(parseFlowJsonl(text)).toHaveLength(2);
  });

  it("handles CRLF line endings the same as LF", () => {
    const text = '{"ts":"2026-08-07T00:00:00Z","action":"a"}\r\n{"ts":"2026-08-07T00:00:01Z","action":"b"}\r\n';
    expect(parseFlowJsonl(text)).toHaveLength(2);
  });

  it("drops a line that fails to parse rather than failing the whole file", () => {
    const text = '{"ts":"2026-08-07T00:00:00Z","action":"a"}\nnot json at all\n{"ts":"2026-08-07T00:00:01Z","action":"b"}';
    expect(parseFlowJsonl(text)).toEqual([
      { ts: "2026-08-07T00:00:00Z", action: "a" },
      { ts: "2026-08-07T00:00:01Z", action: "b" },
    ]);
  });

  it("parses the leading {\"_type\":\"schema\"} header line fine — it is dropped downstream, not here", () => {
    const text = '{"_type":"schema"}\n{"ts":"2026-08-07T00:00:00Z","action":"a"}';
    const parsed = parseFlowJsonl(text);
    expect(parsed).toHaveLength(2);
    expect(parsed[0]).toEqual({ _type: "schema" });
  });

  it("an empty file parses to an empty array", () => {
    expect(parseFlowJsonl("")).toEqual([]);
  });
});

describe("firstRecordDate", () => {
  it("derives the date from the first record's ts (first 10 chars)", () => {
    expect(firstRecordDate([{ ts: "2026-08-07T02:09:42.000Z" } as never])).toBe("2026-08-07");
  });

  it("is null for an empty array — a caller supplies its own placeholder", () => {
    expect(firstRecordDate([])).toBeNull();
  });

  it("is null when the first record has no ts — the schema-header-first quirk legacy also has", () => {
    expect(firstRecordDate([{ _type: "schema" } as never, { ts: "2026-08-07T00:00:00Z" } as never])).toBeNull();
  });
});

describe("fetchStaticFlowRecords", () => {
  afterEach(() => vi.unstubAllGlobals());

  it("fetches and parses the source", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response('{"ts":"2026-08-07T00:00:00Z","action":"a"}\n', { status: 200 })),
    );
    const records = await fetchStaticFlowRecords("./demo-flow.jsonl");
    expect(records).toEqual([{ ts: "2026-08-07T00:00:00Z", action: "a" }]);
  });

  it("is [] on a non-2xx response — no daemon to report a status from", async () => {
    vi.stubGlobal("fetch", vi.fn(async () => new Response("not found", { status: 404 })));
    expect(await fetchStaticFlowRecords("./missing.jsonl")).toEqual([]);
  });

  it("is [] on a network failure — matching legacy's own silent catch", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => {
        throw new Error("network down");
      }),
    );
    expect(await fetchStaticFlowRecords("./demo-flow.jsonl")).toEqual([]);
  });
});

/**
 * A machine's identity is its `machine_uid`; its `machine_id` is only a label,
 * and one machine really does accumulate several. `machine_id` defaults to the
 * hostname, and macOS reports both `MacBook-Pro` and `MacBook-Pro.local`
 * depending on how the daemon was started, so one stable uid ends up with
 * records under both. Renaming a machine does the same.
 *
 * That broke the "which uid is this daemon" lookup, which compared specs'
 * name against `nameOf`'s single answer: `nameOf` returned the older alias,
 * specs reported the current one, nothing matched, and the fallback handed
 * back the NAME as though it were a uid. Downstream, the local machine
 * classified itself as remote on a fleet-card drill — no residency ledger,
 * and a note telling the operator to go view the machine they were already on.
 *
 * The multi-alias case is the point of these tests; the single-alias cases are
 * here so a fix that simply returned the first uid every time would fail.
 */
describe("machineNames / localMachineUid — identity is the uid, not the label", () => {
  const rec = (uid: string, name: string, ts = "2026-08-13T10:00:00Z") =>
    ({ ts, machine_uid: uid, machine_id: name }) as never;
  const beat = (uid: string, display: string): [string, never] =>
    [uid, { machine_uid: uid, display_name: display, schema_version: "1.19.0", beat_ts_ms: 1 } as never];

  const UID = "F9ACF59C-0E8B-5092-A6B4-7C07070737D2";
  const OTHER = "382A2016-41FD-5729-BF22-9C1A91F1BEDD";

  it("collects every alias a uid has used, across records and its presence beat", () => {
    const data = [rec(UID, "MacBook-Pro.local"), rec(UID, "MacBook-Pro"), rec(OTHER, "m1-max-32gb-studio")];
    const live = new Map([beat(UID, "MacBook-Pro")]);
    expect(machineNames(data, live, UID)).toEqual(new Set(["MacBook-Pro.local", "MacBook-Pro"]));
    expect(machineNames(data, live, OTHER)).toEqual(new Set(["m1-max-32gb-studio"]));
  });

  it("finds the uid when specs names an alias that is NOT the one nameOf reports", () => {
    // The regression, exactly: the OLDER alias sorts first, so `nameOf` answers
    // "MacBook-Pro.local" while `/machine/specs` reports "MacBook-Pro".
    const data = [rec(UID, "MacBook-Pro.local", "2026-08-13T09:00:00Z"), rec(UID, "MacBook-Pro", "2026-08-13T22:00:00Z")];
    const live = new Map([beat(UID, "MacBook-Pro")]);
    expect(nameOf(data, live, UID)).toBe("MacBook-Pro.local");
    expect(localMachineUid(data, live, "MacBook-Pro")).toBe(UID);
    // and the other alias resolves to the same machine
    expect(localMachineUid(data, live, "MacBook-Pro.local")).toBe(UID);
  });

  it("still resolves the ordinary single-alias case, and does not match a different machine", () => {
    const data = [rec(UID, "MacBook-Pro"), rec(OTHER, "m1-max-32gb-studio")];
    const live = new Map([beat(UID, "MacBook-Pro"), beat(OTHER, "m1-max-32gb-studio")]);
    expect(localMachineUid(data, live, "MacBook-Pro")).toBe(UID);
    expect(localMachineUid(data, live, "m1-max-32gb-studio")).toBe(OTHER);
  });

  it("falls back to the raw name when no uid has produced a record or beat yet", () => {
    // A freshly booted daemon — the case the `?? machineId` fallback exists for.
    expect(localMachineUid([], new Map(), "MacBook-Pro")).toBe("MacBook-Pro");
    expect(localMachineUid([], new Map(), null)).toBeNull();
  });
});
