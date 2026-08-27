import { Component, type ErrorInfo, type ReactNode } from "react";

/**
 * (#2027) A blast-radius bound for one lens.
 *
 * There was no error boundary anywhere in this app. `main.tsx` mounts with a
 * bare `createRoot().render()`, so ANY render throw — in any lens, from any
 * malformed payload — unmounted the whole tree and left a white page. Not a
 * degraded lens: a blank browser window with every other lens gone too.
 *
 * That was reachable with committed data. `machineGauge.ts` and
 * `MachineHealthRegion.tsx` dereference `resources.machine.*` unguarded (21
 * sites) while chaining `resources.pool?.*` right beside them, and
 * `resources` arrives through a `fetchJson<...>` CAST with no runtime schema
 * check. A hand-edited or schema-drifted `docs/demo/demo-machine.json` —
 * exactly the file an operator would trim to publish a demo — white-screened
 * darkmux.com/demo entirely. Found by a QA agent that rendered a thin payload
 * rather than reading the types.
 *
 * Deliberately NOT a fix for the unguarded dereferences themselves. Those are
 * worth guarding on their own terms, and a boundary that made them invisible
 * would be worse than the crash: a lens that silently shows an error card
 * whenever its data drifts teaches nobody anything. This bounds the DAMAGE —
 * one lens fails, the rest of the app keeps working, and the operator can
 * still navigate away — and it says loudly what happened.
 *
 * The error text is rendered, not just logged. An operator reading "this lens
 * crashed" with a message can file something useful; a blank page cannot be
 * reported at all.
 */
interface Props {
  /** Which lens this wraps — named in the fallback so the report is specific. */
  name: string;
  children: ReactNode;
}

interface State {
  error: Error | null;
}

export class LensErrorBoundary extends Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    // Console, not a flow record: a render crash must not itself attempt a
    // network write, which is a second thing that can fail while handling
    // the first.
    console.error(`[darkmux] the ${this.props.name} lens crashed while rendering`, error, info.componentStack);
  }

  /** Re-mounting the subtree is the only recovery that makes sense here: the
   * crash came from data this lens read, and its queries refetch on mount. */
  private reset = () => this.setState({ error: null });

  render() {
    const { error } = this.state;
    if (!error) return this.props.children;
    return (
      <div className="lenscrash" role="alert" data-lens={this.props.name}>
        <div className="lenscrash__title">the {this.props.name} lens stopped rendering</div>
        <div className="lenscrash__msg">{String(error.message || error)}</div>
        <div className="lenscrash__hint">
          Every other tab still works. This usually means the data this lens read is missing a field it
          expected — check the daemon's response, or the committed fixture if this is a static build.
        </div>
        <button type="button" className="pcbtn" onClick={this.reset}>
          try again
        </button>
      </div>
    );
  }
}
