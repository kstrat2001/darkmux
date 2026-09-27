/**
 * (#2915) The machine page's Utility section: darkmux's own jobs on this
 * machine's ONE utility model (#2914), as machine STATE rather than config.
 *
 * - the model: its id, its declared window (`internal.utility.n_ctx`, from
 *   `/machine/specs`, this machine only), whether it is resident and its
 *   measured footprint (gestalt's residency row for it);
 * - the live job, the same reading as the fleet card's utility strip;
 * - each job's recent usage (calls, tokens) from its usage records: one row
 *   per known job, then one "other" row, all present even at zero.
 *
 * This supersedes the `utility` badge on the model's residency row. Every
 * line is always present (the live line reads "idle", a job with no calls
 * reads 0), so nothing appears or disappears as jobs start and end.
 */

import { fmtC, memBytes } from "../../lib/format";
import { uidOf, T } from "../../lib/flow";
import { utilityJobWord, utilityStrip, utilityUsageByJob, type UtilityStrip } from "../../lib/utilityJobs";
import type { FlowRecord, MachineResourcesModel, MachineSpecs } from "../../types/handwritten";

export interface UtilitySectionView {
  strip: UtilityStrip;
  /** "darkmux:qwen3-4b-instruct-2507", or the placeholder when none is known. */
  modelLine: string;
  /** "window 120,000 · resident · 14.61 GiB", with "—" for what this page cannot know. */
  factsLine: string;
  /** "idle", "radio routing · 3s", "compacting · 12s", "compacting · stalled". */
  liveLine: string;
  /** (#2958) The page's flow window has not answered yet: the live line
   *  says "no signal" and the strip draws quiet, rather than "idle". */
  noSignal: boolean;
  jobs: Array<{ word: string; calls: string; tokens: string; known: boolean }>;
}

export function utilitySectionView(args: {
  data: readonly FlowRecord[];
  uid: string;
  nowMs: number;
  /** This machine's own `/machine/specs`, when the page is about it. */
  specs: MachineSpecs | null;
  isLocal: boolean;
  /** The residency row for the utility model, when resident (local only). */
  residentRow: MachineResourcesModel | null;
  /** (#2958) Whether the records this reads have arrived. Omitted: true. */
  settled?: boolean;
}): UtilitySectionView {
  const noSignal = args.settled === false;
  const binding = args.isLocal ? (args.specs?.utility_model ?? null) : null;
  const strip = utilityStrip(args.data, args.uid, args.nowMs, binding);
  const win = binding?.n_ctx != null ? `window ${binding.n_ctx.toLocaleString("en-US")}` : "window —";
  const residency = !args.isLocal
    ? "residency unknown (another machine)"
    : strip.resident === true
      ? `resident${args.residentRow?.current_bytes != null ? ` · ${memBytes(args.residentRow.current_bytes)}` : ""}`
      : strip.resident === false
        ? "not loaded"
        : "no utility model registered";
  const job = strip.job;
  const liveLine = noSignal ? "no signal" : job ? (job.stalled ? `${job.word} · stalled` : `${job.word} · ${Math.max(0, Math.floor((args.nowMs - job.sinceMs) / 1000))}s`) : "idle";
  const mine = args.data.filter((r) => uidOf(r) === args.uid && T(r.ts) <= args.nowMs);
  // (#2915 review, C7) A FIXED set of rows, so the section is one size
  // whatever ran: one per known job, then ONE "other" row folding every job
  // this build does not know and every utility record that names none (a
  // routing record from before 1.61.0), present even at zero.
  const usage = utilityUsageByJob(mine);
  const other = usage.filter((u) => !u.known).reduce((acc, u) => ({ calls: acc.calls + u.calls, tokens: acc.tokens + u.tokens }), { calls: 0, tokens: 0 });
  const row = (word: string, calls: number, tokens: number, known: boolean) => ({
    word,
    calls: `${calls.toLocaleString("en-US")} ${calls === 1 ? "call" : "calls"}`,
    tokens: `${fmtC(tokens)} tokens`,
    known,
  });
  const jobs = [...usage.filter((u) => u.known).map((u) => row(utilityJobWord(u.job), u.calls, u.tokens, true)), row("other", other.calls, other.tokens, false)];
  return {
    strip,
    modelLine: strip.model ?? "no utility model seen",
    factsLine: `${win} · ${residency}`,
    liveLine,
    noSignal,
    jobs,
  };
}
