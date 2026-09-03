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
import { useLayoutEffect, useMemo, useRef } from "react";
import ReactFlow, {
  Background,
  Controls,
  MiniMap,
  Handle,
  Position,
  MarkerType,
  ReactFlowProvider,
  type Edge,
  type Node,
  type NodeMouseHandler,
  type NodeProps,
} from "reactflow";
import "reactflow/dist/style.css";
import { StepRow } from "./StepRow";
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

function PhaseGroup({ data }: NodeProps<{ label: string; status: string; description?: string }>) {
  return (
    <div className={`phasegroup s-${data.status || "planned"}`} title={data.description || ""}>
      <Handle type="target" id="phase-in" position={Position.Top} style={{ opacity: 0 }} />
      <Handle type="source" id="phase-out" position={Position.Bottom} style={{ opacity: 0 }} />
      <div className="pg-label">
        <span className="pg-kind">PHASE</span>
        <span className="pg-name">{data.label || ""}</span>
      </div>
    </div>
  );
}

const nodeTypes = { missionNode: MissionNode, phaseGroup: PhaseGroup };

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
        data: { label: n.label, status: n.status, description: n.description },
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
  useLayoutEffect(() => {
    const el = canvasRef.current;
    if (!el) return;
    const fit = () => {
      const top = el.getBoundingClientRect().top + window.scrollY;
      const h = Math.max(240, window.innerHeight - top);
      el.style.height = `${h}px`;
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
  const layout = useMemo(() => computeLayout(graphNodes), [graphNodes]);
  const rfNodes = useMemo(
    () => toRfNodes(graphNodes, layout, metrics, now, selectedStepId, onSelectStep),
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
        </ReactFlow>
      </ReactFlowProvider>
    </div>
  );
}
