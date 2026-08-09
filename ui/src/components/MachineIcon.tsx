/** `ICON.machine` — viewer.html:935. A chip outline.
 *
 * Shared rather than local to one lens: it appears on the fleet cards AND in
 * the `#meta` headline beside the online-machine count. The meta one was
 * MISSING in this port until the operator noticed the bare number, and the
 * reason it went unnoticed is worth keeping — an inline SVG contributes no
 * text to `innerText`, so the parity goldens matched byte-for-byte with the
 * icon absent. Purely visual elements are structurally invisible to a
 * text-extraction harness. */
export function MachineIcon() {
  return (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8} strokeLinecap="round" strokeLinejoin="round">
      <rect x="7" y="7" width="10" height="10" rx="1.5" />
      <path d="M10 7V4.5M14 7V4.5M10 19.5V17M14 19.5V17M7 10H4.5M7 14H4.5M19.5 10H17M19.5 14H17" />
    </svg>
  );
}
