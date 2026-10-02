import { describe, it, expect, vi, afterEach } from "vitest";
import { fetchStaticFlowRecords, firstRecordDate, buildFlowWindow, bodyTruncated, machPresent, missionReplayDate } from "./flow";
import { ingest, ingestJsonl, recordsAsOf, __asOfFilterRuns, type NormRecord } from "./ingest";
import { norm, normAll, type RawRecord } from "../testing/records";
import { tokensOffMeter } from "../lenses/fleet/savings";
import { DEFAULT_POLICY, lifecycleAt } from "./lifecycle";
import { runIndex, sessionRun, __runIndexBuilds } from "./runRef";

/**
 * (#1801) The static-demo record pipeline: `ingestJsonl` (the flowSrc
 * branch's own line-by-line parse),
 * `fetchStaticFlowRecords` (the GET + parse, its own silent-empty-on-failure
 * contract), and `firstRecordDate` (the RAW[0].ts date derivation,
 *). All three are exercised indirectly by
 * `useRouteRecords.test.tsx`/`PlaybackLens.test.tsx`'s static-mode cases;
 * these cover the pure-function edges those integration tests don't reach on
 * their own (a malformed line, a CRLF file, a schema-header-first file).
 */

describe("ingestJsonl", () => {
  const A = { ts: "2026-08-07T00:00:00Z", action: "a", tMs: Date.parse("2026-08-07T00:00:00Z") };
  const B = { ts: "2026-08-07T00:00:01Z", action: "b", tMs: Date.parse("2026-08-07T00:00:01Z") };

  it("parses one record per line", () => {
    const text = '{"ts":"2026-08-07T00:00:00Z","action":"a"}\n{"ts":"2026-08-07T00:00:01Z","action":"b"}';
    expect(ingestJsonl(text)).toEqual([A, B]);
  });

  it("drops blank lines (including a trailing newline) rather than choking on them", () => {
    const text = '{"ts":"2026-08-07T00:00:00Z","action":"a"}\n\n   \n{"ts":"2026-08-07T00:00:01Z","action":"b"}\n';
    expect(ingestJsonl(text)).toHaveLength(2);
  });

  it("handles CRLF line endings the same as LF", () => {
    const text = '{"ts":"2026-08-07T00:00:00Z","action":"a"}\r\n{"ts":"2026-08-07T00:00:01Z","action":"b"}\r\n';
    expect(ingestJsonl(text)).toHaveLength(2);
  });

  it("drops a line that fails to parse rather than failing the whole file", () => {
    const text = '{"ts":"2026-08-07T00:00:00Z","action":"a"}\nnot json at all\n{"ts":"2026-08-07T00:00:01Z","action":"b"}';
    expect(ingestJsonl(text)).toEqual([A, B]);
  });

  it("drops the leading {\"_type\":\"schema\"} header line at the boundary", () => {
    const text = '{"_type":"schema"}\n{"ts":"2026-08-07T00:00:00Z","action":"a"}';
    expect(ingestJsonl(text)).toEqual([A]);
  });

  it("an empty file parses to an empty array", () => {
    expect(ingestJsonl("")).toEqual([]);
  });
});

describe("firstRecordDate", () => {
  it("derives the UTC day from the first record's parsed time, in file order", () => {
    expect(firstRecordDate(normAll([{ ts: "2026-08-07T02:09:42.000Z" }, { ts: "2026-08-01T00:00:00Z" }]))).toBe("2026-08-07");
    // An offset timestamp names its UTC day, not the local date in its text.
    expect(firstRecordDate(normAll([{ ts: "2026-08-07T22:00:00-05:00" }]))).toBe("2026-08-08");
  });

  it("is null for an empty array — a caller supplies its own placeholder", () => {
    expect(firstRecordDate([])).toBeNull();
  });

  it("is null when the first record has no ts: it reads the first record, not the earliest", () => {
    expect(firstRecordDate(normAll([{ action: "operator.note" }, { ts: "2026-08-07T00:00:00Z" }]))).toBeNull();
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
    expect(records).toEqual([{ ts: "2026-08-07T00:00:00Z", action: "a", tMs: Date.parse("2026-08-07T00:00:00Z") }]);
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
 * (#794 regression coverage, restored post-#1806) The live SSE tail must be
 * idempotent — a re-delivered record (reconnect / snapshot-stream overlap;
 * `/flow/:date/stream` is at-least-once, and `startFlowTail`,
 * `lib/sse.ts:74`, appends every message it receives with NO dedup of its
 * own) must not be double-counted. On the port that guarantee lives
 * entirely in `buildFlowWindow`'s `seen`-Set filter — `useFlowWindow`
 * concatenates the raw day-fetch with whatever `useLiveTail` has appended
 * to the SSE-tail cache slot and feeds the result through
 * `buildFlowWindow` before anything (including the savings hero) reads it.
 * These tests exercise that filter directly, and the consumer (
 * `tokensOffMeter`) that would silently double-count without it — legacy's
 * equivalent coverage (`live_tail_dedups_records`, source-text assertions
 * against `SEEN_KEYS`/`recKey` in `viewer.html`) retired with that file
 * (#1806); this is its behavioral replacement against the port's own dedup
 * boundary.
 */
describe("buildFlowWindow dedup (#794)", () => {
  const ts = "2026-08-08T00:00:00.000Z";
  const nowMs = Date.parse(ts);

  const tokenRecord: NormRecord = norm({
    ts,
    session_id: "s-live",
    action: "dispatch.complete",
    category: "telemetry",
    source: "tokens",
    payload: { total_tokens: 300, prompt_tokens: 250, completion_tokens: 50 },
  });

  it("a record fed twice (identical recKey) collapses to one", () => {
    const result = buildFlowWindow([], [tokenRecord, { ...tokenRecord }], nowMs);
    expect(result).toHaveLength(1);
  });

  it("distinct records (different session_id) both survive — this is dedup, not dedup-by-content", () => {
    const other: NormRecord = { ...tokenRecord, session_id: "s-other" };
    const result = buildFlowWindow([], [tokenRecord, other], nowMs);
    expect(result).toHaveLength(2);
  });

  it("tokensOffMeter over a re-delivered-record window does not double-count (#794)", () => {
    const start = norm({ ts, session_id: "s-live", action: "dispatch.start", handle: "coder" });
    // Simulates the SSE at-least-once redelivery `startFlowTail` does nothing
    // to prevent: the identical telemetry record appears twice in what
    // `useFlowWindow` hands to `buildFlowWindow`.
    const window = buildFlowWindow([], [start, tokenRecord, { ...tokenRecord }], nowMs);
    const meter = tokensOffMeter(window);
    expect(meter.total).toBe(300);
  });

  it("RED-PROVE: without the dedup filter, the same window WOULD double-count (documents what buildFlowWindow prevents)", () => {
    const start = norm({ ts, session_id: "s-live", action: "dispatch.start", handle: "coder" });
    // The undeduped shape `startFlowTail`'s append actually produces —
    // straight concatenation, no `seen`-Set. If `buildFlowWindow` ever loses
    // its dedup filter, this is the number the savings hero would show.
    const undeduped = [start, tokenRecord, { ...tokenRecord }];
    const meter = tokensOffMeter(undeduped);
    expect(meter.total).toBe(600);
    // The real path never sees this — buildFlowWindow always runs first.
    const deduped = buildFlowWindow([], [start, tokenRecord, { ...tokenRecord }], nowMs);
    expect(tokensOffMeter(deduped).total).toBe(300);
  });
});

/**
 * (#2123) Presence (`/fleet/dispatches/live`, Redis-backed) only ever gets a
 * beat from `dispatch.internal`'s own container-heartbeat thread
 * (`crates/darkmux-crew/src/dispatch_internal.rs` — the ONE writer,
 * grep-confirmed). A mission/review dispatch fanning out through
 * `dispatch.map` (hosted/remote probe + judge seats — no container, no
 * heartbeat) never writes a beat for ANY of its sessions. On a
 * Redis-enabled multi-machine fleet (the operator's real topology — a
 * Studio hub whose OWN `dispatch.internal` work keeps presence non-empty
 * almost continuously), the pre-#2123 live-set merge treated ANY non-empty
 * presence set as globally authoritative and never even looked at the
 * flow-derived fallback — so a genuinely-live review mission's own session,
 * never beaten, read as not-running. This is the fleet card's "0 running"
 * / machine card stuck "idle" half of #2123 (the Runs-lens listing itself
 * is a SEPARATE, server-side path — see `crates/darkmux-serve/src/runs.rs`'s
 * `build_runs_2123_*` tests, which prove that side was already correct on
 * main; the operator's daemon was almost certainly serving a stale binary
 * built before an earlier session-liveness fix).
 */
describe("presence coverage is partial, not all-or-nothing (#2123)", () => {
  const NOW = Date.parse("2026-08-29T16:07:30Z");

  /** A review-shaped session: `dispatch.start` a few minutes ago, no
   * terminal record, fresh telemetry: open on its own records. */
  const reviewSession: NormRecord[] = normAll([
    { ts: "2026-08-29T15:46:31Z", session_id: "owner/repo@deadbeef", action: "dispatch.start" },
    { ts: "2026-08-29T16:07:12Z", session_id: "owner/repo@deadbeef", action: "machine.telemetry" },
  ]);
  const phase = (data: NormRecord[], presence: Set<string>) =>
    lifecycleAt(sessionRun(data, "owner/repo@deadbeef", NOW)!, NOW, DEFAULT_POLICY, presence).phase;

  it("unrelated presence never shadows a run its own records hold open (regression guard)", () => {
    // Presence has a beat, but for a DIFFERENT session entirely — the
    // Studio hub's own dispatch.internal work, not this machine's review
    // mission.
    expect(phase(reviewSession, new Set(["some-other-machines-dispatch-internal-session"]))).toBe("open");
  });

  it("with presence empty (Redis off/degraded) the records alone decide", () => {
    expect(phase(reviewSession, new Set())).toBe("open");
  });

  it("presence never reopens a run its records closed", () => {
    const terminal: NormRecord[] = normAll([
      { ts: "2026-08-29T15:46:31Z", session_id: "owner/repo@deadbeef", action: "dispatch.start" },
      { ts: "2026-08-29T15:50:00Z", session_id: "owner/repo@deadbeef", action: "dispatch.complete" },
    ]);
    expect(phase(terminal, new Set(["owner/repo@deadbeef"]))).toBe("closed");
  });
});

describe("bodyTruncated + ingest's body shapes through the guard (#2206)", () => {
  it("bodyTruncated: only a plain-object body with a truthy flag reads truncated", () => {
    expect(bodyTruncated({ truncated: true })).toBe(true);
    expect(bodyTruncated({ truncated: false })).toBe(false);
    expect(bodyTruncated({})).toBe(false);
    // Every non-object body — including the bare-array shape the other
    // `/flow/...` callers produce — reads false, exactly as the original
    // `!body || typeof body !== "object" || Array.isArray(body)` did.
    expect(bodyTruncated([])).toBe(false);
    expect(bodyTruncated([{ truncated: true }])).toBe(false);
    expect(bodyTruncated(null)).toBe(false);
    expect(bodyTruncated(undefined)).toBe(false);
    expect(bodyTruncated("truncated")).toBe(false);
    expect(bodyTruncated(0)).toBe(false);
  });

  it("ingest: bare array, object envelope unwrap, everything else empty", () => {
    const recs = [{ ts: "2026-08-19T00:00:00Z" }];
    const one = [{ ts: "2026-08-19T00:00:00Z", tMs: Date.parse("2026-08-19T00:00:00Z") }];
    expect(ingest(recs)).toEqual(one);
    expect(ingest({ records: recs })).toEqual(one);
    expect(ingest({ flow: recs })).toEqual(one);
    expect(ingest({})).toEqual([]);
    expect(ingest(null)).toEqual([]);
    expect(ingest(undefined)).toEqual([]);
    expect(ingest("nope")).toEqual([]);
    expect(ingest(42)).toEqual([]);
  });
});

describe("missionReplayDate (header owns liveness — a RUNNING mission is live, not a recording)", () => {
  // Mission lifecycle records ride the mission's own session (`mission-<id>`).
  const rec = (action: string, ts: string) => norm({ ts, action, session_id: "mission-m", mission_id: "m" });
  it("is null while the mission has no terminal record, whatever day its records carry", () => {
    const records = [rec("mission.start", "2026-09-03T17:10:00Z"), rec("dispatch.start", "2026-09-03T17:11:00Z")];
    expect(missionReplayDate(records)).toBeNull();
  });
  it("is the earliest record's day once the mission has closed", () => {
    const records = [
      rec("dispatch.complete", "2026-09-03T18:00:00Z"),
      rec("mission.start", "2026-09-03T17:10:00Z"),
      rec("mission.close", "2026-09-03T18:01:00Z"),
    ];
    expect(missionReplayDate(records)).toBe("2026-09-03");
  });
  it("treats an aborted mission as a recording too", () => {
    expect(missionReplayDate([rec("mission.start", "2026-09-02T01:00:00Z"), rec("mission.abort", "2026-09-02T01:05:00Z")])).toBe("2026-09-02");
  });
  it("is null for an empty slice", () => {
    expect(missionReplayDate([])).toBeNull();
  });
});

describe("(#2911) runIndex: the per-window run index", () => {
  const rec = (sid: unknown, action: string, ts: string) => norm({ ts, session_id: sid, action } as RawRecord);
  const dup = rec("b", "dispatch.turn", "2026-09-26T10:00:04Z");
  const data: NormRecord[] = [
    rec("a", "dispatch.start", "2026-09-26T10:00:00Z"),
    rec("b", "dispatch.start", "2026-09-26T10:00:01Z"),
    rec(undefined, "machine.telemetry", "2026-09-26T10:00:02Z"),
    rec("", "dispatch.start", "2026-09-26T10:00:02Z"),
    rec("a", "dispatch.complete", "2026-09-26T10:00:03Z"),
    // The same record object twice: the index keeps it twice, in place.
    dup,
    rec("a", "dispatch.turn", "2026-09-26T10:00:05Z"),
    dup,
  ];
  const recordsOf = (d: readonly NormRecord[], sid: string) => runIndex(d).groupsOfSession(sid).flatMap((g) => g.records);

  it("groups exactly the records a whole-window scan finds, in window order", () => {
    for (const sid of ["a", "b", "missing"]) {
      expect(recordsOf(data, sid)).toEqual(data.filter((r) => r.session_id === sid));
    }
    expect(recordsOf(data, "b")).toEqual([data[1], dup, dup]);
  });

  it("a record with no session id belongs to no run", () => {
    expect(runIndex(data).groups.every((g) => g.sessionId !== "")).toBe(true);
    expect(runIndex(data).groupOf(data[2])).toBeNull();
  });

  it("hands out groups as readonly, so a push cannot corrupt later lookups", () => {
    // Type-level: `bun run typecheck` fails if a group widens back to a
    // mutable array (the directive below would then be unused). Never run.
    const typeOnly = () => {
      // @ts-expect-error a run's records are readonly
      runIndex(data).groupsOfSession("a")[0].records.push(data[0]); // eslint-disable-line @typescript-eslint/no-unsafe-call
    };
    void typeOnly;
  });

  // Pins that a lifecycle reads a run's records THROUGH the index (the #2911
  // cost fix), not with a whole-window scan that happens to agree. The
  // instrument deliberately breaks the never-mutated-after-read contract: a
  // record appended after the index is built is invisible to the index and
  // visible to any scan, so the two routes give different answers.
  it("answers from the window's index, not a whole-window scan", () => {
    const t = Date.parse("2026-09-26T10:00:10Z");
    const win: NormRecord[] = [rec("x", "dispatch.start", "2026-09-26T10:00:00Z")];
    expect(sessionRun(win, "s", t)).toBeNull();
    win.push(rec("s", "dispatch.start", "2026-09-26T10:00:09Z"));
    expect(sessionRun(win, "s", t)).toBeNull();
    // Control: a fresh array (a fresh index) does see it.
    expect(lifecycleAt(sessionRun([...win], "s", t)!, t, DEFAULT_POLICY).phase).toBe("open");
  });

  it("indexes a window once, and a new window array gets its own index", () => {
    const win = [...data];
    const before = __runIndexBuilds();
    const first = runIndex(win);
    expect(runIndex(win)).toBe(first);
    expect(__runIndexBuilds()).toBe(before + 1);
    const next = [...win, rec("a", "dispatch.turn", "2026-09-26T10:00:04Z")];
    expect(recordsOf(next, "a")).toHaveLength(4);
    expect(__runIndexBuilds()).toBe(before + 2);
  });
});

describe("(#2911) recordsAsOf: the window as of now, without a filter per tick", () => {
  const at = (ts: string, handle: string) => norm({ ts, action: "operator.note", handle });
  const t = (ts: string) => Date.parse(ts);

  it("matches filter(ts <= now) at every now; a ts that does not parse is kept at every now (bad-timestamp policy)", () => {
    const win = [
      at("2026-09-26T10:00:00Z", "a"),
      at("2026-09-26T10:00:05Z", "b"),
      at("not a date", "bad"),
      at("2026-09-26T10:00:05Z", "b2"),
      at("2026-09-26T10:00:09Z", "c"),
    ];
    for (let s = -1; s <= 11; s++) {
      const now = t("2026-09-26T10:00:00Z") + s * 1000;
      expect(recordsAsOf(win, now)).toEqual(win.filter((r) => r.tMs === null || r.tMs <= now));
      expect(recordsAsOf(win, now).map((r) => r.handle)).toContain("bad");
    }
  });

  it("with nothing ahead of now, returns the window itself on every tick, filtering nothing", () => {
    const win = [at("2026-09-26T10:00:00Z", "a"), at("2026-09-26T10:00:05Z", "b")];
    const runs = __asOfFilterRuns();
    for (let s = 5; s < 10; s++) {
      expect(recordsAsOf(win, t("2026-09-26T10:00:00Z") + s * 1000)).toBe(win);
    }
    expect(__asOfFilterRuns()).toBe(runs);
  });

  it("with records ahead, filters once, reuses it until now crosses the next one, then re-filters", () => {
    const win = [
      at("2026-09-26T10:00:00Z", "a"),
      at("2026-09-26T10:00:05Z", "b"),
      at("2026-09-26T10:00:09Z", "c"),
    ];
    const runs = __asOfFilterRuns();
    const first = recordsAsOf(win, t("2026-09-26T10:00:01Z"));
    expect(first.map((r) => r.handle)).toEqual(["a"]);
    expect(__asOfFilterRuns()).toBe(runs + 1);
    // Ticks that cross nothing: same array, no filter.
    expect(recordsAsOf(win, t("2026-09-26T10:00:02Z"))).toBe(first);
    expect(recordsAsOf(win, t("2026-09-26T10:00:04.999Z"))).toBe(first);
    expect(__asOfFilterRuns()).toBe(runs + 1);
    // Crossing b: one re-filter, then stable again.
    const second = recordsAsOf(win, t("2026-09-26T10:00:05Z"));
    expect(second.map((r) => r.handle)).toEqual(["a", "b"]);
    expect(recordsAsOf(win, t("2026-09-26T10:00:08Z"))).toBe(second);
    expect(__asOfFilterRuns()).toBe(runs + 2);
    // Crossing the last one: the window itself.
    expect(recordsAsOf(win, t("2026-09-26T10:00:09Z"))).toBe(win);
    // A clock that steps back below an included record re-filters rather
    // than serving a result that still holds it.
    expect(recordsAsOf(win, t("2026-09-26T10:00:04Z")).map((r) => r.handle)).toEqual(["a"]);
    expect(__asOfFilterRuns()).toBe(runs + 3);
  });
});


describe("machPresent asks about a machine by uid, in either case", () => {
  const UID = "00000000-0000-4000-8000-ABCDEF000021";
  it("a presence beat keyed in the other case is the machine being present", () => {
    const beat = { machine_uid: UID.toLowerCase(), display_name: "x", schema_version: "1", beat_ts_ms: 1 } as never;
    expect(machPresent([], new Map([[UID.toLowerCase(), beat]]), Date.now(), UID)).toBe(true);
  });
  it("the machine's own online edge, stamped with the uid in the other case, counts", () => {
    const edge = norm({ ts: "2026-09-01T10:00:00Z", machine_uid: UID.toLowerCase(), action: "machine.online" });
    expect(machPresent([edge], new Map(), Date.parse("2026-09-01T11:00:00Z"), UID)).toBe(true);
  });
  it("another machine's edge says nothing", () => {
    const edge = norm({ ts: "2026-09-01T10:00:00Z", machine_uid: "00000000-0000-4000-8000-ABCDEF000022", action: "machine.online" });
    expect(machPresent([edge], new Map(), Date.parse("2026-09-01T11:00:00Z"), UID)).toBeNull();
  });
});
