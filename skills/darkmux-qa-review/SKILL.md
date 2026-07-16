---
name: darkmux-qa-review
description: Dispatch the darkmux `code-reviewer` role against the current branch's diff via `darkmux crew dispatch --json`. Uses the default internal runtime; findings return inline.
user_invocable: true
allowed-tools: "Bash(darkmux:*),Bash(git:*),Bash(jq:*)"
---

# Darkmux QA review

Dispatches the **code-reviewer** role against the current branch's diff via `darkmux crew dispatch` (default internal runtime, container-bounded). The dispatch reads the role manifest at `templates/builtin/roles/code-reviewer.json` + the bundled `.md` system prompt, runs in a fresh per-dispatch Docker container, and returns a structured JSON envelope via `--json`.

## Step 1 — Determine review scope

Same as the FH `qa-review` skill: branch-scoped diff from `origin/main`, excluding lockfiles.

```bash
BRANCH=$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo "(unknown)")
REPO=$(basename "$(git rev-parse --show-toplevel 2>/dev/null)" 2>/dev/null || echo "(no git repo)")
DEFAULT_BRANCH=$(git symbolic-ref --short refs/remotes/origin/HEAD 2>/dev/null || echo origin/main)
MERGE_BASE=$(git merge-base HEAD "$DEFAULT_BRANCH" 2>/dev/null || true)

if [ -n "$MERGE_BASE" ] && [ "$MERGE_BASE" != "$(git rev-parse HEAD)" ]; then
  CHANGES_RAW=$({ git diff --name-only HEAD 2>/dev/null; git diff --name-only "$MERGE_BASE" HEAD 2>/dev/null; } | sort -u | grep -v '^$' || true)
else
  CHANGES_RAW=$(git diff --name-only HEAD 2>/dev/null | sort -u | grep -v '^$' || true)
fi

LOCKFILE_RE='(^|/)(package-lock\.json|pnpm-lock\.yaml|yarn\.lock|Pipfile\.lock|poetry\.lock|requirements.*\.lock|Gemfile\.lock|go\.sum|Cargo\.lock)$'
CHANGES=$(echo "$CHANGES_RAW" | grep -Ev "$LOCKFILE_RE" || true)

test -n "$CHANGES" || { echo "No reviewable changes — only lockfiles in scope."; exit 0; }
echo "Scope: repo=$REPO branch=$BRANCH (merge-base=${MERGE_BASE:0:7} on $DEFAULT_BRANCH)"
echo "Files:"
echo "$CHANGES"
```

If empty, stop and report "no reviewable changes."

## Step 2 — Dispatch the review

```bash
DIFF=$(git diff "$MERGE_BASE" HEAD 2>/dev/null | head -300)
RUN_ID="darkmux-qa-review-$(date +%s)-$$"

OUTPUT=$(darkmux crew dispatch code-reviewer \
  --json \
  --session-id "$RUN_ID" \
  --timeout 600 \
  --message "QA review request.

Repo: $REPO
Branch: $BRANCH

Files changed:
$CHANGES

Diff (truncated to 300 lines):
\`\`\`diff
$DIFF
\`\`\`

Provide concise, actionable findings (3–7 bullets max). For each finding, classify as **MUST FIX** (security/correctness — blocks merge) or **CONSIDER** (style/clarity/follow-up). Avoid the framing 'acceptable but worth documenting' — if the behavior is acceptable, MUST it be documented? If yes, the docs are MUST FIX. If no, drop the finding. Trace through inputs at each finding so the reasoning is visible. Start your reply with **\"code-reviewer review for $REPO/$BRANCH:\"** so the speaker is clear.")
```

## Step 3 — Parse the JSON envelope

With `--json`, the dispatch's stdout is a single-line structured envelope per the darkmux-runtime contract:

```json
{
  "result": "stop" | "error",
  "final_assistant": "...",
  "metrics": { "model": "...", "wall_ms": 2135, "turns": 1, ... },
  "trajectory_path": "/workspace/.darkmux-runtime/trajectory.jsonl"
}
```

Extract the reply text:

```bash
REPLY=$(echo "$OUTPUT" | jq -r '.final_assistant // empty')
RESULT=$(echo "$OUTPUT" | jq -r '.result // "unknown"')

if [ "$RESULT" != "stop" ] || [ -z "$REPLY" ]; then
  echo "dispatch did not complete cleanly (result=$RESULT)"
  echo "$OUTPUT" | jq '.'
  exit 1
fi
```

## Step 4 — Hand off

Show the user the reply. Ask: "Want to address any of these findings, or move on?"

## Notes

- **No Discord delivery.** Unlike the FH `qa-review` skill (which posts to `#finhero-qa`), the darkmux variant returns findings inline only.
- **No multi-auditor dispatch.** For code review, darkmux ships the `code-reviewer` role (this skill dispatches it). Other engagement roles ship too — including `legal-research` for legal questions — but a dedicated multi-specialist *diff auditor* crew (e.g. a `devops` auditor; `devops` is genuinely not in the schema) isn't wired up yet. Those would be added when the team is staffed for those concerns.
- **The runtime is internal (Docker container) — the only dispatch path.** This skill uses darkmux's in-house container-bounded runtime; the parser below assumes the `--json` envelope shape it emits.
- **No agent-registry sync step needed.** The internal runtime reads role manifests directly each dispatch — no separate registry to reconcile.
- **Why this skill instead of `qa-review`:** the FH skill is scoped to FinHero-era infrastructure (Discord channel, qa/devops/legal agents). This darkmux variant routes through the namespace-managed agent and respects the operator-sovereignty contract.
