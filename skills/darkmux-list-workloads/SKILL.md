---
name: darkmux-list-workloads
description: List the workloads available to 'darkmux lab run' — both bundled built-ins and any user-defined manifests. Use this to discover workload IDs before invoking 'darkmux-lab-run'.
user_invocable: true
allowed-tools: "Bash(darkmux:*)"
---

# List workloads

ARGUMENTS: none — this skill takes no arguments.

## Step 1 — List

```bash
darkmux lab workloads
```

## Step 2 — Output shape

A simple newline-separated list of workload IDs:

```
quick-q
long-agentic
bounded-todo
probe-recall
```

## Step 3 — Suggest follow-ups

Surface the list to the user, then offer the natural next steps:

- "Pass any workload ID to `darkmux-lab-run` to dispatch it"
- "Each ID maps to a JSON manifest under `templates/builtin/workloads/` (built-in) or `.darkmux/workloads/` (user-defined)"

## Notes

- "(no workloads found ...)" means neither the built-in templates nor any user-defined workloads were located. Check the darkmux installation, or set `DARKMUX_TEMPLATES_DIR` to point at the templates source.
- User-defined workloads under `.darkmux/workloads/<id>.json` (or `.darkmux/workloads/<id>/workload.json`) shadow built-ins of the same name.
- Workload manifests follow the schema documented in the darkmux repo's `templates/builtin/workloads/` examples.
