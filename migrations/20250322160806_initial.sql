CREATE TABLE masstuffy_records (
    id         bigserial NOT NULL,
    flags      int4      NOT NULL,
    date       timestamp NOT NULL,
    identifier text      NOT NULL,
    collection text      NOT NULL,
    filename   text      NOT NULL,
    "offset"   bigint    NOT NULL,
    "type"     text      NOT NULL
);

CREATE INDEX masstuffy_record_id_idx
    ON masstuffy_records USING hash (identifier);
CREATE UNIQUE INDEX masstuffy_record_id_unq
        ON masstuffy_records(identifier);