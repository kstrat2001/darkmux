/**
 * `● ready · N ⬚ · last run …` — viewer.html:1283.
 *
 * This exists because the port had flattened legacy's three elements into one
 * plain string:
 *
 *   legacy: <span class="rdot ok">●</span> ready ·
 *           <span class="mco" title="N machines online">N <svg/></span> · last run …
 *   port:   "● ready · N " + " · last run …"
 *
 * The text was identical, so all 22 goldens passed — while the dot had lost
 * the `ok` class that colours it green and the machine icon was simply gone.
 * `#meta` IS an extracted parity region; extraction compares `innerText`, and
 * neither a CSS class nor an inline SVG contributes any text. A purely visual
 * element is structurally invisible to a text-extraction harness, which is
 * why `metaChrome.test.tsx` asserts STRUCTURE (element present, class
 * applied) rather than appearance.
 *
 * The double space before "· last run" is legacy's own artifact — the icon
 * SVG sits between two text nodes and breaks the whitespace-collapse run.
 * The previous string version hard-coded that space to reproduce the SHADOW
 * the icon casts while omitting the icon. Rendering the real structure gets
 * it for free, and the test pins the exact text so parity stays honest.
 */
import { MachineIcon } from "./MachineIcon";
import { fleetCoverageMessage, type DegradedFleetSource } from "./FleetCoverageNotice";

/**
 * (#2683) `coverage` is the degraded fleet-source state, or `null` when
 * presence is being read cleanly.
 *
 * `n` is presence-derived, and presence is fleet-membership truth here — so
 * "2 machines online" is a claim about RIGHT NOW. When the fleet substrate is
 * stale or unreadable the count still renders (the last answer beats no
 * answer, and blanking it would read as "zero machines", a worse lie), but it
 * stops being asserted bare: the ⚠ says the number is not being backed up,
 * and the title carries the same sentence the shared `FleetCoverageNotice`
 * under the masthead is spelling out in full.
 *
 * The healthy render is byte-identical to before — marker and substituted
 * title appear only in the degraded states, so the parity goldens (`#meta` is
 * an extracted `innerText` region) are untouched.
 */
export function ReadyHeadline({ n, ago, coverage = null }: { n: number; ago: string; coverage?: DegradedFleetSource | null }) {
  return (
    <>
      {/* No `● ready`: the machine CARDS below state health per machine, and
          one green dot cannot represent two machines in different states. It
          also collided with the `● live` badge inches away — two green dots
          asserting different things, with nothing telling you which to
          believe. The count and the icon carry what is left. */}
      <span
        className="mco"
        data-coverage={coverage ? coverage.state : undefined}
        title={coverage ? fleetCoverageMessage(coverage) : `${n} machine${n === 1 ? "" : "s"} online`}
      >
        {coverage ? <span className="mco__warn">⚠ </span> : null}
        {n} <MachineIcon />
      </span>
      {ago ? ` · last dispatch ${ago}` : null}
    </>
  );
}
