import { ACTION, type NormRecord } from "../lib/ingest";
import { norm } from "./records";

/**
 * (Matrix review, 5.3.0) Two real coder executions from the laptop's flow
 * file of 2026-10-18 (the `pepper-grinder-deep` lab workload on
 * `qwen3.6-35b-a3b-turboquant-mlx`, LM Studio, which reports no prefill
 * progress), as flow records. Projected like `pepperGrinderRun.ts`: each
 * record's whole-second `ts`, action and the payload fields the live scope
 * reads; arguments, results and machine identity are left out. The values
 * are the real ones.
 *
 * The session ids are swapped for engagement-shaped ones: the originals are
 * lab runs, which never reach the fleet hub (contract 3), and these
 * fixtures are read both as this machine's file and as a peer's hub subset.
 *
 * - `PG1` (11:06:20 to the end of run 1792321441): turn 8's opener is
 *   sampled at 11:06:34.892, 108 ms BEFORE the whole-second `ts` (11:06:35)
 *   of the `dispatch.tool` written ahead of it, and its first chunk comes 9 s
 *   later (11:06:44.007).
 * - `PG2` (all of run 1792325841): turn 10 writes visible text for 11.8 s;
 *   turns 6 and 8 write an `edit` call for 15-17 s; turns 3, 4 and 7 open
 *   their stream inside the second of the previous `dispatch.tool`.
 */
type Row = [string, string, Record<string, unknown>];

function records(rows: Row[], sid: string, execution: string): NormRecord[] {
  return rows.map(([hms, action, payload]) => {
    const raw = { ts: `2026-10-18T${hms}Z`, action, session_id: sid, execution_id: execution, handle: "coder", payload };
    const r = norm(raw);
    return r.action === ACTION.DispatchStart ? norm({ ...raw, source: "crew_dispatch", category: "work" }) : r;
  });
}

/** Unix ms of a `HH:MM:SS(.sss)` wall time on the runs' day (UTC). */
export const deepAt = (hms: string): number => Date.parse(`2026-10-18T${hms}Z`);

const PG1_SID = "dispatch-coder-1792321441-pg1-0.adhoc.coder.pepper-grinder";
const PG1_ROWS: Row[] = [
  ["11:04:06","dispatch.start",{"prompt_chars":3192}],
  ["11:06:20","dispatch.turn.heartbeat",{"turn_seq":5,"partial_index":56,"cumulative_chars":220,"sampled_at_ms":1792321580524,"generated_chars":1824}],
  ["11:06:20","dispatch.turn",{"turn_seq":5,"tool_calls_count":1,"generation_ms":7835,"tool_names":["edit"]}],
  ["11:06:20","telemetry.tokens",{"call_kind":"turn","completion_tokens":492,"turn_seq":5}],
  ["11:06:20","dispatch.tool",{"tool_name":"edit"}],
  ["11:06:20","dispatch.turn.heartbeat",{"turn_seq":6,"cumulative_chars":0,"sampled_at_ms":1792321580541,"generated_chars":0,"prompt_chars":62399}],
  ["11:06:21","dispatch.turn.heartbeat",{"turn_seq":6,"partial_index":1,"cumulative_chars":0,"sampled_at_ms":1792321580956,"generated_chars":3}],
  ["11:06:21","dispatch.turn",{"turn_seq":6,"tool_calls_count":1,"generation_ms":1142,"tool_names":["bash"]}],
  ["11:06:21","telemetry.tokens",{"call_kind":"turn","completion_tokens":63,"turn_seq":6}],
  ["11:06:22","dispatch.tool",{"tool_name":"bash"}],
  ["11:06:22","dispatch.turn.heartbeat",{"turn_seq":7,"cumulative_chars":0,"sampled_at_ms":1792321582046,"generated_chars":0,"prompt_chars":68835}],
  ["11:06:23","dispatch.turn.heartbeat",{"turn_seq":7,"partial_index":1,"cumulative_chars":0,"sampled_at_ms":1792321583195,"generated_chars":3}],
  ["11:06:25","dispatch.turn.heartbeat",{"turn_seq":7,"partial_index":173,"cumulative_chars":0,"sampled_at_ms":1792321585208,"generated_chars":653}],
  ["11:06:27","dispatch.turn.heartbeat",{"turn_seq":7,"partial_index":336,"cumulative_chars":0,"sampled_at_ms":1792321587232,"generated_chars":1318}],
  ["11:06:29","dispatch.turn.heartbeat",{"turn_seq":7,"partial_index":506,"cumulative_chars":0,"sampled_at_ms":1792321589246,"generated_chars":2026}],
  ["11:06:31","dispatch.turn.heartbeat",{"turn_seq":7,"partial_index":681,"cumulative_chars":82,"sampled_at_ms":1792321591279,"generated_chars":2815}],
  ["11:06:33","dispatch.turn.heartbeat",{"turn_seq":7,"partial_index":708,"cumulative_chars":192,"sampled_at_ms":1792321593667,"generated_chars":2929,"phase":"writing_tool_call","tool_name":"edit"}],
  ["11:06:35","dispatch.turn.heartbeat",{"turn_seq":7,"partial_index":709,"cumulative_chars":192,"sampled_at_ms":1792321594873,"generated_chars":3832}],
  ["11:06:35","dispatch.turn",{"turn_seq":7,"tool_calls_count":1,"generation_ms":12838,"tool_names":["edit"]}],
  ["11:06:35","telemetry.tokens",{"call_kind":"turn","completion_tokens":1003,"turn_seq":7}],
  ["11:06:35","dispatch.tool",{"tool_name":"edit"}],
  ["11:06:35","dispatch.turn.heartbeat",{"turn_seq":8,"cumulative_chars":0,"sampled_at_ms":1792321594892,"generated_chars":0,"prompt_chars":70234}],
  ["11:06:44","dispatch.turn.heartbeat",{"turn_seq":8,"partial_index":1,"cumulative_chars":0,"sampled_at_ms":1792321604007,"generated_chars":4}],
  ["11:06:44","dispatch.turn",{"turn_seq":8,"tool_calls_count":1,"generation_ms":10017,"tool_names":["bash"]}],
  ["11:06:44","telemetry.tokens",{"call_kind":"turn","completion_tokens":78,"turn_seq":8}],
  ["11:06:45","dispatch.tool",{"tool_name":"bash"}],
  ["11:06:45","dispatch.turn.heartbeat",{"turn_seq":9,"cumulative_chars":0,"sampled_at_ms":1792321605168,"generated_chars":0,"prompt_chars":76480}],
  ["11:06:46","dispatch.turn.heartbeat",{"turn_seq":9,"partial_index":1,"cumulative_chars":0,"sampled_at_ms":1792321606395,"generated_chars":3}],
  ["11:06:48","dispatch.turn.heartbeat",{"turn_seq":9,"partial_index":133,"cumulative_chars":0,"sampled_at_ms":1792321608294,"generated_chars":524}],
  ["11:06:50","dispatch.turn.heartbeat",{"turn_seq":9,"partial_index":285,"cumulative_chars":0,"sampled_at_ms":1792321610312,"generated_chars":1097}],
  ["11:06:52","dispatch.turn.heartbeat",{"turn_seq":9,"partial_index":438,"cumulative_chars":0,"sampled_at_ms":1792321612349,"generated_chars":1696}],
  ["11:06:54","dispatch.turn.heartbeat",{"turn_seq":9,"partial_index":595,"cumulative_chars":485,"sampled_at_ms":1792321614380,"generated_chars":2399}],
  ["11:06:56","dispatch.turn.heartbeat",{"turn_seq":9,"partial_index":738,"cumulative_chars":1129,"sampled_at_ms":1792321616397,"generated_chars":3043}],
  ["11:06:58","dispatch.turn.heartbeat",{"turn_seq":9,"partial_index":836,"cumulative_chars":1485,"sampled_at_ms":1792321618433,"generated_chars":3399}],
  ["11:07:00","dispatch.turn.heartbeat",{"turn_seq":9,"partial_index":946,"cumulative_chars":1995,"sampled_at_ms":1792321620536,"generated_chars":3909}],
  ["11:07:02","dispatch.turn.heartbeat",{"turn_seq":9,"partial_index":1038,"cumulative_chars":2347,"sampled_at_ms":1792321622542,"generated_chars":4261}],
  ["11:07:03","dispatch.turn",{"turn_seq":9,"tool_calls_count":0,"generation_ms":18307}],
  ["11:07:03","telemetry.tokens",{"call_kind":"turn","completion_tokens":1116,"turn_seq":9}],
  ["11:07:04","dispatch.complete",{"completion_tokens":13477}],
];
export const pg1Records = (): NormRecord[] => records(PG1_ROWS, PG1_SID, "exec-65e1b5ad7466d-6948-0");

const PG2_SID = "dispatch-coder-1792325841-pg2-0.adhoc.coder.pepper-grinder";
const PG2_ROWS: Row[] = [
  ["12:17:21","dispatch.start",{"prompt_chars":3192}],
  ["12:17:22","dispatch.turn.heartbeat",{"turn_seq":1,"cumulative_chars":0,"sampled_at_ms":1792325842061,"generated_chars":0,"prompt_chars":16555}],
  ["12:17:23","dispatch.turn.heartbeat",{"turn_seq":1,"partial_index":1,"cumulative_chars":0,"sampled_at_ms":1792325843680,"generated_chars":3}],
  ["12:17:25","dispatch.turn.heartbeat",{"turn_seq":1,"partial_index":127,"cumulative_chars":3,"sampled_at_ms":1792325845866,"generated_chars":680}],
  ["12:17:27","dispatch.turn",{"turn_seq":1,"tool_calls_count":5,"generation_ms":5334,"tool_names":["read","read","read","read","read"]}],
  ["12:17:27","telemetry.tokens",{"call_kind":"turn","completion_tokens":383,"turn_seq":1}],
  ["12:17:27","dispatch.tool",{"tool_name":"read"}],
  ["12:17:27","dispatch.tool",{"tool_name":"read"}],
  ["12:17:27","dispatch.tool",{"tool_name":"read"}],
  ["12:17:27","dispatch.tool",{"tool_name":"read"}],
  ["12:17:27","dispatch.tool",{"tool_name":"read"}],
  ["12:17:27","dispatch.turn.heartbeat",{"turn_seq":2,"cumulative_chars":0,"sampled_at_ms":1792325847405,"generated_chars":0,"prompt_chars":36030}],
  ["12:17:29","dispatch.turn.heartbeat",{"turn_seq":2,"partial_index":1,"cumulative_chars":0,"sampled_at_ms":1792325849127,"generated_chars":3}],
  ["12:17:31","dispatch.turn.heartbeat",{"turn_seq":2,"partial_index":189,"cumulative_chars":0,"sampled_at_ms":1792325851101,"generated_chars":778}],
  ["12:17:33","dispatch.turn.heartbeat",{"turn_seq":2,"partial_index":380,"cumulative_chars":0,"sampled_at_ms":1792325853130,"generated_chars":1562}],
  ["12:17:35","dispatch.turn.heartbeat",{"turn_seq":2,"partial_index":572,"cumulative_chars":0,"sampled_at_ms":1792325855167,"generated_chars":2334}],
  ["12:17:37","dispatch.turn.heartbeat",{"turn_seq":2,"partial_index":767,"cumulative_chars":0,"sampled_at_ms":1792325857191,"generated_chars":3142}],
  ["12:17:39","dispatch.turn.heartbeat",{"turn_seq":2,"partial_index":943,"cumulative_chars":0,"sampled_at_ms":1792325859204,"generated_chars":3873}],
  ["12:17:41","dispatch.turn",{"turn_seq":2,"tool_calls_count":1,"generation_ms":13658,"tool_names":["read"]}],
  ["12:17:41","telemetry.tokens",{"call_kind":"turn","completion_tokens":1114,"turn_seq":2}],
  ["12:17:41","dispatch.tool",{"tool_name":"read"}],
  ["12:17:41","dispatch.turn.heartbeat",{"turn_seq":3,"cumulative_chars":0,"sampled_at_ms":1792325861068,"generated_chars":0,"prompt_chars":36558}],
  ["12:17:41","dispatch.turn.heartbeat",{"turn_seq":3,"partial_index":1,"cumulative_chars":2,"sampled_at_ms":1792325861417,"generated_chars":2}],
  ["12:17:42","dispatch.turn",{"turn_seq":3,"tool_calls_count":1,"generation_ms":1124,"tool_names":["search"]}],
  ["12:17:42","telemetry.tokens",{"call_kind":"turn","completion_tokens":74,"turn_seq":3}],
  ["12:17:42","dispatch.tool",{"tool_name":"search"}],
  ["12:17:42","dispatch.turn.heartbeat",{"turn_seq":4,"cumulative_chars":0,"sampled_at_ms":1792325862197,"generated_chars":0,"prompt_chars":36848}],
  ["12:17:42","dispatch.turn.heartbeat",{"turn_seq":4,"partial_index":1,"cumulative_chars":2,"sampled_at_ms":1792325862464,"generated_chars":2}],
  ["12:17:44","dispatch.turn.heartbeat",{"turn_seq":4,"partial_index":168,"cumulative_chars":696,"sampled_at_ms":1792325864334,"generated_chars":696}],
  ["12:17:46","dispatch.turn.heartbeat",{"turn_seq":4,"partial_index":338,"cumulative_chars":1407,"sampled_at_ms":1792325866346,"generated_chars":1407}],
  ["12:17:48","dispatch.turn.heartbeat",{"turn_seq":4,"partial_index":371,"cumulative_chars":1568,"sampled_at_ms":1792325868768,"generated_chars":2188}],
  ["12:17:48","dispatch.turn",{"turn_seq":4,"tool_calls_count":1,"generation_ms":6581,"tool_names":["edit"]}],
  ["12:17:48","telemetry.tokens",{"call_kind":"turn","completion_tokens":547,"turn_seq":4}],
  ["12:17:48","dispatch.tool",{"tool_name":"edit"}],
  ["12:17:48","dispatch.turn.heartbeat",{"turn_seq":5,"cumulative_chars":0,"sampled_at_ms":1792325868781,"generated_chars":0,"prompt_chars":41798}],
  ["12:17:49","dispatch.turn.heartbeat",{"turn_seq":5,"partial_index":1,"cumulative_chars":0,"sampled_at_ms":1792325869281,"generated_chars":3}],
  ["12:17:51","dispatch.turn.heartbeat",{"turn_seq":5,"partial_index":24,"cumulative_chars":2,"sampled_at_ms":1792325871489,"generated_chars":690}],
  ["12:17:51","dispatch.turn",{"turn_seq":5,"tool_calls_count":1,"generation_ms":2717,"tool_names":["edit"]}],
  ["12:17:51","telemetry.tokens",{"call_kind":"turn","completion_tokens":199,"turn_seq":5}],
  ["12:17:51","dispatch.tool",{"tool_name":"edit"}],
  ["12:17:51","dispatch.turn.heartbeat",{"turn_seq":6,"cumulative_chars":0,"sampled_at_ms":1792325871505,"generated_chars":0,"prompt_chars":42481}],
  ["12:17:51","dispatch.turn.heartbeat",{"turn_seq":6,"partial_index":1,"cumulative_chars":0,"sampled_at_ms":1792325871755,"generated_chars":3}],
  ["12:17:53","dispatch.turn.heartbeat",{"turn_seq":6,"partial_index":178,"cumulative_chars":0,"sampled_at_ms":1792325873725,"generated_chars":767}],
  ["12:17:55","dispatch.turn.heartbeat",{"turn_seq":6,"partial_index":284,"cumulative_chars":53,"sampled_at_ms":1792325875945,"generated_chars":1199,"phase":"writing_tool_call","tool_name":"edit"}],
  ["12:17:58","dispatch.turn.heartbeat",{"turn_seq":6,"partial_index":284,"cumulative_chars":53,"sampled_at_ms":1792325877951,"generated_chars":1199,"phase":"writing_tool_call","tool_name":"edit"}],
  ["12:18:00","dispatch.turn.heartbeat",{"turn_seq":6,"partial_index":284,"cumulative_chars":53,"sampled_at_ms":1792325879958,"generated_chars":1199,"phase":"writing_tool_call","tool_name":"edit"}],
  ["12:18:02","dispatch.turn.heartbeat",{"turn_seq":6,"partial_index":284,"cumulative_chars":53,"sampled_at_ms":1792325881964,"generated_chars":1199,"phase":"writing_tool_call","tool_name":"edit"}],
  ["12:18:04","dispatch.turn.heartbeat",{"turn_seq":6,"partial_index":284,"cumulative_chars":53,"sampled_at_ms":1792325883971,"generated_chars":1199,"phase":"writing_tool_call","tool_name":"edit"}],
  ["12:18:06","dispatch.turn.heartbeat",{"turn_seq":6,"partial_index":284,"cumulative_chars":53,"sampled_at_ms":1792325885978,"generated_chars":1199,"phase":"writing_tool_call","tool_name":"edit"}],
  ["12:18:08","dispatch.turn.heartbeat",{"turn_seq":6,"partial_index":284,"cumulative_chars":53,"sampled_at_ms":1792325887982,"generated_chars":1199,"phase":"writing_tool_call","tool_name":"edit"}],
  ["12:18:10","dispatch.turn.heartbeat",{"turn_seq":6,"partial_index":284,"cumulative_chars":53,"sampled_at_ms":1792325889986,"generated_chars":1199,"phase":"writing_tool_call","tool_name":"edit"}],
  ["12:18:10","dispatch.turn.heartbeat",{"turn_seq":6,"partial_index":285,"cumulative_chars":53,"sampled_at_ms":1792325890917,"generated_chars":5989}],
  ["12:18:10","dispatch.turn",{"turn_seq":6,"tool_calls_count":1,"generation_ms":19423,"tool_names":["edit"]}],
  ["12:18:10","telemetry.tokens",{"call_kind":"turn","completion_tokens":1686,"turn_seq":6}],
  ["12:18:10","dispatch.tool",{"tool_name":"edit"}],
  ["12:18:11","dispatch.turn.heartbeat",{"turn_seq":7,"cumulative_chars":0,"sampled_at_ms":1792325890935,"generated_chars":0,"prompt_chars":48001}],
  ["12:18:17","dispatch.turn.heartbeat",{"turn_seq":7,"partial_index":1,"cumulative_chars":0,"sampled_at_ms":1792325896803,"generated_chars":2}],
  ["12:18:17","dispatch.turn",{"turn_seq":7,"tool_calls_count":1,"generation_ms":6551,"tool_names":["bash"]}],
  ["12:18:17","telemetry.tokens",{"call_kind":"turn","completion_tokens":63,"turn_seq":7}],
  ["12:18:17","dispatch.tool",{"tool_name":"bash"}],
  ["12:18:17","dispatch.turn.heartbeat",{"turn_seq":8,"cumulative_chars":0,"sampled_at_ms":1792325897697,"generated_chars":0,"prompt_chars":57235}],
  ["12:18:19","dispatch.turn.heartbeat",{"turn_seq":8,"partial_index":1,"cumulative_chars":0,"sampled_at_ms":1792325898984,"generated_chars":3}],
  ["12:18:21","dispatch.turn.heartbeat",{"turn_seq":8,"partial_index":163,"cumulative_chars":0,"sampled_at_ms":1792325900856,"generated_chars":656}],
  ["12:18:23","dispatch.turn.heartbeat",{"turn_seq":8,"partial_index":293,"cumulative_chars":262,"sampled_at_ms":1792325903460,"generated_chars":1238,"phase":"writing_tool_call","tool_name":"edit"}],
  ["12:18:25","dispatch.turn.heartbeat",{"turn_seq":8,"partial_index":293,"cumulative_chars":262,"sampled_at_ms":1792325905469,"generated_chars":1238,"phase":"writing_tool_call","tool_name":"edit"}],
  ["12:18:27","dispatch.turn.heartbeat",{"turn_seq":8,"partial_index":293,"cumulative_chars":262,"sampled_at_ms":1792325907476,"generated_chars":1238,"phase":"writing_tool_call","tool_name":"edit"}],
  ["12:18:29","dispatch.turn.heartbeat",{"turn_seq":8,"partial_index":293,"cumulative_chars":262,"sampled_at_ms":1792325909483,"generated_chars":1238,"phase":"writing_tool_call","tool_name":"edit"}],
  ["12:18:31","dispatch.turn.heartbeat",{"turn_seq":8,"partial_index":293,"cumulative_chars":262,"sampled_at_ms":1792325911493,"generated_chars":1238,"phase":"writing_tool_call","tool_name":"edit"}],
  ["12:18:34","dispatch.turn.heartbeat",{"turn_seq":8,"partial_index":293,"cumulative_chars":262,"sampled_at_ms":1792325914501,"generated_chars":1238,"phase":"writing_tool_call","tool_name":"edit"}],
  ["12:18:36","dispatch.turn.heartbeat",{"turn_seq":8,"partial_index":293,"cumulative_chars":262,"sampled_at_ms":1792325916510,"generated_chars":1238,"phase":"writing_tool_call","tool_name":"edit"}],
  ["12:18:38","dispatch.turn.heartbeat",{"turn_seq":8,"partial_index":294,"cumulative_chars":262,"sampled_at_ms":1792325918070,"generated_chars":5236}],
  ["12:18:38","dispatch.turn",{"turn_seq":8,"tool_calls_count":1,"generation_ms":20384,"tool_names":["edit"]}],
  ["12:18:38","telemetry.tokens",{"call_kind":"turn","completion_tokens":1539,"turn_seq":8}],
  ["12:18:38","dispatch.tool",{"tool_name":"edit"}],
  ["12:18:38","dispatch.turn.heartbeat",{"turn_seq":9,"cumulative_chars":0,"sampled_at_ms":1792325918086,"generated_chars":0,"prompt_chars":61581}],
  ["12:18:39","dispatch.turn.heartbeat",{"turn_seq":9,"partial_index":1,"cumulative_chars":0,"sampled_at_ms":1792325918936,"generated_chars":4}],
  ["12:18:40","dispatch.turn",{"turn_seq":9,"tool_calls_count":1,"generation_ms":2622,"tool_names":["bash"]}],
  ["12:18:40","telemetry.tokens",{"call_kind":"turn","completion_tokens":144,"turn_seq":9}],
  ["12:18:41","dispatch.tool",{"tool_name":"bash"}],
  ["12:18:41","dispatch.turn.heartbeat",{"turn_seq":10,"cumulative_chars":0,"sampled_at_ms":1792325920970,"generated_chars":0,"prompt_chars":67438}],
  ["12:18:42","dispatch.turn.heartbeat",{"turn_seq":10,"partial_index":1,"cumulative_chars":0,"sampled_at_ms":1792325921976,"generated_chars":3}],
  ["12:18:44","dispatch.turn.heartbeat",{"turn_seq":10,"partial_index":153,"cumulative_chars":0,"sampled_at_ms":1792325923953,"generated_chars":573}],
  ["12:18:46","dispatch.turn.heartbeat",{"turn_seq":10,"partial_index":321,"cumulative_chars":652,"sampled_at_ms":1792325925983,"generated_chars":1263}],
  ["12:18:48","dispatch.turn.heartbeat",{"turn_seq":10,"partial_index":450,"cumulative_chars":1189,"sampled_at_ms":1792325928013,"generated_chars":1800}],
  ["12:18:50","dispatch.turn.heartbeat",{"turn_seq":10,"partial_index":561,"cumulative_chars":1710,"sampled_at_ms":1792325930072,"generated_chars":2321}],
  ["12:18:52","dispatch.turn.heartbeat",{"turn_seq":10,"partial_index":689,"cumulative_chars":2220,"sampled_at_ms":1792325932072,"generated_chars":2831}],
  ["12:18:52","dispatch.turn",{"turn_seq":10,"tool_calls_count":0,"generation_ms":11813}],
  ["12:18:52","telemetry.tokens",{"call_kind":"turn","completion_tokens":765,"turn_seq":10}],
  ["12:18:53","dispatch.complete",{"completion_tokens":6514}],
];
export const pg2Records = (): NormRecord[] => records(PG2_ROWS, PG2_SID, "exec-65e1c611754c8-234d-0");
