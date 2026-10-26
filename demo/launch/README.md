# Northstar Supply launch fixture

A fictional home-goods retailer and wholesale supplier, created for Rowboat launch screenshots. All names, businesses, street addresses and transactions are invented. Emails use the reserved `example.com` domain; addresses use fictional cities and `00000` postal codes.

## Start and connect

From the repository root:

```bash
bash scripts/launch-demo.sh up
bash scripts/launch-demo.sh status
```

| Setting | Value |
| --- | --- |
| Suggested profile name | Northstar Supply |
| Engine | PostgreSQL 18 |
| Host / port | `127.0.0.1` / `15436` |
| Database | `northstar` |
| Username | `rowboat` |
| Fixture-only password | `rowboat-demo` |
| TLS | Disable |
| Container | `rowboat-launch-postgres` |
| Persistent Podman volume | `rowboat-launch-postgres-data` |

Requires rootless Podman and the already-cached `docker.io/library/postgres:18` image. The script never pulls an image. Only loopback TCP port 15436 is published. Container and volume have `io.rowboat.launch=true` and a checkout-path ownership label; mismatched ownership, mounts, images or port bindings are rejected.

## Screenshot data

Launch capture uses [`orders.sql`](orders.sql): a readable October-orders query
above its actual results in every bundled Rowboat theme. Generated PNGs, gallery,
contact sheet and reproduction instructions are in
[`assets/launch/`](../../assets/launch/README.md).

`public.orders` contains exactly **240 deterministic rows** on initial creation, dated **October 6–26, 2026**. Its visible leading columns are `order_number`, `customer`, `email`, `status`, `total`, `currency`, and `placed_at`. Further columns include channel, region, item count, JSON shipping address and nullable notes. `order_number` is the primary key. Amounts on refunded orders are the original order amounts, not net revenue.

Use this query for the curated first rows, or sort the Orders table by `placed_at` descending:

```sql
SELECT order_number, customer, email, status, total, currency, placed_at,
       channel, region, items, shipping_address, notes
FROM public.orders
ORDER BY placed_at DESC, order_number DESC;
```

The **22 tables** make a realistic explorer and relationship diagram:

- Customers: `customers`, `customer_addresses`, `customer_segments`, `customer_segment_memberships`.
- Catalog: `suppliers`, `categories`, `brands`, `products`, `product_variants`.
- Stock and purchasing: `warehouses`, `inventory_levels`, `inventory_movements`, `purchase_orders`, `purchase_order_items`.
- Sales and fulfillment: `orders`, `order_items`, `payments`, `shipments`, `returns`, `return_items`, `discounts`, `order_discounts`.

The other **21 tables are intentionally empty**, with actual keys, foreign keys, indexes and constraints. Orders are independent customer/address snapshots with nullable, unused `customer_id` references. No invented child records or implied reconciled accounting ledger accompany these summary rows.

## Persistence and lifecycle

```bash
bash scripts/launch-demo.sh stop
bash scripts/launch-demo.sh up
```

Stopping retains the volume. Repeated `up` calls preserve all existing rows and demo edits, including intentional deletions. `seed.sql` uses a transaction, an advisory lock, fail-fast SQL, and a database comment marking successful initialization. It skips initialized databases and refuses unrecognized existing public tables rather than replacing them. Removing that marker does not force a reseed over existing objects.

The script has no reset command. To **explicitly erase this demo**, first stop it with the guarded script, then manually remove only these owned resources:

```bash
bash scripts/launch-demo.sh stop
podman rm rowboat-launch-postgres
podman volume rm rowboat-launch-postgres-data
bash scripts/launch-demo.sh up
```

This manual reset permanently deletes demo edits. Ordinary `up`, `stop`, and `status` never delete containers, volumes or rows. The running fixture is intentionally left available for screenshots and local exploration; there is no host service or automatic startup configuration.

Quick verification without installing a host database client:

```bash
podman exec -e PGPASSWORD=rowboat-demo rowboat-launch-postgres \
  psql -X -v ON_ERROR_STOP=1 -h 127.0.0.1 -U rowboat -d northstar \
  -c 'SELECT status, count(*) FROM public.orders GROUP BY status ORDER BY status;'
```

### Verified fixture — 2026-10-26

- PostgreSQL **18.6**; **22 tables**, **27 foreign keys**, **60 indexes** (including primary/unique indexes).
- **240 orders**, **30 fictional customers**, **80 distinct totals**, **180 NULL notes**; all other tables empty.
- Status counts: Delivered **188**, Paid **11**, Processing **9**, Refunded **15**, Shipped **17**.
- Validated column order, reserved email domain, positive totals/item counts, JSON addresses, date bounds, and all FK/check constraints.
- Repeated `up` and `stop` → `up` preserved the table OID, row transaction versions and complete order-data fingerprint. Container left running for captures.
