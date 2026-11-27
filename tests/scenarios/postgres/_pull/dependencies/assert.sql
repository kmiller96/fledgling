SELECT pg_get_serial_sequence('public.books', 'id') = 'public.books_id_seq'
   AND EXISTS (
       SELECT FROM pg_sequences
       WHERE schemaname = 'public' AND sequencename = 'job_seq'
         AND start_value = 5 AND increment_by = 2
   );
