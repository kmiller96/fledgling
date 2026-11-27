CREATE TABLE must_not_commit (id integer);
CREATE INDEX CONCURRENTLY books_id_idx ON books (id);
