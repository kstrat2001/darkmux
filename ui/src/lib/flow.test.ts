import { describe, it, expect, vi, afterEach } from "vitest";
import { parseFlowJsonl, fetchStaticFlowRecords, firstRecordDate } from "./flow";

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
