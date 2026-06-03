# This is the source-of-truth Homebrew formula for darkmux. When the custom tap
# repo (kstrat2001/homebrew-darkmux per #618) is created, this file is copied
# into the tap as Formula/darkmux.rb. Editing it here keeps the formula
# version-controlled alongside the source it formulates.
#
# Operator-facing install path once the tap exists:
#   brew tap kstrat2001/darkmux
#   brew install --HEAD darkmux         # while only head is available
#   brew install darkmux                # once v0.5.0 is tagged and the formula
#                                       # has its stable url + sha256 block
#
# For local development / smoke testing:
#   brew install --build-from-source ./packaging/homebrew/darkmux.rb
#
# Tracking: #618 (this formula), #178 (skill it pairs with), #176 (sibling
# skill), #280 (related fleet primitive that simplifies post-formula).

class Darkmux < Formula
  desc "Profile multiplexer + lab for local LLM stacks (LMStudio, Ollama)"
  homepage "https://darkmux.com"
  license "MIT"

  # Pre-v0.5.0 posture: head-only. Once Cargo.toml ships a real semver tag
  # (item 4 in #618), add a stable url + sha256 block here and homebrew users
  # can drop `--HEAD` from the install command.
  #
  # When that lands, replace this comment with:
  #   url "https://github.com/kstrat2001/darkmux/archive/refs/tags/v0.5.0.tar.gz"
  #   sha256 "<run `shasum -a 256` against the GitHub tarball>"
  head "https://github.com/kstrat2001/darkmux.git", branch: "main"

  depends_on "rust" => :build

  # Redis is OPTIONAL — operator decides single-machine vs hub:
  #   brew install darkmux                         # CLI only (single-machine)
  #   brew install darkmux && brew install redis   # hub posture
  # Documented in caveats below, not as a hard depends_on. A formula that
  # always pulls redis would force every CLI user to install a daemon they
  # don't run.

  def install
    # Workspace-aware install. The root [[bin]] target in Cargo.toml is the
    # only thing that needs to land in bin/. The std_cargo_args helper sets
    # --locked + --root + --path so the result lands at #{bin}/darkmux.
    system "cargo", "install", *std_cargo_args(path: ".")

    # Wrapper script for `brew services start darkmux`. The wrapper reads
    # DARKMUX_REDIS_URL from macOS Keychain at process-start so the password
    # never lives in the launchd plist file. Operator stores the password
    # once via `security add-generic-password -a $USER -s darkmux-redis -w`
    # (see caveats); the wrapper does the read + export + exec.
    libexec.install "packaging/homebrew/darkmux-serve-wrapped"
    chmod 0755, libexec/"darkmux-serve-wrapped"
  end

  service do
    run [opt_libexec/"darkmux-serve-wrapped"]
    keep_alive true
    log_path var/"log/darkmux/serve.out"
    error_log_path var/"log/darkmux/serve.err"
    working_dir Dir.home
    # NOTE: env vars baked in at plist-generation time (NOT process-start).
    # Anything that needs to be resolved live (Redis URL from Keychain,
    # operator-named DARKMUX_MACHINE_ID) is handled by the wrapper script
    # above, which reads them at exec time. The static defaults below are
    # the safe-to-bake values.
    environment_variables(
      DARKMUX_REDIS_STREAM: "darkmux:flow",
      DARKMUX_REDIS_MAXLEN: "10000",
    )
  end

  def caveats
    <<~EOS
      darkmux is installed but the `serve` daemon is NOT started by default.

      Scope: this formula ships the `darkmux` CLI + `serve` daemon + the
      keychain wrapper + bundled skills. It does NOT ship the
      `darkmux-runtime` Docker image — `darkmux crew dispatch` and
      `darkmux lab run` need that image, which requires a source checkout +
      `docker build -t darkmux-runtime:latest runtime/` (a published image
      is tracked in #618). brew = complete install for `swap` / `status` /
      `profiles` / `fleet` / `flow` / `serve` / `doctor` and for the hub
      coordinator role. For local dispatches, supplement with a runtime
      image from a source checkout.

      For a single-machine CLI install (no daemon needed):
        # Already done. Run `darkmux --help` to explore.

      For a hub machine (Redis-backed, multi-machine fleet coordinator):
        1. brew install redis
           brew services start redis

        2. Set Redis password and store in Keychain. The wrapper script reads
           it at process-start (never lands in any file on disk).
             security add-generic-password -a "$USER" -s darkmux-redis -w
             # paste the password when prompted

           Then add `requirepass` to Redis matching that password:
             DARKMUX_REDIS_PASS=$(security find-generic-password \\
                                  -a "$USER" -s darkmux-redis -w)
             echo "requirepass \\"$DARKMUX_REDIS_PASS\\"" | \\
                  sudo tee -a #{HOMEBREW_PREFIX}/etc/redis.conf > /dev/null
             brew services restart redis

        3. In your shell rc (~/.zshrc), set per-machine identity:
             export DARKMUX_MACHINE_ID=studio     # operator-named; not hostname
             export DARKMUX_ORCHESTRATOR=claude-opus-4-7

           (The wrapper script picks up DARKMUX_MACHINE_ID from your env at
           launchd-load time. Re-run `brew services restart darkmux` after
           any change.)

        4. brew services start darkmux

      For the production-grade hub posture — Redis AOF persistence, audit
      substrate (BLAKE3 hash-chained JSONL; any post-hoc edit is surfaced by
      `darkmux flow integrity-check`), log rotation, and daily integrity
      checks — see the guide:

        https://darkmux.com/guide/always-on-hub.html

      The Phase 2 launchd-plist section there is the under-the-hood version
      of what `brew services start darkmux` does for you; the other phases
      (Redis hardening, audit, log rotation, integrity check) remain
      operator-driven steps that compose on top of the brew-managed service.
    EOS
  end

  test do
    assert_match "darkmux", shell_output("#{bin}/darkmux --version")
    assert_match "fleet",   shell_output("#{bin}/darkmux fleet --help")
    assert_match "doctor",  shell_output("#{bin}/darkmux --help")
    # Doctor should run end-to-end without panic; non-zero exit is fine
    # (it'll warn about most checks in a fresh install with no profile + no
    # Redis + no LMStudio).
    system bin/"darkmux", "doctor"
  end
end
