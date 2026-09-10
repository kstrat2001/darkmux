/**
 * The React Flow canvas renderer (#1868) — a straight port of
 * `mission-graph.html`'s `MissionNode`/`PhaseGroup`/`toRfNodes`/`toRfEdges`
 * onto a REAL `reactflow` dependency (bundled by Vite, see `ui/package.json`)
 * instead of that page's vendored `assets/vendor/reactflow-bundle.min.js`
 * IIFE. DOM class vocabulary (`.phasegroup`, `.mnode.k-<kind>.s-<status>`,
 * `.mn-kind`, `.mn-label`, `.mn-steps`) is kept IDENTICAL to the legacy page
 * on purpose — `tests/parity/next-parity-graph.spec.ts` grades this against
 * the SAME goldens `mission-graph-goldens.spec.ts` captured from the
 * standalone page, and the e2e behavioral specs assert on these classes too.
 */
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import ReactFlow, {
  Background,
  Controls,
  MiniMap,
  Handle,
  Position,
  MarkerType,
  ReactFlowProvider,
  useReactFlow,
  useStore,
  type Edge,
  type Node,
  type NodeMouseHandler,
  type NodeProps,
  type OnNodesChange,
} from "reactflow";
import "reactflow/dist/style.css";
import { StepRow } from "./StepRow";
import { WorkStatus } from "../../components/WorkStatus";
import { useIsMobile } from "../../hooks/useIsMobile";
import {
  recordDimensions,
  withMeasuredDimensions,
  clampCanvasHeight,
  type NodeDimensionsMap,
} from "./measuredDims";
import {
  computeLayout,
  drawnEdges,
  stepMeterFor,
  type GraphEdge,
  type GraphNode,
  type MetricsMap,
} from "./graph";

interface MissionNodeData {
  label: string;
  kind: string;
  status: string;
  description?: string;
  steps: GraphNode["steps"];
  metrics: MetricsMap;
  now: number;
  /** (#2189, step drill-in) Threaded through from `MissionCanvas`'s own
   * props, same as `metrics`/`now` above — see this file's own doc on
   * `onNodeClick` for why a single-step task also gets a whole-card click
   * target on top of the row's own. */
  selectedStepId?: string | null;
  onSelectStep?: (stepId: string) => void;
}

function MissionNode({ data }: NodeProps<MissionNodeData>) {
  const steps = data.steps || [];
  const phaseHandles =
    data.kind === "phase" ? (
      <>
        <Handle type="target" id="phase-in" position={Position.Top} style={{ opacity: 0 }} />
        <Handle type="source" id="phase-out" position={Position.Bottom} style={{ opacity: 0 }} />
      </>
    ) : null;
  return (
    <div className={`mnode k-${data.kind} s-${data.status}`} title={data.description || data.label}>
      <Handle type="target" id="lr-in" position={Position.Left} style={{ opacity: 0 }} />
      {phaseHandles}
      <div className="mn-kind">{data.kind}</div>
      <div className="mn-label">{data.label}</div>
      {steps.length ? (
        <div className="mn-steps">
          {steps.map((s) => (
            <StepRow
              key={s.id}
              step={s}
              meter={stepMeterFor(s, data.metrics, data.now)}
              extraClass="mn-step-row"
              selected={data.selectedStepId === s.id}
              onSelect={data.onSelectStep}
            />
          ))}
        </div>
      ) : null}
      <Handle type="source" id="lr-out" position={Position.Right} style={{ opacity: 0 }} />
    </div>
  );
}

function PhaseGroup({
  data,
}: NodeProps<{ label: string; status: string; description?: string; statusNote?: string }>) {
  // (#2406, post-review) The phase box used to carry NO status word at
  // all — a degraded phase was conveyed purely by border color, and the
  // counts reached the page only as a `title=`. Tooltips do not exist on
  // touch and this viewer is driven from a phone, so both the word and
  // the breakdown render as REAL TEXT. The word is the shared
  // `WorkStatus` chip (no snowflake indicator); the note sits beside it
  // and is present only for `running`/`degraded` (see
  // `mission_graph.rs::phase_status_note`). `title` stays the
  // description, as it was before this packet — the note no longer needs
  // to hitch a ride on it.
  return (
    <div className={`phasegroup s-${data.status || "planned"}`} title={data.description || ""}>
      <Handle type="target" id="phase-in" position={Position.Top} style={{ opacity: 0 }} />
      <Handle type="source" id="phase-out" position={Position.Bottom} style={{ opacity: 0 }} />
      <div className="pg-label">
        <span className="pg-kind">PHASE</span>
        {/* The name, the chip and the counts share ONE line on purpose: the
            label block is absolutely positioned at the box's top-left and
            has only `BAND_PAD` (56px, `graph.ts`) of clearance before the
            first task card. A third stacked line renders UNDER that card —
            observed at 1280x900 before this layout, with the counts row
            half-hidden behind it. */}
        <span className="pg-state">
          <span className="pg-name">{data.label || ""}</span>
          <WorkStatus status={data.status} className="pg-tag" />
          {data.statusNote ? <span className="pg-note">{data.statusNote}</span> : null}
        </span>
      </div>
    </div>
  );
}

const nodeTypes = { missionNode: MissionNode, phaseGroup: PhaseGroup };

/**
 * (#2376) React Flow's `fitView` boolean prop only fires once, at mount —
 * rotating a phone (390×844 portrait → 844×390 landscape) grows the pane
 * from 358px to ~812px wide, but the zoom transform stays pinned at the
 * portrait-fit scale because nothing ever asks React Flow to recompute it.
 * `useStore`'s `width`/`height` selectors read the SAME numbers `fitView()`
 * itself resolves against (`@reactflow/core`'s `useResizeHandler` writes
 * them into the store from its own `ResizeObserver` on the renderer node,
 * and `fitView()` reads them straight back out) — reacting to a change
 * there, rather than guessing with a `requestAnimationFrame` after our own
 * resize handler runs, means this only ever fires once React Flow's own
 * measurement has actually caught up with the new pane size. The very
 * first firing (mount) is skipped: `MissionCanvas`'s `fitView` prop already
 * covers that one, and this component's job is only the recompute an
 * ALREADY-mounted canvas needs on a later resize.
 *
 * `geometrySignature` closes a SECOND race that width/height alone
 * missed — found by instrumenting a live rotate-BACK (portrait → landscape
 * → portrait), not by reasoning about it up front. Two earlier attempts
 * measurably failed the SAME manual repro (rotate twice, read the real
 * `.react-flow__viewport` transform) before this one held:
 *
 * 1. Passing `MissionCanvas`'s own `narrow` boolean down as an extra
 *    dependency. `narrow` flips the instant `MissionCanvas` re-renders, but
 *    React Flow's OWN `StoreUpdater` component (`@reactflow/core`) applies
 *    a changed `nodes` PROP into its internal store from its OWN
 *    `useEffect` (`useStoreUpdater(nodes, setNodes)`) — logging both sides
 *    showed THIS component's effect firing BEFORE that one in the same
 *    commit. Reading `narrow` raced one cycle ahead of the store:
 *    `s.getNodes()` still held the OLD positions at the exact moment this
 *    effect used the NEW `narrow` value to decide whether to fire, and
 *    `fitView()` read those stale positions right back out.
 * 2. Depending on `useStore`'s own node X-positions (`max(x) - min(x)`
 *    across measured nodes) instead — closer, but still measurably wrong
 *    on the SAME repro. A phase-group box's rendered CSS width changes
 *    between the narrow and wide layouts (`computeLayout`'s per-band
 *    `box.w`), and React Flow re-measures a node's `width`/`height` via
 *    its OWN per-node `ResizeObserver` (`NodeRenderer`, `@reactflow/core`)
 *    — a real, browser-scheduled callback, independent of both React's
 *    commit cycle AND the `StoreUpdater` effect above. Task positions had
 *    already updated to the narrow column by the time this effect ran, but
 *    the phase box's stale (wide) MEASURED width was still what
 *    `getNodesBounds` summed into the fit — reproducing the exact same
 *    wrong scale as the original bug, because a wide phase box dominates
 *    the bounding width whether or not the tasks inside it are narrow.
 *
 * `geometrySignature` reads BOTH position and measured size for every
 * node — the same inputs `getNodesBounds` itself sums — so it can only
 * change in the render where `fitView()`'s actual inputs actually did,
 * regardless of which of the three independent async sources (this
 * component's own effect order, `StoreUpdater`'s effect, or a per-node
 * `ResizeObserver`) is the one still catching up. It is deliberately NOT
 * "did the `nodes` array change at all": `rfNodes` is rebuilt on every
 * metrics/clock tick (see the `#2325` comment on `rfNodes` below), and
 * depending on that reference directly would re-invoke `fitView()` every
 * tick, fighting anyone trying to pan or zoom by hand. Rounding
 * position/size to whole pixels keeps the signature stable across a tick
 * that only touches `data.metrics`, which never moves or resizes a node.
 */
function RefitOnResize() {
  const { fitView } = useReactFlow();
  const width = useStore((s) => s.width);
  const height = useStore((s) => s.height);
  const geometrySignature = useStore((s) =>
    s
      .getNodes()
      .map((n) => {
        const p = n.positionAbsolute ?? n.position;
        return `${n.id}:${Math.round(p.x)},${Math.round(p.y)},${n.width ?? "?"},${n.height ?? "?"}`;
      })
      .join("|"),
  );
  const mountedRef = useRef(false);
  useEffect(() => {
    if (!mountedRef.current) {
      mountedRef.current = true;
      return;
    }
    fitView();
  }, [width, height, geometrySignature, fitView]);
  return null;
}

function toRfNodes(
  graphNodes: GraphNode[],
  layout: ReturnType<typeof computeLayout>,
  metrics: MetricsMap,
  now: number,
  selectedStepId: string | null | undefined,
  onSelectStep: ((stepId: string) => void) | undefined,
): Node[] {
  return graphNodes.map((n) => {
    const pos = layout.positions[n.id] || { x: 0, y: 0 };
    if (n.kind === "phase") {
      const box = layout.boxes[n.id] || { x: pos.x, y: pos.y, w: 320, h: 160 };
      return {
        id: n.id,
        type: "phaseGroup",
        position: { x: box.x, y: box.y },
        style: { width: box.w, height: box.h },
        data: { label: n.label, status: n.status, description: n.description, statusNote: n.statusNote },
        draggable: false,
        selectable: false,
        zIndex: 0,
      };
    }
    return {
      id: n.id,
      type: "missionNode",
      position: pos,
      // (#2104) The card's width is the layout's decision (content class),
      // not a CSS cap: a card with metric rows is wider than a bare one, and
      // the phase box and next column were sized around that same number.
      style: { width: layout.widths[n.id] },
      zIndex: 1,
      data: {
        label: n.label,
        kind: n.kind,
        status: n.status,
        description: n.description,
        steps: n.steps || [],
        metrics,
        now,
        selectedStepId,
        onSelectStep,
      },
      draggable: true,
    };
  });
}

function toRfEdges(graphEdges: GraphEdge[], graphNodes: GraphNode[]): Edge[] {
  return drawnEdges(graphEdges, graphNodes).map((e) => {
    const isPhaseOrder = e.kind === "phase_order";
    return {
      id: e.id,
      source: e.source,
      target: e.target,
      sourceHandle: isPhaseOrder ? "phase-out" : "lr-out",
      targetHandle: isPhaseOrder ? "phase-in" : "lr-in",
      className: "edge-" + e.kind,
      animated: false,
      markerEnd:
        e.kind === "depends_on" || isPhaseOrder
          ? { type: MarkerType.ArrowClosed, color: isPhaseOrder ? "#5af0a3" : "#2a8a96" }
          : undefined,
    };
  });
}

export function MissionCanvas({
  nodes: graphNodes,
  edges: graphEdges,
  metrics,
  now,
  note,
  minimapOn,
  selectedStepId,
  onSelectStep,
}: {
  nodes: GraphNode[];
  edges: GraphEdge[];
  metrics: MetricsMap;
  now: number;
  note?: string;
  minimapOn: boolean;
  /** (#2189, step drill-in) See `MissionGraphLens`'s own doc for where
   * these two come from and where the resulting route write lands. */
  selectedStepId?: string | null;
  onSelectStep?: (stepId: string) => void;
}) {
  // (#2058) The canvas fills whatever viewport is left below it. React Flow
  // pins its controls and minimap to the canvas's own bottom edge; a canvas
  // taller than the window put them below the fold with no way to reach
  // them. `min-height: 0` flex chains above this do not give it a definite
  // height, so measure once and on resize: the distance from the canvas's
  // top to the window's bottom is exactly the height it may have.
  const canvasRef = useRef<HTMLDivElement | null>(null);
  // (#2376) Whether the CANVAS's own rendered box is taller than it is
  // wide — the input the phone layout branch below actually needs. Seeded
  // from the window's own aspect so the very first render already guesses
  // right in the common case (nothing has measured the real box yet); the
  // `fit()` callback below corrects it from the real box on every
  // measurement, so a header/inset that eats enough vertical space to flip
  // the aspect wins over the window-level guess.
  const [isPortraitPane, setIsPortraitPane] = useState<boolean>(() =>
    typeof window !== "undefined" ? window.innerHeight > window.innerWidth : false,
  );
  useLayoutEffect(() => {
    const el = canvasRef.current;
    if (!el) return;
    const fit = () => {
      const rect = el.getBoundingClientRect();
      const top = rect.top + window.scrollY;
      // (operator, 2026-09-05) The viewport's bottom is not the CONTENT's
      // bottom on a phone: the last `--phone-drawer-closed-h` belong to the collapsed phone
      // drawer's tab bar, which is fixed. #2058 already knew the canvas must
      // not run past the fold and stopped exactly one bar short of being
      // right — the zoom controls and the minimap, which React Flow pins to
      // the canvas's own bottom edge, ended up half under "Machine info |
      // Events".
      //
      // The inset is READ from `.app-shell`'s own resolved `padding-bottom`
      // rather than restated here. That padding is the ONE rule every other
      // lens already sizes to (`styles.css`'s phone block, `calc(var(--phone-drawer-closed-h) +
      // env(safe-area-inset-bottom))`), and `getComputedStyle` resolves the
      // `calc` and the safe-area inset to real pixels — so this cannot drift
      // from the bar's actual height, and it is exactly `0` on a desktop
      // where that rule does not apply, leaving desktop untouched.
      const shell = el.closest(".app-shell");
      const inset = shell ? parseFloat(getComputedStyle(shell).paddingBottom) || 0 : 0;
      // (#2618 — CI regression from #2520 round 1) `available` alone is the
      // wrong ceiling on a landscape phone: 844×390 leaves only ~103px above
      // the drawer, and flooring `el.style.height` to exactly that shrinks
      // `fitView`'s pane until the wide (desktop-shape) layout hits React
      // Flow's own 0.1 `minZoom` — the graph FITS and is unreadable, the
      // regression `mission-lens-phone-graph-fit.spec.js`'s landscape case
      // caught. #2520 round 1 dropped the previous flat `240` floor entirely
      // on the theory that flooring higher than `available` ran the canvas
      // under the drawer with "no scroll position" able to reach it — but
      // `el`'s own parent (`.body missionlens__body`, `styles.css`) already
      // carries an unrelated `min-height: 480px`, so that overflow, and a
      // page tall enough to scroll to it, already exist today regardless of
      // what height THIS canvas asks for; round 1 just left the difference
      // as dead blank space below a needlessly shrunken graph. `floor` reads
      // that same reserved room back out (never a second hardcoded `480`
      // that could drift from the CSS rule that actually governs it, same
      // pattern as `inset` above) so the canvas actually FILLS it instead —
      // legible on a landscape phone, a no-op everywhere `available` is
      // already bigger (portrait, desktop), and reachable by an ordinary
      // page scroll: React Flow's controls are `position: absolute` inside
      // this scrolling box, not `position: fixed` like the drawer, so they
      // move with the page exactly as any other flow content would.
      const floor = el.parentElement ? parseFloat(getComputedStyle(el.parentElement).minHeight) || 0 : 0;
      const h = clampCanvasHeight(window.innerHeight - top - inset, floor);
      el.style.height = `${h}px`;
      // (#2376) The ACTUAL rendered pane's aspect, not the window's — a
      // landscape phone's canvas is short and wide even though `isMobile`
      // (see below) correctly stays true through the rotation, and the
      // wide-pane case wants the desktop's side-by-side columns, not a
      // tall single one. `rect.width` is this element's CSS-driven width
      // (this write only ever touches `height`, so it's unaffected by the
      // line above); `h` is the exact number React Flow's own pane resolves
      // to next, since this div is its immediate 100%-sized container. That
      // makes this the real pane box `fitView` fits against — reading it
      // here (rather than reaching for React Flow's own width/height store
      // values) avoids a real architectural snag: this effect runs in the
      // component that CREATES `<ReactFlowProvider>`, not one of its
      // descendants, and the store isn't readable from outside its own
      // context without restructuring the component tree to nest the
      // layout computation inside the provider — this box is the same
      // number without any of that.
      setIsPortraitPane(h > rect.width);
    };
    fit();
    window.addEventListener("resize", fit);
    // (mainstay-unification packet, #2058 regression) `resize` alone missed
    // a real case: `.missionlens .top`'s own height can change AFTER this
    // effect's first measurement, with no window resize event to catch it —
    // e.g. the live-tail status flipping from "live" to "reconnecting"
    // shortly after mount lengthens the pill enough to wrap the header row.
    // The canvas's own top shifts down, but the one-time height stays keyed
    // to the OLD (higher) top — its bottom edge, and React Flow's controls
    // pinned to it, then overflow the viewport by exactly however much the
    // header grew. Observing the header row directly re-fits on ANY of its
    // height changes, not just a viewport resize — `.top` is a plain
    // sibling in this flex column, sized purely by its own content
    // (`flex: 0 0 auto`), so setting the canvas's own height here can never
    // feed back into `.top`'s size and loop.
    const headerEl = el.closest(".missionlens")?.querySelector(":scope > .top");
    let ro: ResizeObserver | undefined;
    if (headerEl && typeof ResizeObserver !== "undefined") {
      ro = new ResizeObserver(fit);
      ro.observe(headerEl);
    }
    return () => {
      window.removeEventListener("resize", fit);
      ro?.disconnect();
    };
  }, []);
  // (#2376) Two DIFFERENT questions, deliberately kept separate. `isMobile`
  // (see that hook's own doc) answers "is this phone chrome?" — including
  // its landscape-phone coarse-pointer fallback, so a phone rotated to a
  // >768px-wide landscape still counts as a phone rather than flipping back
  // to desktop chrome. `isPortraitPane` (above) answers a different
  // question: "does the RENDERED canvas want a tall layout?" A landscape
  // phone is still a phone (isMobile stays true) but its canvas is short
  // and wide, and stacking a phase's tasks into one tall column is exactly
  // the wrong trade there — it's the desktop's side-by-side columns that
  // suit a wide-short pane, regardless of which device drew it. `narrow`
  // is the AND of both: phone chrome AND a pane that's actually taller
  // than it is wide.
  const isMobile = useIsMobile();
  const narrow = isMobile && isPortraitPane;
  const layout = useMemo(() => computeLayout(graphNodes, narrow), [graphNodes, narrow]);
  // (#2325) React Flow measures each node once and keeps the result in its own
  // store — but a CONTROLLED `nodes` update throws that measurement away, and
  // an unmeasured node renders `visibility: hidden`. Since this canvas rebuilds
  // its node array on every metrics/clock tick, the graph painted and then went
  // blank a second later. `measuredDims`'s own doc has the full mechanism; the
  // fix is the other half of RF's controlled contract — take the `dimensions`
  // changes back through `onNodesChange` and stamp them onto the nodes we hand
  // over, so a rebuilt node is already measured. A ref (not state) on purpose:
  // the map is READ while building the next array, and making it state would
  // schedule a render for a value that only matters at the next rebuild.
  const dimsRef = useRef<NodeDimensionsMap>({});
  const onNodesChange = useCallback<OnNodesChange>((changes) => {
    dimsRef.current = recordDimensions(dimsRef.current, changes);
  }, []);
  const rfNodes = useMemo(
    () =>
      withMeasuredDimensions(
        toRfNodes(graphNodes, layout, metrics, now, selectedStepId, onSelectStep),
        dimsRef.current,
      ),
    [graphNodes, layout, metrics, now, selectedStepId, onSelectStep],
  );
  const rfEdges = useMemo(() => toRfEdges(graphEdges, graphNodes), [graphEdges, graphNodes]);

  // (#2189, step drill-in) A whole-card click, for the common case a task
  // node carries exactly ONE step (the operator's own crawl-unit example —
  // "five CRAWL.UNIT nodes", each one step). A node with zero or several
  // steps takes no action here — the individual `StepRow`'s own click (see
  // that component's own doc) is the only way to pick ONE of several, and
  // there's nothing to select for a phase group or a step-less task. Phase
  // nodes are already `selectable:false` in `toRfNodes`, but React Flow
  // still calls `onNodeClick` for a non-selectable node, so this checks
  // `n.kind === "task"` itself rather than relying on that flag.
  const onNodeClick: NodeMouseHandler = (_event, node) => {
    if (!onSelectStep) return;
    const gn = graphNodes.find((n) => n.id === node.id);
    if (!gn || gn.kind !== "task") return;
    const steps = gn.steps || [];
    if (steps.length === 1) onSelectStep(steps[0].id);
  };

  return (
    <div className="canvas missionlens__canvas" ref={canvasRef}>
      {note ? <div className="note">{note}</div> : null}
      <ReactFlowProvider>
        <ReactFlow
          nodes={rfNodes}
          edges={rfEdges}
          onNodesChange={onNodesChange}
          nodeTypes={nodeTypes}
          fitView
          minZoom={0.1}
          maxZoom={2}
          proOptions={{ hideAttribution: true }}
          onNodeClick={onSelectStep ? onNodeClick : undefined}
        >
          <Background color="#1f1f24" gap={24} />
          <Controls />
          {minimapOn ? <MiniMap pannable zoomable style={{ background: "#131316" }} /> : null}
          <RefitOnResize />
        </ReactFlow>
      </ReactFlowProvider>
    </div>
  );
}
