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
    │       ├── [slug](.[id]).[ext]
    │       └── [slug](.[id]).json
    └── repository
        ├── [slug].[part](.[dict_id]).warc(.[compression_method])
        ├── [slug].cdx.gz # when .cdx is enough large, flush inside
        ├── [slug].cdx
        └── [slug].json
```

