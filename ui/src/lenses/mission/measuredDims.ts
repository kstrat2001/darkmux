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
 * (#2520) The absolute smallest height `clampCanvasHeight` will ever
 * return, regardless of `available` or `floor`.
 *
 * NOT a usability minimum — it exists only so `MissionCanvas`'s
 * `el.style.height` write is always a valid, positive CSS length. Setting
 * `height` to `0px` or a negative value is either invisible or silently
 * ignored by the browser (the previous height stays applied), which is the
 * literal "collapsed to nothing" failure this guards against. Deliberately
 * tiny — a real usability floor belongs in `floor` instead (a value read
 * from the container's own layout, not a second hardcoded constant here
 * that would drift from it; see `clampCanvasHeight`'s own doc, #2618).
 */
export const MIN_VALID_CANVAS_PX = 1;

/**
 * (#2520, round 2 — #2618 CI regression) How tall the mission canvas's own
 * container may be, given how much vertical room is actually left below its
 * top edge, and (optionally) a legible-content floor supplied by the caller.
 *
 * Round 1 of #2520 replaced a flat `Math.max(240, available)` floor —
 * tuned against desktop and portrait-phone viewports, where `available` is
 * comfortably above 240px — with `Math.max(MIN_VALID_CANVAS_PX, available)`
 * (no usability floor at all), on the theory that flooring higher than
 * `available` ran the canvas (and React Flow's pinned controls/minimap)
 * UNDER the phone drawer with "no scroll position" able to reach them. That
 * theory was wrong: `MissionCanvas`'s own container (`.missionlens .body`,
 * `styles.css`) already carries a `min-height: 480px` from an unrelated
 * rule, so on a landscape phone (844×390, ~103px available above the
 * drawer) the PAGE was already taller than the viewport and already
 * scrolling — round 1 just left the extra ~377px of already-reserved room
 * BLANK below a canvas shrunk to fit only the sliver visible without
 * scrolling, and `fitView` shrank the graph to match (measured: scale
 * bottomed at React Flow's own 0.1 `minZoom`, unreadable — the #2618
 * regression this function's second parameter fixes).
 *
 * `floor` is that same reachable, already-reserved room, expressed as a
 * value rather than invented here: `MissionCanvas` reads it from the live
 * container's own `min-height` (`getComputedStyle`, same pattern already
 * used for the drawer inset) rather than restating `480` as a second
 * literal that could drift from the CSS rule that actually governs it.
 * Preferring `floor` over `available` when it is larger does not add any
 * new overflow past what `.missionlens .body`'s own CSS already causes
 * today — it only lets the canvas actually FILL that space instead of
 * leaving it blank — and the resulting overflow is confirmed reachable:
 * React Flow's controls are `position: absolute` inside the scrolling
 * canvas box, not `position: fixed` like the drawer, so they move with an
 * ordinary page scroll same as any other flow content would.
 *
 * `available` can also be transiently ≤0 (the very first layout pass,
 * before the sticky header/drawer inset have settled), which is the case
 * `MIN_VALID_CANVAS_PX` genuinely exists to catch — see its own doc. Pure;
 * `floor` defaults to `0` so a caller with nothing to contribute (or a
 * test exercising the original #2520 behavior) gets the old shape back.
 */
export function clampCanvasHeight(available: number, floor: number = 0): number {
  return Math.max(MIN_VALID_CANVAS_PX, floor, available);
}
