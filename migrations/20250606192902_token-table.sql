CREATE TABLE masstuffy_tokens (
    token   char(36)    NOT NULL,
    comment TEXT        NOT NULL,
    
    -- *_perms_kind:
    --    0: none
    --    1: any
    --    2: list (collection_1,collection_2,...)
    --    3: prefix
    read_perms_kind     smallint    NOT NULL,
    read_perms          TEXT        NOT NULL,
    write_perms_kind    smallint    NOT NULL,
    write_perms         TEXT        NOT NULL,
    delete_perms_kind   smallint    NOT NULL,
    delete_perms        TEXT        NOT NULL,

    PRIMARY KEY (token)
);
