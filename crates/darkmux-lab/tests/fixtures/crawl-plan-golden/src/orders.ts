export function placeOrder(id: string) {
  try {
    submit(id);
  } catch (e) {
    log(e);
  }
  return id;
}

export function cancelOrder(id: string) {
  fetchOrder(id).catch(err => {
    log(err);
  });
  return id;
}
