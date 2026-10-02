/** The form of a status lamp: what the dot says about the evidence behind the
 * reading, before its color says anything about the state. One vocabulary for
 * the run page's state lamps and the fleet card's status lamp (#3030).
 *  - Filled: proven work, or a lit state lamp;
 *  - Hollow: proven quiet;
 *  - Dashed: no reading (the viewer does not know);
 *  - Off: an unlit lamp, or a machine known to be offline. */
export enum LampForm {
  Filled = "filled",
  Hollow = "hollow",
  Dashed = "dashed",
  Off = "off",
}
