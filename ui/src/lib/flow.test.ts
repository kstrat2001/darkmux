import { describe, it, expect, vi, afterEach } from "vitest";
import {
  parseFlowJsonl,
  fetchStaticFlowRecords,
  firstRecordDate,
  machineNames,
  localMachineUid,
  nameOf,
  buildFlowWindow,
  liveSessionSet,
  flowLiveSessions,
  bodyTruncated,
  asRecordArray,
} from "./flow";
import { tokensOffMeter } from "../lenses/fleet/savings";
import type { FlowRecord } from "../types/handwritten";

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

  const tokenRecord: FlowRecord = {
    ts,
    session_id: "s-live",
    action: "dispatch.complete",
    category: "telemetry",
    source: "tokens",
    payload: { total_tokens: 300, prompt_tokens: 250, completion_tokens: 50 },
  };

  it("a record fed twice (identical recKey) collapses to one", () => {
    const result = buildFlowWindow([], [tokenRecord, { ...tokenRecord }], nowMs);
    expect(result).toHaveLength(1);
  });

  it("distinct records (different session_id) both survive — this is dedup, not dedup-by-content", () => {
    const other: FlowRecord = { ...tokenRecord, session_id: "s-other" };
    const result = buildFlowWindow([], [tokenRecord, other], nowMs);
    expect(result).toHaveLength(2);
  });

  it("tokensOffMeter over a re-delivered-record window does not double-count (#794)", () => {
    const start: FlowRecord = { ts, session_id: "s-live", action: "dispatch.start", handle: "coder" };
    // Simulates the SSE at-least-once redelivery `startFlowTail` does nothing
    // to prevent: the identical telemetry record appears twice in what
    // `useFlowWindow` hands to `buildFlowWindow`.
    const window = buildFlowWindow([], [start, tokenRecord, { ...tokenRecord }], nowMs);
    const meter = tokensOffMeter(window);
    expect(meter.total).toBe(300);
    expect(meter.local).toBe(300);
  });

  it("RED-PROVE: without the dedup filter, the same window WOULD double-count (documents what buildFlowWindow prevents)", () => {
    const start: FlowRecord = { ts, session_id: "s-live", action: "dispatch.start", handle: "coder" };
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
    // The point of this test is `localMachineUid`, and it needs `nameOf` and
    // specs to DISAGREE — otherwise it passes without exercising anything.
    //
    // (#2030) `nameOf` now answers with the most RECENT alias rather than the
    // first one listed, so the divergence is built the other way round: the
    // window's newest record says "MacBook-Pro.local" while `/machine/specs`
    // still reports "MacBook-Pro". Simply flipping the expected string would
    // have left both sides agreeing and quietly made this vacuous.
    const data = [rec(UID, "MacBook-Pro", "2026-08-13T09:00:00Z"), rec(UID, "MacBook-Pro.local", "2026-08-13T22:00:00Z")];
    const live = new Map([beat(UID, "MacBook-Pro")]);
    expect(nameOf(data, live, UID)).toBe("MacBook-Pro.local");
    // specs reports the OTHER alias, and identity still resolves.
    expect(localMachineUid(data, live, "MacBook-Pro")).toBe(UID);
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

// ── nameOf resolves the CURRENT name, not the first one seen (#2030) ──────
describe("nameOf recency", () => {
  const UID = "F9ACF59C-0E8B-5092-A6B4-7C07070737D2";
  const rec = (ts: string, machine_id: string): FlowRecord =>
    ({ ts, machine_id, machine_uid: UID, action: "machine.online" }) as unknown as FlowRecord;

  it("a single stale record cannot outvote every later one", () => {
    // The operator's actual case: one stray record naming a different machine
    // landed in their flow directory, and their machine page showed that name
    // from then on. Hundreds of correct records arrived afterwards and none
    // displaced it, because the lookup took the FIRST match in the window.
    const data = [
      rec("2026-08-26T13:42:41Z", "m5-ultra-256gb"), // the stray, listed first
      rec("2026-08-27T09:00:00Z", "MacBook-Pro"),
      rec("2026-08-27T21:19:40Z", "MacBook-Pro"),
    ];
    expect(nameOf(data, new Map(), UID)).toBe("MacBook-Pro");
  });

  it("still tracks a genuine rename, in either listing order", () => {
    // The behaviour the old code was reaching for — a machine really does
    // change names — must survive. Newest wins regardless of array order.
    const older = rec("2026-08-01T00:00:00Z", "MacBook-Pro.local");
    const newer = rec("2026-08-27T00:00:00Z", "MacBook-Pro");
    expect(nameOf([older, newer], new Map(), UID)).toBe("MacBook-Pro");
    expect(nameOf([newer, older], new Map(), UID)).toBe("MacBook-Pro");
  });

  it("falls back to the presence beat, then the uid, when no record names it", () => {
    expect(nameOf([], new Map([[UID, { display_name: "studio" } as never]]), UID)).toBe("studio");
    expect(nameOf([], new Map(), UID)).toBe(UID);
  });

  it("keeps the first match when timestamps are unusable, rather than picking arbitrarily", () => {
    const a = rec("not-a-date", "first");
    const b = rec("also-bad", "second");
    expect(nameOf([a, b], new Map(), UID)).toBe("first");
  });
});

/**
 * (#2123) Presence (`/fleet/sessions/live`, Redis-backed) only ever gets a
 * beat from `dispatch.internal`'s own container-heartbeat thread
 * (`crates/darkmux-crew/src/dispatch_internal.rs` — the ONE writer,
 * grep-confirmed). A mission/review dispatch fanning out through
 * `dispatch.map` (hosted/remote probe + judge seats — no container, no
 * heartbeat) never writes a beat for ANY of its sessions. On a
 * Redis-enabled multi-machine fleet (the operator's real topology — a
 * Studio hub whose OWN `dispatch.internal` work keeps presence non-empty
 * almost continuously), the pre-fix `liveSessionSet` treated ANY non-empty
 * presence set as globally authoritative and never even looked at the
 * flow-derived fallback — so a genuinely-live review mission's own session,
 * never beaten, read as not-running. This is the fleet card's "0 running"
 * / machine card stuck "idle" half of #2123 (the Runs-lens listing itself
 * is a SEPARATE, server-side path — see `crates/darkmux-serve/src/runs.rs`'s
 * `build_runs_2123_*` tests, which prove that side was already correct on
 * main; the operator's daemon was almost certainly serving a stale binary
 * built before an earlier session-liveness fix).
 */
describe("liveSessionSet — presence coverage is partial, not all-or-nothing (#2123)", () => {
  const NOW = Date.parse("2026-08-29T16:07:30Z");

  /** A review-shaped session: `dispatch start` a few minutes ago, no
   * terminal record, fresh telemetry — exactly what `flowLiveSessions`
   * needs to call it live. Uses the space-separated `darkmux-crew`/CLI
   * vocabulary (`buildFlowWindow`/`normalizeRecords` is what dots it in
   * production; these fixtures are ALREADY normalized, matching how every
   * consumer of `flowWindow.data` actually receives them). */
  const reviewSession: FlowRecord[] = [
    { ts: "2026-08-29T15:46:31Z", session_id: "owner/repo@deadbeef", action: "dispatch.start" },
    { ts: "2026-08-29T16:07:12Z", session_id: "owner/repo@deadbeef", action: "telemetry.process" },
  ];

  it("BEFORE the fix would have shadowed a genuinely-live session under unrelated presence (regression guard)", () => {
    // Presence has a beat, but for a DIFFERENT session entirely — the
    // Studio hub's own dispatch.internal work, not this machine's review
    // mission.
    const presence = new Set(["some-other-machines-dispatch-internal-session"]);
    const result = liveSessionSet(reviewSession, presence, NOW, true);
    expect(result.has("owner/repo@deadbeef")).toBe(true);
    // Presence's own coverage must still be honored, not discarded.
    expect(result.has("some-other-machines-dispatch-internal-session")).toBe(true);
  });

  it("still returns the flow-derived set untouched when presence is empty (Redis off/degraded)", () => {
    const result = liveSessionSet(reviewSession, new Set(), NOW, true);
    expect(result).toEqual(flowLiveSessions(reviewSession, NOW, true));
  });

  it("still returns presence untouched when the flow-derived fallback finds nothing live (e.g. everything already terminal)", () => {
    const terminal: FlowRecord[] = [
      { ts: "2026-08-29T15:46:31Z", session_id: "owner/repo@deadbeef", action: "dispatch.start" },
      { ts: "2026-08-29T15:50:00Z", session_id: "owner/repo@deadbeef", action: "dispatch.complete" },
    ];
    const presence = new Set(["some-other-session"]);
    expect(liveSessionSet(terminal, presence, NOW, true)).toEqual(presence);
  });

  it("replay mode stays presence-agnostic (flowLiveSessions itself gates off liveMode)", () => {
    const presence = new Set(["a-replayed-session"]);
    expect(liveSessionSet(reviewSession, presence, NOW, false)).toEqual(presence);
  });
});

describe("bodyTruncated + asRecordArray through the guard (#2206)", () => {
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

  it("asRecordArray: array passthrough, object envelope unwrap, everything else empty", () => {
    const recs = [{ ts: "2026-08-19T00:00:00Z" }];
    expect(asRecordArray(recs)).toBe(recs);
    expect(asRecordArray({ records: recs })).toEqual(recs);
    expect(asRecordArray({ flow: recs })).toEqual(recs);
    expect(asRecordArray({})).toEqual([]);
    expect(asRecordArray(null)).toEqual([]);
    expect(asRecordArray(undefined)).toEqual([]);
    expect(asRecordArray("nope")).toEqual([]);
    expect(asRecordArray(42)).toEqual([]);
  });
});
