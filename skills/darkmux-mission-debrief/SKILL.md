---
name: darkmux-mission-debrief
description: Run a mission's debrief — the post-mission review ceremony that turns one mission's transient signal into durable engagement lessons. Reads `darkmux mission debrief <id> --json` (the loop pathologies darkmux's detectors flagged across the mission's runs, the corrections the reviewer recorded, and the mission's phases + how each ended), reviews the engagement's own docs, and — for a coding mission — `git show`s the shipped work, then proposes durable lessons WITH the why and records each via `darkmux lessons add`. Lessons then brief every future crew in this engagement as a `<lessons>` block (#994/#1000). Read + propose + approve — the skill proposes; the operator approves before any write. Run it at mission completion (the close nudge prompts it). NASA vocabulary: Mission · Crew · Debrief · Lessons.
user_invocable: true
allowed-tools: "Bash(darkmux:*),Bash(git:*),Read,Glob,Grep"
---

# Mission debrief

This skill runs the **debrief** ceremony for one completed mission: it sifts the mission's transient signal — the detector **cautions** and the reviewer's **corrections**, both perishable — and decides which is a durable **lesson** worth keeping for *every future mission* in this engagement. Each lesson is recorded **with the reasoning behind it** ("include the why"), so a fresh-context local model can apply it with judgment rather than re-deriving or re-repeating.

It closes the loop the operator named: **detect → distill → inject → don't-repeat.** Detect and inject are automatic (cautions are captured to the flow stream; corrections + cautions are carried phase→phase live). The debrief is the **distill** step — and it is a *cross-mission* act: the within-mission learning already happened live; the debrief banks what this mission taught for the *next* one. This is NASA's Lessons Learned practice, applied locally.

**Operator-sovereignty (#44):** the skill *proposes* lessons; nothing is written until the operator approves. The "why" cannot be auto-generated meaningfully — that judgment is the operator's, and the skill draws it out rather than inventing it.

**Scope:** lessons are engagement-scoped. Per-repo lessons land in this repo's `<repo>/.darkmux/lessons.db`; only genuinely universal conventions (house style, language) go `--global`. A dispatch in another repo never sees this one's lessons — keep it that way.

## Step 0 — Identify the mission + the lessons store

Ask the operator which mission to debrief (the id under `~/.darkmux/missions/`). Then show what's already recorded so the skill doesn't propose duplicates:

```bash
darkmux mission status        # the board — find the completed mission's id
darkmux lessons list          # what's already banked (repo + global tiers)
```

If `darkmux lessons list` errors, stop and surface it.

## Step 1 — Read the debrief material

```bash
darkmux mission debrief <mission-id> --json
```

This is the mission's history, scoped to its dispatch sessions, in one place:

- **`cautions`** — the loop pathologies darkmux's detectors flagged across the mission's runs (repeated-tool cycles, looping reasoning, tool-failure cascades), each naming the file it happened in when known. **Recurring** patterns are lessons; a one-off is noise.
- **`corrections`** — the adjudication notes the reviewer recorded on the mission's dispatches (#849): what was overridden and why. These are first-class lesson candidates — a correction made once should not have to be made again.
- **`phases`** — the mission's phases and how each ended (`complete` / `abandoned` / …). The shape of what the mission actually did.

For each recurring caution or repeated correction, the lesson is **not** "the detector fired" or "the reviewer corrected X once" — it is the *durable rule*: what the runs kept getting wrong, what to do instead, and **why**. The detection points at *where* to look; the operator supplies *what the lesson is*. (e.g. detector saw repeated edits to `loop_runner.rs` → the lesson might be "the retry loop is bounded at N on purpose — don't add another retry path; the loop entrenches its first answer.")

## Step 2 — Read the engagement's own docs + the shipped work

```bash
ls CLAUDE.md AGENTS.md DESIGN.md README.md CONTRIBUTING.md 2>/dev/null
```

Read whichever exist — pull out conventions, constraints, and decisions already written down, and cross-reference them against what the runs got wrong (a caution that contradicts a documented rule is a strong lesson: the rule isn't landing).

**For a coding mission**, look at the shipped work to ground the lessons in the actual diff — the debrief verb does NOT reconstruct diffs (the flow stream carries the activity, not the patch text), so pull it from git directly:

```bash
git log --oneline -15           # find the mission's shipped commits
git show <sha>                  # inspect a shipped phase's actual change
```

A non-coding mission has no diff — Steps 1 + 2's docs carry it.

## Step 3 — Interview the operator (fill the gaps)

A short, targeted interview — not a form. Ask only what the material didn't already answer:

- For each recurring caution: "is this a real lesson, or just how that run went? If real, what should a future dispatch do instead, and why?"
- For each correction: "should this become a standing rule for this engagement, or was it one-off to that phase?"
- Open: "what did this mission teach about this codebase that isn't written down anywhere — that the next crew (or a local model) would get wrong?"

Keep it to a handful of questions. Stop when the operator's out of additions.

## Step 4 — Propose the lessons (operator approves before any write)

Present the candidates as a numbered list. For each: a **title** (the rule), a **body** (the rule + the why — the load-bearing part), an optional **file scope**, and the **tier** (repo default; `--global` only for universal). Example shape:

```
1. [repo] Bound the retry loop
   The runtime caps retries at N on purpose — the loop entrenches its first
   answer (self-verification dilemma), so re-running rarely corrects it. Don't
   add another retry path; escalate to a fresh-context review instead.
   (file: runtime/src/loop_runner.rs)

2. [global] American English
   House style across all engagements; no British spellings.
```

Ask the operator to approve, edit, drop, or re-scope each. **Do not write anything yet.**

## Step 5 — Record the approved lessons

For each approved lesson, one `darkmux lessons add` (the `--file` and `--global` flags as decided):

```bash
darkmux lessons add \
  --title "Bound the retry loop" \
  --body "The runtime caps retries at N on purpose — the loop entrenches its first answer (self-verification dilemma), so re-running rarely corrects it. Don't add another retry path; escalate to a fresh-context review instead." \
  --file runtime/src/loop_runner.rs
```

```bash
darkmux lessons add --title "American English" --body "House style across all engagements; no British spellings." --global
```

Then confirm what landed:

```bash
darkmux lessons list
```

## Step 6 — Tell the operator how it pays off

The banked lessons surface to the next coder dispatch in this repo as a `<lessons>` block at the top of the brief (authoritative — followed, not just suggested). The dispatch logs "carrying N engagement lesson(s) into the brief." Two ways to see the effect:

- Run a `darkmux mission launch coder-phase` dispatch in this repo and watch the brief carry the lessons.
- For a measured A/B, use the loop lab to compare a dispatch's verdict with vs without the lessons — the empirical test of whether a given lesson actually changed behavior. That's how you learn which lessons are worth keeping and how to phrase the why.

## Notes

- **The why is the point.** A lesson that only states a rule ("don't rename the field") is weak; one that carries the reasoning ("don't rename `apim_key` — three downstream configs match on the literal string") lets a model apply judgment. If a proposed body has no why, push for one in the interview before recording it.
- **Curated, not automatic.** The skill never promotes a caution or correction to a lesson on its own — the operator decides what's a durable lesson vs a one-off. A detector firing is evidence, not a verdict.
- **Cross-mission, not within-mission.** Within a mission, corrections + cautions already carry phase→phase live. The debrief banks lessons for the *next* mission; run it at completion, not mid-arc (though `darkmux mission debrief` is read-only and safe to run any time you're curious).
- **Engagement-scoped by construction.** Per-repo is the default; reserve `--global` for conventions true everywhere. Don't let one engagement's specifics leak into the global tier.
