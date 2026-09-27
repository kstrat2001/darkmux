import { UtilityGlyph } from "../../components/UtilityGlyph";
import type { UtilitySectionView } from "./utilitySectionView";

/** (#2915) The machine page's Utility section. See `utilitySection.ts`. */
export function UtilitySection({ view }: { view: UtilitySectionView }) {
  return (
    <div className="mm-utility" data-testid="machine-utility">
      <div className="mm-utility__title">utility · last 24h</div>
      <div className="mm-utility__model">
        <span className="mm-utility__id">{view.modelLine}</span>
      </div>
      <div className="mm-utility__facts">{view.factsLine}</div>
      <div className="mm-utility__live">
        <UtilityGlyph strip={view.strip} />
        <span>{view.liveLine}</span>
      </div>
      <div className="mm-utility__jobs" role="table" aria-label="utility jobs, last 24h">
        {view.jobs.map((j) => (
          <div className="mm-utility__job" role="row" key={j.word} data-known={j.known ? "true" : "false"}>
            <span role="cell">{j.word}</span>
            <span role="cell">{j.calls}</span>
            <span role="cell">{j.tokens}</span>
          </div>
        ))}
      </div>
    </div>
  );
}
