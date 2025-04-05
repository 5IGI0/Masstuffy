# File Layout

_Please note this is subject to change._

## Layout

```
├── config.json
└── data
    ├── buffer
    │   └── [slug].[id]
    │       ├── manifest.json
    │       └── records
    │           ├── 1
    │           ├── 2
    │           ├── 3
    │           └── ...
    ├── dict
    │   └── [compress_method]
    │       └── [slug](.[id]).[ext]
    └── repository
        └── [collection_uuid]
            ├── records.[part](.[dict_id]).warc(.[compression_method])
            ├── index.cdx.gz # when .cdx is enough large, flush inside
            ├── index.cdx
            └── manifest.json
```

