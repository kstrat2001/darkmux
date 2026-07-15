# Vendored JS for the mission-graph lens (#1284 Packet 5)

Self-contained, no-CDN bundle serving the `/mission/<id>/graph` page's
React Flow diagram. Committed as static bytes so the daemon binary stays
fully offline-capable, matching `viewer.html`'s own `include_str!` posture.

## Contents

| File | What | License |
|---|---|---|
| `reactflow-bundle.min.js` | React 18.3.1 + ReactDOM 18.3.1 + reactflow 11.11.4, bundled into one minified IIFE that assigns `window.MissionGraphVendor` (`{ React, createRoot, ReactFlow, Background, Controls, MiniMap, Handle, Position, MarkerType, ReactFlowProvider }`). No source map. | MIT (all three) |
| `reactflow-bundle.min.css` | reactflow's `dist/style.css`, minified. | MIT |
| `LICENSE-react`, `LICENSE-react-dom`, `LICENSE-reactflow` | Upstream MIT license text, copied verbatim from each package's own `LICENSE` file at the pinned version. | MIT |

All three upstream packages are MIT-licensed — compatible with darkmux's own MIT license, no attribution notices required beyond keeping these `LICENSE-*` files in the tree.

## Why one merged bundle, not three separate vendor files

`reactflow` imports `react`/`react-dom` as ES modules; bundling it separately from React and aliasing the import to a `window.React` global needs an extra shim layer. Merging all three into one `esbuild --bundle` pass sidesteps that — the app-side code (`assets/mission-graph.html`) reads everything off `window.MissionGraphVendor`, and the whole vendor surface rebuilds from one command. The app's OWN logic (data fetch, SSE handling, layout, React tree) stays outside this bundle, hand-written directly in `mission-graph.html` with `React.createElement` (no JSX, so no build step is needed for darkmux's own code — only the third-party vendor bundle needs a bundler).

## Pinned versions

- `react` 18.3.1
- `react-dom` 18.3.1
- `reactflow` 11.11.4

React 18 (not 19) was pinned deliberately: `reactflow` 11.x's peer range (`>=17`) technically accepts React 19, but the combination isn't the tested-together pair upstream ships against — React 18 + reactflow 11 is the well-worn combination, and this vendor bundle has no upstream support loop to lean on if a React-19-specific incompatibility surfaced.

## Rebuilding the bundle

```bash
mkdir -p /tmp/rf-build && cd /tmp/rf-build
npm init -y >/dev/null
npm install --no-save react@18.3.1 react-dom@18.3.1 reactflow@11.11.4 esbuild@0.28.1

cat > entry.js <<'EOF'
import React from 'react';
import * as ReactDOMClient from 'react-dom/client';
import ReactFlow, {
  Background, Controls, MiniMap, Handle, Position, MarkerType, ReactFlowProvider,
} from 'reactflow';

window.MissionGraphVendor = {
  React, createRoot: ReactDOMClient.createRoot, ReactFlow,
  Background, Controls, MiniMap, Handle, Position, MarkerType, ReactFlowProvider,
};
EOF

npx esbuild entry.js --bundle --minify --format=iife --platform=browser \
  --outfile=reactflow-bundle.min.js
npx esbuild node_modules/reactflow/dist/style.css --minify \
  --outfile=reactflow-bundle.min.css

cp reactflow-bundle.min.js reactflow-bundle.min.css \
  <darkmux-repo>/crates/darkmux-serve/assets/vendor/
cp node_modules/react/LICENSE      <darkmux-repo>/crates/darkmux-serve/assets/vendor/LICENSE-react
cp node_modules/react-dom/LICENSE  <darkmux-repo>/crates/darkmux-serve/assets/vendor/LICENSE-react-dom
cp node_modules/reactflow/LICENSE  <darkmux-repo>/crates/darkmux-serve/assets/vendor/LICENSE-reactflow
```

Bump a pinned version by changing the three `npm install` version numbers above and re-running. Re-run `cargo test -p darkmux-serve` after any bundle rebuild — the route tests assert the served bytes are non-empty and content-typed correctly, not a content hash, so a legitimate rebuild never breaks CI on its own.
