CREATE VIEW open_orders AS SELECT id FROM orders WHERE status = 'open';
