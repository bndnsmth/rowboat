CREATE TABLE IF NOT EXISTS customers (
    id INTEGER PRIMARY KEY,
    name VARCHAR(120) NOT NULL,
    email VARCHAR(180) NOT NULL UNIQUE,
    city VARCHAR(80),
    plan VARCHAR(32) NOT NULL DEFAULT 'starter',
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS products (
    id INTEGER PRIMARY KEY,
    sku VARCHAR(32) NOT NULL UNIQUE,
    name VARCHAR(120) NOT NULL,
    price DECIMAL(12,2) NOT NULL,
    stock INTEGER NOT NULL DEFAULT 0,
    active BOOLEAN NOT NULL DEFAULT TRUE
);

CREATE TABLE IF NOT EXISTS orders (
    id INTEGER PRIMARY KEY,
    customer_id INTEGER NOT NULL,
    status VARCHAR(24) NOT NULL DEFAULT 'pending',
    total DECIMAL(12,2) NOT NULL,
    note TEXT,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT orders_customer_fk FOREIGN KEY (customer_id) REFERENCES customers(id)
);

CREATE TABLE IF NOT EXISTS order_items (
    order_id INTEGER NOT NULL,
    product_id INTEGER NOT NULL,
    quantity INTEGER NOT NULL,
    unit_price DECIMAL(12,2) NOT NULL,
    PRIMARY KEY (order_id, product_id),
    CONSTRAINT order_items_order_fk FOREIGN KEY (order_id) REFERENCES orders(id),
    CONSTRAINT order_items_product_fk FOREIGN KEY (product_id) REFERENCES products(id)
);

INSERT INTO customers (id,name,email,city,plan)
SELECT 1,'Avery Chen','avery@example.test','Portland','pro' WHERE NOT EXISTS (SELECT 1 FROM customers WHERE id=1);
INSERT INTO customers (id,name,email,city,plan)
SELECT 2,'Jordan Ellis','jordan@example.test','Austin','team' WHERE NOT EXISTS (SELECT 1 FROM customers WHERE id=2);
INSERT INTO customers (id,name,email,city,plan)
SELECT 3,'Morgan Reed','morgan@example.test','Seattle','starter' WHERE NOT EXISTS (SELECT 1 FROM customers WHERE id=3);
INSERT INTO customers (id,name,email,city,plan)
SELECT 4,'Sam Okafor','sam@example.test','Chicago','pro' WHERE NOT EXISTS (SELECT 1 FROM customers WHERE id=4);
INSERT INTO customers (id,name,email,city,plan)
SELECT 5,'Riley Park','riley@example.test','Denver','team' WHERE NOT EXISTS (SELECT 1 FROM customers WHERE id=5);
INSERT INTO products (id,sku,name,price,stock)
SELECT 1,'KEY-001','Mechanical keyboard',149.00,84 WHERE NOT EXISTS (SELECT 1 FROM products WHERE id=1);
INSERT INTO products (id,sku,name,price,stock)
SELECT 2,'DESK-002','Desk mat',35.00,212 WHERE NOT EXISTS (SELECT 1 FROM products WHERE id=2);
INSERT INTO products (id,sku,name,price,stock)
SELECT 3,'HUB-003','USB-C hub',79.00,46 WHERE NOT EXISTS (SELECT 1 FROM products WHERE id=3);
INSERT INTO products (id,sku,name,price,stock)
SELECT 4,'ARM-004','Monitor arm',119.00,28 WHERE NOT EXISTS (SELECT 1 FROM products WHERE id=4);
INSERT INTO orders (id,customer_id,status,total,note)
SELECT 1001,1,'shipped',184.00,'Leave at front desk' WHERE NOT EXISTS (SELECT 1 FROM orders WHERE id=1001);
INSERT INTO orders (id,customer_id,status,total,note)
SELECT 1002,2,'processing',307.00,NULL WHERE NOT EXISTS (SELECT 1 FROM orders WHERE id=1002);
INSERT INTO orders (id,customer_id,status,total,note)
SELECT 1003,4,'pending',119.00,'Gift wrap requested' WHERE NOT EXISTS (SELECT 1 FROM orders WHERE id=1003);
INSERT INTO order_items (order_id,product_id,quantity,unit_price)
SELECT 1001,1,1,149.00 WHERE NOT EXISTS (SELECT 1 FROM order_items WHERE order_id=1001 AND product_id=1);
INSERT INTO order_items (order_id,product_id,quantity,unit_price)
SELECT 1001,2,1,35.00 WHERE NOT EXISTS (SELECT 1 FROM order_items WHERE order_id=1001 AND product_id=2);
INSERT INTO order_items (order_id,product_id,quantity,unit_price)
SELECT 1002,1,1,149.00 WHERE NOT EXISTS (SELECT 1 FROM order_items WHERE order_id=1002 AND product_id=1);
INSERT INTO order_items (order_id,product_id,quantity,unit_price)
SELECT 1002,3,2,79.00 WHERE NOT EXISTS (SELECT 1 FROM order_items WHERE order_id=1002 AND product_id=3);
INSERT INTO order_items (order_id,product_id,quantity,unit_price)
SELECT 1003,4,1,119.00 WHERE NOT EXISTS (SELECT 1 FROM order_items WHERE order_id=1003 AND product_id=4);

CREATE OR REPLACE VIEW order_summary AS
SELECT o.id, c.name AS customer, c.city, o.status, o.total, o.created_at
FROM orders o JOIN customers c ON c.id = o.customer_id;
