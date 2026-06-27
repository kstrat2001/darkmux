# darkmux self-review runner

`.github/workflows/darkmux-review.yml` lets darkmux review its own PRs on a
**local model**, in public — darkmux dogfooding itself. This doc is the one-time
setup for the self-hosted runner that powers it.

## How it runs (and why it's safe on a public repo)

The workflow is **`workflow_dispatch` only** — it never auto-fires on a PR
event. A stranger's PR (or a fork) cannot trigger it; only a maintainer with
write access launches it:

```bash
gh workflow run darkmux-review.yml -f pr=<PR_NUMBER>
# optional overrides (#1057), for testing the reviewer against another setup:
#   -f role=<name>     crew role to dispatch (default: pr-reviewer)
#   -f profile=<name>  profile/model to dispatch with (default: review;
#                      falls back to default_profile if undefined on the runner)
```

(or the **Run workflow** button under Actions → *darkmux self-review*). The job
reads only the PR **diff** plus its **title and description** via the GitHub API
— all data, never checked out or executed — and dispatches them to darkmux's
tool-less `pr-reviewer` role in the sandboxed, network-isolated internal runtime.
(The title + description give the reviewer the change's stated intent, so it
assesses the diff against its purpose instead of flagging the bug a fix removes —
#1053.) The findings post back
as native inline review comments. The only checkout is of trusted `main` (the
review tooling), not the PR.

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
   - A **`review` profile** in the runner's `~/.darkmux/profiles.json`, pointing
     at a lab-validated review model. The workflow dispatches with `--profile
     review` (#1054) — it NAMES the profile and lets *this machine* map it to a
     model, instead of pinning a model id in the public repo. If you haven't
     defined `review`, the dispatch falls back to your `default_profile`. Pick a
     model that emits its answer in the message **content**: `qwen/qwen3-8b` does;
     the qwen3_5-family thinking models (e.g. `qwen3.6-35b-a3b`) route their answer
     to `reasoning_content` and leave content empty under the current LMStudio
     reasoning-parser config, yielding **empty reviews**. The model must be
     available in LMStudio (loaded or JIT-loadable). Add a `review` entry to the
     `profiles` object of your existing `~/.darkmux/profiles.json` (this is a
     fragment to insert, not a whole file):
     ```json
     "review": { "models": [{ "id": "qwen/qwen3-8b", "n_ctx": 32000, "role": "primary" }] }
     ```
   - `python3` + `gh` on PATH (GitHub's runner image bundles `gh`).

## Notes

- The review is **advisory** (no merge gate) and runs a local 8B-class model:
  strong on obvious/security/test-coverage issues, shallower on deep semantic /
  cross-file bugs — pair it with a human/frontier pass on substantive PRs.
- The model choice is operator-tunable **on the runner**: edit the `review`
  profile in `~/.darkmux/profiles.json` (the workflow names `--profile review`;
  it never pins a model id in the repo). Pick a model that emits its answer in the
  message **content** (not only `reasoning_content`) — qwen3-8b does; the
  qwen3_5-family thinking models currently don't (see the prerequisite note above).
