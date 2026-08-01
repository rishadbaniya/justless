import { formatOrder } from './format.js';

export async function fetchOrders() {
  const response = await fetch('/api/orders');
  const orders = await response.json() as string[];
  return orders.map(formatOrder);
}
