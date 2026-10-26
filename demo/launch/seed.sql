\set ON_ERROR_STOP on
BEGIN;
SET LOCAL lock_timeout = '15s';
SET LOCAL search_path = public, pg_catalog;
SELECT pg_advisory_xact_lock(1765846882, 1);

DO $$
BEGIN
    IF current_database() <> 'northstar' OR current_user <> 'rowboat' THEN
        RAISE EXCEPTION 'Seed only the owned northstar launch fixture as rowboat';
    END IF;
END
$$;

SELECT coalesce(shobj_description(oid, 'pg_database') =
    'Northstar Supply — fictional Rowboat launch fixture v1', false) AS already_seeded
FROM pg_database WHERE datname = current_database() \gset

\if :already_seeded
    \echo 'Northstar already initialized; existing data and edits preserved.'
\else
    DO $$
    BEGIN
        IF EXISTS (
            SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
            WHERE n.nspname = 'public' AND c.relkind IN ('r', 'p', 'v', 'm', 'S', 'f')
        ) THEN
            RAISE EXCEPTION 'Unrecognized populated database; refusing to seed over existing objects';
        END IF;
    END
    $$;

    CREATE TABLE public.customers (
        id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
        name text NOT NULL,
        email text NOT NULL UNIQUE,
        customer_type text NOT NULL CHECK (customer_type IN ('Retail', 'Wholesale')),
        company_name text,
        payment_terms_days integer NOT NULL DEFAULT 0 CHECK (payment_terms_days >= 0),
        created_at timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP
    );
    CREATE TABLE public.customer_addresses (
        id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
        customer_id bigint NOT NULL REFERENCES public.customers(id),
        label text NOT NULL,
        line1 text NOT NULL,
        line2 text,
        city text NOT NULL,
        region text NOT NULL,
        postal_code text NOT NULL,
        country char(2) NOT NULL DEFAULT 'US',
        is_default boolean NOT NULL DEFAULT false
    );
    CREATE INDEX customer_addresses_customer_idx ON public.customer_addresses(customer_id);
    CREATE TABLE public.customer_segments (
        id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
        name text NOT NULL UNIQUE,
        description text,
        created_at timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP
    );
    CREATE TABLE public.customer_segment_memberships (
        customer_id bigint NOT NULL REFERENCES public.customers(id),
        segment_id bigint NOT NULL REFERENCES public.customer_segments(id),
        joined_at timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
        PRIMARY KEY (customer_id, segment_id)
    );
    CREATE INDEX customer_segment_memberships_segment_idx ON public.customer_segment_memberships(segment_id);
    CREATE TABLE public.suppliers (
        id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
        name text NOT NULL UNIQUE,
        contact_name text,
        email text,
        lead_time_days integer NOT NULL DEFAULT 14 CHECK (lead_time_days >= 0),
        active boolean NOT NULL DEFAULT true
    );
    CREATE TABLE public.categories (
        id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
        name text NOT NULL,
        slug text NOT NULL UNIQUE,
        parent_id bigint REFERENCES public.categories(id)
    );
    CREATE INDEX categories_parent_idx ON public.categories(parent_id);
    CREATE TABLE public.brands (
        id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
        name text NOT NULL UNIQUE,
        description text,
        active boolean NOT NULL DEFAULT true
    );
    CREATE TABLE public.products (
        id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
        sku text NOT NULL UNIQUE,
        name text NOT NULL,
        category_id bigint REFERENCES public.categories(id),
        brand_id bigint REFERENCES public.brands(id),
        supplier_id bigint REFERENCES public.suppliers(id),
        unit_price numeric(10,2) NOT NULL CHECK (unit_price >= 0),
        cost_price numeric(10,2) CHECK (cost_price >= 0),
        description text,
        active boolean NOT NULL DEFAULT true
    );
    CREATE INDEX products_category_idx ON public.products(category_id);
    CREATE INDEX products_brand_idx ON public.products(brand_id);
    CREATE INDEX products_supplier_idx ON public.products(supplier_id);
    CREATE TABLE public.product_variants (
        id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
        product_id bigint NOT NULL REFERENCES public.products(id),
        sku text NOT NULL UNIQUE,
        attributes jsonb NOT NULL DEFAULT '{}',
        price_adjustment numeric(10,2) NOT NULL DEFAULT 0,
        weight_grams integer CHECK (weight_grams > 0)
    );
    CREATE INDEX product_variants_product_idx ON public.product_variants(product_id);
    CREATE TABLE public.warehouses (
        id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
        code text NOT NULL UNIQUE,
        name text NOT NULL,
        address jsonb NOT NULL,
        timezone text NOT NULL DEFAULT 'America/New_York',
        active boolean NOT NULL DEFAULT true
    );
    CREATE TABLE public.inventory_levels (
        variant_id bigint NOT NULL REFERENCES public.product_variants(id),
        warehouse_id bigint NOT NULL REFERENCES public.warehouses(id),
        on_hand integer NOT NULL DEFAULT 0 CHECK (on_hand >= 0),
        reserved integer NOT NULL DEFAULT 0 CHECK (reserved >= 0 AND reserved <= on_hand),
        reorder_point integer NOT NULL DEFAULT 10 CHECK (reorder_point >= 0),
        updated_at timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
        PRIMARY KEY (variant_id, warehouse_id)
    );
    CREATE INDEX inventory_levels_warehouse_idx ON public.inventory_levels(warehouse_id);
    CREATE TABLE public.inventory_movements (
        id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
        variant_id bigint NOT NULL REFERENCES public.product_variants(id),
        warehouse_id bigint NOT NULL REFERENCES public.warehouses(id),
        movement_type text NOT NULL CHECK (movement_type IN ('Receipt', 'Sale', 'Return', 'Adjustment', 'Transfer')),
        quantity integer NOT NULL CHECK (quantity <> 0),
        reference text,
        occurred_at timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP
    );
    CREATE INDEX inventory_movements_variant_time_idx ON public.inventory_movements(variant_id, occurred_at DESC);
    CREATE INDEX inventory_movements_warehouse_idx ON public.inventory_movements(warehouse_id);
    CREATE TABLE public.purchase_orders (
        id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
        po_number text NOT NULL UNIQUE,
        supplier_id bigint NOT NULL REFERENCES public.suppliers(id),
        warehouse_id bigint NOT NULL REFERENCES public.warehouses(id),
        status text NOT NULL CHECK (status IN ('Draft', 'Ordered', 'Partially received', 'Received', 'Cancelled')),
        ordered_at timestamp,
        expected_at date,
        total numeric(12,2) NOT NULL DEFAULT 0 CHECK (total >= 0)
    );
    CREATE INDEX purchase_orders_supplier_idx ON public.purchase_orders(supplier_id);
    CREATE INDEX purchase_orders_warehouse_idx ON public.purchase_orders(warehouse_id);
    CREATE TABLE public.purchase_order_items (
        purchase_order_id bigint NOT NULL REFERENCES public.purchase_orders(id),
        line_number integer NOT NULL CHECK (line_number > 0),
        variant_id bigint NOT NULL REFERENCES public.product_variants(id),
        quantity_ordered integer NOT NULL CHECK (quantity_ordered > 0),
        quantity_received integer NOT NULL DEFAULT 0 CHECK (quantity_received >= 0 AND quantity_received <= quantity_ordered),
        unit_cost numeric(10,2) NOT NULL CHECK (unit_cost >= 0),
        PRIMARY KEY (purchase_order_id, line_number)
    );
    CREATE INDEX purchase_order_items_variant_idx ON public.purchase_order_items(variant_id);

    -- These first seven columns are the screenshot-friendly order summary.
    -- Customer/address snapshots survive account deletion or later profile edits.
    CREATE TABLE public.orders (
        order_number text PRIMARY KEY CHECK (order_number ~ '^NS-[0-9]{5}$'),
        customer text NOT NULL,
        email text NOT NULL,
        status text NOT NULL CHECK (status IN ('Paid', 'Processing', 'Shipped', 'Delivered', 'Refunded')),
        total numeric(10,2) NOT NULL CHECK (total >= 0),
        currency char(3) NOT NULL DEFAULT 'USD',
        placed_at timestamp NOT NULL,
        channel text NOT NULL CHECK (channel IN ('Online store', 'Wholesale', 'Marketplace')),
        region text NOT NULL,
        items integer NOT NULL CHECK (items > 0),
        shipping_address jsonb NOT NULL,
        notes text,
        customer_id bigint REFERENCES public.customers(id)
    );
    COMMENT ON TABLE public.orders IS 'Fictional Northstar Supply order snapshots for Rowboat launch screenshots. No real customers or transactions.';
    COMMENT ON COLUMN public.orders.total IS 'Original order amount, including for Refunded orders; not net revenue.';
    CREATE INDEX orders_placed_at_idx ON public.orders(placed_at DESC);
    CREATE INDEX orders_status_placed_at_idx ON public.orders(status, placed_at DESC);
    CREATE INDEX orders_customer_idx ON public.orders(customer);
    CREATE INDEX orders_email_idx ON public.orders(email);
    CREATE INDEX orders_customer_id_idx ON public.orders(customer_id) WHERE customer_id IS NOT NULL;
    CREATE TABLE public.order_items (
        order_number text NOT NULL REFERENCES public.orders(order_number),
        line_number integer NOT NULL CHECK (line_number > 0),
        variant_id bigint REFERENCES public.product_variants(id),
        product_name text NOT NULL,
        sku text NOT NULL,
        quantity integer NOT NULL CHECK (quantity > 0),
        unit_price numeric(10,2) NOT NULL CHECK (unit_price >= 0),
        discount_amount numeric(10,2) NOT NULL DEFAULT 0 CHECK (discount_amount >= 0),
        PRIMARY KEY (order_number, line_number)
    );
    CREATE INDEX order_items_variant_idx ON public.order_items(variant_id);
    CREATE TABLE public.payments (
        id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
        order_number text NOT NULL REFERENCES public.orders(order_number),
        method text NOT NULL CHECK (method IN ('Card', 'Bank transfer', 'Account credit')),
        status text NOT NULL CHECK (status IN ('Pending', 'Captured', 'Failed', 'Refunded')),
        amount numeric(10,2) NOT NULL CHECK (amount > 0),
        currency char(3) NOT NULL DEFAULT 'USD',
        processor_reference text UNIQUE,
        processed_at timestamp
    );
    CREATE INDEX payments_order_idx ON public.payments(order_number);
    CREATE TABLE public.shipments (
        id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
        order_number text NOT NULL REFERENCES public.orders(order_number),
        warehouse_id bigint REFERENCES public.warehouses(id),
        carrier text NOT NULL,
        service text NOT NULL,
        tracking_number text UNIQUE,
        shipped_at timestamp,
        delivered_at timestamp,
        shipping_cost numeric(10,2) CHECK (shipping_cost >= 0)
    );
    CREATE INDEX shipments_order_idx ON public.shipments(order_number);
    CREATE INDEX shipments_warehouse_idx ON public.shipments(warehouse_id);
    CREATE TABLE public.returns (
        id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
        return_number text NOT NULL UNIQUE,
        order_number text NOT NULL REFERENCES public.orders(order_number),
        reason text NOT NULL,
        status text NOT NULL CHECK (status IN ('Requested', 'Approved', 'Received', 'Refunded', 'Declined')),
        requested_at timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,
        resolved_at timestamp,
        refund_total numeric(10,2) CHECK (refund_total >= 0)
    );
    CREATE INDEX returns_order_idx ON public.returns(order_number);
    CREATE TABLE public.return_items (
        return_id bigint NOT NULL REFERENCES public.returns(id),
        line_number integer NOT NULL CHECK (line_number > 0),
        order_number text NOT NULL,
        order_line_number integer NOT NULL,
        quantity integer NOT NULL CHECK (quantity > 0),
        condition text NOT NULL CHECK (condition IN ('Unopened', 'Resalable', 'Damaged')),
        restock boolean NOT NULL DEFAULT false,
        PRIMARY KEY (return_id, line_number),
        FOREIGN KEY (order_number, order_line_number) REFERENCES public.order_items(order_number, line_number)
    );
    CREATE INDEX return_items_order_line_idx ON public.return_items(order_number, order_line_number);
    CREATE TABLE public.discounts (
        id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
        code text NOT NULL UNIQUE,
        name text NOT NULL,
        discount_type text NOT NULL CHECK (discount_type IN ('Percentage', 'Fixed amount')),
        value numeric(10,2) NOT NULL CHECK (value > 0),
        starts_at timestamp,
        ends_at timestamp,
        active boolean NOT NULL DEFAULT true,
        CHECK (ends_at IS NULL OR starts_at IS NULL OR ends_at > starts_at)
    );
    CREATE TABLE public.order_discounts (
        order_number text NOT NULL REFERENCES public.orders(order_number),
        discount_id bigint NOT NULL REFERENCES public.discounts(id),
        amount numeric(10,2) NOT NULL CHECK (amount > 0),
        PRIMARY KEY (order_number, discount_id)
    );
    CREATE INDEX order_discounts_discount_idx ON public.order_discounts(discount_id);

    -- Entirely invented customer names and addresses; every email uses example.com.
    -- Reference tables intentionally remain empty: orders are standalone snapshots.
    WITH customers(id, customer, email, region, city, street) AS (
        VALUES
        (0, 'Cedarbrook Mercantile', 'purchasing.cedarbrook@example.com', 'Northeast', 'Cedarhaven', '18 Lantern Quay'),
        (1, 'Avery Ellis', 'avery.ellis@example.com', 'West', 'Juniper Bay', '42 Copperleaf Lane'),
        (2, 'Juniper Workshop', 'orders.juniper@example.com', 'Midwest', 'Alder Falls', '6 Millstone Arcade'),
        (3, 'Harborlight Provisioners', 'hello.harborlight@example.com', 'Northeast', 'Cedarhaven', '27 Beacon Courtyard'),
        (4, 'Morgan Reed', 'morgan.reed@example.com', 'Southeast', 'East Marlow', '15 Orchard Crescent'),
        (5, 'Elm & Ember Studio', 'studio.elmandember@example.com', 'West', 'Juniper Bay', '9 Emberworks Mews'),
        (6, 'Copper Finch Cafe', 'supply.copperfinch@example.com', 'Midwest', 'Alder Falls', '31 Copper Finch Walk'),
        (7, 'Quinn Harper', 'quinn.harper@example.com', 'Southwest', 'Mesa Willow', '58 Moonflower Terrace'),
        (8, 'Willowbank Outfitters', 'buying.willowbank@example.com', 'West', 'Lakehaven', '4 Willowbank Passage'),
        (9, 'Orchard Row Market', 'orders.orchardrow@example.com', 'Southeast', 'East Marlow', '22 Orchard Row'),
        (10, 'Solstice Home Goods', 'team.solsticehome@example.com', 'Southwest', 'Mesa Willow', '12 Sunstone Arcade'),
        (11, 'Foxglove Paper Co.', 'wholesale.foxglove@example.com', 'Northeast', 'Cedarhaven', '36 Foxglove Yard'),
        (12, 'Birch & Beam', 'orders.birchandbeam@example.com', 'Midwest', 'Alder Falls', '8 Birchworks Lane'),
        (13, 'Tern & Timber', 'hello.ternandtimber@example.com', 'West', 'Juniper Bay', '64 Tern Landing'),
        (14, 'Emery Wells', 'emery.wells@example.com', 'Northeast', 'Cedarhaven', '17 Meadowglass Close'),
        (15, 'Rowan Vale', 'rowan.vale@example.com', 'Midwest', 'Alder Falls', '29 Alderstitch Road'),
        (16, 'Mira Lane', 'mira.lane@example.com', 'Southeast', 'East Marlow', '11 Marlow Garden Walk'),
        (17, 'Theo Ashford', 'theo.ashford@example.com', 'West', 'Lakehaven', '53 Fernwell Crescent'),
        (18, 'Lena Briar', 'lena.briar@example.com', 'Southwest', 'Mesa Willow', '7 Desertbell Court'),
        (19, 'Aster & Anvil', 'orders.asterandanvil@example.com', 'Midwest', 'Alder Falls', '25 Anvil House Row'),
        (20, 'Quiet Harbor Books', 'shop.quietharbor@example.com', 'Northeast', 'Cedarhaven', '3 Lantern Library Walk'),
        (21, 'Maple Thread Collective', 'buying.maplethread@example.com', 'West', 'Lakehaven', '46 Maple Thread Mews'),
        (22, 'Prairie Lantern', 'supply.prairielantern@example.com', 'Midwest', 'Alder Falls', '19 Prairie Lantern Row'),
        (23, 'Sage & Stone Pantry', 'orders.sageandstone@example.com', 'Southwest', 'Mesa Willow', '14 Sagehouse Arcade'),
        (24, 'Wrenfield Goods', 'team.wrenfield@example.com', 'Southeast', 'East Marlow', '38 Wrenfield Crossing'),
        (25, 'Jules Linden', 'jules.linden@example.com', 'West', 'Juniper Bay', '21 Linden Loom Lane'),
        (26, 'Arden Moss', 'arden.moss@example.com', 'Northeast', 'Cedarhaven', '44 Mosslight Terrace'),
        (27, 'Fern & Foundry', 'studio.fernfoundry@example.com', 'Midwest', 'Alder Falls', '16 Fern Foundry Yard'),
        (28, 'October House', 'hello.octoberhouse@example.com', 'Southeast', 'East Marlow', '5 October House Walk'),
        (29, 'Blue Spruce General', 'orders.bluespruce@example.com', 'West', 'Lakehaven', '33 Spruceglass Landing')
    ), featured(n, customer_id, status, total, items, channel, placed_at, notes) AS (
        VALUES
        (0, 0, 'Paid', 386.40::numeric, 8, 'Wholesale', timestamp '2026-10-26 10:42:00', 'Autumn window display restock'),
        (1, 1, 'Processing', 89.95, 3, 'Online store', timestamp '2026-10-26 10:18:00', 'Gift wrap requested'),
        (2, 2, 'Shipped', 1248.00, 24, 'Wholesale', timestamp '2026-10-25 16:52:00', 'Ship complete; no split cartons'),
        (3, 3, 'Delivered', 642.75, 15, 'Wholesale', timestamp '2026-10-25 09:34:00', 'Delivered to receiving desk'),
        (4, 4, 'Refunded', 74.50, 2, 'Online store', timestamp '2026-10-24 15:12:00', 'Returned unopened; refund issued'),
        (5, 5, 'Paid', 2156.80, 40, 'Wholesale', timestamp '2026-10-24 11:08:00', 'Opening order for the winter collection'),
        (6, 6, 'Delivered', 328.20, 12, 'Wholesale', timestamp '2026-10-23 16:24:00', NULL),
        (7, 7, 'Shipped', 156.35, 4, 'Online store', timestamp '2026-10-23 13:47:00', NULL),
        (8, 8, 'Processing', 1849.60, 32, 'Wholesale', timestamp '2026-10-22 14:06:00', 'Awaiting the final carton from the warehouse'),
        (9, 9, 'Delivered', 517.25, 18, 'Wholesale', timestamp '2026-10-22 09:21:00', 'Monthly essentials replenishment'),
        (10, 10, 'Paid', 798.00, 20, 'Wholesale', timestamp '2026-10-21 18:14:00', NULL),
        (11, 11, 'Delivered', 264.90, 10, 'Wholesale', timestamp '2026-10-21 17:03:00', 'Paper goods for the fall workshop')
    ), generated AS (
        SELECT n,
            coalesce(f.customer_id, (n * 7 + n / 11) % 30) AS customer_id,
            coalesce(f.status, CASE
                WHEN n % 17 = 0 THEN 'Refunded'
                WHEN n < 60 AND n % 7 = 0 THEN 'Processing'
                WHEN n < 60 AND n % 5 = 0 THEN 'Paid'
                WHEN n < 90 AND n % 4 = 0 THEN 'Shipped'
                ELSE 'Delivered' END) AS status,
            coalesce(f.items, CASE WHEN n % 3 = 0 THEN 12 + (n * 7 % 9) * 6 ELSE 1 + n * 5 % 7 END) AS items,
            coalesce(f.channel, CASE WHEN n % 3 = 0 THEN 'Wholesale' WHEN n % 7 = 0 THEN 'Marketplace' ELSE 'Online store' END) AS channel,
            coalesce(f.placed_at, timestamp '2026-10-21 16:30:00' - (n - 12) * interval '95 minutes') AS placed_at,
            f.total AS featured_total,
            CASE WHEN n < 12 THEN f.notes
                WHEN n % 17 = 0 THEN 'Refund completed after return inspection'
                WHEN n % 19 = 0 THEN 'Leave cartons with the receiving team'
                WHEN n % 23 = 0 THEN 'Repeat order; use the previous packing layout'
                WHEN n % 29 = 0 THEN 'Consolidate packaging where possible'
                WHEN n % 31 = 0 THEN 'Gift message included in the packing slip'
                ELSE NULL END AS notes
        FROM generate_series(0, 239) AS series(n)
        LEFT JOIN featured f USING (n)
    )
    INSERT INTO public.orders (
        order_number, customer, email, status, total, currency, placed_at,
        channel, region, items, shipping_address, notes
    )
    SELECT 'NS-' || (10482 - g.n), c.customer, c.email, g.status,
        coalesce(g.featured_total, round(
            g.items * (ARRAY[19.95, 24.50, 38.00, 46.75, 12.95, 64.00, 89.50, 15.00, 27.95, 52.00, 9.75, 78.00]::numeric[])[1 + g.n * 5 % 12]
            * CASE WHEN g.channel = 'Wholesale' THEN 0.85 ELSE 1 END
            * 1.0725 + CASE WHEN g.items < 5 THEN 8.50 ELSE 0 END, 2)),
        'USD', g.placed_at, g.channel, c.region, g.items,
        jsonb_build_object('line1', c.street, 'city', c.city, 'region', c.region,
            'postal_code', '00000', 'country', 'US'),
        g.notes
    FROM generated g JOIN customers c ON c.id = g.customer_id
    ORDER BY g.n;

    DO $$
    BEGIN
        IF (SELECT count(*) FROM public.orders) <> 240 THEN
            RAISE EXCEPTION 'Expected exactly 240 showcase orders';
        END IF;
        IF (SELECT count(*) FROM information_schema.tables
            WHERE table_schema = 'public' AND table_type = 'BASE TABLE') <> 22 THEN
            RAISE EXCEPTION 'Expected exactly 22 demo tables';
        END IF;
    END
    $$;
    COMMENT ON DATABASE northstar IS 'Northstar Supply — fictional Rowboat launch fixture v1';
    ANALYZE public.orders;
    \echo 'Initialized Northstar Supply: 22 tables, 240 fictional orders.'
\endif
COMMIT;
