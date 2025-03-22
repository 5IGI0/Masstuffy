ALTER TABLE masstuffy_records ADD uri TEXT;

CREATE INDEX masstuffy_records_uri_idx
ON masstuffy_records USING hash (uri);