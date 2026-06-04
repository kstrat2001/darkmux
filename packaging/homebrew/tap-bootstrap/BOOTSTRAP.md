# Tap repo bootstrap

This directory holds the files needed to create the
`kstrat2001/homebrew-darkmux` GitHub repo. The repo doesn't exist yet
(tracked in [#618](https://github.com/kstrat2001/darkmux/issues/618)) —
when it's created, copy these files in, plus a `Formula/` directory
containing the formula from `../darkmux.rb`.

## One-time creation steps

```bash
# 1. Create the repo on GitHub (public, MIT license, minimal README)
gh repo create kstrat2001/homebrew-darkmux \
    --public \
    --license MIT \
    --description "Homebrew tap for darkmux (Rust CLI for local LLM stacks)"

# 2. Clone it locally
cd /tmp
git clone git@github.com:kstrat2001/homebrew-darkmux
cd homebrew-darkmux

# 3. Copy the bootstrap files (LICENSE comes from `gh repo create --license MIT`
#    but you can overwrite with the darkmux one for consistency)
DARKMUX_PUB=/path/to/darkmux-public
cp "$DARKMUX_PUB/packaging/homebrew/tap-bootstrap/LICENSE" LICENSE
cp "$DARKMUX_PUB/packaging/homebrew/tap-bootstrap/README.md" README.md

# 4. Add the formula
mkdir -p Formula
cp "$DARKMUX_PUB/packaging/homebrew/darkmux.rb" Formula/darkmux.rb

# 5. Commit + push
git add LICENSE README.md Formula/darkmux.rb
git commit -m "feat: initial formula + tap docs (refs darkmux#618)"
git push
```

After step 5 the tap is live. Anyone can run:

```bash
brew tap kstrat2001/darkmux
brew install --HEAD darkmux
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
