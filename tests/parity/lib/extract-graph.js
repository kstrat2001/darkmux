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
// `mission-graph-goldens.spec.ts` (packet 1) captured `goldens/mission-
// graph-canvas.txt` / `goldens/mission-graph-timeline.txt` from the live
// standalone page using the extractors above this comment.
//
// `next-parity-graph.spec.ts` (packet 2, THIS addition — #1868) grades the
// PORT (`MissionGraphLens`, `#mission=<id>`) against those SAME goldens,
// reusing `extractPhaseGroupsText`/`extractMissionNodesText`/
// `extractEdgeCount`/`extractTimelineNodesText`/`extractCanvasGolden`/
// `extractTimelineGolden` VERBATIM — the port keeps `.phasegroup`/`.mnode`/
// `.steprow`/`.tlphase`/`.tltask`/`.tlt-step` byte-identical to the
// standalone page on purpose (see `MissionCanvas.tsx`/
// `MissionTimelineView.tsx`'s own doc), so those sections grade
// byte-for-byte with ZERO new extraction code.
//
// The header and events sections do NOT reuse `extractHeaderText`/
// `extractEventRowsText` — the port has its own chrome (a real app-shell
// masthead sits above the lens) and its events pane is `EventLogColumn`
// (`.eventlog__rec` rows), not the standalone page's `.evrow` markup. Two
// NEW port-side extractors below normalize each to the SAME shape the
// standalone extractors already produce (mission id + status for the
// header; `time | action | subject` triples for events), so the SPEC can
// still assert real equality against the golden's corresponding section —
// just via a normalizing comparator instead of a raw string diff. See each
// function's own doc for the one field EventLogColumn's row doesn't render
// natively (`handle`) and how it's recovered.

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

// ─── Port-side extractors (#1868 packet 2) ─────────────────────────────────
// `MissionGraphLens` (`ui/src/lenses/mission/`), not the standalone page.
// See this module's own doc above for why these are separate functions
// rather than reusing `extractHeaderText`/`extractEventRowsText`.

/** The port's own header — mission id (`.midname`) + status (`.mstatus`),
 * normalized to the SAME two facts `sectionOf(golden, "header")` is graded
 * against via `headerFactsOf` below. Deliberately does NOT try to match the
 * standalone page's `.top` bar byte-for-byte (refresh/view/minimap/events/
 * legend button labels, the live pill) — that's real, DIFFERENT chrome (a
 * real app-shell masthead sits above this lens too), not a parity target. */
async function extractPortHeaderText(page) {
  // (#2332) The header SHOWS the id's name (epoch stripped, hash in the
  // sub-line) and carries the full id on `data-mission-id` (and `title`);
  // the parity fact is the full id, so read the attribute, falling back to
  // the text for an id the name-split leaves whole.
  const nameEl = page.locator(".missionlens .midname");
  const missionId =
    (await nameEl.getAttribute("data-mission-id").catch(() => null)) || (await nameEl.textContent().catch(() => "")) || "";
  const status = (await page.locator(".missionlens .mstatus").textContent().catch(() => "")) || "";
  return normalize(`${missionId.trim()} | ${status.trim()}`);
}

/** Pull "mission id | status" out of a captured standalone-page golden's
 * own `=== header ===` section, so `extractPortHeaderText`'s output can be
 * graded against a REAL fact from the golden rather than a value hand-typed
 * into the spec. The standalone header's line 3 is always the raw mission
 * id (`g.mission_id`, unstyled — no text-transform rule touches it) and
 * line 4 is the uppercase status badge (`.mstatus`, `text-transform:
 * uppercase`) — see `mission-graph.html`'s own header JSX order (nav,
 * brand, midname, mstatus, ...). Lowercases the status back, since the
 * port's OWN `.mstatus` carries no such CSS rule (this app's status text is
 * lowercase everywhere else too — see `App.tsx`'s `routeChrome` doc on the
 * same "uppercase the STRING directly, never lean on CSS" discipline). */
function headerFactsOf(fullGoldenText) {
  const section = sectionOf(fullGoldenText, "header");
  const lines = section
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean);
  // lines[0] is the "=== header ===" marker itself (`sectionOf` returns the
  // marker line PREPENDED to the body); lines[1] the "‹ VIEWER" back link;
  // lines[2] the brand ("darkmux · mission" on canvas, bare "darkmux" on
  // the timeline's narrower header — the `.sub` text hides below 700px, see
  // mission-graph.html's own CSS); the mission id is the first line after
  // that, in both viewports.
  const missionId = lines[3];
  const status = lines[4];
  return normalize(`${missionId} | ${status.toLowerCase()}`);
}

/** `EventLogColumn`'s own rows (`.eventlog__rec`), normalized to the SAME
 * `time | action | subject` triple shape `extractEventRowsText` produces
 * from the standalone page's `.evrow`s. Two real differences from that
 * page's markup, both accounted for rather than silently glossed over:
 *
 * 1. `activityOf()` (`lib/eventFilters.ts`) is a richer label mapping than
 *    the standalone page's raw `rec.action` display — but for every action
 *    this fixture's records carry ("mission start"/"phase start"/
 *    "phase complete"/"mission close"), `activityOf` has no explicit branch
 *    and falls through to `a || "other"`, i.e. the RAW action string
 *    verbatim. So the two sides agree for this fixture's action vocabulary
 *    without any translation layer — a real property of the shared code,
 *    not a coincidence papered over by normalization.
 * 2. `EventLogColumn`'s row never renders `handle` as visible text (unlike
 *    the standalone page's `.evh`) — this app's shared event log wasn't
 *    designed around a "subject" concept. #1868 added a minimal, real
 *    `title` attribute carrying the record's `handle` to `.eventlog__rec`
 *    (see that component's own doc for why this is a genuine addition, not
 *    test scaffolding: hover provenance, same convention `.smodel`/
 *    `.mn-label` already use elsewhere in this app) — read via
 *    `getAttribute("title")` here, which is how the "subject" field
 *    survives into this extractor at all.
 *
 * A bare `.eventlog__rec` selector, NOT `.missionlens .eventlog__rec` —
 * pre-mainstay-unification, `EventLogColumn` mounted TWICE on `#mission=<id>`
 * (the always-mounted App-level instance, CSS-hidden and still carrying the
 * FLEET-WIDE window's rows since `mission` was excluded from
 * `showsEventLog`, plus this lens's own separate mission-scoped instance),
 * so a bare selector caught 8 real rows plus 50 (`LOG_CAP`) unrelated
 * fleet-window rows from the hidden one — a real find while building this
 * extractor, not a hypothetical. `MissionGraphLens` no longer mounts its own
 * `EventLogColumn` (it reports its scoped events upward via `onEvents`
 * instead — see that component's own doc), so there is exactly ONE instance
 * on the page again, same as every other route; the `.missionlens` scope is
 * gone because the thing it used to exclude no longer exists. */
async function extractPortEventsText(page) {
  return page.$$eval(".eventlog__rec", (els) =>
    els.map((el) => {
      const timeEl = el.querySelector(".eventlog__rectime");
      const actEl = el.querySelector(".eventlog__ractivity");
      const time = timeEl ? timeEl.textContent.trim() : "";
      const action = actEl ? actEl.textContent.trim() : "";
      const subject = el.getAttribute("title") || "";
      return `${time} | ${action} | ${subject}`;
    }),
  );
}

/** Slice one `=== name ===` section out of a full golden file, stopping at
 * the NEXT `=== ` marker (unlike `extract-next-lens.js`'s `stageSectionOf`/
 * `catalogSectionOf`, which slice to end-of-string because `#stage` is
 * always their golden's LAST section) — every mission-graph golden has
 * multiple sections after `header`, so this harness needs the narrower cut. */
function sectionOf(fullGoldenText, name) {
  const marker = `=== ${name} ===\n`;
  const start = fullGoldenText.indexOf(marker);
  if (start === -1) throw new Error(`sectionOf: no "${marker.trim()}" marker found in golden text`);
  const contentStart = start + marker.length;
  const nextMarker = fullGoldenText.indexOf("\n=== ", contentStart);
  const body = nextMarker === -1 ? fullGoldenText.slice(contentStart) : fullGoldenText.slice(contentStart, nextMarker + 1);
  return `${marker}${body}`;
}

/** The golden's own `=== events ===` section, trimmed to just its row
 * lines — already in the EXACT `time | action | subject` shape
 * `extractPortEventsText` produces (see that function's own doc), so this
 * is a real byte-equality target, not a second normalization pass. */
function eventsBodyOf(fullGoldenText) {
  const section = sectionOf(fullGoldenText, "events");
  const body = section.slice(section.indexOf("\n") + 1).trim();
  return body === "(none)" ? "" : body;
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
  extractPortHeaderText,
  headerFactsOf,
  extractPortEventsText,
  sectionOf,
  eventsBodyOf,
};
