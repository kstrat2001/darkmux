import type { FlowRecord } from "../types/handwritten";
import { ingest, ingestRecord, type NormRecord } from "../lib/ingest";

/** A test fixture's raw record: the wire shape, loosely typed so a fixture
 *  can carry any field (a hand-shaped payload, an extra top-level key) and
 *  name its action by constant (`ACTION.DispatchStart`, whose runtime value
 *  is the wire string). */
export type RawRecord = { [K in keyof FlowRecord]?: unknown } & Record<string, unknown>;

/** One fixture record, through the same boundary the app uses. Throws on a
 *  value `ingest` would drop (a non-object, a schema header), so a fixture
 *  that does not describe a record fails loudly instead of vanishing. */
export function norm(raw: RawRecord): NormRecord {
  const r = ingestRecord(raw);
  if (!r) throw new Error(`not a flow record: ${JSON.stringify(raw)}`);
  return r;
}

/** Fixture records, through the same boundary the app uses. */
export function normAll(raws: readonly RawRecord[]): NormRecord[] {
  return ingest(raws);
}
