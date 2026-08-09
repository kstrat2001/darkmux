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

export function ReadyHeadline({ n, ago }: { n: number; ago: string }) {
  return (
    <>
      {/* No `● ready`: the machine CARDS below state health per machine, and
          one green dot cannot represent two machines in different states. It
          also collided with the `● live` badge inches away — two green dots
          asserting different things, with nothing telling you which to
          believe. The count and the icon carry what is left. */}
      <span className="mco" title={`${n} machine${n === 1 ? "" : "s"} online`}>
        {n} <MachineIcon />
      </span>
      {ago ? ` · last dispatch ${ago}` : null}
    </>
  );
}
