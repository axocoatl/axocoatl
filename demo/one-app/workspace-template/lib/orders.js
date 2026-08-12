/** Round a dollar amount to cents. */
export function roundCents(amount) {
  return Math.round((amount + Number.EPSILON) * 100) / 100;
}

/** Sum quantity × unit price for an order. */
export function orderSubtotal(order) {
  return roundCents(
    order.items.reduce(
      (sum, item) => sum + item.quantity * item.unitPrice,
      0,
    ),
  );
}

/**
 * Apply an optional percentage or fixed discount.
 *
 * The seeded implementation contains the one regression used in the demo.
 */
export function applyDiscount(subtotal, discount) {
  if (!discount) return roundCents(subtotal);
  const off = discount.kind === "percent"
    ? subtotal * (discount.value / 100)
    : discount.value;
  return roundCents(subtotal - off);
}

/** Subtotal, discount, then tax. */
export function orderTotal(order, taxRate = 0) {
  const discounted = applyDiscount(orderSubtotal(order), order.discount);
  return roundCents(discounted * (1 + taxRate));
}
