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
            <StepRow key={s.id} step={s} meter={stepMeterFor(s, data.metrics, data.now)} extraClass="mn-step-row" />
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

function toRfNodes(graphNodes: GraphNode[], layout: ReturnType<typeof computeLayout>, metrics: MetricsMap, now: number): Node[] {
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
      zIndex: 1,
      data: { label: n.label, kind: n.kind, status: n.status, description: n.description, steps: n.steps || [], metrics, now },
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
}: {
  nodes: GraphNode[];
  edges: GraphEdge[];
  metrics: MetricsMap;
  now: number;
  note?: string;
  minimapOn: boolean;
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
    return () => window.removeEventListener("resize", fit);
  }, []);
  const layout = useMemo(() => computeLayout(graphNodes), [graphNodes]);
  const rfNodes = useMemo(() => toRfNodes(graphNodes, layout, metrics, now), [graphNodes, layout, metrics, now]);
  const rfEdges = useMemo(() => toRfEdges(graphEdges, graphNodes), [graphEdges, graphNodes]);

  return (
    <div className="canvas missionlens__canvas" ref={canvasRef}>
      {note ? <div className="note">{note}</div> : null}
      <ReactFlowProvider>
        <ReactFlow nodes={rfNodes} edges={rfEdges} nodeTypes={nodeTypes} fitView minZoom={0.1} maxZoom={2} proOptions={{ hideAttribution: true }}>
          <Background color="#1f1f24" gap={24} />
          <Controls />
          {minimapOn ? <MiniMap pannable zoomable style={{ background: "#131316" }} /> : null}
        </ReactFlow>
      </ReactFlowProvider>
    </div>
  );
}
