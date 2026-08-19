# Vendored license notices for the built viewer

`vite build` produces one self-contained `dist/index.html`, committed to
`crates/darkmux-serve/assets/next.html` and `include_str!`'d into the binary.
That single file **embeds** react, react-dom, @tanstack/react-query, and
(#1868) reactflow — the mission-graph lens's canvas renderer — and it is
served at `GET /` and republished as `docs/demo/index.html` on the website.

MIT requires its notice to be "included in all copies or substantial portions
of the Software." A minified bundle compiled into a binary and served to
browsers is a copy — so the notice has to travel *with the artifact*, not only
sit in the source tree beside it.

Vite's minifier strips `@license` banners by default, and it did: before
#1842, `grep -c "Copyright (c)" next.html` returned **0** across 323 KB of
bundled MIT code.

## How the notice gets into the build

`vite.config.ts` has a small `vendorLicenseNotice()` plugin. It reads every
`LICENSE-*` file in this directory at build time and injects them as an HTML
comment at the top of the output document, ahead of `<!doctype html>`.

An HTML comment rather than a JS banner, deliberately: it survives
`vite-plugin-singlefile`'s inlining, it is the first thing in view-source, and
it does not depend on upstream packages shipping `@license` markers of their
own.

## Keeping these current

These files are copied verbatim from `node_modules/<pkg>/LICENSE`. **When a
dependency is added, removed or bumped to a version with different copyright
text, update this directory.** The plugin fails the build if the directory is
empty, so a wholesale deletion cannot ship silently — but it cannot detect a
*stale* notice, and nothing else will either.

| file | package | version at vendoring |
|---|---|---|
| `LICENSE-react` | react | 18.3.1 |
| `LICENSE-react-dom` | react-dom | 18.3.1 |
| `LICENSE-tanstack-react-query` | @tanstack/react-query | 5.101.4 |
| `LICENSE-reactflow` | reactflow | 11.11.4 |

The React 18 pin is deliberate — `crates/darkmux-serve/assets/vendor/README.md`
documented it for the (pre-#1868) standalone mission-graph page's own separate
vendored bundle, which used the same react/reactflow version pair this `ui/`
dependency now uses for real (#1868 folded that page's canvas renderer into
this build; the standalone bundle + its README are scheduled for removal in
#1868's third packet).
