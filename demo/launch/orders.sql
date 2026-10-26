-- October orders · Northstar Supply
SELECT order_number,
       customer,
       status,
       total, currency,
       region, channel,
       placed_at::date AS ordered_on
FROM public.orders
WHERE placed_at >= DATE '2026-10-01'
ORDER BY placed_at DESC, order_number DESC
LIMIT 100;
