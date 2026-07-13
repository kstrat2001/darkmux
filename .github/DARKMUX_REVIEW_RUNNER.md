# darkmux self-review runner

`.github/workflows/darkmux-review.yml` lets darkmux review its own PRs on a
**local review pipeline** — a crew of local models, not a single reviewer — in
public, darkmux dogfooding itself (#1222 Phase B). This doc is the one-time
setup for the self-hosted runner that powers it.

## How it runs (and why it's safe on a public repo)

The workflow is **`workflow_dispatch` only** — it never auto-fires on a PR
event. A stranger's PR (or a fork) cannot trigger it; only a maintainer with
write access launches it:

```bash
gh workflow run darkmux-review.yml -f pr=<PR_NUMBER>
# optional overrides, for testing the review pipeline against another setup:
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
findings) in the sandboxed, network-isolated internal runtime. The pipeline's
own GitHub file source (used when a probe or the judge wants to see more of a
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
   - `darkmux` on PATH (`cargo install --path .` from this repo, or `brew install darkmux`).
   - A **Rust toolchain** (`cargo`) — the workflow builds the reference `--bundler`
     plugin (`darkmux-bundler-rust`, #1319) from the trusted `main` checkout on
     every run, since darkmux's own source is Rust and the built-in bundler is
     TypeScript-only. If your runner setup only ever installed the `darkmux`
     binary via `brew` (no local toolchain), install one (`rustup` or `brew
     install rust`) — this is the same toolchain `cargo install --path .` above
     already needs if you built `darkmux` from source.
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

If your runner is still on the pre-crew `diff-review` profile + `pr-reviewer`
role setup, here's the path to the crew-based review pipeline:

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
    "qwen4b": {
      "description": "Probe seat, second voice — a fast 4B instruct at 32K.",
      "models": [{ "id": "qwen3-4b-instruct-2507", "n_ctx": 32000 }]
    },
    "judge-35b-moe": {
      "description": "Judge seat — the 35B MoE weighing the probes' findings.",
      "models": [{ "id": "qwen/qwen3.6-35b-a3b", "n_ctx": 32000 }]
    }
  },

  "crews": {
    "review-deep": {
      "description": "32GB-tier review pipeline: a wide instruct pair on the probe seat plus a fast MoE judge — the measured reference configuration.",
      "seats": {
        "review-probe": [
          { "profile": "devstral", "k": 3, "max_tokens": 3000 },
          { "profile": "qwen4b", "k": 2, "max_tokens": 4000 }
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
lms get qwen3-4b-instruct-2507
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

**A note on deep-reasoner probe seats (do NOT add one on a 32GB M1-class box).**
High-memory machines (64GB+, current-generation Max/Ultra bandwidth) can add a
third probe staffing — a dense ~27B reasoner with a `bundle_selector` scoping it
to a few high-value bundles — which measurably reaches a bug class the wide
pair cannot (deep relational/temporal mechanisms). The cost is real: one deep
draw is a 20-30K-token reasoning pass, roughly 7-9 minutes on an M5-class Max
and 30-60+ minutes per draw on an M1 Max — hours per PR. On bandwidth-limited
32GB machines, skip the deep seat (the wide-pair crew above is the validated
reference configuration) and treat deep prosecution as a higher-tier machine's
job or, in a future release, a remote-staffed seat.

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

- The review is **advisory** (no merge gate). Review quality depends on the
  crew you staff: a probe seat surfaces candidates, the judge seat weighs
  them — pair it with a human/frontier pass on substantive PRs regardless of
  crew composition.
- The model choice is operator-tunable **on the runner**: edit the
  `review-deep` crew (or whichever crew you dispatch) in
  `~/.darkmux/profiles.json`. The workflow never pins a model id in the repo
  — it only ever names a crew (#1054).
- **Data boundary — REMOTE (hosted-endpoint) seats send code off-box (#1260).**
  A remote-staffed seat (probe / judge / verify pointed at a hosted endpoint
  profile) transmits the diff, the surrounding code, and the extracted facts to
  that endpoint. Only staff a remote seat whose endpoint is **cleared for the
  code it will see** — an org-approved deployment (e.g. private/proprietary code
  → the org's own Azure tenant only, never a personal-key third-party vendor).
  This is operator-explicit by construction: profiles name their own endpoint
  and nothing auto-routes; darkmux never picks a remote endpoint for you.
