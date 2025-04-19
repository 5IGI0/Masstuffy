# Endpoints

## Getting Records

`/id/:flags/:id` - get a record by its identifier\
`/url/:flags/:date/:url` - get a record by its url and date

### id
record's identifier as defined by `WARC-Record-ID`.

### date
record's `WARC-Date` under `YYYYmmddHHMMSS` format.\
if the date is not present, it will seek to the nearest available one.

### flags

allows you to choose the output format and options, each flag is a character, each character will enable or disable a behavior, refer to the table below.

flags|description
-|-
h|Print WARC headers to the response's body.
d|Force download (will set `Content-Type: application/octet-stream`)
