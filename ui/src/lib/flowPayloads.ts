/**
 * The viewer's reading of two flow-record payloads.
 *
 * A flow record's `payload` is free-form per action: the wire contract types it
 * as `Record<string, unknown>` (the generated `FlowRecord`), and nothing on the
 * server side gives these two actions a struct to generate from. These are the
 * fields the run page reads out of `dispatch.start` / `dispatch.complete`
 * payloads, kept in one place. They are NOT a daemon response type: a field
 * that is absent from a record reads as `undefined`.
 */

/** The subset of `dispatch.complete`/`dispatch.error`'s `payload` the
 * runs-list row (`recentRow()` in `viewer.html`) reads for its collapsed
 * summary line — the only state the `<details>` element's closed-by-default
 * `innerText` ever exposes (its `.rrdetail` expansion is hidden markup, out
 * of the parity harness's extraction target; see `tests/parity/README.md`).
 * `endpoint` widened for the session drill-in's `runRegions()` port
 * (`lenses/session/sessionRun.ts`) — the review path stamps the remote
 * endpoint only on the terminal payload, not on start (see that module's
 * own doc, ported from viewer.html:2131-2135). */
export interface DispatchCompletePayload {
  /** (#2011) The RUNTIME's own measure of the execution, in milliseconds —
   * `dispatch_start_instant.elapsed()` at the moment the terminal record is
   * built (`crates/darkmux-crew/src/dispatch_internal.rs`'s
   * `dispatch_complete_payload`). Emitted on every internal, direct and
   * remote completion, and on the error records too.
   *
   * Optional because two real terminals lack it: a `session.end` close-edge
   * carries NO payload at all (`presence_reconciler.rs`'s
   * `build_session_end_record` sets `payload: None`), and archived records
   * predating the field exist. Consumers fall back to subtracting the start
   * and terminal timestamps — see `lenses/session/sessionRun.ts`. */
  wall_ms?: number;
  /** (#2863 review) `wall_ms` INCLUDES this time — "wall stays wall"
   * (`dispatch_internal.rs`'s own comment on `build_dispatch_complete_payload`).
   * The sum of EVERY inter-turn rest this execution took, and how many —
   * routine `turn_delay` cool-downs, thermal governor pauses, battery
   * pauses, and operator holds ALL land here. NOT thermal-only, so the UI
   * must not label this figure "thermal rest". */
  rest_ms?: number;
  rests?: number;
  /** (#2863 review round 2, finding 4) Of `rest_ms` above, the portion
   * attributable to a PACED rest — `reason != "turn_delay"` on the
   * runtime's own `runtime.rest` event (`dispatch_internal.rs`'s own
   * comment: "a manual operator pause or the thermal governor, never
   * routine turn-to-turn cool-down"). Verified against that comment rather
   * than assumed thermal-only: it also covers a battery pause (whose
   * `reason` is `"battery"`, also `!= "turn_delay"`) and an operator hold,
   * so the UI labels this share "paced", not "thermal" — the field cannot
   * distinguish which governor caused it. */
  paced_rest_ms?: number;
  total_turns?: number;
  total_tools?: number;
  total_tokens?: number;
  total_compactions?: number;
  result_class?: string;
  exit_code?: number;
  prompt_tokens?: number;
  completion_tokens?: number;
  endpoint?: string;
}

/** The subset of `dispatch.start`'s `payload` the session drill-in's
 * `runRegions()` port (`lenses/session/sessionRun.ts`) reads for the brief
 * kv rows — viewer.html:2130 (`sp=(d&&d.payload)||{}`) onward. `prompt`
 * (the #1127 full-text field) is real but rarely emitted (see that
 * source's own comment: "the prompt TEXT is not emitted today (only
 * prompt_chars)") — both are typed since the source code branches on
 * `sp.prompt` truthy first. */
export interface DispatchStartPayload {
  runtime?: string;
  image?: string;
  workspace?: string;
  endpoint?: string;
  prompt?: string;
  prompt_chars?: number;
  /** The resolved-runtime-knobs block, `{<knob>: {value, source}}`
   * (`resolved_runtime_bounds_json`, `crates/darkmux-crew/src/
   * dispatch_internal.rs`). Only the keys the run-page rest-reason cards
   * read are named here; the block carries more (max_turns, max_tokens,
   * …) unread by this port. */
  bounds?: {
    turn_delay_ms?: { value?: number | null; source?: string };
    thermal_pacing_enabled?: { value?: boolean | null; source?: string };
    battery_pause_enabled?: { value?: boolean | null; source?: string };
    battery_pause_floor_pct?: { value?: number | null; source?: string };
    [key: string]: unknown;
  };
}
