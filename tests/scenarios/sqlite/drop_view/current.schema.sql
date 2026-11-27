CREATE TABLE orders (id INTEGER PRIMARY KEY, status TEXT);
CREATE VIEW open_orders AS SELECT id FROM orders WHERE status = 'open';
