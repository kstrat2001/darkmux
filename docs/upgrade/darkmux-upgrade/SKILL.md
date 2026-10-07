---
name: darkmux-upgrade
description: Upgrade a darkmux 3.x home (`~/.darkmux`, or `$DARKMUX_HOME`) to 5.0 safely. Use it when the user is moving from 3.x, or when `darkmux doctor` reports retired keys, spellings, or paths, or darkmux refuses to start over a leftover env var or a `profiles.json` shape. Backs the home up first, then applies exactly what `darkmux doctor` names, in order, re-running doctor after each step and stopping to ask on anything that needs the user's judgment. One-time and optional; `darkmux init` does not install it.
user_invocable: true
allowed-tools: "Bash(darkmux:*), Bash(cp:*), Bash(cmp:*), Bash(mv:*), Bash(mkdir:*), Bash(ls:*), Bash(find:*), Bash(grep:*), Bash(jq:*), Bash(bash:*), Bash(lms:*), Bash(env:*), Bash(date:*), Read, Edit"
---

# Upgrade a darkmux 3.x home to 5.0

The 5.0 release refuses retired config keys, env vars, and the `profiles.json` and mission-config shapes it names, instead of guessing what they meant. Each refusal names its fix in `darkmux doctor`. Everything else from before 5.0 (the old `crew/` layout, mission state in an old spelling, the old lab directory, retired verbs, leftover skills and roles) is no longer read or reported, and steps 5 to 7b cover those by hand. This skill is the safe order for applying all of it to the user's own files. It is a one-time procedure: follow it start to finish, once.

You edit the user's own state: their config, profiles, missions, and shell rc. Treat every file as theirs.

## When to use it

- The user installed 5.0 over a 3.x home, and darkmux now refuses to start, or `darkmux doctor` shows failures about retired keys, spellings, or paths.
- Every user who ran `darkmux init` on 3.13 hits at least two refusals in `profiles.json`: a bare-string `internal.utility` and an inline `endpoint` object on a model.

Do not use it on a fresh install, or to change settings that are not refused.

## Safety rules (these hold for every step)

1. **Back up before the first write** (step 1). Never skip it.
2. **Never overwrite.** Move with `mv -n`. When you rename a key, do nothing if the new key already exists, and report it.
3. **Fold, never delete, the user's own text.** A `_notes` field is the user's prose: append it to `description`. Delete only what doctor says nothing reads.
4. **Apply exactly what `darkmux doctor` names.** Do not tidy anything else. Re-run `darkmux doctor` after every step and read the new output before going on.
5. **Stop and ask on a judgment call.** The window size of the utility model, the id for an endpoint, which of two copies of a file is current, and whether to delete anything ambiguous are the user's decisions. Propose, then wait.
6. **Never print a secret.** Do not show an endpoint's `auth` value, any Keychain item, a machine uid, or a tailnet host name (`*.ts.net`) in your output or in a summary. When you show a JSON edit, redact those fields.
7. **The `darkmux` you run is the new one, called by path.** A 3.x install (brew) may be first on `PATH`, so a bare `darkmux` can be the old binary. Set `NEW` to the new binary's full path and run `$NEW` everywhere this skill says `darkmux`. Never run a 3.x binary against the home once you start editing it.

## Step 0: See what doctor says

```bash
$NEW --version    # must report the new major; if not, you have the wrong binary
$NEW doctor
```

`doctor` and `config` run whatever retired env vars are set. Every other command refuses to start while one of the three refused vars is set (`DARKMUX_CREW_DIR`, `DARKMUX_REMOTE_MAX_TOKENS_PER_STEP`, `DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION`), and only warns, then carries on, for the others (step 2 lists them). Doctor's failures are the work list for config, profiles, mission configs, workloads and fixtures. Steps 5 to 7b are not reported by doctor: check them yourself. The steps below are in the order that worked, but doctor is the authority on what applies to the files it names.

The row `user file keys: profiles.json` lists every refused key in that file at once, so read the whole row before editing. Still expect to run doctor several times: fixing one file can reveal the next.

## Step 1: Back up

Find the root (`$DARKMUX_HOME` when set, else `~/.darkmux`), then clone it. On macOS (APFS) the clone is fast and takes almost no space:

```bash
ROOT="${DARKMUX_HOME:-$HOME/.darkmux}"
cp -c -R "$ROOT" "$ROOT.backup-$(date +%Y%m%d)"
```

On another filesystem use `cp -a "$ROOT" "$ROOT.backup-$(date +%Y%m%d)"`.

Verify the backup before going on:

- Compare per-top-level-folder file counts between `$ROOT` and the backup with `find`. `liveness/` may differ by a few files, because live processes write there; that is expected.
- A copy of a live home also copies its lock files. If you later run doctor against a copy while 3.x is still running, its `host sampler` row warns `stale lock: pid <n>` naming the live 3.x process. That is expected and clears once that daemon stops.
- Run `cmp` on every file you are about to edit (`config.json`, `profiles.json`) against its backup copy.

If a check fails, stop and tell the user.

## Optional: run side by side with a live 3.x

If 3.x must keep running, upgrade a copy instead of the live home. Copy it (as in step 1), point `DARKMUX_HOME` at the copy for the new binary, and start the new daemon on another port.

**Trap: a copied home keeps absolute paths into the original.** `audit.dir` and any other path under `dirs` or elsewhere in the copy's `config.json` still point at the original home, so the new version would write into 3.x's audit chain. Search the copy's `config.json` for the original root and repoint each hit at the copy, then confirm with `$NEW doctor` that nothing resolves outside `$DARKMUX_HOME`. Set the copy's `redis.enabled` to false unless the new version should share the stream with 3.x.

## Step 2: The shell rc and open shells

A retired env var is either refused or ignored with a warning, by whether ignoring it is safe (`RETIRED_SETTINGS` in `darkmux_types::config`, applied at the CLI entry for every command except `doctor` and `config`):

- **Refused: the command does not start.** `DARKMUX_CREW_DIR` (state location: `DARKMUX_HOME` is the one root now), and `DARKMUX_REMOTE_MAX_TOKENS_PER_STEP` and `DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION` (ignoring a spend cap would remove it; the limits moved to the endpoint, step 4d).
- **Warned and ignored: the command runs, printing `env var ... is ignored: ...`.** `DARKMUX_NOTEBOOK_DIR`, `DARKMUX_RADIO_ROUTER_PROFILE`, `DARKMUX_ACK_DIR`, `DARKMUX_LOG`, `DARKMUX_REMOTE_STEP_BUDGET_POLICY`, `DARKMUX_REMOTE_CONCURRENT_CAP`, `DARKMUX_MACHINE_ROLLUP_ENABLED` and `DARKMUX_MACHINE_ROLLUP_PERIOD_SECONDS`.

Remove every one you find the same way. Doctor's row `retired env vars (5.0)` names each one it finds.

1. Find the `export` line in the user's shell rc (`~/.zshrc`, `~/.bashrc`, or a file it sources) and remove it with the Edit tool. It is the user's file: show the line you removed. If they use the same name for something outside darkmux, say so and let them decide. **If 3.x keeps running on this machine, leave the export in place** (3.x still reads it) and prefix each new-binary command with `env -u DARKMUX_NOTEBOOK_DIR` instead.
2. **Trap: an already-open shell keeps the old value.** A refused var (`DARKMUX_CREW_DIR`, the two `DARKMUX_REMOTE_MAX_TOKENS_*` vars) keeps the new binary from starting in that shell even after the rc is fixed, and a warned one keeps printing its warning. Open a new terminal, run `unset <VAR>` in the current one, or prefix commands with `env -u <VAR>`. Re-sourcing `.zshrc` may print harmless `compdef` noise.

## Step 3: `config.json`

Leftovers at their old defaults (what `init` wrote) only warn and are safe to delete; a value the user set is refused until it is moved (#3057). Doctor shows the first as a warning and the second as a failure.

`darkmux config` cannot delete a key, so these are hand edits (the Edit tool, or `jq` writing a `.new` file that you check with `jq -e .` before moving it into place). Delete each of these when present:

| Key | Why (from doctor) |
|---|---|
| `dirs.notebook` | Retired in 5.0 (#2913). The notebook verbs are gone. |
| `orchestrator` | Removed in #1766; `init` wrote it before 1.8. |
| `role_profiles.radio-router` | No effect since 5.0 (#2914); radio routing runs on `internal.utility`. |
| `remote` (the whole block) | Retired in 5.0 (#3035): limits live on the endpoint, and nothing is carried over. Do step 4d first if the user ever set a number there, then delete the block. 3.x's `init` wrote it into every config. |

Drop `dirs` if it becomes empty. Other retired keys have their fix in doctor's message: apply exactly what it names. Three of them are moves or respellings rather than deletions:

<!-- flow-action-guard:allow-start: retired config keys, not flow actions -->
- `gh` becomes `cmd` (`gh.enabled` and `gh.allowed` move to `cmd.enabled` and `cmd.allowed`, #2003).
<!-- flow-action-guard:allow-end -->
- `runtime.daemon_auth_enabled` is replaced by `serve.token_keychain` (#2988). Move the user's value there. Setting `serve.read_auth` to true is the user's decision: ask.
<!-- flow-action-guard:allow-start: retired hook action spellings, not emitted actions -->
- Hook rules that match on a retired action spelling are failed by doctor, which names each rule. Respell them: `step *` becomes `step.*` and `mission *` becomes `mission.*`. The dotted glob can match more than the old spelling did (it also matches actions the spaced glob never covered), so tell the user before changing a rule that acts on the match.
<!-- flow-action-guard:allow-end -->

## Step 4: `profiles.json`

Write JSON edits with `jq` to a `.new` file, check it with `jq -e .`, then move it over the original (the backup from step 1 is what makes this safe). Show the user what changed, with any `auth` value redacted.

### 4a. A bare-string or missing `internal.utility`

```json
"internal": { "utility": "qwen/qwen3-4b-instruct-2507" }
```

becomes

```json
"internal": { "utility": { "id": "qwen/qwen3-4b-instruct-2507", "n_ctx": 68000 } }
```

**A home may have no `internal.utility` at all.** Then this step is registering one, and both the model and the window are the user's call. One home chose `qwen/qwen3-4b-2507` at `32768`. The model must be downloaded and loadable in LM Studio (`lms ls` lists it); doctor's `utility model` row warns when the registered model is not loaded.

**Judgment: ask for `n_ctx`.** It is the window the utility model loads at. A good default is the window most of the user's profiles already load that model at; `lms ps` shows what it is loaded at right now. Until this is fixed, compaction is off on every dispatch and radio cannot route.

```bash
jq --argjson n 68000 '.internal.utility |= (if type == "string" then {id: ., n_ctx: $n} else . end)' profiles.json > profiles.json.new
# no internal.utility yet: register one
jq --arg id "MODEL-ID" --argjson n 32768 '.internal.utility //= {id: $id, n_ctx: $n}' profiles.json > profiles.json.new
```

### 4b. An inline `endpoint` object on a model

```json
{ "id": "gpt-5.1", "endpoint": { "url": "https://...", "auth": "..." } }
```

becomes a top-level `endpoints` map plus an id on the model:

```json
"endpoints": { "azure-openai": { "url": "https://...", "auth": "..." } }
...
{ "id": "gpt-5.1", "endpoint": "azure-openai" }
```

Doctor prints the suggested id and the exact path (`profiles.<name>.models[<i>].endpoint`). **Judgment: the id.** Doctor's suggestion is fine unless the user names it otherwise. Models with an identical endpoint object share one id. Do not print the `auth` values.

```bash
jq --arg p "PROFILE" --argjson i 0 --arg id "ENDPOINT-ID" \
  '.endpoints[$id] = .profiles[$p].models[$i].endpoint | .profiles[$p].models[$i].endpoint = $id' \
  profiles.json > profiles.json.new
```

### 4c. Keys with no effect, and renames

Delete or rename each of these. Doctor now names each as an unknown key and prints the closest valid one:

| Key | Fix |
|---|---|
| `profiles.<p>.models[].role` (#590) | Delete. Bind roles to profiles with `darkmux config set role_profiles.<role> <profile>` if the user relied on them. |
| `hooks` at the top level (removed with `swap`, #1426) | Delete. |
| `crews` at the top level (2.0, #1426) | Delete. |
| `profiles.<p>.runtime.configPath` or `config_path` (#1405) | Delete. |
| `profiles.<p>.runtime.contextTokens` (#709) | Rename to `context_tokens`. If `context_tokens` already exists, do not overwrite: report both values and ask. |
| `profiles.<p>.runtime.compaction.mode`, `model`, `customInstructions`, `maxHistoryShare`, `recentTurnsPreserve` (#1405) | Delete. Drop `compaction` and `runtime` if they become empty. |
| `profiles.<p>._notes` | The user's text. Append it to that profile's `description` (join with a space; if it is a list of lines, join them), then delete `_notes`. Never delete it alone. |

```bash
jq 'del(.hooks, .crews) | del(.profiles[].models[].role)' profiles.json > profiles.json.new
```

Any other key doctor names as unknown: it prints the closest valid key. Rename or delete exactly as it says.

### 4d. The `remote` limits move to the endpoint

5.0 retired the `remote` block in `config.json` (`remote.max_tokens_per_step`, `remote.max_tokens_per_execution`, `remote.step_budget_policy`, `remote.concurrent_cap`) because "remote" was the wrong axis: a local server on the same machine is an endpoint too. The limits are written per endpoint in `profiles.json`, and **nothing is carried over**: they are off until set.

| Old setting | New field, on `endpoints.<id>` |
|---|---|
| `remote.max_tokens_per_step` / `remote.max_tokens_per_execution` | `limits.tokens_per_dispatch` (per dispatch, one role execution, not per step) |
| `remote.step_budget_policy` | `limits.policy` (`off`, `warn`, `wait`; `wait` needs a `limits.window`) |
| `remote.concurrent_cap` | `limits.concurrent_calls`, on an endpoint with a `url` only (absent, its calls run one at a time; on a `managed` endpoint the field is refused) |

**Judgment: whether to set anything, and on which endpoint.** Read the `remote` block in the user's `config.json`. If every value is `null`, absent, or `concurrent_cap: 1` (the 3.x defaults), there is nothing to move: delete the block. 3.x's own default of `max_tokens_per_execution: 500000` was a safety net, not a choice: say so and ask whether they want a cap on a billed endpoint. If they set a number deliberately, ask which endpoint it applies to (one number was machine-wide; limits are per endpoint) and write it:

```bash
jq --arg id "ENDPOINT-ID" --argjson n 500000 '.endpoints[$id].limits.tokens_per_dispatch = $n' profiles.json > profiles.json.new
```

A `dispatch.map` step's `bucket_group` or `bucket_budget` in a mission config is refused by name: delete the key, and use the endpoint's rolling `limits.window` for a whole-run budget.

After this step, `darkmux doctor` should load `profiles.json`. If a row still fails, read it and repeat 4a to 4d for what it now names.

## Step 5: Lab runs (everyone upgrading from 3.x: do not skip)

3.13 wrote lab runs to `<root>/runs`. 5.0 reads `<root>/lab` and no longer checks the old place, so runs left in `<root>/runs` are simply not read: `darkmux run list --kind lab` shows none of them, with no warning. Move them yourself, never overwriting:

```bash
ls "$ROOT/runs" "$ROOT/lab" 2>&1 | head        # what is on each side
mv -n "$ROOT/runs" "$ROOT/lab"                 # when "$ROOT/lab" does not exist
rmdir "$ROOT/lab" && mv -n "$ROOT/runs" "$ROOT/lab"   # when an empty "$ROOT/lab" already exists
```

When runs are on both sides, merge entry by entry (`mv -n "$ROOT/runs"/* "$ROOT/lab"/`, then `rmdir "$ROOT/runs"`, which fails loudly on anything skipped) and report any name that stayed behind. If `DARKMUX_LAB_DIR` or `dirs.lab` is set, the lab directory is that path instead of `<root>/lab`: use it as the destination.

## Step 6: The old `crew/` layout

darkmux no longer checks for state under `<root>/crew/`, and the loader resolves `<root>/<subdir>/` only, so anything left there is invisible. Look with `ls "$ROOT/crew"`. Read before moving, and use `mv -n` for every move:

- `crew/roles`, `crew/crews`, `crew/skills`, `crew/missions`, `crew/phases` move to the same names at the root. When the destination exists, merge entry by entry, and compare any name that already exists on both sides with the user instead of choosing.
- Pre-#148 **flat** mission and phase files (`crew/missions/<id>.json`, `missions/<id>.json`, `phases/<id>.json` directly under the root) and `crew/sprints` are not read by 5.0 and `darkmux mission migrate` no longer exists. **Keep, do not delete:** move them to `<root>/archive/pre-148-missions/`.
- `crew/role-model-pins.json` is never moved. Nothing reads it. Ask before deleting.
- The autonomous-dispatch preamble override moves to `<root>/AUTONOMOUS_DISPATCH_PREAMBLE.md`.

`rmdir "$ROOT/crew"` succeeds only when it is empty; whatever remains is the user's own.

## Step 7: Mission state under `missions/<id>/`

darkmux no longer refuses a mission file in an old spelling, so each of these goes unnoticed until you look. A `mission.json` that still says `sprint_ids` loads as a mission with no phases. A task file that still says `sprint_id` does not load at all, because `phase_id` is required: the load of that phase's whole task list fails, so the phase shows no tasks and its steps cannot run. The `jq` rename below is the remedy for both. Check for the old spellings and rename them:

| In | Old | New |
|---|---|---|
| `mission.json` | key `sprint_ids` | `phase_ids` |
| `mission.json` | key `closed_ts` | `finalized_ts` |
| `mission.json` | status value `closed` | `finalized` |
| `mission.json` | status value `paused` (the retired no-op pause) | `active` |
| `tasks/<phase>/*.json` | key `sprint_id` | `phase_id` |
| the mission directory | `sprints/` | `phases/` |

Find them with `grep -l -e sprint_ids -e closed_ts -e '"closed"' -e '"paused"' "$ROOT"/missions/*/mission.json` and `ls -d "$ROOT"/missions/*/sprints`. Rename keys in place, keeping their order, and never overwrite: skip the file and report it if the new key already exists. For each `mission.json`:

```bash
jq 'with_entries(if .key == "sprint_ids" and (has("phase_ids") | not) then .key = "phase_ids"
                 elif .key == "closed_ts" and (has("finalized_ts") | not) then .key = "finalized_ts" else . end)
    | if .status == "closed" then .status = "finalized" elif .status == "paused" then .status = "active" else . end' mission.json > mission.json.new
```

Use the same `with_entries` shape with `sprint_id` to `phase_id` for each task file. For the directory: `mv -n sprints phases` when `phases/` does not exist. When both exist, merge with `mv -n` per entry and report any name that stayed behind.

A large home can have hundreds of files, so loop in a script and report the counts (files, renames, directories).

## Step 7b: Leftovers darkmux no longer reports

darkmux used to find and name these. It now ignores them, so check once by hand:

- **Installed skills that darkmux no longer ships.** `darkmux init` used to delete retired `darkmux-*` skill directories; it now leaves them. A leftover can still teach an agent a dead verb. List `~/.claude/skills/darkmux-*` and the other agent skill directories `darkmux init` writes to, compare with `ls skills/` in the source tree (or the release's bundled list), and ask before deleting any. `darkmux doctor` still warns on an installed skill that differs from the bundled copy.
- **Retired roles in the user tier.** `<root>/roles/mission-compiler.json` (and its `.md`), `scribe.json` and `scribe.md` retired with `mission propose` and `lab notebook`. Nothing dispatches them, though a leftover `.json` still shows in `darkmux role list`. Ask, then delete.
- **A role manifest with `"role_family": "admin"`.** The value was renamed to `"utility"` long ago and a manifest still using it is now rejected as an unknown family. Set `"role_family": "utility"`.
<!-- flow-action-guard:allow-start — names the retired spellings to say what they now read as -->
- **Retired verbs.** A script that calls one now gets the usual unrecognized-subcommand error instead of a message naming the replacement. The replacements: `mission dispatch`, `mission add-phase`, `mission start`, `mission pause` and `mission resume` are gone (use `mission launch <config>`, `mission abort <id>` and `mission finalize <id>`); `lab run list|inspect|stats|compare` became `darkmux run list --kind lab`, `run inspect`, `run stats` and `run compare`; `lab eval` became `lab run <workload>` and `mission launch review`; `finding list --dispatch` is `--execution`; `--session-id` and `--session` on `dispatch`, `flow` and `memory correction list` are `--name` and `--execution`; `--runs` is `--repeat`; `mission status --missions` is `--named`; `dispatch --phase-id` is gone; `swap`, `status`, `model` and `fleet` folded into `machine` (`machine status`, `machine eject`, `machine list`); `lessons` is `memory lesson`.
<!-- flow-action-guard:allow-end — names the retired spellings to say what they now read as -->
- **Pre-2.6.0 audit files** (the struct-hash format: a header with no `hash_format`) are no longer verified. `darkmux flow integrity-check` reports each as a break at line 1 with 0 records checked (exit 2), `darkmux doctor` fails its `audit integrity` row naming the file, and the audit sink refuses to extend such a file. Nothing is recomputed, so this is not evidence of editing. The old "legacy" warning, `--strict` and exit 3 are gone. Archive the file (`mv -n` it aside) so a fresh chain starts; a torn-tail warning is separate and unchanged.
<!-- flow-action-guard:allow-start — names the retired spellings to say what they now read as -->
- **Flow archives written by 3.x.** Their free-form session ids (`task-<id>`, `mission-run-<m>-<p>`, `step-<id>`) are not read as sessions, and records in a retired action spelling read as an unknown action: they stay on disk and show in the event log, but attach to no mission, and no host-load track is drawn from a pre-5.0 `telemetry.process` record.
<!-- flow-action-guard:allow-end — names the retired spellings to say what they now read as -->

## Step 8: Mission configs, workloads, fixtures

Doctor lists each file under `user file keys: <file>`. The fixes it names:

- **Mission configs** (`<root>/mission-configs/*.json`):
  - Delete the top-level `panel` key. Every launchable config runs from the editor panel as `/mission launch <id>`.
  - Delete `role_id` on a task whose step is `mission.verify`; it had no effect (#2953).
  - `gh_verb` is renamed `cmd`, with `schema_version` set to `"3.0"`. A task's `expand` key was removed: declaring the expanded tasks explicitly is a judgment, so ask.
  - **Renamed step kinds (#2430).** `crawl.unit` is now `dispatch.unit` and `crawl.summary` is now `dispatch.summary`. A config that still names either is refused, and **one such file blocks every `mission launch`, the built-in `crawl` and `review` included**, so fix these first. Doctor names the file. Rewrite only those two ids, in only the files doctor names (the quotes keep it from touching any other id):

    ```bash
    sed -i.bak -e 's/"crawl\.unit"/"dispatch.unit"/g' -e 's/"crawl\.summary"/"dispatch.summary"/g' "$ROOT/mission-configs/<file>.json"
    ```

    Show the diff against the `.bak` before moving on, then delete the `.bak`. (`crawl.plan` and `plan.sites` keep their names.) Missions already run keep reading as they were; nothing under `missions/` needs editing.
  - An older `schema_version` major only warns. Leave it, or bump it after the file passes.
  - A task's `notes` in a mission config is refused as an unknown key (doctor names it, e.g. `phases[0].tasks[1].notes`). It is the user's own text, so fold it into that task's `description` (if it is a list of lines, join them), then delete `notes`. Never delete it alone.
- **Workloads** (`<root>/workloads/*.json`): delete `workload.expected.test_count_baseline` (#2833; a coding workload's baseline lives in its fixture's `baseline.test_count`). Rename `workload.agent` to `role` (#328).
- **Lab fixtures** (each registered fixture's `.fixture.json`, which may live outside the darkmux root): delete `hash_exclude` and `hash_include` (#610).

## Step 9: Finish

```bash
darkmux doctor
```

The retired config-key and `profiles.json` failures should be gone. What remains falls into two groups; report both and leave them to the user:

- **Not an upgrade blocker, the user's call:** temp-directory residue, stale skills (`darkmux init` refreshes them), the roster's loopback address or identity, state-file permissions, legacy audit files, stray hook outboxes, a Redis password.
- **A 3.x daemon still consuming the retired queue:** the row `retired work queue` fails while one is. The consumer can be another machine, or this machine's own 3.x daemon: check the consumer names in the row, and stopping this machine's 3.x `darkmux serve` is then the fix. Setting `redis.enabled` to false hides the row while the hole is still open, so do not use it to make the row go away.

End with a short report: the backup path, each file changed, the judgments the user made (window size, endpoint ids, endpoint limits), anything left as `LEFTOVERS`, and the `darkmux doctor` summary line. Do not paste any credential, machine uid, or tailnet name into it.

To undo everything, restore from the backup: `mv -n "$ROOT" "$ROOT.failed"` then `cp -c -R "$ROOT.backup-<date>" "$ROOT"`. The user decides when the backup is deleted.
