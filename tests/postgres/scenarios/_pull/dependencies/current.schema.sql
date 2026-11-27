CREATE SEQUENCE job_seq START WITH 5 INCREMENT BY 2;
CREATE TABLE books (id serial PRIMARY KEY);
CREATE TABLE authors (id integer PRIMARY KEY, book_id integer REFERENCES books(id));
CREATE VIEW z_books AS SELECT id FROM books;
CREATE VIEW a_books AS SELECT id FROM z_books;
