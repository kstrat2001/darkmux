// Extraction helpers for the STANDALONE mission-graph page
// (`crates/darkmux-serve/assets/mission-graph.html`, served at
// `/mission/:id/graph`): the parity-golden capture side of #1868 packet 1.
// Deliberately its own module, separate from `lib/extract-lens.js`: that
// file's `extractLensText`/`waitSettled`/frozen-clock helpers are built
// around `viewer.html`'s four-region `#crumb`/`#meta`/`#logscope`/`#stage`
// shape (and its React-port successor, `next.html`); this page has no
// `#stage` at all, and its own regions (`.top`, `.phasegroup`, `.mnode`,
// `.tlphase`/`.tltask`/`.tlt-step`, `.evrow`) are a different DOM grammar
// entirely. `normalize`/`installFrozenClock`/`installControllableClock` ARE
// reused verbatim from `extract-lens.js` (imported below) since those three
// have nothing lens-specific about them.
//
// `mission-graph-goldens.spec.ts` (packet 1, THIS packet) imports these to
// capture `goldens/mission-graph-canvas.txt` / `goldens/mission-graph-
// timeline.txt` from the live standalone page. The future graph-lens PR
// (#1868's later packet, porting this page into `ui/src`) is meant to
// import the SAME functions to grade the port against these same goldens;
// that reuse is the whole point of a dedicated module rather than inlining
// the extraction in the spec file.

const { normalize } = require("./extract-lens.js");

/**
 * The masthead / metrics strip (`.top`: mission id, status badge, live
 * pill, token/turn meter, host-activity readout, view/minimap/events/legend
 * buttons). Analogous to `extract-lens.js`'s `extractTopbarText`, but this
 * page has no separate `#crumb`/`#meta`/`#logscope`, so `.top` carries all
 * of it in one bar.
 */
async function extractHeaderText(page) {
  const el = page.locator(".top");
  if ((await el.count()) === 0) return "(empty)";
  return normalize(await el.innerText());
}

/**
 * Every `.phasegroup` (the canvas renderer's phase container node) in DOM
 * order: status class plus the "PHASE"/label text pair. One string per
 * group.
 */
async function extractPhaseGroupsText(page) {
  return page.$$eval(".phasegroup", (els) =>
    els.map((el) => {
      const statusClass = [...el.classList].find((c) => c.startsWith("s-")) || "s-unknown";
      const kindEl = el.querySelector(".pg-kind");
      const nameEl = el.querySelector(".pg-name");
      const kind = kindEl ? kindEl.textContent.trim() : "";
      const name = nameEl ? nameEl.textContent.trim() : "";
      return `[${statusClass}] ${kind} ${name}`.trim();
    })
  );
}

/**
 * Every `.mnode` (the canvas renderer's task/phase card, `MissionNode`) in
 * DOM order: kind class, status class, label (`.mn-label`), title
 * attribute (the description/label tooltip), and its `.steprow` children
 * (each: status class plus the row's own flattened text, lead/seat/model/
 * meter). One multi-line block per node, blocks joined by the caller.
 */
async function extractMissionNodesText(page) {
  return page.$$eval(".mnode", (els) =>
    els.map((el) => {
      const classes = [...el.classList];
      const kindClass = classes.find((c) => c.startsWith("k-")) || "k-unknown";
      const statusClass = classes.find((c) => c.startsWith("s-")) || "s-unknown";
      const labelEl = el.querySelector(".mn-label");
      const label = labelEl ? labelEl.textContent.trim() : "";
      const title = el.getAttribute("title") || "";
      const steps = [...el.querySelectorAll(".steprow")].map((s) => {
        const sc = [...s.classList].find((c) => c.startsWith("s-")) || "s-unknown";
        // `stepRowEls` (mission-graph.html) renders its child spans with no
        // whitespace text node between them, so a raw `.textContent` runs
        // adjacent labels together (e.g. a step name immediately followed
        // by its model id). Pull the named parts individually and join
        // them with an explicit separator instead.
        const parts = [".slead", ".sname", ".smodel", ".mn-step-meter"]
          .map((sel) => s.querySelector(sel))
          .map((n) => (n ? n.textContent.trim() : ""))
          .filter(Boolean);
        return `    [${sc}] ${parts.join(" | ")}`;
      });
      const head = `[${kindClass} ${statusClass}] ${label} (title="${title}")`;
      return steps.length ? `${head}\n${steps.join("\n")}` : head;
    })
  );
}

/** Count of `.react-flow__edge` elements React Flow renders for the canvas
 * graph's edges. The parity-relevant number is the COUNT (edge routing and
 * curvature is React Flow's own layout, not darkmux content). */
async function extractEdgeCount(page) {
  return page.locator(".react-flow__edge").count();
}

/**
 * Every `.evrow` (the mission-scoped events panel's rows) in DOM order: the
 * `.evt` (time) / `.eva` (action) / `.evh` (handle) parts, joined. Shared
 * by BOTH renderers, since the events panel is the same component
 * regardless of which body renderer (canvas/timeline) is active.
 */
async function extractEventRowsText(page) {
  return page.$$eval(".evrow", (els) =>
    els.map((el) => {
      const evt = el.querySelector(".evt");
      const eva = el.querySelector(".eva");
      const evh = el.querySelector(".evh");
      const parts = [
        evt ? evt.textContent.trim() : "",
        eva ? eva.textContent.trim() : "",
        evh ? evh.textContent.trim() : "",
      ];
      return parts.join(" | ");
    })
  );
}

/**
 * Every `.tlphase` / `.tltask` / `.tlt-step` (the mobile vertical-timeline
 * renderer's node vocabulary) in DOCUMENT order: a single combined
 * selector so phases/tasks/steps interleave the same way they render,
 * rather than being grouped by kind. `.tlt-step` also carries `.steprow`
 * (shared row vocabulary with the canvas's `.mn-step-row`), but the
 * `.tlt-step` class is what scopes this to the timeline's OWN step rows;
 * see `stepRowEls`/`renderMissionTimeline` in mission-graph.html.
 *
 * Task cards render their `.tlt-steps` (and therefore any `.tlt-step`
 * children) only while `expanded[task.id]` is true. Nothing auto-expands
 * on load (see the spec's own module doc for why), so callers that want
 * step-level content in this extraction must open every task card first
 * (click `.tlt-hd`) before calling this.
 */
async function extractTimelineNodesText(page) {
  return page.$$eval(".tlphase, .tltask, .tlt-step", (els) =>
    els.map((el) => {
      const classes = [...el.classList];
      const statusClass = classes.find((c) => c.startsWith("s-")) || "s-unknown";
      if (classes.includes("tlphase")) {
        const nameEl = el.querySelector(".tlph-name");
        const tagEl = el.querySelector(".tlph-tag");
        const name = nameEl ? nameEl.textContent.trim() : "";
        const tag = tagEl ? tagEl.textContent.trim() : "";
        return `PHASE [${statusClass}] ${name} (${tag})`;
      }
      if (classes.includes("tltask")) {
        const open = classes.includes("open") ? " open" : " closed";
        const nameEl = el.querySelector(".tlt-name");
        const name = nameEl ? nameEl.textContent.trim() : "";
        return `  TASK [${statusClass}${open}] ${name}`;
      }
      // .tlt-step (co-classed with .steprow): same `stepRowEls` shape the
      // canvas node's `.mn-step-row` uses, so pull the same named parts
      // rather than a raw `.textContent` (see `extractMissionNodesText`'s
      // own comment for why: no whitespace text node separates the spans).
      const parts = [".slead", ".sname", ".smodel", ".mn-step-meter"]
        .map((sel) => el.querySelector(sel))
        .map((n) => (n ? n.textContent.trim() : ""))
        .filter(Boolean);
      return `    STEP [${statusClass}] ${parts.join(" | ")}`;
    })
  );
}

/**
 * Click every collapsed `.tlt-hd` task header (in DOM order, one at a time;
 * clicking flips local React state, so this doesn't need the network-race
 * caution `waitSettled` in `extract-lens.js` exists for) and wait for each
 * task to carry the `.open` class before moving to the next. Idempotent:
 * skips a task already open. Used so `extractTimelineNodesText` can capture
 * `.tlt-step` rows, which only exist in the DOM once their owning task is
 * expanded.
 */
async function expandAllTimelineTasks(page, expect) {
  const count = await page.locator(".tltask").count();
  for (let i = 0; i < count; i++) {
    const task = page.locator(".tltask").nth(i);
    const alreadyOpen = (await task.getAttribute("class")) || "";
    if (/\bopen\b/.test(alreadyOpen)) continue;
    await task.locator(".tlt-hd").click();
    await expect(task).toHaveClass(/\bopen\b/);
  }
}

/** Full canvas-mode golden text: header, phasegroups, nodes, edge count,
 * and events, normalized. `=== section ===` markers match the labeling
 * convention `extract-lens.js`'s goldens already use. */
async function extractCanvasGolden(page) {
  const [header, phasegroups, nodes, edgeCount, events] = await Promise.all([
    extractHeaderText(page),
    extractPhaseGroupsText(page),
    extractMissionNodesText(page),
    extractEdgeCount(page),
    extractEventRowsText(page),
  ]);
  const text =
    `=== header ===\n${header}\n\n` +
    `=== phasegroups ===\n${phasegroups.length ? phasegroups.join("\n") : "(none)"}\n\n` +
    `=== nodes ===\n${nodes.length ? nodes.join("\n\n") : "(none)"}\n\n` +
    `=== edges ===\nedge_count: ${edgeCount}\n\n` +
    `=== events ===\n${events.length ? events.join("\n") : "(none)"}\n`;
  return normalize(text);
}

/** Full timeline-mode golden text: header, the interleaved phase/task/step
 * vocabulary, and events, normalized. Caller must have already expanded
 * every task (`expandAllTimelineTasks`) if step rows should be included. */
async function extractTimelineGolden(page) {
  const [header, nodes, events] = await Promise.all([
    extractHeaderText(page),
    extractTimelineNodesText(page),
    extractEventRowsText(page),
  ]);
  const text =
    `=== header ===\n${header}\n\n` +
    `=== timeline ===\n${nodes.length ? nodes.join("\n") : "(none)"}\n\n` +
    `=== events ===\n${events.length ? events.join("\n") : "(none)"}\n`;
  return normalize(text);
}

module.exports = {
  extractHeaderText,
  extractPhaseGroupsText,
  extractMissionNodesText,
  extractEdgeCount,
  extractEventRowsText,
  extractTimelineNodesText,
  expandAllTimelineTasks,
  extractCanvasGolden,
  extractTimelineGolden,
};
