---
name: darkmux-mod-create
description: Write the mod a darkmux finding describes, from the frontier tier. darkmux's local seats detect (a `create_finding` tool call); this skill is the other half — a frontier subagent reads the finding, opens the tree it was observed in, writes the smallest applying unified diff, and records it with `darkmux mod create`. Invoke it when a hooks receiver (or `finding list`) shows findings with no mod, when a `review` run is waiting on `mod_wait_seconds`, or when the operator says "write the mods for <mission>". darkmux never calls a frontier model itself — this skill is how the frontier reaches it. (#2310)
user_invocable: true
allowed-tools: "Bash(darkmux:*),Bash(jq:*),Bash(git:*),Bash(cat:*),Bash(ls:*),Bash(grep:*),Bash(sed:*),Bash(diff:*),Read,Task"
---

# darkmux mod create — the frontier writes the kit

darkmux splits a review into **detection** and **proposal**. Detection runs on a
local seat inside darkmux's own runtime: the model calls `create_finding` and
darkmux writes the record. Proposal — turning that finding into a patch that
actually applies — moved to the frontier tier on 2026-09-05, after measurement:
across three local coder seats, 1 of 4, 0 of 3 and 1 of 10 kits applied cleanly,
where a clean-context frontier session given the same instructions wrote 4 of 4.

**darkmux still never calls a frontier model.** The seam is a hook plus this
skill: darkmux fires a flow record when a finding is recorded, an orchestrator
session (yours) sees it, and this skill's subagent writes the mod through the
ordinary `darkmux mod create` verb. Nothing in darkmux knows a frontier exists.

## Input

One of:

- **A finding key** — `<dispatch>/<seq>`, e.g. `crawl-crawl-1788455407-fc5219-u-0001/1`.
- **A mission id** — `crawl-1788455407-fc5219`. Work every finding that has no mod
  yet, **one subagent per finding**. Do not batch several findings into one
  subagent: each is a separate small reading job, and a shared context is exactly
  where a kit starts citing the wrong file.

## Step 1 — Read the finding

The store's root resolves the same way every darkmux path does
(`env(DARKMUX_FINDINGS_DIR) > config.json dirs.findings > <DARKMUX_HOME>/findings`).
Never read the files directly to find it — ask the verb:

```bash
darkmux finding show <dispatch>/<seq> --json          # one finding, whole
darkmux finding list --mission <mission-id> --json    # every finding in a run
darkmux mod list --mission <mission-id> --json        # what already has a mod
```

The record carries `emitted` (the model's own `file` / `line` / `pattern` /
`evidence` / `why`) and `context` (`workspace`, `source`, `sha`) — the tree the
finding was observed in, pinned.

**The STORED record's `file` is already repo-relative** — `darkmux finding show`
gives you the path the kit needs, as-is. The host mapped it at write time: the
model wrote its sandbox's coordinates (`/workspace/<source>/src/a.ts`), and
`build_record` maps the `/workspace/<source>/` prefix off before storing,
keeping the source id in the record's own `source` field so nothing is lost
(`findings.rs`, `FindingRecord::emitted` / `::source`).

Where you WILL see the container path is the **raw hook or flow record** —
`payload.emitted.file` on the fired `dispatch.tool` record is the model's
verbatim argument, unmapped. So if you are working from the stream rather than
the store, strip the `/workspace/<source>/` prefix yourself, or just read the
stored record instead, which is the simpler move. Either way the kit's paths are
repo-relative: `--- a/src/a.ts`, never the mount path.

## Step 2 — Open the tree the finding was observed in

The kit has to be a diff against **that** tree at **that** sha, not against
whatever is checked out on the operator's machine right now.

The materialized checkout is `<tree_root>/<source>`, where `tree_root` is the
directory holding one subdirectory per workspace source. The run recorded it: the
`create-mod` task's steps carry `for_key` and `workdir` in their config, and
`workdir` **is** `tree_root`.

```bash
MISSION=<mission-id>; KEY=<dispatch>/<seq>
TREE_ROOT=$(jq -r --arg k "$KEY" 'select(.config.for_key == $k) | .config.workdir' \
  ~/.darkmux/missions/"$MISSION"/steps/*/*.json | head -1)
SOURCE=$(darkmux finding show "$KEY" --json | jq -r .context.source)
CHECKOUT="$TREE_ROOT/$SOURCE"
```

If that path is gone (a cleaned-up run, or a finding synced from a flow file with
no run directory left), **re-materialize** rather than guessing: check the
workspace spec's origin out at the finding's own `context.sha` into a scratch
directory and read there. Say in your report which of the two you did.

Read the named file **around the named line** — enough context to judge, not the
whole file.

## Step 3 — Decide whether the finding holds

**If it does not hold, record nothing.** Say why, name what you read, and stop.
A wrong mod is worse than no mod: it passes a gate on an unrelated test target
and delivers as a one-click suggestion on a PR. `deliver.github_review` renders a
finding with no mod as a question ("worth a double check"), which is the honest
outcome for an unconfirmed intuition.

**Declining costs the run nothing.** A wait that ends with no mod completes
cleanly (`{"waited": true, "found": false, …}`, exit 0), the gate records its
ordinary no-mod skip, and the run stays Clean — a decline is a supported outcome,
not a failure. The same is true of a **search-form** finding, where the right
answer is a thread listing instances rather than a patch: record no mod and say
what you found.

## Step 4 — Write the smallest unified diff

The kit is applied by `git apply` inside `mods.gate`, so its shape is load-bearing:

- **Repo-relative paths, exactly as the finding's `file` spells them** after the
  container prefix is stripped — `--- a/src/x.ts` / `+++ b/src/x.ts`. Never the
  mount path, never an absolute path.
- **Context lines copied exactly** as they appear in the file you read, including
  indentation. One hunk per changed region; correct `@@` counts.
- **Plain text, not inside a code fence.** The kit is stored byte for byte.
- **Ends with a newline.**
- **Smallest change that resolves the finding.** Touch nothing the finding does
  not name; do not reformat, do not "while I'm here".

Verify before recording — this is cheap and catches every shape mistake:

```bash
git -C "$CHECKOUT" apply --check /tmp/kit.diff && echo applies
```

## Step 5 — Record it

```bash
darkmux mod create \
  --by "<your model or handle, e.g. claude-fable-5.1>" \
  --for "$KEY" \
  --kit-kind unified-diff \
  --kit /tmp/kit.diff          # or `-` to read stdin
```

`--for` is repeatable (one mod may answer several findings) and `--kit` takes a
path or `-`. **stdout is the new mod key, alone** — everything else goes to
stderr — so `MOD=$(darkmux mod create …)` is safe. Every call mints a NEW key;
recording twice makes two mods, not one.

**`--for <key>` needs that finding present in THIS machine's store.** A key
naming no stored finding is refused before anything is written, because the
mod would carry a link nothing can follow — the usual cause is a typo or a key
copied from an example. If the finding was recorded elsewhere (another
machine, or a run whose records have not landed yet), `darkmux finding sync`
replays the flow stream into the store first; `darkmux finding list` shows
what is there. `--allow-missing-finding` records the link anyway, for the case
you mean it. (#2386)

Then confirm and hand the key back:

```bash
darkmux mod show "$MOD"
```

darkmux does not open the kit. Its only judgment is the gate, which copies the
checkout, `git apply`s the kit and runs the run's `test_command` in the patched
copy. A gate-passed unified-diff kit renders as a one-click GitHub suggestion;
anything else renders as a patch comment or a question.

## The Claude monitor — watching for fired `create_finding` matches

The operator adds this rule to **their own** `~/.darkmux/config.json` (config is
operator state; darkmux never writes it for them):

```json
{
  "hooks": {
    "enabled": true,
    "rules": [
      { "match": { "action": "dispatch.tool",
                   "payload.tool_name": "create_finding",
                   "payload.ok": true },
        "http": "http://127.0.0.1:8790/events" }
    ]
  }
}
```

### What to watch, and what each source actually guarantees

Verified against the live sink (`crates/darkmux-flow/src/hooks.rs`) and the
operator's own hook files on 2026-09-05:

- **`~/.darkmux/hooks/<host-port>-<rule-hash>.outbox.jsonl`** — every matched
  record, appended whole (the raw flow record, before any `transform`). Delivery
  does **not** remove lines; the `.cursor` sibling holds a byte offset and the
  `.last` sibling the last delivery outcome. **But it is a queue, not an
  archive**: once the cursor passes 8 MiB (`DEFAULT_COMPACTION_THRESHOLD_BYTES`)
  `maybe_compact_outbox` rewrites the file down to its undelivered tail and
  resets the cursor to 0, and a write landing while the undelivered tail is over
  `DARKMUX_HOOKS_MAX_OUTBOX_MB` is dropped. So tail it if you like, but track
  your position by **record identity, not byte offset** — the file can shrink
  under you.
- **`~/.darkmux/flows/<YYYY-MM-DD>.jsonl`** — the day's flow file, the durable
  source. Filter it on the same three fields the hook rule matches. This is the
  one to prefer when you care about not missing a finding.

### The recipe

```bash
# Durable: the day's flow file, filtered on the hook rule's own three fields.
jq -c 'select(.action == "dispatch.tool"
              and .payload.tool_name == "create_finding"
              and .payload.ok == true)
       | {key: (.session_id + "/" + (.payload.emit_seq|tostring)),
          mission: .mission_id, file: .payload.emitted.file, why: .payload.emitted.why}' \
  ~/.darkmux/flows/$(date +%F).jsonl
```

**The finding key is not a field on the record** — it is composed:
`session_id + "/" + payload.emit_seq`. Confirmed against the live store, whose
directories are named by `session_id` with one numbered subdirectory per
`emit_seq`.

The same filter works on an outbox file (the lines are the same records), which
is the cheaper watch when a rule is already firing:

```bash
tail -f ~/.darkmux/hooks/*.outbox.jsonl | jq -c 'select(.payload.tool_name == "create_finding" and .payload.ok == true)'
```

Cross-check what still needs a mod before dispatching anything — the store is the
truth, the stream is the notification:

```bash
darkmux finding list --mission "$MISSION" --json | jq -r '.findings[].key' | sort > /tmp/f
darkmux mod list --mission "$MISSION" --json | jq -r '.mods[].for[]' | sort -u > /tmp/m
comm -23 /tmp/f /tmp/m      # findings with no mod yet
```

## Running inside a `review` run's wait window

`review`'s `create-mods` phase does not dispatch a coder. Its first step is a
bounded shell poll of `darkmux mod list --for <key>`, so a mod you record while
the run is waiting is picked up on the next ~5s cycle, gated, and delivered as an
inline suggestion.

- **`mod_wait_seconds` defaults to `0` — do not wait.** That is correct for the
  unattended path (a self-hosted runner has no orchestrator session to receive
  the hook and no frontier seat): the step completes at once, the gate finds no
  mod, and every finding delivers as a question. Detections only, no suggestions.
- **An attended run opts in**: `--param mod_wait_seconds=<N>`. Pick N from how
  long you actually need, and keep it under
  `runtime.step_command_timeout_seconds` (default 600) — that bound governs every
  `procedural.shell` command and will kill the poll first if it is smaller.

So: launch with a wait long enough for you to work, watch the stream, and record
each mod as its finding lands. Findings you decline still deliver — as questions,
which is what they are.

## What this skill must not do

- **Never edit the operator's tree.** The checkout is the finding's evidence; a
  mod is a proposal about it. Write the diff to a scratch file.
- **Never record a mod for a finding you did not read the code for.** The
  finding's `why` is a local model's claim, not a fact.
- **Never `darkmux mod create` twice for one finding "to fix" the first.** Keys
  are minted, not derived; you would leave two mods and the gate would rule on
  both.
- **Never run the run's tests yourself.** `mods.gate` does that, in a patched
  copy, which is what makes the confirmation mechanical.
