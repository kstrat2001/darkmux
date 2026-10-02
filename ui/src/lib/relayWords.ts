// The one wording for a relayed run: the machine that RAN it owns it, and the
// machine that asked is shown as "from <machine>". The board's subtitle and
// the run page's header both read it.
import type { Run } from "../types/generated/Run";
import type { RunRelay } from "../types/generated/RunRelay";

export const relayedFromText = (relay: RunRelay): string => `from ${relay.asked_on_machine}`;

/** The relay fact of a run row, `null` for work that ran where it was asked or no row. */
export const relayOf = (row: Run | null): RunRelay | null => row?.relay ?? null;
