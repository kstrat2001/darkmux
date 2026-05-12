---
name: darkmux-qa-review
description: Dispatch the darkmux `code-reviewer` crew member against the current branch's diff. Runs through `darkmux crew dispatch`, which pre-flight-verifies the openclaw agent matches the role manifest before invoking. Findings return inline.
user_invocable: true
allowed-tools: "Bash(darkmux:*),Bash(git:*),Bash(openclaw:*),Bash(jq:*),Bash(plutil:*)"
---

# Darkmux QA review

Dispatches the **darkmux/code-reviewer** agent against the current branch's diff via `darkmux crew dispatch`. The dispatch path uses the role manifest at `templates/builtin/crew/roles/code-reviewer.json` + the bundled `.md` system prompt, with pre-flight checks that verify the openclaw agent registry matches the manifest before running. Operator-sovereignty: drift in the openclaw config (stale system prompt, wrong tool palette) bails before launching, with a clear `darkmux crew sync` repair pointer.

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

## Step 2 — Verify dispatch is ready

Before sending, run a fast pre-flight via `darkmux crew dispatch ... --skip-preflight` is NOT used; the real `dispatch` invocation runs the pre-flight automatically and bails loud on drift. But you can verify ahead of time:

```bash
darkmux crew sync --dry-run
```

If output shows `would add` or `would update` for `darkmux/code-reviewer`, run `darkmux crew sync` first to bring the openclaw agent registry in line with the manifest — otherwise the dispatch will bail with a "drift" message and the repair pointer.

## Step 3 — Dispatch the review

```bash
DIFF=$(git diff "$MERGE_BASE" HEAD 2>/dev/null | head -300)
RUN_ID="darkmux-qa-review-$(date +%s)-$$"

darkmux crew dispatch code-reviewer \
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

Provide concise, actionable findings (3–7 bullets max). For each finding, classify as **MUST FIX** (security/correctness — blocks merge) or **CONSIDER** (style/clarity/follow-up). Avoid the framing 'acceptable but worth documenting' — if the behavior is acceptable, MUST it be documented? If yes, the docs are MUST FIX. If no, drop the finding. Trace through inputs at each finding so the reasoning is visible. Start your reply with **\"code-reviewer review for $REPO/$BRANCH:\"** so the speaker is clear."
```

The dispatch's stdout is openclaw's JSON envelope. The final reply is at `.result.meta.finalAssistantVisibleText` (or `.result.payloads[].text` joined). Parse with:

```bash
REPLY=$(echo "$OUTPUT" | jq -r '
  (if .result and .result.meta and .result.meta.finalAssistantVisibleText
     then .result.meta.finalAssistantVisibleText
     elif .result and .result.payloads
     then (.result.payloads | map(.text // empty) | map(select(length > 0)) | join("\n\n"))
     else .reply // empty
   end)')
```

## Step 4 — Hand off

Show the user the reply. Ask: "Want to address any of these findings, or move on?"

## Notes

- **No Discord delivery by default.** Unlike the FH `qa-review` skill (which posts to `#finhero-qa`), the darkmux variant returns findings inline only. If you want Discord delivery, add `--deliver discord:<channel-id>` to the dispatch command.
- **No multi-auditor dispatch.** Darkmux currently only ships a `code-reviewer` role. `devops` and `legal` roles are not yet in the schema — they'd be added when the team is staffed for those concerns.
- **Pre-flight is automatic.** If the openclaw agent's system prompt or tool palette has drifted from the manifest, the dispatch bails before sending. Run `darkmux crew sync` to reconcile.
- **Why this skill instead of `qa-review`:** the FH skill is scoped to FinHero-era infrastructure (Discord channel, qa/devops/legal openclaw agents). This darkmux variant routes through the namespace-managed agent and respects the operator-sovereignty contract.
