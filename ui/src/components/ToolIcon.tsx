import type { ReactNode } from "react";
import type { ToolIconKind } from "../lib/scopeMorph";

/**
 * (#2890) The glowing line icon the scope's TOOLS center shows for the tool
 * being run: read = eye, edit = pencil, write = page with a plus, bash =
 * terminal prompt, search = magnifier, anything else = gear. Shapes are the
 * operator-approved prototype's (`scope-states-prototype.html`) verbatim.
 * Same stroke-only glyph convention as `ActivityIcon.tsx`.
 *
 * No tool NAME is ever printed; the icon is the whole message. Tests assert
 * on `data-tool-icon`, since an inline SVG adds nothing to `innerText`.
 */
const GLYPH: Record<ToolIconKind, ReactNode> = {
  read: (
    <>
      <path d="M2 12s3.6-6.5 10-6.5S22 12 22 12s-3.6 6.5-10 6.5S2 12 2 12z" />
      <circle cx="12" cy="12" r="3" />
    </>
  ),
  edit: (
    <>
      <path d="M15.5 4.5l4 4L9 19H5v-4L15.5 4.5z" />
      <path d="M13.5 6.5l4 4" />
    </>
  ),
  write: (
    <>
      <path d="M6 3h8l4 4v14H6z" />
      <path d="M14 3v4h4" />
      <path d="M12 11v6M9 14h6" />
    </>
  ),
  bash: (
    <>
      <rect x="3" y="5" width="18" height="14" rx="2" />
      <path d="M7 10l3 2-3 2" />
      <path d="M12 15h5" />
    </>
  ),
  search: (
    <>
      <circle cx="10.5" cy="10.5" r="5.5" />
      <path d="M15 15l5 5" />
    </>
  ),
  other: (
    <>
      <circle cx="12" cy="12" r="3" />
      <path d="M12 3v3M12 18v3M3 12h3M18 12h3M5.6 5.6l2.1 2.1M16.3 16.3l2.1 2.1M5.6 18.4l2.1-2.1M16.3 7.7l2.1-2.1" />
    </>
  ),
};

export function ToolIcon({ kind, className }: { kind: ToolIconKind; className?: string }) {
  return (
    <svg
      className={className}
      data-tool-icon={kind}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={1.6}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      {GLYPH[kind]}
    </svg>
  );
}
