# darkmux self-review runner

`.github/workflows/darkmux-review.yml` lets darkmux review its own PRs on a
**local review funnel** — a crew of local models, not a single reviewer — in
public, darkmux dogfooding itself (#1222 Phase B). This doc is the one-time
setup for the self-hosted runner that powers it.

## How it runs (and why it's safe on a public repo)

The workflow is **`workflow_dispatch` only** — it never auto-fires on a PR
event. A stranger's PR (or a fork) cannot trigger it; only a maintainer with
write access launches it:

```bash
gh workflow run darkmux-review.yml -f pr=<PR_NUMBER>
# optional overrides, for testing the funnel against another setup:
#   -f crew=<name>   review crew to dispatch (default: the DARKMUX_REVIEW_CREW
#                    repo variable, falling back to "review-deep")
#   -f mode=<mode>   sequential | parallel | auto (default: auto)
#   -f k=<n>         override draws-per-seat for every staffing in the crew
```

(or the **Run workflow** button under Actions → *darkmux self-review*). The job
reads the PR **diff** plus its **title and description** via the GitHub API —
all data, never checked out or executed — and dispatches them to `darkmux
pr-review run`, which drives the named crew's seats (a `review-probe` seat
that argues the diff, then a `review-judge` seat that weighs the probes'
findings) in the sandboxed, network-isolated internal runtime. The funnel's
own GitHub file source (used when a probe or the judge wants to see more of a
changed file than the diff shows) also reads file contents via the API as
data, never executed — same trust class as the diff. (The title + description
give the funnel the change's stated intent, so it assesses the diff against
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
   - `darkmux` on PATH (`cargo install --path .` from this repo, or `brew install darkmux`).
   - The `darkmux-runtime` Docker image present (Docker running; `darkmux` pulls/uses
     `darkmux-runtime:latest`).
   - A **`review-deep` crew** in the runner's `~/.darkmux/profiles.json`, naming
     the profiles (never a raw model id) that staff its `review-probe` and
     `review-judge` seats (#1054, #1222 Phase B packet 1). See the **Studio
     migration checklist** below for a copy-pasteable example.
   - `jq` + `gh` on PATH (GitHub's runner image bundles both). The review
     payload is rendered as part of `darkmux pr-review run` (`--emit`) — no
     `python3` needed; `jq` just splits the rendered `{mode, review, comment}`
     for `gh`.

## Studio migration checklist (moving from the single-reviewer setup)

If your runner is still on the pre-funnel `diff-review` profile + `pr-reviewer`
role setup, here's the path to the crew-based funnel:

**(a) Update darkmux.**

```bash
brew upgrade darkmux
```

**(b) Add a `crews` section to `~/.darkmux/profiles.json`.** Crews are a
top-level sibling of `profiles`, keyed by crew name; each crew's `seats` map
names which profiles (and optionally which explicit model within a profile)
staff each seat, with a draws-per-item `k` and an optional `bundle_selector`
to scope which fact families a staffing draws from. **Profile names are
machine-specific** — the example below assumes profiles named `devstral` and
`qwen3.6-27b-review` pointing at models you've actually downloaded; substitute
your own. This is a fragment to merge into your existing `profiles.json`, not
a whole file:

```json
{
  "profiles": {
    "devstral": {
      "description": "Probe seat, plain draws — devstral at 32K.",
      "models": [{ "id": "mistralai/devstral-small-2507", "n_ctx": 32000 }]
    },
    "qwen3.6-27b-review": {
      "description": "Probe seat, bundle-scoped draws — qwen3.6-27b at 32K.",
      "models": [{ "id": "qwen/qwen3.6-27b", "n_ctx": 32000 }]
    },
    "judge-35b-moe": {
      "description": "Judge seat — the 35B MoE weighing the probes' findings.",
      "models": [{ "id": "qwen/qwen3.6-35b-a3b", "n_ctx": 32000 }]
    }
  },

  "crews": {
    "review-deep": {
      "description": "32GB-tier review funnel: two probe staffings (one plain, one bundle-scoped) plus a single judge staffing.",
      "seats": {
        "review-probe": [
          { "profile": "devstral", "k": 3 },
          {
            "profile": "qwen3.6-27b-review",
            "k": 1,
            "max_tokens": 24000,
            "bundle_selector": { "fact_families": ["param-flow"], "max_bundles": 8 }
          }
        ],
        "review-judge": [
          { "profile": "judge-35b-moe", "max_tokens": 20000 }
        ]
      }
    }
  }
}
```

**(c) Make sure the models are downloaded.**

```bash
lms get mistralai/devstral-small-2507
lms get qwen/qwen3.6-27b
# skip if the judge model is already on the box (it likely is — same family
# darkmux's other roles use)
lms get qwen/qwen3.6-35b-a3b
```

**(d) Dispatch mode: `sequential` is the 32GB-tier default.** A 32GB machine
can't hold every seat's model resident at once — `sequential` loads one seat's
model at a time (each probe staffing in turn, then the judge last), unloading
between seats. `parallel` is for machines with enough headroom to keep the
whole crew loaded simultaneously; `auto` lets darkmux decide from the runner's
observed RAM. Set it per dispatch with `-f mode=sequential` (or leave the
workflow's `auto` default — it should reach the same conclusion on a 32GB box).

**(e) Verify.**

```bash
darkmux pr-review run \
  --worktree <any local repo> \
  --diff <small.diff> \
  --crew review-deep \
  --mode sequential
```

A clean run prints (or writes, if you pass `--emit`) a `{mode, review,
comment}` payload with `mode: "review"` and at least the judge seat's
findings. If it comes back `degraded`, re-check step (b)/(c) before dispatching
against a real PR.

**Changing the workflow's default crew without editing YAML.** Set the repo
(or environment) variable `DARKMUX_REVIEW_CREW` — Settings → Secrets and
variables → Actions → Variables — to the crew name you want as the default
when `-f crew=` is left blank. The workflow reads it via `${{ vars.DARKMUX_REVIEW_CREW
|| 'review-deep' }}`, so setting/clearing the variable changes the default on
every future dispatch with no PR against this repo.

## Notes

- The review is **advisory** (no merge gate). Funnel quality depends on the
  crew you staff: a probe seat surfaces candidates, the judge seat weighs
  them — pair it with a human/frontier pass on substantive PRs regardless of
  crew composition.
- The model choice is operator-tunable **on the runner**: edit the
  `review-deep` crew (or whichever crew you dispatch) in
  `~/.darkmux/profiles.json`. The workflow never pins a model id in the repo
  — it only ever names a crew (#1054).
