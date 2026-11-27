CREATE TABLE orders (id INTEGER PRIMARY KEY, status TEXT, reference TEXT);
CREATE TABLE shipments (id INTEGER PRIMARY KEY);
CREATE INDEX orders_by_reference ON orders (reference);
CREATE VIEW open_orders AS SELECT reference FROM orders WHERE status = 'open';
