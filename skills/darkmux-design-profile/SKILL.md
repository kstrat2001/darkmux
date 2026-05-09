---
name: darkmux-design-profile
description: Generate a darkmux profile from a description of how the user's agent will be used. The user describes their agent's task shape (single-turn / mid / long-agentic) and constraints; this skill picks an appropriate model + task class + knobs and emits a profile JSON ready to paste into ~/.darkmux/profiles.json.
---

# Design a darkmux profile from an agent description

## When to use this skill

The user is describing an agent (or use case) that needs its own profile entry — for example: *"I want a code-review agent that does single-turn audits on small PRs"* or *"I'm building a multi-file refactor assistant that needs a lot of context."* They don't necessarily know the right n_ctx, whether to pair a compactor, or what compaction knobs matter.

This skill turns that description into a working profile JSON.

## Step 1 — Gather the inputs

Ask the user (in one short message) for:

1. **A profile name** — short kebab-case identifier (e.g. `code-review`, `refactor-deep`, `notes-fast`).
2. **What the agent does** — 1-2 sentences describing the task shape.
3. **Which model** — exact LMStudio modelKey (run `lms ls` if they're unsure). Optional; if unspecified, fall back to whatever's currently their default-profile primary.

If they've already given enough information to infer all three, skip the prompt.

## Step 2 — Classify the task

From their description, pick ONE task class:

- **`fast`** — single-turn audits, classification, short Q&A, code review of 1-2 files, anything that completes in a single dispatch. No compactor. Slim ctx (~32K).
- **`mid`** — TODO fills, focused refactors of 3-10 files, mixed-shape agentic work that *might* go multi-turn but rarely needs >100K context. Companion 4B compactor. Tuned compaction knobs.
- **`long`** — open-ended audit, multi-file refactor, exploratory test authoring, anything where the agent might fire compaction multiple times in a single dispatch. Maximum primary ctx + companion compactor at 120K.

Default if you're unsure: **`mid`**. It's the most general-purpose; the user can re-draft as `fast` or `long` later if it doesn't fit.

## Step 3 — Generate the draft

Run:

```bash
darkmux profile draft <name> --model <model-id> --task-class <fast|mid|long>
```

This emits a profile JSON to stdout with:
- Inline `_notes` explaining the heuristic choices made
- Sensible n_ctx capped at the model's claimed maxContextLength
- Compaction knobs (only when a compactor is paired): `maxHistoryShare 0.35`, `recentTurnsPreserve 5`, `customInstructions` preserve-verbatim string

## Step 4 — Show + offer to install

Display the JSON output to the user. Then:

- Tell them where it goes: the `profiles` block of `~/.darkmux/profiles.json`
- Mention they can tune any field (especially `description` and `customInstructions`) before saving
- After they've added it, suggest `darkmux doctor` to verify the registry parses

**Do NOT auto-edit `~/.darkmux/profiles.json`** — that's user state. Always print and let them paste.

## Step 5 — When to push back

If the user describes a workload that doesn't fit the heuristics — for example:

- *"I want long-agentic on a 1.7B model"* — note that tiny models don't benefit from compaction overhead; suggest `mid` or `fast` instead.
- *"I want fast on a 122B model"* — note that XL models barely fit even short ctx; suggest the user verify RAM via `darkmux doctor` first.
- *"I have a model that's not in `lms ls`"* — `darkmux profile draft` will still work with conservative defaults, but warn the user that the heuristics are weaker without metadata.

The goal is a profile the user trusts. If the heuristics produce something dubious, say so before they paste.

## Examples

```
User: I'm building a single-turn PR review agent on Hermes 70B.
Skill: <runs `darkmux profile draft hermes-pr-review --model nousresearch/hermes-4-70b --task-class fast`>
        <displays JSON, suggests pasting into ~/.darkmux/profiles.json>
```

```
User: My note-taking agent does open-ended structured writing.
Skill: <asks: do you want it to handle long sessions, or just be reliable for shorter ones?>
       <user clarifies: shorter, predictable>
       <runs `darkmux profile draft notes-fast --model qwen3.6-35b-a3b-turboquant-mlx --task-class fast`>
```
