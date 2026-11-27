CREATE TABLE shipments (id INTEGER PRIMARY KEY);
ALTER TABLE "orders" ADD COLUMN "reference" TEXT;
CREATE INDEX orders_by_reference ON orders (reference);
CREATE VIEW open_orders AS SELECT reference FROM orders WHERE status = 'open';
