CREATE INDEX books_title_idx ON public.books USING btree (title);
CREATE VIEW public.book_titles AS SELECT title FROM public.books;
