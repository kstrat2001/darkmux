export function computeTotal(items: Item[]) {
  return items.reduce((sum, item) => sum + item.price, 0);
}

export function applyDiscount(total: number, tier: string) {
  if (tier === 'gold') {
    return total * 0.8;
  }
  return total * 0.9;
}
