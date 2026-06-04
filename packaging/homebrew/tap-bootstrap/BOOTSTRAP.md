# Tap repo bootstrap

This directory holds the source-of-truth files for the
[`kstrat2001/homebrew-darkmux`](https://github.com/kstrat2001/homebrew-darkmux)
GitHub repo, plus the maintainer-facing docs for keeping it in sync.

**Status: tap repo created and seeded 2026-06-04** (commit `610d05b`).
Anyone can now run:

```bash
brew tap kstrat2001/darkmux
brew install --HEAD darkmux
```

## How this was initially bootstrapped (for posterity)

```bash
# 1. Create the repo on GitHub (public, MIT license)
gh repo create kstrat2001/homebrew-darkmux \
    --public \
    --license MIT \
    --description "Homebrew tap for darkmux (Rust CLI for managing local LLM stacks)" \
    --homepage "https://darkmux.com"

# 2. Clone it locally
gh repo clone kstrat2001/homebrew-darkmux /tmp/homebrew-darkmux
cd /tmp/homebrew-darkmux

# 3. Copy the bootstrap files (LICENSE overwrites the gh-generated one for
#    byte-identical match with darkmux's LICENSE)
DARKMUX_PUB=/path/to/darkmux-public
cp "$DARKMUX_PUB/packaging/homebrew/tap-bootstrap/LICENSE" LICENSE
cp "$DARKMUX_PUB/packaging/homebrew/tap-bootstrap/README.md" README.md

# 4. Add the formula
mkdir -p Formula
cp "$DARKMUX_PUB/packaging/homebrew/darkmux.rb" Formula/darkmux.rb

# 5. Commit + push
git add LICENSE README.md Formula/darkmux.rb
git commit -m "feat: initial tap content — formula + LICENSE + README (refs kstrat2001/darkmux#618)"
git push
```

## Ongoing maintenance — formula sync

The source-of-truth formula lives in
`packaging/homebrew/darkmux.rb` in the main darkmux repo. Two ways
to keep the tap in sync:

### A. Manual sync (simple; fine for early days)

After any formula change in the main repo, copy the file into the tap
repo:

```bash
cd /path/to/homebrew-darkmux
cp /path/to/darkmux-public/packaging/homebrew/darkmux.rb Formula/darkmux.rb
git diff Formula/darkmux.rb     # sanity check
git add Formula/darkmux.rb
git commit -m "sync: pull formula from darkmux@<sha>"
git push
```

### B. Automated sync (when manual gets old)

A GitHub Actions workflow in the main darkmux repo that, on push to
`main` that touches `packaging/homebrew/darkmux.rb`, opens a PR in
the tap repo with the updated file. Tracked as a follow-up to
[darkmux#618](https://github.com/kstrat2001/darkmux/issues/618).

## Future: bottled releases

Once Cargo.toml ships a real semver tag (item 4 in
[darkmux#618](https://github.com/kstrat2001/darkmux/issues/618)), the
formula gains a stable `url` + `sha256` block and operators can drop
`--HEAD`. The next step after that is BOTTLES — pre-compiled binaries
per macOS version uploaded to the tap's GitHub releases. The bottling
workflow lives IN THE TAP REPO (not in the main darkmux repo) per
Homebrew convention.

Bottle workflow template: see
[Homebrew/actions/setup-homebrew](https://github.com/Homebrew/actions/tree/master/setup-homebrew)
and the `brew test-bot` flow.

## Legal posture

The tap is a sibling repo to darkmux, NOT a fork. It contains:

- The formula file (pure metadata; references darkmux source)
- A LICENSE (MIT, matching darkmux)
- A README (this directory's README.md — the version that gets copied
  in is the tap-facing one)
- Optionally: a CI workflow for bottling

The wrapper script (`darkmux-serve-wrapped`) is NOT in the tap repo —
it lives in the main darkmux repo at
`packaging/homebrew/darkmux-serve-wrapped`. The formula's `install`
block pulls it from the darkmux source tree at build time
(`libexec.install "packaging/homebrew/darkmux-serve-wrapped"`).

This keeps:
- Tap repo small + focused (formula + metadata only)
- Wrapper script's license + maintenance co-located with darkmux
  (MIT, single source of truth)
- No code duplication between repos

See `packaging/homebrew/README.md` in the main repo for more context.
