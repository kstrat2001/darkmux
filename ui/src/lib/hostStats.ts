/**
 * (#2107) The ONE host-load aggregation both the session drill-in's SYSTEM
 * pane (`lenses/session/sessionRun.ts`) and the global machine drawer
 * (`components/MachineDrawer.tsx`) fold `telemetry.process` samples
 * through — one implementation so the two surfaces can never report
 * different numbers for the same window of samples.
 *
 * This is display-side aggregation over records the app already holds (the
 * flow window / route records / session slice it fetched for other reasons)
 * — it adds no request and no probe, matching the host-side reduction's own
 * "observer must not join the observed" discipline
 * (`crates/darkmux-crew/src/dispatch_internal.rs`'s `reduce_host_stats`,
 * which this module deliberately mirrors in SHAPE — peak/mean, not the
 * exact rounding rule, since the two run on different data volumes and
 * serve different readers: the Rust side is the single authoritative
 * reduction over ALL of a dispatch's own samples written into the
 * envelope; this is a live, continuously-refreshed client-side view over
 * whatever slice of the flow stream the current route already has in
 * memory).
 */

/** The bare shape every `telemetry.process` record's `payload` carries —
 * see `run_telemetry_sampler`'s doc in `dispatch_internal.rs` for the
 * producer side. All three are independently optional (a failed host read
 * omits its own field for that tick, never the whole sample). */
export interface ProcSamplePoint {
  cpu?: number;
  mem?: number;
  gpu?: number;
}

export interface MetricAggregate {
  /** The latest sample's value — `null` when this metric never reported. */
  now: number | null;
  /** Mean over every reading this metric reported. */
  avg: number | null;
  /** The largest reading this metric reported (the SYSTEM pane's pre-#2107
   * "PEAK" — kept as `high` for the wider aggregate/meter vocabulary the
   * drawer's `now · avg · max` labels use). */
  high: number | null;
  /** 95th percentile (nearest-rank), for callers with room to show it. */
  p95: number | null;
}

export interface HostAggregate {
  cpu: MetricAggregate;
  mem: MetricAggregate;
  gpu: MetricAggregate;
  /** How many samples (of ANY metric) went into this aggregate — the same
   * "0 and never-measured are different claims" rule the host-side
   * `HostStats.samples` field follows. */
  count: number;
}

const EMPTY_METRIC: MetricAggregate = { now: null, avg: null, high: null, p95: null };

function aggregateMetric(valuesInOrder: number[]): MetricAggregate {
  if (valuesInOrder.length === 0) return EMPTY_METRIC;
  const now = valuesInOrder[valuesInOrder.length - 1];
  const high = Math.max(...valuesInOrder);
  const avg = valuesInOrder.reduce((a, b) => a + b, 0) / valuesInOrder.length;
  const sorted = [...valuesInOrder].sort((a, b) => a - b);
  // Nearest-rank, same convention as the Rust-side `reduce_metric`.
  const rank = Math.ceil(sorted.length * 0.95);
  const idx = Math.min(Math.max(rank - 1, 0), sorted.length - 1);
  const p95 = sorted[idx];
  return { now, avg, high, p95 };
}

/**
 * Fold a chronologically-ordered list of `telemetry.process` sample
 * payloads into a per-metric aggregate. Order matters ONLY for `now` (the
 * latest reading) — callers pass samples in the same ascending-`ts` order
 * the flow stream / route records already carry them in.
 */
export function aggregateHostSamples(points: ProcSamplePoint[]): HostAggregate {
  const cpu: number[] = [];
  const mem: number[] = [];
  const gpu: number[] = [];
  for (const p of points) {
    if (typeof p.cpu === "number" && Number.isFinite(p.cpu)) cpu.push(p.cpu);
    if (typeof p.mem === "number" && Number.isFinite(p.mem)) mem.push(p.mem);
    if (typeof p.gpu === "number" && Number.isFinite(p.gpu)) gpu.push(p.gpu);
  }
  return {
    cpu: aggregateMetric(cpu),
    mem: aggregateMetric(mem),
    gpu: aggregateMetric(gpu),
    count: points.length,
  };
}

/** Round a percent for display — every caller wants the same "%d%" shape;
 * centralized so the SYSTEM pane and the drawer round identically. `null`
 * stays `null` (never coerced to 0 — absence is a different claim). */
export function roundPct(v: number | null): number | null {
  return v === null ? null : Math.round(v);
}
