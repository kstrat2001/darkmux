---
name: darkmux-escalation-handler
description: Pick up a darkmux dispatch that hit its operator-configured escalation bound (TerminalReason::EscalationTriggered). Reads the salvageable state from the run directory, summarizes what local-tier accomplished, and continues the work in the frontier-tier session. Invoke this skill when you see a dispatch terminate with `result: "escalation_compaction_limit_reached"` (or future `escalation_*` variants) — the frontier-tier orchestrator is now the agent. (#377)
user_invocable: true
allowed-tools: "Bash(darkmux:*),Bash(cat:*),Bash(jq:*),Bash(ls:*),Bash(grep:*),Bash(wc:*),Read"
---

# Darkmux escalation handler

This skill picks up where a local-tier dispatch left off when it hit an operator-configured escalation bound. The dispatch didn't fail — it **escalated** by design. The local tier (4B utility agents + 35B specialists running in the per-dispatch container) did what it could; past a known wall-clock or compaction-count bound, the contract says hand off to the frontier-tier orchestrator (this session) with the salvageable state.

## When to invoke

You're looking at one of:

- A `darkmux lab run` or `darkmux dispatch` that ended with `result: "escalation_compaction_limit_reached"`, `result: "escalation_cumulative_tokens_exceeded"`, or `result: "escalation_intra_turn_stall_exhausted"`. All escalation results share the `escalation_*` prefix; future variants will too.
- A run manifest under `~/.darkmux/runs/<run-id>/manifest.json` whose `ok: false` carries an escalation-shaped error string.
- A flow record on the topology viewer with `terminal_reason: EscalationTriggered`.

If the dispatch ended with `result: "stop"` (clean finish) or `result: "max_turns"` (loop cap, not escalation), this is the WRONG skill — those don't trigger handoff.

## Important — what escalation means (and doesn't)

- **Escalation is not failure.** The operator set `bail_after_compactions` (or a future bound) deliberately. Hitting it means the local tier did what was bounded and is now properly handing off — same as a manager escalating a ticket from L1 to L2.
- **The salvageable state is the value.** Trajectory, persisted compaction artifacts, partial files written to the workspace, the conversation history up to the bail point — all of it is yours to continue from. The operator does NOT want a fresh-start re-implementation; they want frontier-tier continuation.
- **Do not silently re-run the same local dispatch.** That would just hit the same bound and escalate again. If the operator wants a *different* local dispatch (different role, different threshold, different model), they'll say so — until then, frontier IS the agent now.
- **The operator may be away.** Escalation is an async hand-off pattern. You may be invoked into a fresh session by the operator coming back hours later to look at why the run escalated. Treat the trajectory + workspace as your ground truth, NOT your memory of "what we were doing."

## Step 1 — Read the run dir

The operator (or the topology viewer) will give you a run id. Find the artifacts:

```bash
RUN_ID=<from operator>
RUN_DIR=~/.darkmux/runs/$RUN_ID
ls -la $RUN_DIR
cat $RUN_DIR/manifest.json | jq '.'
```

What you're looking for in the manifest:

- `ok: false` (escalation manifests as non-ok in the host layer for back-compat with consumers that grep on ok)
- An error string containing one of `escalation_compaction_limit_reached`, `escalation_cumulative_tokens_exceeded`, or `escalation_intra_turn_stall_exhausted`
- `sandbox` — the workspace path the agent was working in (this is your continuation workspace)
- `workload` + `profile` — context for what was being attempted

## Step 2 — Read what the local tier accomplished

Three durable signals:

```bash
# Final model turn — finish_reason tells you WHY the bound fired
# ("length" = truncation), with token usage and any tool calls:
grep '"type":"model.completed"' $RUN_DIR/trajectory.jsonl | tail -1 | jq '{finish_reason, usage, tool_calls}'

# Turn count + compaction count (gauges what the local tier got through):
grep -c '"type":"model.completed"' $RUN_DIR/trajectory.jsonl
grep -c '"type":"compaction"' $RUN_DIR/trajectory.jsonl
grep -c '"type":"tool.completed"' $RUN_DIR/trajectory.jsonl

# Persisted compaction artifacts (tier-2 dispatches only) — these have
# the structured-slot summaries from each compaction in JSON form:
ls $RUN_DIR/compaction-*.json 2>/dev/null
# OR if your workspace mount overwrites these:
ls $SANDBOX/.darkmux-runtime/compaction-*.json 2>/dev/null
```

For each compaction artifact, the `objective`, `current_truth`, and `next_concrete_actions` slots tell you what the agent thought it was doing + what it intended to do next. This is your **handoff brief** — work-state preserved in a structured form designed for exactly this moment.

## Step 3 — Inspect the workspace for partial work

```bash
SANDBOX=$(cat $RUN_DIR/manifest.json | jq -r '.sandbox')
cd $SANDBOX
git status --short  # if the workspace is a git repo, this shows what changed
ls -la  # general sense of what files exist
```

If the workspace is a git working tree, `git status` + `git diff` give you EXACTLY what the local tier modified. If not, you're reading files cold; prioritize the ones the compaction artifacts named in their `active_files` slot.

## Step 4 — Summarize back to the operator

In your own conversational reply (not in a new file), tell the operator:

1. **What the local tier accomplished**: "Wrote N tests in <files>; verified <X>; got Y turns in." Cite the trajectory + compaction artifacts.
2. **Why escalation fired**: "Hit `bail_after_compactions = N` after compaction #N at turn #M." Operator set this bound; surface it back so they remember.
3. **What's left to do**: Derived from the compaction artifact's `next_concrete_actions` slot + a fresh read of the workspace state. Frame as "to finish what was started" — NOT "to start over."
4. **What you propose to do**: Continue the work in this frontier-tier session. State the next 1-2 concrete steps you'll take if the operator says "continue." Do NOT take steps yet — escalation handoff is a `read-and-propose` skill at this stage.

## Step 5 — On operator's "continue" signal, take over

Frontier-tier means: you (this session) become the agent. You have your own tools (Edit, Read, Bash). You don't need to dispatch another local container; you ARE the continuation. Pick up where the compaction artifact's `next_concrete_actions` left off, using the workspace as live state.

Some operator-facing principles:

- **You're not slower than local tier; you're slower-and-smarter.** Operator escalated specifically so a more capable layer takes over. Don't try to act like a 4B utility agent; act like the frontier-tier orchestrator you are.
- **Verify before declaring done.** Whatever the operator's `verify_criteria` was on the workload, run it (or have the operator run it) before reporting success.
- **Preserve the escalation trail.** When the work is done, mention in your reply where the run-dir is and how the work post-escalation differed from what local-tier had done — this is the lab signal that future tuning depends on.

## Pause-posture roles (escalation_posture: pause)

A role manifest can declare `escalation_posture: "pause"` (instead of the default `"auto"`). Today the runtime treats both the same — it emits the EscalationTriggered terminal regardless. The host/skill layer is where the distinction kicks in:

- **auto** (default): you should proceed with Step 4-5 above. The operator wanted automatic handoff; that's what they set the role up for.
- **pause**: you should STOP at Step 4. Don't propose to continue. Just summarize what local tier did + what's left. The operator wants to make the next-step decision themselves. Wait for their signal.

This is operator-sovereignty applied to the escalation hand-off: some work is too judgment-bearing to auto-continue (research roles, financial-decision roles, anything where the operator wants the explicit yes/no before more work happens).

## When the trail is cold

If the run is hours-old (or days-old) and you have no fresh operator context:

- Don't infer what the operator wanted from the workload prompt alone — that's stale.
- Read the manifest + the trajectory's final turns + the workspace state. Report what you find.
- Ask the operator: *"This dispatch escalated at <time>. Workspace shows <state>. Do you want me to continue, restart, or just summarize?"*
- Default to summarize-only unless explicitly told to act.

The operator may have moved on, archived the work, or solved it elsewhere. Don't assume the escalation is still hot.

## Cross-references

- `#377` — escalation mechanism implementation (this skill is part of)
- `#357` — schema landing of `ReserveConfig.bail_after_compactions`
- `LAB_NOTEBOOK.md` Beat 44 closure (operator-named) — *"bound the cost and escalate past the bound"* doctrine
- `methodology/article-notes/COMPACTION_TIER2_ARC.md` — article-shaping notes; this skill is the operational expression of "KISS as a compounding design principle"
