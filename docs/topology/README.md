# /topology — moved to /demo

The standalone `/topology` fleet-diagram page has been folded into the unified
observability **demo** at [`/demo`](../demo/). Opening `darkmux.com/topology/`
now redirects there.

## What `/demo` is today

A **static, badged playback** of a curated sample fleet — one inlined flow-record
fixture rendered through a drill-down viewer (mission → fleet → machine →
subsystem). It is explicitly a *demo of sample data*, not a live tool:

- **No live source.** It does not subscribe to `darkmux serve`; it ships an inline
  fixture and makes no daemon fetch — which is what makes it safe to host on an
  HTTPS page (no CORS / mixed-content).
- **No `?fixture=`, drag-drop, or `?test=1`.** Those were capabilities of the
  pre-redirect topology viewer; the demo loads one fixed dataset.

## Where the live tool is headed

A daemon-hosted, drillable viewer over the live flow stream — with fixture loading
and a distinct live-vs-playback mode — is the observability-unification work:
[#556](https://github.com/kstrat2001/darkmux/issues/556) (epic),
[#558](https://github.com/kstrat2001/darkmux/issues/558) (unified viewer),
[#554](https://github.com/kstrat2001/darkmux/issues/554) (daemon hosts it at its
own origin). The design lives in `docs/architecture/observability-unification-plan.md`;
the concepts it renders (flow records, schema 1.8.0, the drill levels) are in
`docs/architecture/CONCEPTS.md`.
