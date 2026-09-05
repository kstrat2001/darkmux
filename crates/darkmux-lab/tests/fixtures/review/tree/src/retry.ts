// (#2310 P4c-2b fixture) existing-solution: a hand-rolled retry-with-backoff
// helper, the classic "this already exists somewhere" catch.
export function retryWithBackoff(fn: () => Promise<void>, attempts: number) {
  let delay = 100;
  for (let i = 0; i < attempts; i++) {
    try {
      return fn();
    } catch (e) {
      delay = delay * 2;
    }
  }
}
