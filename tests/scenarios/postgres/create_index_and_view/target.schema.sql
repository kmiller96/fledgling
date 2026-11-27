CREATE TABLE books (id bigint PRIMARY KEY, title text NOT NULL);
CREATE INDEX books_title_idx ON books (title);
CREATE VIEW book_titles AS SELECT title FROM public.books;
