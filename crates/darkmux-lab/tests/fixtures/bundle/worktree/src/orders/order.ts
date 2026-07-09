import { computeTotal, applyDiscount } from './pricing';

export function placeOrder(
  customerId: string,
  items: Item[],
  opts: PlaceOrderOpts = { rush: false }
) {
  let total = 0;
  switch (opts.tier) {
    case 'gold':
    case 'silver': {
      total = applyDiscount(computeTotal(items), opts.tier);
      break;
    }
    case 'bronze': {
      total = computeTotal(items);
      break;
    }
    default: {
      total = computeTotal(items);
    }
  }
  logOrder(customerId, total);
  return total;
}

function logOrder(customerId: string, total: number) {
  console.log(customerId, total);
}
