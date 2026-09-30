---
name: darkmux-upgrade
description: Upgrade a darkmux 3.x home (`~/.darkmux`, or `$DARKMUX_HOME`) to 5.0 safely. Use it when the user is moving from 3.x, or when `darkmux doctor` reports retired keys, spellings, or paths, or darkmux refuses to start over a leftover env var or a `profiles.json` shape. Backs the home up first, then applies exactly what `darkmux doctor` names, in order, re-running doctor after each step and stopping to ask on anything that needs the user's judgment. One-time and optional; `darkmux init` does not install it.
user_invocable: true
allowed-tools: "Bash(darkmux:*), Bash(cp:*), Bash(cmp:*), Bash(mv:*), Bash(mkdir:*), Bash(ls:*), Bash(find:*), Bash(jq:*), Bash(bash:*), Bash(lms:*), Bash(env:*), Bash(date:*), Read, Edit"
---

# Upgrade a darkmux 3.x home to 5.0

The 5.0 release refuses retired config keys, env vars, and file shapes instead of guessing what they meant. Each refusal names its fix in `darkmux doctor`. This skill is the safe order for applying those fixes to the user's own files. It is a one-time procedure: follow it start to finish, once.

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
7. **The `darkmux` you run is the new one.** Do not run a 3.x binary against the home once you start editing it.

## Step 0: See what doctor says

```bash
darkmux --version
darkmux doctor
```

`doctor` and `config` are the only commands that run while a retired env var is set; every other command refuses. Doctor's failures are the work list. The steps below are in the order that worked, but doctor is the authority on what applies to this home.

The loader stops at the first problem in a file, so `doctor` may show only one `profiles.json` problem until that one is fixed. The row `user file keys: profiles.json` lists every refused key at once, but only once the file loads. Expect to run doctor several times.

## Step 1: Back up

Find the root (`$DARKMUX_HOME` when set, else `~/.darkmux`), then clone it. On macOS (APFS) the clone is fast and takes almost no space:

```bash
ROOT="${DARKMUX_HOME:-$HOME/.darkmux}"
cp -c -R "$ROOT" "$ROOT.backup-$(date +%Y%m%d)"
```

On another filesystem use `cp -a "$ROOT" "$ROOT.backup-$(date +%Y%m%d)"`.

Verify the backup before going on:

- Compare per-top-level-folder file counts between `$ROOT` and the backup with `find`. `liveness/` may differ by a few files, because live processes write there; that is expected.
- Run `cmp` on every file you are about to edit (`config.json`, `profiles.json`) against its backup copy.

If a check fails, stop and tell the user.

## Step 2: The shell rc and open shells

A retired env var is refused at start. `DARKMUX_NOTEBOOK_DIR` (retired in 4.0, #2913) is the common one; `DARKMUX_CREW_DIR` and `DARKMUX_RADIO_ROUTER_PROFILE` are refused the same way. Doctor's row `retired env vars (4.0)` names each one it finds.

1. Find the `export` line in the user's shell rc (`~/.zshrc`, `~/.bashrc`, or a file it sources) and remove it with the Edit tool. It is the user's file: show the line you removed. If they use the same name for something outside darkmux, say so and let them decide.
2. **Trap: an already-open shell keeps the old value.** The new binary refuses to start in that shell even after the rc is fixed. Open a new terminal, run `unset DARKMUX_NOTEBOOK_DIR` in the current one, or prefix commands with `env -u DARKMUX_NOTEBOOK_DIR`. Re-sourcing `.zshrc` may print harmless `compdef` noise.

## Step 3: `config.json`

`darkmux config` cannot delete a key, so these are hand edits (the Edit tool, or `jq` writing a `.new` file that you check with `jq -e .` before moving it into place). Delete each of these when present:

| Key | Why (from doctor) |
|---|---|
| `dirs.notebook` | Retired in 4.0 (#2913). The notebook verbs are gone. |
| `orchestrator` | Removed in #1766; `init` wrote it before 1.8. |
| `role_profiles.radio-router` | No effect since 4.0 (#2914); radio routing runs on `internal.utility`. |

Drop `dirs` if it becomes empty. Other retired keys have their fix in doctor's message: apply exactly what it names. Two of them are moves rather than deletions:

- `gh` becomes `cmd` (`gh.enabled` and `gh.allowed` move to `cmd.enabled` and `cmd.allowed`, #2003).
- `runtime.daemon_auth_enabled` is replaced by `serve.token_keychain` (#2988). Move the user's value there. Setting `serve.read_auth` to true is the user's decision: ask.

## Step 4: `profiles.json`

Write JSON edits with `jq` to a `.new` file, check it with `jq -e .`, then move it over the original (the backup from step 1 is what makes this safe). Show the user what changed, with any `auth` value redacted.

### 4a. A bare-string `internal.utility` (every 3.13 user)

```json
"internal": { "utility": "qwen/qwen3-4b-instruct-2507" }
```

becomes

```json
"internal": { "utility": { "id": "qwen/qwen3-4b-instruct-2507", "n_ctx": 68000 } }
```

**Judgment: ask for `n_ctx`.** It is the window the utility model loads at. A good default is the window most of the user's profiles already load that model at; `lms ps` shows what it is loaded at right now. Until this is fixed, compaction is off on every dispatch and radio cannot route.

```bash
jq --argjson n 68000 '.internal.utility |= (if type == "string" then {id: ., n_ctx: $n} else . end)' profiles.json > profiles.json.new
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

Delete or rename each of these when doctor names it:

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

After this step, `darkmux doctor` should load `profiles.json`. If a row still fails, read it: the loader stops at the first problem, so repeat 4a to 4c for what it now names.

## Step 5: Lab runs

Doctor's row `lab runs location` fails when `<root>/runs` still holds runs; 4.0 reads `<root>/lab`. Run the exact `mv` it prints, which is `mv -n <root>/runs <root>/lab` and only correct when `lab` does not exist. When runs are on both sides, doctor warns and prints a merge command instead; use that.

## Step 6: The old `crew/` layout

Doctor's row `beat-33 crew/ layout` fails when state is still under `<root>/crew/`. It prints a script; read it before running it. Every move in it is `mv -n`, so a name that already exists at the destination is left where it is and the script prints a `LEFTOVERS` line. Compare those two copies with the user, and let them pick which to delete.

**Trap: the script is bash, not zsh.** In zsh the `.[!.]*` glob aborts the whole command when nothing matches. Current doctor prints the script wrapped in `bash <<'DARKMUX_CREW_MERGE'` at column zero, unwrapped, so the block runs as printed from any shell. If the script you were shown is not wrapped, run it with `bash -c` or save it to a file and run `bash file`.

What it does:

- `crew/roles`, `crew/crews`, `crew/skills`, `crew/missions`, `crew/phases` move to the same names at the root, merging entry by entry when the destination exists.
- Pre-#148 **flat** mission and phase files (`crew/missions/<id>.json`) and `crew/sprints` are **kept, not deleted**: they move to `<root>/archive/pre-148-missions/`. 5.0 reads neither, and `darkmux mission migrate` no longer exists, so the archive is the answer. (An older build moved those files into `missions/`, where the next check then refused them. If doctor's row `mission state files` names flat files under `missions/` or `phases/`, its remedy is the same archive move.)
- `crew/role-model-pins.json` is never moved. Nothing reads it; doctor says to delete it. Ask before deleting.
- The autonomous-dispatch preamble override moves to `<root>/AUTONOMOUS_DISPATCH_PREAMBLE.md`.

Run doctor again. The row should pass, and `crew/` should be gone (`rmdir` succeeds only when it is empty; whatever remains is either the user's own or a `LEFTOVERS` line).

## Step 7: Mission state under `missions/<id>/`

Doctor's row `mission state files` names every file and the one-line fix. The renames:

| In | Old | New |
|---|---|---|
| `mission.json` | key `sprint_ids` | `phase_ids` |
| `mission.json` | key `closed_ts` | `finalized_ts` |
| `mission.json` | status value `closed` | `finalized` |
| `tasks/<phase>/*.json` | key `sprint_id` | `phase_id` |
| the mission directory | `sprints/` | `phases/` |

Rename keys in place, keeping their order, and never overwrite: skip the file and report it if the new key already exists. For each `mission.json`:

```bash
jq 'with_entries(if .key == "sprint_ids" and (has("phase_ids") | not) then .key = "phase_ids"
                 elif .key == "closed_ts" and (has("finalized_ts") | not) then .key = "finalized_ts" else . end)
    | if .status == "closed" then .status = "finalized" else . end' mission.json > mission.json.new
```

Use the same `with_entries` shape with `sprint_id` to `phase_id` for each task file. For the directory: `mv -n sprints phases` when `phases/` does not exist. When both exist, merge with `mv -n` per entry and report any name that stayed behind.

A large home can have hundreds of files, so loop in a script and report the counts (files, renames, directories). Re-run doctor at the end.

## Step 8: Mission configs, workloads, fixtures

Doctor lists each file under `user file keys: <file>`. The fixes it names:

- **Mission configs** (`<root>/mission-configs/*.json`):
  - Delete the top-level `panel` key. Every launchable config runs from the editor panel as `/mission launch <id>`.
  - Delete `role_id` on a task whose step is `mission.verify`; it had no effect (#2953).
  - `gh_verb` is renamed `cmd`, with `schema_version` set to `"3.0"`. A task's `expand` key was removed: declaring the expanded tasks explicitly is a judgment, so ask.
  - An older `schema_version` major only warns. Leave it, or bump it after the file passes.
  - A task's free-text `notes` is not refused. If the user wants it kept where darkmux reads it, fold it into the task's `description`; never delete it.
- **Workloads** (`<root>/workloads/*.json`): delete `workload.expected.test_count_baseline` (#2833; a coding workload's baseline lives in its fixture's `baseline.test_count`). Rename `workload.agent` to `role` (#328).
- **Lab fixtures** (each registered fixture's `.fixture.json`, which may live outside the darkmux root): delete `hash_exclude` and `hash_include` (#610).

## Step 9: Finish

```bash
darkmux doctor
```

The retired-state failures should be gone. What remains falls into two groups; report both and leave them to the user:

- **Not an upgrade blocker, the user's call:** temp-directory residue, stale skills (`darkmux init` refreshes them), the roster's loopback address or identity, state-file permissions, legacy audit files, stray hook outboxes, a Redis password.
- **Clears when another machine upgrades:** the row `retired work queue` fails while a 3.x peer daemon is still consuming the retired Redis queue. Nothing on this machine fixes it.

End with a short report: the backup path, each file changed, the judgments the user made (window size, endpoint ids), anything left as `LEFTOVERS`, and the `darkmux doctor` summary line. Do not paste any credential, machine uid, or tailnet name into it.

To undo everything, restore from the backup: `mv "$ROOT" "$ROOT.failed"` then `cp -c -R "$ROOT.backup-<date>" "$ROOT"`. The user decides when the backup is deleted.
