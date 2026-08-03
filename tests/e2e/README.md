
## Selective runs — use these, not the cargo suite

The whole browser suite is **~11 seconds for 41 tests**. The cargo workspace
suite is ~10 minutes. Every UI defect found by operator testing on 2026-08-03
(#1618, #1620, #1621) lived in this suite's domain, not cargo's — so for
anything rendered, this is the loop, and cargo is not.

```bash
cd tests/e2e

npm run list             # every test, by file:line and name
npm run ui               # all 41 (~11s)
npm run grep -- sideways # one test by NAME substring — ~1s
npm run one -- viewer-panel.spec.js      # one FILE
npm run graph            # the mission-graph specs
npm run panel            # the console-panel specs
npm run phone            # the narrow-width specs
npm run headed -- chrome-order.spec.js   # watch it in a real browser
npm run debug  -- chrome-order.spec.js   # step through it
```

### The rule that makes them worth anything

**A UI test that passes proves nothing until the fix has been reverted under
it.** Two of the three tests in `chrome-order.spec.js` were written wrong the
first time and passed against the defect they were meant to catch — a
fixed-width panel fixture cannot reproduce a wrong-WIDTH negotiation. Only
reverting the fix said so. Red-prove, every time; it costs one second here.
