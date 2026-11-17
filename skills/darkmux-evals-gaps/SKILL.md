---
name: darkmux-evals-gaps
description: Find the runs that would fill the most useful gaps in the darkmux evals data (evals.darkmux.com) and propose them as exact `darkmux lab run` commands with time estimates, so the site's charts and comparisons have continuity. Reads the site's API through darkmux (your upload token never leaves darkmux), compares coverage across setups and current eval versions, and checks what this machine can run. Read and propose only; nothing runs without the operator's approval.
user_invocable: true
allowed-tools: "Bash(darkmux:*), Bash(jq:*)"
---

# Evals gaps

Use this when the operator asks what to run next for darkmux evals, which runs would make the site's comparisons more useful, or why a chart has holes. The output is a short, ranked list of proposed runs this machine can do, each with its reason, the command and an estimate. The operator picks; you run only what they approve.

## Never handle the token

Every read goes through `darkmux lab site get`, which attaches the upload token stored for that site and prints JSON. Never read the keychain, `~/.darkmux/evals-token*` or any token file, and never pass a token on a command line. A reader without an account gets the public basics, which is all this skill needs. If `lab site get` reports no token for the site, say so and continue with what it returns; don't ask the operator to paste a token.

## Step 1: what the site has

```bash
darkmux lab site get /api/v1/evals > /tmp/evals-gaps-evals.json
darkmux lab site get /api/v1/runs --all-pages > /tmp/evals-gaps-runs.json
```

Add `--site <url>` to both when the operator is working against a preview. The runs file is one JSON array of runs. The fields used below are `eval.evalSlug`, `eval.slug`, `eval.current`, `workload.path`, `passed`, `activeSeconds`, `tokensPerSecond`, and `stack.{model, engine, quant, chip, hardware}`.

## Step 2: what this machine can run

```bash
darkmux --version
darkmux machine status
darkmux lab fixture list
darkmux lab workload list
darkmux profile list --json
```

- A current eval version declaring fixture schema 1.2 needs darkmux 5.9 or later. If the binary is older, say so first: every proposal for those evals would be refused.
- A workload is runnable here only when its eval is registered at the current version (`lab fixture list` shows the version). If an eval is registered at an older version, or not at all, propose registering the tagged checkout before any run: `git clone https://github.com/<repo> && git -C <dir> checkout <version tag>` then `darkmux lab fixture register <dir>`. Check that registering won't repoint a fixture another session relies on; if it might, suggest a scratch `DARKMUX_HOME`.
- Map site setups to local profiles by model and engine: a profile whose model, or whose endpoint's served model, is the setup's model. Propose only setups with a matching profile. Name the profile in the command.

## Step 3: the coverage matrix

A setup is model family + engine + quant + machine (provider runs, `stack.hardware == "unknown"`, have no machine). Group the current versions' runs by setup and workload:

```bash
jq -r '
def fam: (.stack.model // "?") | split("/") | last | ascii_downcase | sub("-(4bit|8bit|mlx)$";"");
def setup: "\(fam) | \(.stack.engine) | \(.stack.quant // "-") | \(if .stack.hardware == "unknown" then "elsewhere" else (.stack.chip // "?") end)";
def wl: (.workload.path // "?") | split("/") | last | sub("\\.json$";"");
map(select(.eval.current)) as $cur
| ($cur | map(wl) | unique) as $wls
| ($cur | group_by(setup))[]
| (.[0] | setup) as $s
| (group_by(wl) | map({key: (.[0] | wl), value: length}) | from_entries) as $c
| "\($s)\t" + ([$wls[] as $w | "\($w)=\($c[$w] // 0)"] | join(" "))
' /tmp/evals-gaps-runs.json
```

Run the same grouping without `select(.eval.current)` to see which setups exist only on older versions.

## Step 4: rank the gaps

Score each candidate by how much it adds to what a reader can compare, then by its cost. In order of value:

1. **A current eval with no local setup, or no provider baseline.** One row with 3 runs gives the eval its first comparison.
2. **Continuity across versions.** A setup with runs on an older version of an eval and none on the current one. Re-running it on the current version keeps the setup's line going when the site moves to the new version.
3. **Rows below 3 runs.** The pass-rate likely range is very wide at 1–2 runs, and one lucky run can move a setup onto the frontier. Top up to 3 runs per workload. For an eval with several workloads (invoice-coding's batches, biz-scrape's two sites), one run per workload counts toward the eval's total, so aim for 3 runs per eval and at least 1 per workload.
4. **Nearly complete rows.** A setup that ran most current evals but not all. Averages across evals only compare setups that ran the same evals, so filling the last one or two makes the setup comparable.
5. **Engine and machine pairs.** The same model on two engines (or two machines) where one side is missing an eval. That pair is the cleanest comparison the site can show, because the weights are the same.

Leave out:
- setups this machine can't run;
- evals whose current version this darkmux can't run;
- cells that already have 3 or more runs, unless the operator asks for more;
- anything the operator has said to avoid (a model that's too slow, an engine they're not using).

## Step 5: estimate and propose

For each proposal, estimate its time from the site's own data: the median `activeSeconds` of that setup on that eval, or else that eval's median across local setups, times the run count. Local runs go one at a time, so sum them. Provider runs (hosted APIs) can go alongside local runs. Give them their own total and note that they cost money.

Present at most about ten proposals, best first:

| # | Why | Command | Runs | Est. time |
|---|---|---|---|---|
| 1 | salt-shaker 1.3 has only a provider baseline | `darkmux lab run salt-shaker -p flint-rec -n 3` | 3 | ~9 min |

Then the totals (local, serial; hosted, parallel), any registration step from Step 2, and the submit step that follows: `darkmux lab submit <run>...`, one eval per submit. A `url` endpoint that declares no `hardware` needs `--hardware submitting-machine` or `--hardware unknown`. Pass the engine labels earlier runs of that setup used (`--engine`, `--engine-backend`, `--engine-version`, `--quant`, `--weights-dir`), so the new runs land in the same row.

## Step 6: run only what's approved

Real runs load models and use this machine's GPU, and hosted runs cost money. Ask which proposals to run. Run only those, local ones one at a time. Don't start builds or other heavy work while they run, because the site publishes their timings. Then offer the submit commands. Never submit without the operator's go-ahead, since a submit publishes to the site.
