//! Table definitions, meta keys, and open/init/verify of the database file.

use std::path::Path;

use redb::{ReadableDatabase, TableDefinition};

use crate::error::{OpenError, open_database, open_redb};
use crate::{Options, SCHEMA_VERSION};

/// `meta`: `schema_version`, `next_slot`, `stored_entries`.
pub(crate) const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
/// Collection key (`u128`, numeric order) -> `CollRecord`.
pub(crate) const COLLECTIONS: TableDefinition<u128, &[u8]> = TableDefinition::new("collections");
/// Slot -> `collection ‖ generation`.
pub(crate) const SLOTS: TableDefinition<u64, &[u8]> = TableDefinition::new("slots");
/// Dead slots whose rows are not purged yet.
pub(crate) const DEAD_SLOTS: TableDefinition<u64, ()> = TableDefinition::new("dead_slots");
/// `slot ‖ position` -> entry value.
pub(crate) const ENTRIES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("entries");
/// `record_id ‖ 0x00 ‖ slot ‖ position` -> date (16 bytes).
pub(crate) const BY_ID: TableDefinition<&[u8], &[u8]> = TableDefinition::new("by_id");
/// `slot ‖ url part ‖ date ‖ position` -> flags.
pub(crate) const URLS: TableDefinition<&[u8], u8> = TableDefinition::new("urls");

/// Meta key: schema version.
pub(crate) const KEY_SCHEMA: &str = "schema_version";
/// Meta key: next free slot id (slot 0 is never used).
pub(crate) const KEY_NEXT_SLOT: &str = "next_slot";
/// Meta key: number of rows in `ENTRIES`.
pub(crate) const KEY_STORED: &str = "stored_entries";

fn need<K: redb::Key + 'static, V: redb::Value + 'static>(
    rt: &redb::ReadTransaction,
    def: TableDefinition<K, V>,
) -> Result<(), OpenError> {
    match rt.open_table(def) {
        Ok(_) => Ok(()),
        Err(redb::TableError::TableDoesNotExist(n)) => {
            Err(OpenError::Corrupt(format!("missing table {n}")))
        }
        Err(e) => Err(open_redb(e)),
    }
}

/// Opens (creating if needed) the database file and checks or initialises the schema.
pub(crate) fn open_db(path: &Path, o: &Options) -> Result<redb::Database, OpenError> {
    let mut b = redb::Builder::new();
    b.set_cache_size(o.cache_bytes);
    b.set_check_integrity_on_open(o.verify_on_open);
    let db = b.create(path).map_err(open_database)?;

    let init = {
        let rt = db.begin_read().map_err(open_redb)?;
        match rt.open_table(META) {
            Ok(meta) => {
                let version = meta
                    .get(KEY_SCHEMA)
                    .map_err(open_redb)?
                    .map(|g| g.value())
                    .ok_or_else(|| OpenError::Corrupt("meta has no schema_version".into()))?;
                if version != SCHEMA_VERSION {
                    return Err(OpenError::SchemaMismatch {
                        found: version,
                        expected: SCHEMA_VERSION,
                    });
                }
                need(&rt, COLLECTIONS)?;
                need(&rt, SLOTS)?;
                need(&rt, DEAD_SLOTS)?;
                need(&rt, ENTRIES)?;
                need(&rt, BY_ID)?;
                need(&rt, URLS)?;
                for key in [KEY_NEXT_SLOT, KEY_STORED] {
                    if meta.get(key).map_err(open_redb)?.is_none() {
                        return Err(OpenError::Corrupt(format!("meta has no {key}")));
                    }
                }
                false
            }
            Err(redb::TableError::TableDoesNotExist(_)) => {
                if rt.list_tables().map_err(open_redb)?.count() != 0
                    || rt.list_multimap_tables().map_err(open_redb)?.count() != 0
                {
                    return Err(OpenError::Corrupt(
                        "not a Masstuffy index (no meta table)".into(),
                    ));
                }
                true
            }
            Err(e) => return Err(open_redb(e)),
        }
    };

    if init {
        let txn = db.begin_write().map_err(open_redb)?;
        {
            let mut meta = txn.open_table(META).map_err(open_redb)?;
            txn.open_table(COLLECTIONS).map_err(open_redb)?;
            txn.open_table(SLOTS).map_err(open_redb)?;
            txn.open_table(DEAD_SLOTS).map_err(open_redb)?;
            txn.open_table(ENTRIES).map_err(open_redb)?;
            txn.open_table(BY_ID).map_err(open_redb)?;
            txn.open_table(URLS).map_err(open_redb)?;
            meta.insert(KEY_SCHEMA, SCHEMA_VERSION).map_err(open_redb)?;
            meta.insert(KEY_NEXT_SLOT, 1u64).map_err(open_redb)?;
            meta.insert(KEY_STORED, 0u64).map_err(open_redb)?;
        }
        txn.commit().map_err(open_redb)?;
    }
    Ok(db)
}
