# pr-review-bench — labeled diff corpus for PR-reviewer evaluation (#1119)

A reproducible benchmark for the `pr-reviewer` role: a curated **mix of diffs,
each ground-truth-labeled**, so any model/profile can be scored on **precision**
(false positives on correct diffs), **recall** (catching planted bugs),
**verdict accuracy**, and **anchor accuracy** — not just eyeballed.

Consumed by the `pr-review-bench` workload provider:
`darkmux lab run pr-review-bench --profile <model>` → a scored report.

## Case layout
Each case is two files in `cases/`:
- `<id>.diff`    — a unified diff (the review input).
- `<id>.label.json` — the ground truth + the dispatch intent.

## Label schema (`<id>.label.json`)
```json
{
  "kind": "clean | bug",
  "intent_title": "the PR title given to the reviewer",
  "intent_body":  "the PR description (author's stated intent)",
  "expect_verdict": "pass | flag",
  "bug_class": "sql-injection | off-by-one | null-deref | inverted-condition | missing-test | contract-drift | …  (bug cases only)",
  "anchor_contains": "substring the correct finding's anchor should contain (bug cases only)",
  "notes": "provenance / why this label"
}
```

## Scoring (provider, #1119)
- **clean** case: any finding = a false positive; `expect_verdict: pass`.
- **bug** case: recall = `verdict==flag` AND a finding whose anchor `contains` the expected line (or a `high`-severity finding of the right class); anchor accuracy tracked separately.
- A `flag` verdict with an **empty findings array** is recorded as a contract violation (under-flag), distinct from a true `pass`.
- The harness must also flag **empty/degenerate** reviews distinctly (e.g. #1050 thinking-family models route to reasoning_content → blank), so a broken model doesn't score as "perfect precision."

## Seed corpus (2026-06-28 bake-off)
3 clean diffs from real merged darkmux PRs (frontier-verified ~0 real findings) +
1 planted SQL-injection. Grow by bug-class and by clean-diff size/domain.
