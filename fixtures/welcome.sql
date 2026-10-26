-- Rowboat / local Podman database
-- This query runs against real, disposable data.
SELECT
    o.id,
    c.name AS customer,
    c.city,
    o.status,
    o.total,
    o.note
FROM orders o
JOIN customers c ON c.id = o.customer_id
ORDER BY o.id;
