// (#2310 P4c-2b fixture) swallowed-error, reused from crawl (confirm=mod).
export function computeTotal(items: number[]) {
  try {
    return items.reduce((a, b) => a + b, 0);
  } catch (e) {
    // swallowed on purpose, for the fixture
  }
}
