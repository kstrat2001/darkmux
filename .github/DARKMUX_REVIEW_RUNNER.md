# darkmux self-review runner

`.github/workflows/darkmux-review.yml` lets darkmux review its own PRs on a
**local review pipeline** — the same `reviewer` seat dispatched once per
planned finding-site across the diff (#2310 P4d) — in public, darkmux
dogfooding itself. This doc is the one-time setup for the self-hosted runner
that powers it.

## How it runs (and why it's safe on a public repo)

The workflow is **`workflow_dispatch` only** — it never auto-fires on a PR
event. A stranger's PR (or a fork) cannot trigger it; only a maintainer with
write access launches it:

```bash
gh workflow run darkmux-review.yml -f pr=<PR_NUMBER>
```

(post-#2431 fix loop) `pr` is the workflow's only `workflow_dispatch` input
today. The `-f mode=<mode>` override once documented here (`sequential` |
`parallel` | `auto`) belonged to the funnel-era pipeline (#2310 P4d deleted
the funnel and its bespoke launcher); the workflow never actually forwarded
it to the dispatch, so it has been removed from `workflow_dispatch.inputs`
entirely rather than left as a knob that silently did nothing. Staffing
lives on the runner's own `~/.darkmux/config.json` `role_profiles` map (see
below), not a per-run flag.

(or the **Run workflow** button under Actions → *darkmux self-review*). The job
reads the PR **diff** plus its **title and description** via the GitHub API —
all data, never checked out or executed — and dispatches them to `darkmux
mission launch review`, which plans each enabled rule against the diff
(`plan.sites`) and dispatches one `crawl.unit` reviewer task per planned site,
then gates and delivers any findings (`create-mods`/`deliver`) in the
sandboxed, network-isolated internal runtime. The pipeline's
own GitHub file source (used when a `reviewer` dispatch wants to see more of a
changed file than the diff shows) also reads file contents via the API as
data, never executed — same trust class as the diff. (The title + description
give the review its stated intent, so it assesses the diff against
its purpose instead of flagging the bug a fix removes — #1053.) The findings
post back as native inline review comments. The only checkout is of trusted
`main` (the review tooling), not the PR.

If you later want it to feel automatic without reopening the public-trigger
surface, add an `issue_comment` trigger gated on
`github.event.comment.author_association == 'OWNER'` so that **only your** `/review`
comment launches it — still maintainer-only.

## One-time runner setup (on the laptop)

1. **Register a self-hosted runner** for `kstrat2001/darkmux` with the label
   `darkmux-review` (the workflow targets `runs-on: [self-hosted, darkmux-review]`):
   - Repo → Settings → Actions → Runners → **New self-hosted runner**, follow the
     download/configure steps, and add `--labels darkmux-review` at the `config.sh`
     step (macOS/arm64 runner package).
   - Run it (`./run.sh`, or install as a service). The laptop must be awake +
     online when you dispatch a review.

2. **Prerequisites on the laptop** (the runner shells out to these):
   - A **Rust toolchain** (`cargo`) — the workflow now builds the `darkmux`
     binary itself, fresh, from the trusted `main` checkout on every run
     (#1359). Install one if the runner doesn't have it (`rustup` or
     `brew install rust`).
     **You do NOT need `darkmux` pre-installed on PATH for this workflow** —
     it builds its own copy into the job's workspace
     (`target/release/darkmux`) and invokes that explicit path, never a
     `~/.cargo/bin/darkmux` (or `brew`-installed) binary that could silently
     drift stale behind `main` (the bug #1359 fixed). Still handy to have
     `darkmux` on PATH for your own interactive use (`darkmux doctor`, the
     verification step below) — just know it's decoupled from what this
     workflow dispatches.
   - The `darkmux-runtime` Docker image present (Docker running; `darkmux` pulls/uses
     `darkmux-runtime:latest`).
   - A profile in the runner's `~/.darkmux/profiles.json` bound to the
     `reviewer` role via `role_profiles.reviewer` (#1475) — every review rule
     dispatches through this one seat. See **Staffing the runner's reviewer
     seat** below for a copy-pasteable example; for guidance on which model
     shape to pick, see [the missions guide's "Staffing the `reviewer`
     seat"](https://darkmux.com/guide/missions.html).
   - `jq` + `gh` on PATH (GitHub's runner image bundles both). The review
     payload is rendered as part of `darkmux mission launch review`
     (`--param emit=...`) — no `python3` needed; `jq` just splits the
     rendered payload's `mode`/`review`/`fallback_comment` fields for `gh`.

## Staffing the runner's reviewer seat

If your runner still has a `crews` map or a `review-probe`/`review-judge`
staffing left over from before #2310 P4d, here's the current setup. Reviewer
staffing is DERIVED, never declared: a plain `profiles` entry plus one
`role_profiles.reviewer` binding — no `crews` map, no probe/judge seats, no
`k`, no `bundle_selector`. Every review rule dispatches through that same
`reviewer` seat once per planned site. For guidance on which model shape to
pick (dense wide-instruct vs MoE reasoner vs hosted endpoint, and the data
boundary on the last one), see [the missions guide's "Staffing the `reviewer`
seat"](https://darkmux.com/guide/missions.html) — this checklist only covers
the runner-specific steps.

**(a) Update darkmux.**

```bash
brew upgrade darkmux
```

**(b) Add a profile to `~/.darkmux/profiles.json` and bind it to the
`reviewer` role.** **Profile names are machine-specific** — the example below
assumes a profile named `review-mid` pointing at a model you've actually
downloaded; substitute your own. This is a fragment to merge into your
existing `profiles.json`, not a whole file:

```json
{
  "profiles": {
    "review-mid": {
      "description": "Reviewer seat — a dense wide-instruct model reading diffs in one pass.",
      "models": [{ "id": "mistralai/devstral-small-2507", "n_ctx": 32000 }]
    }
  }
}
```

```bash
darkmux config set role_profiles.reviewer review-mid
```

**(c) Make sure the model is downloaded.**

```bash
lms get mistralai/devstral-small-2507
```

**(d) Dispatch ordering — no residency knob.** An earlier funnel-era pipeline
had a `sequential` / `parallel` / `auto` residency split across its
probe/judge staffing; #2310 P4d deleted that pipeline and its bespoke
launcher. The shipped `review.json` pipeline runs each planned unit as a
`crawl.unit` dispatch under the generic scheduler, which owns dispatch
ordering directly — there is nothing to set here. Staffing (which model the
`reviewer` role resolves to) lives entirely on the runner's own
`~/.darkmux/config.json` `role_profiles` map, per (b) above.

**(e) Verify.**

```bash
darkmux mission launch review \
  --param workspace=<workspace-spec.json naming any local repo> \
  --param diff_file=<small.diff>
```

`review.json` has no `crew`/`worktree`/dispatch-`mode` inputs — role staffing
comes from `~/.darkmux/config.json`'s `role_profiles.reviewer` binding on
this machine, not a launch param.

No `--timeout` needed: when it is omitted, the review launcher defaults each
single-shot call to 3600 seconds — the same per-call ceiling the retired
`pr-review run` used — not `mission launch`'s generic 600-second default.

A clean run prints (or writes, if you pass `--param emit=<path>`) a rendered
payload with `mode: "review"` and at least one finding from the `reviewer`
seat's dispatches. If it comes back `degraded`, re-check step (b)/(c) before
dispatching against a real PR.

## Notes

- The review is **advisory** (no merge gate). Review quality depends on
  which model you bind to the `reviewer` seat — pair it with a human/frontier
  pass on substantive PRs regardless of which model you choose.
- The model choice is operator-tunable **on the runner**: change the
  `role_profiles.reviewer` binding via `darkmux config set` (or hand-edit
  `~/.darkmux/config.json`). The workflow never pins a model id or profile
  in the repo — staffing lives entirely on the runner (#1475).
- **Data boundary — a REMOTE (hosted-endpoint) `reviewer` seat sends code
  off-box (#1260).** A remote-staffed `reviewer` seat transmits the diff and
  any additional file contents it requests to that endpoint (the optional
  `mod_seat_profile` coder seat behind `create-mods` carries the same
  boundary for the finding + source it fixes). Only staff a remote seat
  whose endpoint is **cleared for the code it will see** — an org-approved
  deployment (e.g. private/proprietary code → the org's own Azure tenant
  only, never a personal-key third-party vendor). This is operator-explicit
  by construction: profiles name their own endpoint and nothing auto-routes;
  darkmux never picks a remote endpoint for you.
