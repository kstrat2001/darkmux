/**
 * (#3022) The HUB badge: this machine's card declares `fleet.mode hub`. It
 * states what the machine declares, read from the card the fleet view holds,
 * so nobody has to ask the machine. The fleet card and the machine lens
 * header both render this one component (nothing when `declared` is false), in the `.chip` look every label in
 * the masthead already uses (a `--sm` size so it fits the card's name row
 * without changing its height).
 */
export function HubBadge({ declared }: { declared: boolean }) {
  if (!declared) return null;
  return (
    <span className="chip chip--sm hub-badge" data-testid="hub-badge" title="this machine declares itself the fleet hub (fleet.mode hub)">
      hub
    </span>
  );
}
