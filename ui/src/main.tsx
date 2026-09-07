import "./styles.css";
import { mountApp } from "./mountApp";

// (#1709) The real boot logic lives in `mountApp.tsx` — extracted so it's a
// plain function `mountApp.test.tsx` can call directly, and so this file
// stays a bare entry point. `mountApp`'s own doc explains the two
// mechanisms (a top-level `BootErrorBoundary` plus `window.onerror`/
// `unhandledrejection`) that between them replace the old bare
// `createRoot(rootEl).render(...)` this file used to call directly, which
// had no guard at all — any render-time throw yielded a silent black
// screen with no diagnosis path.
mountApp();
