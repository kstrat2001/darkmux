#!/usr/bin/env node
/**
 * Render the darkmux brand mark into every icon a browser or OS asks for.
 *
 * Three SVG sources, not one, because an icon is not one drawing resized —
 * each size shows as much of the idea as its pixels can carry, while the
 * silhouette stays constant so it reads as one mark:
 *
 *   mark-full.svg           four channels converging on a node, one output.
 *                           Legible from 64px up; at 16px it is a smudge.
 *   mark-trapezoid-out.svg  the schematic mux symbol plus its output line.
 *   mark-trapezoid.svg      the symbol alone. Survives 16px intact.
 *
 * Lives under `ui/` because node resolves imports from the SCRIPT's own
 * directory, and playwright is `ui/`'s dependency:
 *     node ui/scripts/build-icons.mjs
 */
import { chromium } from "playwright";
import { readFileSync, writeFileSync, mkdirSync, copyFileSync, readdirSync } from "fs";
import { dirname, resolve } from "path";
import { fileURLToPath } from "url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");
// (#2022) The SVGs live under `ui/src/brand/` — inside the build root — so the
// VIEWER can import them directly (`?raw`) and render the mark inline. Keeping
// them in `docs/` would have forced a second hand-maintained copy in the
// bundle, which is the drift this generator exists to prevent.
//
// `docs/media/icon/` is now an OUTPUT: the site's <img> tags keep working
// because the sources are copied there, not because a second original lives
// there.
const SRC = `${ROOT}/ui/src/brand`;
const SITE_SVG = `${ROOT}/docs/media/icon`;
// (#2022) Two consumers, one generator. `docs/` is the marketing site;
// `crates/darkmux-serve/assets/` is what the DAEMON embeds and serves as a
// PWA, and it shipped a different mark entirely — an operator who installed
// the viewer got the old diamond while the website showed the new one.
//
// Same sources, same sizes, one script: a future mark change updates both or
// neither, which is the only way two copies of an identity stay in step.
const SITE = `${ROOT}/docs`;
const VIEWER = `${ROOT}/crates/darkmux-serve/assets`;
const GROUND = "#0b0e14";

// `opaque` matters and is not cosmetic. iOS composites a transparent
// apple-touch-icon onto BLACK, and Android's maskable path fills whatever it
// crops to — so those two get an explicit ground rather than surviving by
// luck. A favicon keeps its transparency: browser tab strips are light in
// light mode, and a hard dark tile there reads as a sticker.
//
// `pad` exists for the maskable variant only. Android crops maskable icons to
// the centre 80%; the full mark's endpoint dots sit at x=6 and x=58 in a
// 64-unit viewBox (9.4% and 90.6%), so both would be clipped — exactly the
// endpoints that make it read as signals entering and leaving.
const TARGETS = [
  { file: "icon-512.png", size: 512, src: "mark-full.svg", opaque: false },
  { file: "icon-512-maskable.png", size: 512, src: "mark-full.svg", opaque: true, pad: 0.8 },
  { file: "icon-192.png", size: 192, src: "mark-full.svg", opaque: false },
  { file: "apple-touch-icon.png", size: 180, src: "mark-full.svg", opaque: true },
  { file: "favicon-32.png", size: 32, src: "mark-trapezoid-out.svg", opaque: false },
  { file: "favicon-16.png", size: 16, src: "mark-trapezoid.svg", opaque: false },
];

// What `crates/darkmux-serve/assets/manifest.webmanifest` actually names,
// plus the touch icon the served page links. Anything else would be a file
// the daemon embeds and never serves.
const VIEWER_FILES = new Set(["icon-192.png", "icon-512.png", "icon-512-maskable.png", "apple-touch-icon.png"]);

const browser = await chromium.launch();
const page = await browser.newPage();
mkdirSync(SITE, { recursive: true });
mkdirSync(SITE_SVG, { recursive: true });
// Publish the sources the site references by URL.
for (const f of readdirSync(SRC).filter((n) => n.endsWith(".svg"))) {
  copyFileSync(`${SRC}/${f}`, `${SITE_SVG}/${f}`);
}
mkdirSync(VIEWER, { recursive: true });

for (const t of TARGETS) {
  const svg = readFileSync(`${SRC}/${t.src}`, "utf8");
  const inner = t.pad ? Math.round(t.size * t.pad) : t.size;
  await page.setViewportSize({ width: t.size, height: t.size });
  await page.setContent(
    `<style>html,body{margin:0;padding:0;width:${t.size}px;height:${t.size}px;
       background:${t.opaque ? GROUND : "transparent"};
       display:flex;align-items:center;justify-content:center}
     svg{display:block;width:${inner}px;height:${inner}px}</style>${svg}`,
  );
  await page.screenshot({ path: `${SITE}/${t.file}`, omitBackground: !t.opaque });
  // The viewer serves a NARROWER set (its manifest only names three), so it
  // gets the files it actually references rather than every size the site has.
  if (VIEWER_FILES.has(t.file)) {
    await page.screenshot({ path: `${VIEWER}/${t.file}`, omitBackground: !t.opaque });
  }
  console.log(`  ${t.file.padEnd(24)} ${String(t.size).padStart(4)}px  ${t.src}${t.pad ? `  (padded to ${t.pad * 100}%)` : ""}${VIEWER_FILES.has(t.file) ? "  +viewer" : ""}`);
}

writeFileSync(
  `${SITE}/site.webmanifest`,
  JSON.stringify(
    {
      name: "darkmux",
      short_name: "darkmux",
      description: "Mission orchestrator and lab for local AI.",
      start_url: "/",
      display: "browser",
      background_color: GROUND,
      theme_color: GROUND,
      icons: [
        { src: "/icon-192.png", sizes: "192x192", type: "image/png", purpose: "any" },
        { src: "/icon-512.png", sizes: "512x512", type: "image/png", purpose: "any" },
        // Split from "any maskable": one file cannot be both without either
        // wasting the full canvas or losing the endpoints to the crop.
        { src: "/icon-512-maskable.png", sizes: "512x512", type: "image/png", purpose: "maskable" },
      ],
    },
    null,
    2,
  ) + "\n",
);
console.log("  site.webmanifest");

await browser.close();
