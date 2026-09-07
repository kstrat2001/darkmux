import { Component, type ErrorInfo, type ReactNode } from "react";
import { BootErrorScreen } from "./BootErrorScreen";

/**
 * (#1709) The shell's own boundary — everything `LensErrorBoundary` does
 * NOT cover. `App.tsx` wraps each individual lens in a `LensErrorBoundary`,
 * so a throw INSIDE a lens is already contained. What was unguarded is
 * everything else that renders: `App` itself, `Masthead`, `NavChrome`,
 * `MachineDrawer`/`PhoneDrawer`, `EventLogColumn` — and, mounted here ABOVE
 * `QueryClientProvider` in `mountApp.tsx`, even a throw from that provider's
 * own render. Before this, any of those throwing unmounted the ENTIRE tree
 * (LensErrorBoundary included) and left a blank page.
 *
 * This is a real React error boundary — `getDerivedStateFromError`/
 * `componentDidCatch`, the same mechanism `LensErrorBoundary` uses — so it
 * catches synchronous throws during render, in lifecycle methods, and in
 * constructors anywhere in the subtree below it. It does NOT catch (by
 * React's own design): errors in event handlers, errors in async callbacks
 * (a `setTimeout`, a `.then()`, an unguarded `async` effect), or an error in
 * this boundary's own fallback. `mountApp.tsx`'s `window.onerror`/
 * `unhandledrejection` listeners are the second, independent mechanism that
 * covers exactly those gaps — see that file's own doc for the full split.
 */
interface Props {
  children: ReactNode;
}

interface State {
  error: Error | null;
}

export class BootErrorBoundary extends Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    // Console, not a flow record — same reasoning as LensErrorBoundary: a
    // boot crash must not itself attempt a network write.
    console.error("[darkmux] the app shell crashed while rendering", error, info.componentStack);
  }

  render() {
    const { error } = this.state;
    if (!error) return this.props.children;
    return <BootErrorScreen error={error} context="render" />;
  }
}
