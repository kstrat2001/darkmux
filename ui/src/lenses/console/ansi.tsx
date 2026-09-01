/**
 * ANSI SGR + OSC 8 hyperlink renderer — a straight port of `viewer.html`'s
 * `renderAnsi(text)` (see that file's own extensive module doc: "the only
 * two escape families darkmux's own output emits", the security posture
 * around `esc()` + a fixed tag vocabulary + the http(s)-only OSC-8 allowlist,
 * and the e2e XSS gate that walks hostile fixtures through this path).
 *
 * The port keeps the SAME two-pass shape (parse into a flat list of styled
 * segments, then render) but the OUTPUT is React elements built from a typed
 * array rather than an HTML string assembled with `esc()` calls — React's
 * own text-node escaping is the `esc()` equivalent here (a raw string passed
 * as JSX children is never interpreted as markup), and there is no
 * `dangerouslySetInnerHTML` anywhere in this module. Same security property
 * as legacy (untrusted server bytes can only ever become text content inside
 * a `span`/`a` chosen by THIS code, never arbitrary markup), reached by a
 * different, more idiomatic-for-React mechanism.
 */
import type { ReactNode } from "react";
import { PANEL_IDS, type PanelId } from "../../lib/route";

/** viewer.html: `const ANSI_SGR_CLASS = {...}`. */
export const ANSI_SGR_CLASS: Record<number, string> = {
  1: "a-bold",
  2: "a-dim",
  3: "a-ital",
  4: "a-underline",
  30: "a-fg0",
  31: "a-fg1",
  32: "a-fg2",
  33: "a-fg3",
  34: "a-fg4",
  35: "a-fg5",
  36: "a-fg6",
  37: "a-fg7",
  90: "a-fg8",
  91: "a-fg9",
  92: "a-fg10",
  93: "a-fg11",
  94: "a-fg12",
  95: "a-fg13",
  96: "a-fg14",
  97: "a-fg15",
};

/** (#2206/#2207, slop-chop pilot) The loopback spellings an OSC 8 target
 * may carry. Extracted token-identically from `panelHref`'s inline check. */
export function isLoopback(hostname: string): boolean {
  return hostname === "127.0.0.1" || hostname === "localhost" || hostname === "[::1]" || hostname === "::1";
}

/** (#2206/#2207, slop-chop pilot) Is `ch` a CSI final byte (`@`..`~`,
 * ECMA-48's terminator range)? Extracted token-identically from the SGR
 * scanner's inline comparison; the `j < text.length` bounds check stays at
 * the call site, exactly as before. */
export function isCsiFinalByte(ch: string): boolean {
  return ch >= "@" && ch <= "~";
}

/**
 * viewer.html: `function panelHref(raw)`. An OSC 8 target is safe to
 * linkify only when it is http(s). `mission status` bakes ABSOLUTE daemon
 * URLs (the CLI picks loopback on a standalone machine) — a loopback href
 * tapped from the phone would open the PHONE's own localhost, not this
 * daemon. A URL whose origin is a daemon origin (same-origin as this page,
 * or a bare loopback form) is rewritten RELATIVE so the link rides whatever
 * origin the page itself was loaded from; a foreign origin is left absolute
 * and intact.
 */
export function panelHref(raw: string): string | null {
  let u: URL;
  try {
    u = new URL(raw, window.location.href);
  } catch {
    return null;
  }
  if (u.protocol !== "http:" && u.protocol !== "https:") return null;
  if (u.origin === window.location.origin) return u.pathname + u.search + u.hash;
  if (isLoopback(u.hostname)) {
    return u.pathname + u.search + u.hash;
  }
  return u.href;
}

/**
 * viewer.html: `function panelSwitchId(href)`. A same-origin
 * `#lens=console&panel=<id>` link whose id is one this build actually has —
 * the CLI emits these (`panel_deep_link` in `src/mission_status.rs`) so a
 * hint like "`--all` for every mission" stays actionable from inside a
 * panel. Returns the id, or `null` for every other link (foreign origin,
 * different lens, or an id this build doesn't recognize — which falls
 * through to a plain link rather than a broken in-page switch).
 */
export function panelSwitchId(href: string): PanelId | null {
  let u: URL;
  try {
    u = new URL(href, window.location.href);
  } catch {
    return null;
  }
  if (u.origin !== window.location.origin) return null;
  const h = new URLSearchParams((u.hash || "").replace(/^#/, ""));
  if (h.get("lens") !== "console") return null;
  const id = h.get("panel");
  return id && (PANEL_IDS as readonly string[]).includes(id) ? (id as PanelId) : null;
}

export interface AnsiSegment {
  text: string;
  classes: string[];
  link: string | null;
  /** Non-null when `link` is an in-page panel switch (`panelSwitchId(link)`
   * resolved to a real id) — the render layer routes THESE through the same
   * delegated switch-panel action the tab picker uses, rather than letting
   * the anchor navigate. */
  switchTo: PanelId | null;
}

/**
 * viewer.html: `function renderAnsi(text)`, minus the HTML-string assembly
 * (see module doc — this returns a flat segment list; `AnsiText` below turns
 * it into React nodes). Parses SGR (`ESC [ ... m`) and OSC 8
 * (`ESC ] 8 ; params ; URL ST`) escapes; every other escape sequence is
 * consumed and discarded, never guessed at, matching legacy's "narrow by
 * design" scope.
 */
export function parseAnsi(text: string): AnsiSegment[] {
  const out: AnsiSegment[] = [];
  let classes: string[] = [];
  let link: string | null = null;
  let i = 0;
  let buf = "";

  function flush() {
    if (!buf) return;
    const switchTo = link ? panelSwitchId(link) : null;
    out.push({ text: buf, classes: classes.slice(), link, switchTo });
    buf = "";
  }

  while (i < text.length) {
    const ch = text[i];
    if (ch !== "\x1b") {
      buf += ch;
      i++;
      continue;
    }

    // CSI ... final-byte (only `m` — SGR — is honored)
    if (text[i + 1] === "[") {
      let j = i + 2;
      while (j < text.length && !isCsiFinalByte(text[j])) j++;
      if (j >= text.length) {
        i = text.length;
        break; // truncated: drop
      }
      const final = text[j];
      const params = text.slice(i + 2, j);
      if (final === "m") {
        flush();
        let codes = params.split(";").filter((x) => x !== "");
        if (!codes.length) codes = ["0"]; // bare ESC[m == reset
        codes.forEach((c) => {
          const n = parseInt(c, 10);
          if (n === 0) {
            classes = [];
            return;
          }
          const cls = ANSI_SGR_CLASS[n];
          if (cls && classes.indexOf(cls) === -1) classes.push(cls);
        });
      }
      i = j + 1;
      continue;
    }

    // OSC 8 — `ESC ] 8 ; params ; URL ST` opens, empty URL closes.
    if (text[i + 1] === "]") {
      let end = text.indexOf("\x1b\\", i); // ST
      const bel = text.indexOf("\x07", i);
      if (bel !== -1 && (end === -1 || bel < end)) end = bel;
      if (end === -1) {
        i = text.length;
        break; // truncated: drop
      }
      const body = text.slice(i + 2, end);
      const stLen = text[end] === "\x07" ? 1 : 2;
      if (body.slice(0, 2) === "8;") {
        const semi = body.indexOf(";", 2);
        const url = semi === -1 ? "" : body.slice(semi + 1);
        flush();
        link = url ? panelHref(url) : null;
      }
      i = end + stLen;
      continue;
    }

    // Any other escape: consume the introducer and move on.
    i += 2;
  }
  flush();
  return out;
}

/** Renders `parseAnsi`'s segments into React nodes: a plain text run, a
 * `<span class="...">` for a styled run, or an `<a>` for a linked run — the
 * same fixed tag vocabulary as legacy's `renderAnsi`. `onPanelSwitch` is
 * called (and the click prevented from navigating) for a link whose
 * `switchTo` resolved; every other link is a real anchor and behaves like
 * one (copy-link, middle-click, `Cmd`-click all keep working). */
export function AnsiText({ text, onPanelSwitch }: { text: string; onPanelSwitch: (id: PanelId) => void }): ReactNode {
  const segments = parseAnsi(text);
  return segments.map((seg, idx) => {
    const content = seg.classes.length ? <span className={seg.classes.join(" ")}>{seg.text}</span> : seg.text;
    if (!seg.link) return <span key={idx}>{content}</span>;
    if (seg.switchTo) {
      const id = seg.switchTo;
      return (
        <a
          key={idx}
          href={seg.link}
          className="a-link"
          data-act="setpanel"
          data-arg={id}
          onClick={(e) => {
            e.preventDefault();
            onPanelSwitch(id);
          }}
        >
          {content}
        </a>
      );
    }
    return (
      <a key={idx} href={seg.link} className="a-link">
        {content}
      </a>
    );
  });
}
