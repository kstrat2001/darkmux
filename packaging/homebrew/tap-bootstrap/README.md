# homebrew-darkmux

Homebrew tap for [darkmux](https://github.com/kstrat2001/darkmux) — a Rust CLI
for managing local LLM stacks (LMStudio, Ollama, llama.cpp) on Apple Silicon.

> **Unofficial third-party tap.** Not affiliated with or endorsed by the
> Homebrew project. The `homebrew-<name>` repo-naming pattern is Homebrew's
> documented convention for community taps; see
> [docs.brew.sh/Taps](https://docs.brew.sh/Taps).

## Install

```bash
brew tap kstrat2001/darkmux
brew install --HEAD darkmux
```

`--HEAD` is required while darkmux is pre-v0.5.0. Once a tagged release exists,
plain `brew install darkmux` will install a stable pinned version.

For a hub machine (Redis-backed multi-machine fleet coordinator):

```bash
brew install redis
brew services start redis
brew services start darkmux
```

See the [always-on hub guide](https://darkmux.com/guide/always-on-hub.html)
for the full setup (Redis hardening, audit substrate, log rotation, daily
integrity checks).

## Upgrade

```bash
brew upgrade --HEAD darkmux
brew services restart darkmux           # if you're running the daemon
```

## Uninstall

```bash
brew services stop darkmux              # important — stops the launchd plist
brew untap kstrat2001/darkmux           # optional; removes the tap reference
brew uninstall darkmux
```

Running `brew uninstall darkmux` without first stopping the service may
leave a dangling launchd plist until the next reboot.

## What's in this tap

- `Formula/darkmux.rb` — the formula. Currently head-only (no v0.5.0 tag
  yet — tracked in [darkmux#618](https://github.com/kstrat2001/darkmux/issues/618)).
  Installs the `darkmux` CLI + the `serve` daemon + a keychain-aware
  wrapper script (`libexec/darkmux-serve-wrapped`) that resolves
  `DARKMUX_REDIS_URL` from macOS Keychain at process-start.

## Scope of `brew install darkmux`

**Included:** the `darkmux` CLI (`swap`, `profiles`, `status`, `doctor`,
`fleet`, `flow`, `init`), the `serve` daemon, the keychain wrapper, and
the bundled skills.

**Not included:** the `darkmux-runtime` Docker image that `darkmux crew
dispatch` and `darkmux lab run` need — that requires a source checkout
of darkmux + `docker build -t darkmux-runtime:latest runtime/`. A
published image is tracked in
[darkmux#618](https://github.com/kstrat2001/darkmux/issues/618).

`brew install darkmux` is the complete install for the **hub posture**
(coordinator running Redis + serve, no local dispatches) and for the
`swap` / `status` / `profiles` flows. For local dispatches on the same
machine, supplement with a runtime image from a source checkout, or
clone darkmux directly and use `cargo install --path .`.

## Privacy / telemetry

The formula does not collect or transmit any data. The wrapper script
reads from your local macOS Keychain (the `darkmux-redis` keychain item
if present) to construct `DARKMUX_REDIS_URL` at daemon start; the
password is never written to any file on disk and never logged.

darkmux itself is operator-controlled — see the
[main project README](https://github.com/kstrat2001/darkmux#readme) and
[operator sovereignty doctrine](https://github.com/kstrat2001/darkmux/blob/main/CLAUDE.md#operator-sovereignty-architectural-principle)
for the architectural framing.

## Warranty / liability

This tap and darkmux are released under the MIT license — no warranty.
See:

- [`LICENSE`](./LICENSE) — MIT license text (this tap)
- [darkmux `LICENSE`](https://github.com/kstrat2001/darkmux/blob/main/LICENSE)
- [darkmux `DISCLAIMER.md`](https://github.com/kstrat2001/darkmux/blob/main/DISCLAIMER.md)
  — plain-English version of the warranty disclaimer, with specifics
  about what darkmux does to your machine

**Pre-1.0 reality:** darkmux is currently v0.4.0. Breaking changes ship
cleanly without deprecation periods until 1.0. `brew install --HEAD`
pulls the latest `main` commit — operators get in-progress work
including any breaking changes. Pin a tag once v0.5.0+ is released if
you want a stable target.

## Formula updates

The source-of-truth formula lives in the main darkmux repo at
[`packaging/homebrew/darkmux.rb`](https://github.com/kstrat2001/darkmux/blob/main/packaging/homebrew/darkmux.rb).
The version in this tap is synced from there.

If the formula here drifts from the main repo, the main repo wins.
File issues against [darkmux#618](https://github.com/kstrat2001/darkmux/issues/618)
or open an issue here.

## Local development against this tap

```bash
# Edit packaging/homebrew/darkmux.rb in the main darkmux repo
brew tap-new --no-git kstrat2001/darkmux    # if not already tapped
TAP_DIR="$(brew --repository)/Library/Taps/kstrat2001/homebrew-darkmux/Formula"
cp /path/to/darkmux/packaging/homebrew/darkmux.rb "$TAP_DIR/"
brew audit --strict kstrat2001/darkmux/darkmux
brew install --HEAD --build-from-source kstrat2001/darkmux/darkmux
```

## License

MIT. See [`LICENSE`](./LICENSE).
