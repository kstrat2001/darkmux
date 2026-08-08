import { LensPlaceholder } from "../../components/LensPlaceholder";

/**
 * The `playback` route — a bare `#<date>` hash (`targetDate()`'s convenience
 * form in `viewer.html`, distinct from the catalog panel's own day-row
 * click, which does a full navigation to `/play/<date>`, a server route
 * this app doesn't reproduce in-SPA). See `route.ts`'s own doc for how this
 * differs from `unknown` (Packet 1–3 treated it as unrecognized; this
 * packet is the one that owns the catalog, so it's named honestly now
 * instead of staying misclassified).
 *
 * Rendering the FULL historical playback view (fetch `/flow/<date>`, derive
 * the fleet-hero render model, paint it) is deliberately NOT this packet's
 * job: `/next`'s fleet lens itself has no byte-parity port yet (`FleetStrip`
 * only covers `/fleet/machines/live` presence — the fleet-hero rendering
 * `fleet.txt`'s goldens actually capture is Packet 5's territory, since it's
 * tied to the same live-tail/SSE derivation pipeline). Building a SECOND,
 * parallel non-live rendering pipeline here — before the live one exists —
 * would be scope creep wearing this packet's clothes, so this stays an
 * honest not-ported notice; a follow-up packet (once the fleet-hero render
 * pipeline exists) can widen this into a real playback view, or simply
 * redirect to the still-fully-functional legacy `/play/<date>` the way the
 * catalog panel's OWN day-row click does — either is a reasonable choice
 * left to that packet, not decided here.
 */
export function PlaybackLens({ date }: { date: string }) {
  return <LensPlaceholder label={`playback for ${date}`} hash={date} />;
}
