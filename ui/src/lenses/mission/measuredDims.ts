/**
 * (#2325) Carry React Flow's own node measurements across a controlled
 * `nodes` update.
 *
 * `MissionCanvas` drives React Flow in CONTROLLED mode: it hands `<ReactFlow
 * nodes={...}>` a freshly built array whenever the mission data, the metrics,
 * or the once-a-second `now` clock changes. RF v11 reacts to a new array
 * identity by calling its store's `setNodes`, and `createNodeInternals`
 * (`@reactflow/core`) rebuilds each node's internals as a plain `{...node}`
 * spread — it carries `handleBounds` forward from the previous internals but
 * NOT the measured `width`/`height`. A node wrapper renders
 * `visibility: initialized ? 'visible' : 'hidden'` with `initialized =
 * !!node.width && !!node.height`, so every rebuild un-measures every node and
 * the whole canvas goes hidden until RF's per-node `ResizeObserver` happens to
 * re-fire. When a rebuild lands while that re-measure is still in flight the
 * canvas stays blank — the graph painted and then disappeared a second later.
 *
 * The documented way out of that in v11 is the other half of the controlled
 * contract: take `onNodesChange` and feed the `dimensions` changes back into
 * the nodes you hand back. These two pure helpers are that feedback path —
 * `recordDimensions` folds a change batch into a per-id map, and
 * `withMeasuredDimensions` stamps the map back onto the nodes so the next
 * `createNodeInternals` spread already carries a measurement and never
 * un-initializes a node it has already measured.
 */
import type { Node, NodeChange } from "reactflow";

export interface NodeDimensions {
  width: number;
  height: number;
}
export type NodeDimensionsMap = Record<string, NodeDimensions>;

/**
 * Fold a React Flow change batch into `prev`, keeping only `dimensions`
 * changes with a real measurement. Returns `prev` UNCHANGED (same reference)
 * when nothing moved, so a caller can cheaply skip work on the selection and
 * position changes that share this callback.
 */
export function recordDimensions(prev: NodeDimensionsMap, changes: NodeChange[]): NodeDimensionsMap {
  let next: NodeDimensionsMap | null = null;
  for (const change of changes) {
    if (change.type !== "dimensions") continue;
    const dims = change.dimensions;
    if (!dims || !dims.width || !dims.height) continue;
    const current = prev[change.id];
    if (current && current.width === dims.width && current.height === dims.height) continue;
    next = next || { ...prev };
    next[change.id] = { width: dims.width, height: dims.height };
  }
  return next || prev;
}

/**
 * Stamp known measurements onto nodes. A node RF has not measured yet is
 * returned as-is (RF measures it on first paint); a node that already carries
 * the same numbers is returned as-is too, so node object identity is only
 * broken when the measurement genuinely changed.
 */
export function withMeasuredDimensions(nodes: Node[], dims: NodeDimensionsMap): Node[] {
  return nodes.map((node) => {
    const measured = dims[node.id];
    if (!measured) return node;
    if (node.width === measured.width && node.height === measured.height) return node;
    return { ...node, width: measured.width, height: measured.height };
  });
}

/**
 * (#2520) The smallest height `clampCanvasHeight` will ever return.
 *
 * NOT a usability minimum — it exists only so `MissionCanvas`'s
 * `el.style.height` write is always a valid, positive CSS length. Setting
 * `height` to `0px` or a negative value is either invisible or silently
 * ignored by the browser (the previous height stays applied), which is the
 * literal "collapsed to nothing" failure this guards against. Deliberately
 * tiny: a real usability floor would just be the SAME wrong idea
 * `clampCanvasHeight`'s own doc describes, at a different constant that
 * would eventually be wrong for some other viewport too.
 */
export const MIN_VALID_CANVAS_PX = 1;

/**
 * (#2520) How tall the mission canvas's own container may be, given how
 * much vertical room is actually left below its top edge.
 *
 * Replaces a flat `Math.max(240, available)` floor that was tuned against
 * desktop and portrait-phone viewports, where `available` is comfortably
 * above 240px. On a landscape phone (844×390) `available` can be well
 * under half that — measured #2520: top 210.6875 + a 76px drawer inset
 * leaves only ~103px above the phone's always-`position:fixed` bottom
 * drawer. Flooring to 240px there does not protect against a collapse; it
 * makes the canvas TALLER than the room it has, running its own bottom
 * edge (and React Flow's controls/minimap, which it pins there) UNDER the
 * drawer — not merely below the fold, since the drawer is fixed to the
 * VIEWPORT rather than the page, so no scroll position ever uncovers them.
 *
 * `available` can also be transiently ≤0 (the very first layout pass,
 * before the sticky header/drawer inset have settled), which is the case
 * this function's floor genuinely exists to catch — see
 * `MIN_VALID_CANVAS_PX`'s own doc. Pure.
 */
export function clampCanvasHeight(available: number): number {
  return Math.max(MIN_VALID_CANVAS_PX, available);
}
