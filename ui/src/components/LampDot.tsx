import { LampForm } from "../lib/lamp";

/** The one lamp dot: the run page's state lamps and the fleet card's status
 * line draw it. Its color is the `--lit` custom property of whatever
 * surrounds it; `form` picks how it is drawn (see `LampForm`). */
export function LampDot({ form }: { form: LampForm }) {
  return <span className="lamp-dot" data-form={form} aria-hidden="true" />;
}
