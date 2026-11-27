CREATE TABLE orders (
    id INTEGER PRIMARY KEY,
    status TEXT NOT NULL DEFAULT 'draft' CHECK (status IN ('draft', 'open'))
);
