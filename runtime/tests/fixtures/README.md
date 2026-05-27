# Real-world fixtures harvested from production traces

Per [#437](https://github.com/kstrat2001/darkmux/issues/437) and Eureka E13
(*"Absence of failure ≠ validation of recovery"*), every time a recovery layer
fires in production, the trigger payload becomes a fixture candidate. These
fixtures act as regression guards — past failures we've seen can't silently
regress when we tighten the parser/detector in a future PR.

## Layout

- `compactor-malformed/` — trigger payloads for `json_repair` + the procedural
  patcher in `compaction.rs`
- `promoter-emissions/` — model outputs that test the plain-text tool-call
  promoter's match (or correct non-match) behavior
- `cycle-traces/` — trajectory slices for cycle-detector validation

Pre-existing fixtures at this level (`compactor_*.json`, `compactor_*.txt`)
are snapshot pins for the compactor request shape, not real-world traces.

## First batch (Beat 47 / 48, harvested 2026-05-27)

| Fixture | Source | What it validates |
|---|---|---|
| `compactor-malformed/beat47-runaway-newlines-active-files.json` | Beat 47 dispatch pre-#435; runtime exit with `EOF while parsing a string` at col 6463 | `json_repair::repair_truncated_json` must terminate the open string + balance containers so the result parses as a `serde_json::Value` |
| `compactor-malformed/beat48-truncated-missing-objective.json` | Beat 48 dispatch post-#435 pre-#436; lexical repair succeeded but schema validation failed with `missing field 'objective'` | `patch_missing_required_fields` must insert the sentinel objective + set `truncation_patched: true`, then `from_value::<StructuredCompactionOutput>` must succeed |
| `promoter-emissions/beat48-run5-orphan-xml-close-tags.txt` | Beat 48 N=5 run 5 reasoning_content; model emitted only `</parameter></function></tool_call>` without the opening | `parse_plain_text_tool_call_blocks` must NOT match (no extractable structure; this is *"unformed tool call"* not *"tool call in wrong field"*) |
| `cycle-traces/beat48-run1-read-edit-bash-test-iteration.jsonl` | Beat 48 N=5 run 1; first 32 trajectory events including the first two `dispatch.cycle.suspected` fires | Documents the empirical cycle pattern operators see during test-iteration loops |

## Sanitization notes

The harvested traces came from the canonical-tests long-agentic workload
(operator-confirmed-public). One internal-project ticket prefix `SYS-2359`
appeared in the compactor's free-text output and was sanitized to
`SAMPLE-1234` (preserves structural shape, drops the recognizable internal
sentinel). Workspace paths use the `/workspace/src/services/refreshToken*`
form which is generic enough — common across auth codebases worldwide.

If future harvests include sentinels (FinHero / OFAL / DEVOPS-* / etc.),
sanitize at extract time before commit. The pre-commit sentinel block in
de-substack does NOT apply to this repo, but the operator-trust principle
does: public OSS repo content should not carry internal-project markers.

## Harvest cadence going forward

Per #437 + E13: any time a recovery layer fires in production, capture the
trigger payload as a fixture candidate. Build the discipline into the
empirical-validation playbook:

1. New recovery layer fires (⚒ / ⚙ / promotion / cycle / failure-cascade)
2. Extract the trigger payload from trajectory / qa-reply
3. Sanitize if needed
4. Commit as fixture + add fixture-replay test
5. Mark the layer's row in Beat 48 §12 recovery-stack snapshot table as
   `fixture pinned ✅`
