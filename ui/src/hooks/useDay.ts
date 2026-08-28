import { useMemo } from "react";
import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../lib/fetcher";
import { queryKeys } from "../lib/queryKeys";
import { getSource } from "../lib/source";
import { asRecordArray, fetchStaticFlowRecords, firstRecordDate, normalizeRecords } from "../lib/flow";
import type { FlowRecord } from "../types/handwritten";

/** (#2086) The loaded DAY this page can replay, from wherever it comes.
 *
 * - A static build has one committed flow file and replays it on every
 *   route: `records` is that file, `date` the build-time day (or the file's
 *   first record's day when no meta names one).
 * - A daemon page has a day only where a date was asked for (`/play/<date>`,
 *   the playback route): `records` is `GET /flow/<date>`.
 * - Anything else — a live daemon route — has nothing to replay: `null`.
 *
 * This replaces five copies of the same two-query shape (a static-file
 * query beside a daemon query with mutually exclusive `enabled` flags) in
 * the shell, the playback lens, the run detail, the runs board and the
 * route-records hook. Every caller shares one cache slot per source
 * (`queryKeys.staticFlowSrc` / `queryKeys.flowDate`), so calling this from
 * several components costs one download, not several. */
export interface Day {
  /** `null` when there is no day to replay (a live route). Normalized:
   * sorted by `ts`, header line dropped, one synthetic per-session runtime
   * row appended (`normalizeRecords`). */
  records: FlowRecord[] | null;
  /** The same day RAW, exactly as parsed. For consumers that synthesize
   * their own derived rows (`flowToRenderModel` appends its own runtime
   * row; feeding it `records` would double it — a review finding on the
   * run detail) or that only need identity fields. */
  raw: FlowRecord[] | null;
  /** True while the day is still downloading; `records` is `null` then too. */
  loading: boolean;
  /** The day being replayed, once known. */
  date: string | null;
  /** A daemon fetch that failed, for the lens that wants to say so. */
  error: { status: number | null; message: string } | null;
}

export function useDay(requestedDate: string | null): Day {
  const source = getSource();
  const flowSrc = source.flow;
  const daemonDate = source.kind === "daemon" ? requestedDate : null;

  const staticQuery = useQuery({
    queryKey: queryKeys.staticFlowSrc(flowSrc ?? ""),
    queryFn: () => fetchStaticFlowRecords(flowSrc ?? ""),
    enabled: flowSrc !== null,
    staleTime: Infinity,
  });
  const dayQuery = useQuery({
    queryKey: queryKeys.flowDate(daemonDate ?? ""),
    queryFn: () => fetchJson<unknown>(`/flow/${encodeURIComponent(daemonDate ?? "")}`),
    enabled: daemonDate !== null,
  });

  return useMemo(() => {
    if (flowSrc !== null) {
      if (staticQuery.data === undefined) return { records: null, raw: null, loading: true, date: source.date, error: null };
      const records = normalizeRecords(staticQuery.data);
      const date = source.date ?? firstRecordDate(staticQuery.data);
      return { records, raw: staticQuery.data, loading: false, date, error: null };
    }
    if (daemonDate !== null) {
      if (dayQuery.data === undefined) return { records: null, raw: null, loading: true, date: daemonDate, error: null };
      if (!dayQuery.data.ok) return { records: null, raw: null, loading: false, date: daemonDate, error: { status: dayQuery.data.status, message: dayQuery.data.message } };
      const raw = asRecordArray(dayQuery.data.data);
      return { records: normalizeRecords(raw), raw, loading: false, date: daemonDate, error: null };
    }
    return { records: null, raw: null, loading: false, date: null, error: null };
  }, [flowSrc, daemonDate, staticQuery.data, dayQuery.data, source]);
}
