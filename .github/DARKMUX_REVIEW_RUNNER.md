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
```

(or the **Run workflow** button under Actions → *darkmux self-review*). The job
then reads only the PR **diff** via the GitHub API — it never checks out or runs
the reviewed PR's code — and dispatches it to darkmux's tool-less `pr-reviewer`
role in the sandboxed, network-isolated internal runtime. The findings post back
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
   - LMStudio reachable at the default endpoint with **`qwen/qwen3-8b`** available
     (loaded, or JIT-loadable). To pin it, the workflow sets `DARKMUX_PROFILES` to
     `.github/darkmux-review-profile.json` — change the model id there if you want
     a different review model. NOTE: qwen3_5-family models (e.g.
     `qwen3.6-35b-a3b`) route their answer to `reasoning_content` and leave the
     message content empty under the current LMStudio reasoning-parser config,
     which yields **empty reviews** — qwen3-8b emits content and is the working
     default until that config is sorted.
   - `python3` + `gh` on PATH (GitHub's runner image bundles `gh`).

## Notes

- The review is **advisory** (no merge gate) and runs a local 8B-class model:
  strong on obvious/security/test-coverage issues, shallower on deep semantic /
  cross-file bugs — pair it with a human/frontier pass on substantive PRs.
- The model choice is operator-tunable in `.github/darkmux-review-profile.json`.
  Pick a model that emits its answer in the message **content** (not only
  `reasoning_content`) — qwen3-8b does; the qwen3_5-family thinking models
  currently don't (see the prerequisite note above).
