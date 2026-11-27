CREATE TABLE orders (id INTEGER PRIMARY KEY, reference TEXT);
CREATE INDEX orders_by_reference ON orders (reference);
