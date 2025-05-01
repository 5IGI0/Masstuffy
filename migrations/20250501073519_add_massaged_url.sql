ALTER TABLE masstuffy_records ADD massaged_url TEXT NOT NULL;

CREATE INDEX masstuffy_record_massaged_urls_idx
    ON masstuffy_records USING btree (massaged_url);