#!/usr/bin/env bash
# (#1842) Copy the built viewer to the crate's assets dir, prepending the
# license notices for the third-party code it embeds.
#
# MIT requires its notice to be "included in all copies or substantial portions
# of the Software." `dist/index.html` embeds react, react-dom and
# @tanstack/react-query; it is compiled into the binary via `include_str!`,
# served at `GET /`, and republished as `docs/demo/index.html` on the website.
# That file is a copy. Before this step, `grep -c "Copyright (c)"` on the built
# artifact returned 0 across 323 KB of bundled MIT code, because Vite's
# minifier strips `@license` banners by default.
#
# Done here rather than in `vite.config.ts` on purpose: the config has no
# `@types/node`, and adding a dependency to read three text files would cost
# more than it buys. A shell step is the smaller surface.
#
# An HTML comment rather than a JS banner: it survives the singlefile
# inlining, it is the first thing in view-source, and it does not depend on
# upstream packages shipping `@license` markers of their own.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
src="$here/dist/index.html"
dest="$here/../crates/darkmux-serve/assets/next.html"
licenses="$here/vendor-licenses"

shopt -s nullglob
files=("$licenses"/LICENSE-*)
if [ ${#files[@]} -eq 0 ]; then
  echo "ui/vendor-licenses/ has no LICENSE-* files — the built viewer would ship" >&2
  echo "bundled MIT code with no attribution. See ui/vendor-licenses/README.md." >&2
  exit 1
fi

{
  echo "<!--"
  echo "This file embeds third-party code. Their license notices follow, as MIT requires."
  echo "Full texts also live in the source tree at ui/vendor-licenses/."
  echo
  for f in "${files[@]}"; do
    echo "================================================================"
    echo "$(basename "$f" | sed 's/^LICENSE-//')"
    echo
    cat "$f"
    echo
  done
  echo "-->"
  cat "$src"
} > "$dest"

echo "wrote $dest (with ${#files[@]} vendored license notice(s))"
