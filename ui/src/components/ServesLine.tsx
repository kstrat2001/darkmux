import { servesParts, type ServesPart } from "../lenses/fleet/cards";

/** The tooltip for one thing a machine serves. A fact about that machine
 * alone (its card's `serves_profiles` / `serves_radio`), so it reads the same
 * from every server: never which peer may use it, and never a relationship
 * with the machine serving the viewer. */
function servesTitle(machine: string, part: ServesPart): string {
  return part.kind === "profiles"
    ? `${machine} serves ${part.text} to peers it allows.`
    : `${machine} serves radio: it answers radio questions for peers it allows.`;
}

/** The fleet card's serves line: "serves 3 profiles · radio". Always rendered,
 * and the same height whether it has words or not, so a card that serves
 * nothing is the size of one that serves both. */
export function ServesLine({ machine, profiles, radio }: { machine: string; profiles: number; radio: boolean }) {
  const parts = servesParts(profiles, radio);
  return (
    <div className="serves" data-testid="serves-line">
      {parts.length > 0 ? "serves " : null}
      {parts.map((part, i) => (
        <span key={part.kind}>
          {i > 0 ? " · " : null}
          <span data-testid={part.kind === "profiles" ? "profiles-served" : "radio-seat"} title={servesTitle(machine, part)}>
            {part.text}
          </span>
        </span>
      ))}
    </div>
  );
}
