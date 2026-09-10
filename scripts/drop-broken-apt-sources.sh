#!/usr/bin/env bash
#
# Drop the Google Chrome apt source that ships in the GitHub Actions Ubuntu
# runner image itself — darkmux does not add it, and no darkmux job installs
# google-chrome-stable via apt anywhere in this repo (Playwright's `install
# --with-deps` fetches its own Chromium binary; it does not need this repo).
#
# When that mirror serves a package index whose hash doesn't match its
# release file — an ordinary mid-publish window on Google's end — `apt-get
# update` fails at exit 100 for the WHOLE runner, so every subsequent
# `apt-get install` on that runner fails too, regardless of what it was
# installing (#2601). Two jobs installing unrelated things (a browser, and
# separately redis) both went red on a pull request whose only change was a
# comment.
#
# Root cause of why the source is even there to break: the browser package's
# own install script now writes the newer deb822 `.sources` format, but the
# runner image's own cleanup step still only removes the older one-line
# `.list` filename. The mismatch is what lets the source survive into the
# image at all — once the runner image's cleanup matches the newer filename,
# this script becomes a no-op safety net rather than the primary fix, and can
# eventually be dropped.
#
# Fix direction chosen: drop the source outright rather than pin apt to
# tolerate a failing third-party index. Tolerating a failure is the more
# dishonest of the two shapes here — it would also mask a REAL problem with
# a source a job actually needs. Dropping a source this repo never uses has
# no such downside: it removes the failure mode entirely instead of hiding
# it. See #2601 for the two shapes considered and the reasoning.
#
# Run this ONCE, before the first `apt-get update` in any job that touches
# apt (including implicitly, e.g. `playwright install --with-deps`) — never
# duplicated per job, so a job that adds its own install later inherits the
# fix instead of the flake.
#
# Matched by CONTENT (any source that DECLARES dl.google.com as one of its
# URIs), not by a hardcoded filename, so a future runner-image rename doesn't
# silently stop this from firing. Covers both the classic one-line `.list`
# format (one `deb`/`deb-src` line per declared source) and the newer deb822
# `.sources` format (one blank-line-separated stanza per declared source).
#
# The match is precise on purpose, because a naive whole-file grep for the
# hostname is a source of real damage, not just false positives:
#   - it fires on the host appearing in a COMMENT, not a real source line
#   - it fires on the host appearing inside a Signed-By KEYRING PATH
#     (e.g. .../keyrings/dl.google.com.gpg), which names no such source
#   - it fires on the host as a SUBSTRING of an unrelated, longer hostname
#     (e.g. notdl.google.com.example.org)
#   - worst case: a file can legally carry more than one declared source
#     (e.g. the distribution archive's own file, with this browser's source
#     appended to it). Deleting the WHOLE FILE on any partial match would
#     delete the distribution archive itself — the index update would then
#     exit 0 with nothing left to fetch, and every later install would fail
#     with a misleading "unable to locate package", which is a worse, silent
#     failure than the flake this script exists to fix.
#
# So: only lines/stanzas that actually DECLARE a source are considered (a
# `deb`/`deb-src` line for .list; a stanza with a `URIs:` field for
# .sources), only the declared URI value is inspected (never comments, never
# option/key paths), and the host must match dl.google.com EXACTLY as a
# hostname, not as a substring. A file is removed only when EVERY declared
# source in it names that host. A file with a mix of sources is left
# entirely in place and a warning is printed instead — this script never
# edits a file down to a subset of its lines/stanzas, only removes a file
# outright when 100% of what it declares is the one host we're after. (The
# browser's own install script does the more surgical stanza-precise thing
# for the older format — commenting out only its own line when other
# sources share the file — but whole-file removal is simpler and safe here
# because it's the ONLY case a file is removed at all.)
#
# Reads and removals escalate to root only when needed: run directly if
# already root (a plain container job with no `sudo` binary at all), else
# via `sudo` if available. A file that can't be read, or a source that can't
# be removed because escalation isn't available or fails, is WARNED about —
# never silently treated as "nothing to drop" the way a suppressed
# permission error would.
set -euo pipefail

# Run "$@" as root: directly if we already are root (common in a plain
# container job that has no `sudo` binary at all), via `sudo` if we're not
# root but `sudo` is available, or fail loudly (exit 127, no command run) if
# neither applies — never blindly assume `sudo` exists and let a "command
# not found" abort the whole script mid-loop after we've already announced
# what we were about to do.
as_root() {
  if [ "$(id -u)" -eq 0 ]; then
    "$@"
  elif command -v sudo >/dev/null 2>&1; then
    sudo "$@"
  else
    return 127
  fi
}

# Extract the hostname from a URI: strip the scheme, then cut at the first
# of / : ? # (whichever comes first) so a port, path, or query string never
# leaks into the comparison.
host_of() {
  local uri="$1" rest
  rest="${uri#*://}"
  rest="${rest%%[/:?#]*}"
  printf '%s' "$rest"
}

# Exact hostname match — never a substring match, so
# notdl.google.com.example.org or dl.google.com.evil.example never counts.
host_matches() {
  local h="${1,,}"
  [ "$h" = "dl.google.com" ]
}

shopt -s nullglob
dropped=0
warned=0

for f in /etc/apt/sources.list.d/*.list /etc/apt/sources.list.d/*.sources; do
  # Try an unprivileged read first — these files are normally world-readable
  # and the common case (no root, no sudo, ordinary permissions) should
  # never need escalation just to look. Only escalate if the plain read
  # fails, and only warn if BOTH fail — never swallow a permission error
  # into a silent "nothing to drop".
  content=""
  if ! content=$(cat "$f" 2>&1); then
    if ! content=$(as_root cat "$f" 2>&1); then
      echo "WARNING: could not read $f ($content) — a Google Chrome apt source here, if any, was NOT checked" >&2
      warned=1
      continue
    fi
  fi

  total=0
  matching=0

  if [[ "$f" == *.sources ]]; then
    # deb822 stanza format: one declared source per blank-line-separated
    # stanza. A stanza only counts as "declared" if it has a URIs: field
    # (skips stray comment-only or metadata-only stanzas).
    stanza=""
    process_stanza() {
      local s="$1" uris_line any_match=0 u h
      uris_line=$(printf '%s\n' "$s" | grep -iE '^[[:space:]]*URIs:' | head -n1) || true
      [ -z "$uris_line" ] && return 0
      uris_line="${uris_line#*:}"
      total=$((total + 1))
      for u in $uris_line; do
        h=$(host_of "$u")
        if host_matches "$h"; then
          any_match=1
        fi
      done
      if [ "$any_match" -eq 1 ]; then
        matching=$((matching + 1))
      fi
    }
    while IFS= read -r line; do
      if [ -z "$(printf '%s' "$line" | tr -d '[:space:]')" ]; then
        process_stanza "$stanza"
        stanza=""
      else
        stanza+="$line"$'\n'
      fi
    done <<<"$content"
    process_stanza "$stanza"
  else
    # Classic one-line format: one declared source per `deb`/`deb-src` line.
    # Comments and blank lines declare nothing; an inline trailing comment
    # after the fields is stripped before extracting the URI.
    while IFS= read -r line; do
      trimmed="$(printf '%s' "$line" | sed -E 's/^[[:space:]]+//')"
      [ -z "$trimmed" ] && continue
      [[ "$trimmed" == \#* ]] && continue
      [[ "$trimmed" =~ ^(deb|deb-src)[[:space:]] ]] || continue
      nocomment="${trimmed%%#*}"
      uri=$(printf '%s' "$nocomment" | grep -oE 'https?://[^[:space:]]+' | head -n1) || true
      [ -z "$uri" ] && continue
      total=$((total + 1))
      host=$(host_of "$uri")
      if host_matches "$host"; then
        matching=$((matching + 1))
      fi
    done <<<"$content"
  fi

  if [ "$total" -eq 0 ]; then
    continue
  fi

  if [ "$matching" -eq "$total" ]; then
    if as_root rm -f "$f"; then
      echo "Dropped runner-shipped apt source: $f"
      dropped=$((dropped + 1))
    else
      echo "WARNING: found a Google Chrome apt source at $f but could not remove it (privilege escalation unavailable or failed) — apt-get update may still fail because of it" >&2
      warned=1
    fi
  elif [ "$matching" -gt 0 ]; then
    echo "WARNING: $f declares a Google Chrome apt source (dl.google.com) alongside at least one other source — leaving the whole file in place rather than risk dropping something a job needs. Edit it by hand if it is causing apt-get update to fail." >&2
    warned=1
  fi
done

if [ "$dropped" -eq 0 ] && [ "$warned" -eq 0 ]; then
  echo "No Google Chrome apt source found on this runner — nothing to drop."
fi
