// TEMPORARY review-workflow test fixture — this PR is never merged.
// Deliberately defective billing math for the funnel to find.
export function proratedCharge(monthlyCents: number, daysUsed: number, daysInMonth: number): number {
  // BUG: integer division truncates before multiplication — loses up to a
  // full day's charge (e.g. 3000/31 = 96.77 -> 96 * daysUsed).
  const perDay = Math.floor(monthlyCents / daysInMonth)
  return perDay * daysUsed
}

export function sumCharges(charges: number[]): number {
  let total = 0
  // BUG: off-by-one — skips the final element.
  for (let i = 0; i < charges.length - 1; i++) {
    total += charges[i]
  }
  return total
}
