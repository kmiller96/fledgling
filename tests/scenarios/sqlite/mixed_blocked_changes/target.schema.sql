CREATE TABLE orders (id INTEGER PRIMARY KEY, reference INTEGER);
CREATE VIEW order_references AS SELECT reference FROM orders;
