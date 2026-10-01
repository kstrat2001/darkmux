import { availabilityWarning, type MachineAvailability } from "../lib/machineAvailability";

/**
 * (#3012, 5.0 R3) The warning mark for a machine this viewer cannot see into:
 * nothing when its activity is `known`, else a small chip in the same look as
 * the HUB badge, with the typed warning (and its remedy) as the tooltip. The
 * fleet card renders it in its name row, so it never changes the card's height.
 */
export function AvailabilityWarning({ availability }: { availability: MachineAvailability }) {
  const warning = availabilityWarning(availability);
  if (warning === null) return null;
  return (
    <span
      className="chip chip--sm avail-warn"
      data-testid="availability-warning"
      data-availability={availability}
      role="img"
      aria-label={warning}
      title={warning}
    >
      ⚠
    </span>
  );
}
