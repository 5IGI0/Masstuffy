# Masstuffy

Masstuffy is an object-storage server that utilizes WARC files.

## Current Status

At the moment, **Masstuffy IS NOT FUNCTIONAL** and is far from being complete. I am developing it to learn Rust and because I couldn't find any self-hosted object storage solutions that meet my criteria:

- Standalone
- Doesn't waste inodes with millions of files
- Optimized to run on a single machine
- Compresses small objects efficiently

After exploring WebArchive, I realized that the WARC file format perfectly suits my needs. It is seekable, compressed, can contain metadata, and is easily extensible.

## Details

### Repository and Collections

- **Repository**: This is where collections are stored.
- **Collections**: A collection is a set of records (or objects), which are essentially WARC files.

### Creating and Managing Collections

Before storing records, you need to create a collection. Depending on what you plan to store, you can enable compression (and use a dictionary if necessary) and then insert all the objects you want.

Additionally, there will be an option to generate the dictionary after the collection has been created. In this case, objects will be stored without compression in a temporary folder. Once a certain threshold is reached, the dictionary will be generated.

### License

Masstuffy is licensed under the Affero General Public License (AGPL).\
This means you are free to use, modify, and distribute the software,\
provided that any modifications are also released under the same license\
and that any network services built using Masstuffy also make their source code available.\
For more details, please refer to the LICENSE file.

## TODO

- [ ] database
  - [X] read and load cdx files to db
  - [ ] mark for deletion
  - [X] search
    - [X] by id
    - [X] by url
- collections
  - [X] create
  - [X] load
  - [x] generate cdx files
  - [ ] regenerate cdx files
  - [X] read
  - [X] append
  - [ ] fs atomicity
  - [ ] compression
    - [X] compress
    - [X] dictionnary
    - [ ] regenerate cdx files
    - [X] dictionnary generation
    - [ ] collection rebuild
  - [X] make async
- cli
  - [x] setup file layout
  - [ ] create collection
    - [X] create
    - [ ] custom dictionnary
  - [X] add records
  - [X] get record
  - [ ] search records
  - [ ] detect when the server runs and send commands to it
- server
  - [X] link to source code (AGPL requirement)
  - [ ] create collection
  - [ ] add records
  - [ ] search records
  - [ ] report records
  - [ ] offload decompression (client-side decompression)
  - [X] get record(s)
    - [X] by id
    - [X] by url