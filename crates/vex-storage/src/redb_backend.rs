//! `redb`-backed implementations of [`ObjectBackend`] and [`RefBackend`].
//!
//! Single-file embedded store. Used for standalone CLI workflows and as the
//! local cache layer in front of cloud backends.

use std::path::{Path, PathBuf};

use redb::{Database, ReadableTable, TableDefinition};
use vex_utils::{Hash256, VexError, VexResult};

use crate::backend::{ObjectBackend, RefBackend};

const TABLE_OBJECTS: TableDefinition<'_, &[u8; 32], &[u8]> = TableDefinition::new("objects");
const TABLE_REFS: TableDefinition<'_, &str, &[u8; 32]> = TableDefinition::new("refs");
const TABLE_CONFIG: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("config");

/// Open or create the redb database file at `<dir>/objects.redb`.
///
/// Both backends share the same database file, since they operate on
/// disjoint tables.
pub fn open_database(dir: impl AsRef<Path>) -> VexResult<std::sync::Arc<Database>> {
    let dir = dir.as_ref();
    std::fs::create_dir_all(dir).map_err(|e| VexError::io_at(dir, e))?;
    let file = dir.join("objects.redb");
    let db = Database::create(&file).map_err(|e| VexError::Storage(format!("open: {e}")))?;
    {
        let wtx = db
            .begin_write()
            .map_err(|e| VexError::Storage(format!("begin_write: {e}")))?;
        let _ = wtx
            .open_table(TABLE_OBJECTS)
            .map_err(|e| VexError::Storage(format!("table: {e}")))?;
        let _ = wtx
            .open_table(TABLE_REFS)
            .map_err(|e| VexError::Storage(format!("table: {e}")))?;
        let _ = wtx
            .open_table(TABLE_CONFIG)
            .map_err(|e| VexError::Storage(format!("table: {e}")))?;
        wtx.commit()
            .map_err(|e| VexError::Storage(format!("commit: {e}")))?;
    }
    Ok(std::sync::Arc::new(db))
}

#[derive(Debug, Clone)]
pub struct RedbObjectBackend {
    db: std::sync::Arc<Database>,
    #[allow(dead_code)]
    path: PathBuf,
}

impl RedbObjectBackend {
    pub fn new(db: std::sync::Arc<Database>, path: PathBuf) -> Self {
        Self { db, path }
    }
}

impl ObjectBackend for RedbObjectBackend {
    fn put(&self, hash: Hash256, framed: &[u8]) -> VexResult<()> {
        let wtx = self
            .db
            .begin_write()
            .map_err(|e| VexError::Storage(format!("begin_write: {e}")))?;
        {
            let mut table = wtx
                .open_table(TABLE_OBJECTS)
                .map_err(|e| VexError::Storage(format!("table: {e}")))?;
            // Caller may opt to skip via `has()`, but we are also idempotent here.
            table
                .insert(hash.as_bytes(), framed)
                .map_err(|e| VexError::Storage(format!("insert: {e}")))?;
        }
        wtx.commit()
            .map_err(|e| VexError::Storage(format!("commit: {e}")))?;
        Ok(())
    }

    fn put_many(&self, objects: &[(Hash256, Vec<u8>)]) -> VexResult<()> {
        if objects.is_empty() {
            return Ok(());
        }
        let wtx = self
            .db
            .begin_write()
            .map_err(|e| VexError::Storage(format!("begin_write: {e}")))?;
        {
            let mut table = wtx
                .open_table(TABLE_OBJECTS)
                .map_err(|e| VexError::Storage(format!("table: {e}")))?;
            for (hash, framed) in objects {
                // Content-addressed writes are idempotent: the same hash has
                // the same validated frame. Avoid a read per object and let
                // redb replace an identical value when it already exists.
                table
                    .insert(hash.as_bytes(), framed.as_slice())
                    .map_err(|e| VexError::Storage(format!("insert: {e}")))?;
            }
        }
        wtx.commit()
            .map_err(|e| VexError::Storage(format!("commit: {e}")))?;
        Ok(())
    }

    fn get(&self, hash: Hash256) -> VexResult<Option<Vec<u8>>> {
        let rtx = self
            .db
            .begin_read()
            .map_err(|e| VexError::Storage(format!("begin_read: {e}")))?;
        let table = rtx
            .open_table(TABLE_OBJECTS)
            .map_err(|e| VexError::Storage(format!("table: {e}")))?;
        let val = table
            .get(hash.as_bytes())
            .map_err(|e| VexError::Storage(format!("get: {e}")))?;
        Ok(val.map(|v| v.value().to_vec()))
    }

    fn get_many(&self, hashes: &[Hash256]) -> VexResult<Vec<Option<Vec<u8>>>> {
        let rtx = self
            .db
            .begin_read()
            .map_err(|e| VexError::Storage(format!("begin_read: {e}")))?;
        let table = rtx
            .open_table(TABLE_OBJECTS)
            .map_err(|e| VexError::Storage(format!("table: {e}")))?;
        hashes
            .iter()
            .map(|hash| {
                table
                    .get(hash.as_bytes())
                    .map(|value| value.map(|value| value.value().to_vec()))
                    .map_err(|e| VexError::Storage(format!("get: {e}")))
            })
            .collect()
    }

    fn has(&self, hash: Hash256) -> VexResult<bool> {
        let rtx = self
            .db
            .begin_read()
            .map_err(|e| VexError::Storage(format!("begin_read: {e}")))?;
        let table = rtx
            .open_table(TABLE_OBJECTS)
            .map_err(|e| VexError::Storage(format!("table: {e}")))?;
        Ok(table
            .get(hash.as_bytes())
            .map_err(|e| VexError::Storage(format!("get: {e}")))?
            .is_some())
    }

    fn delete(&self, hash: Hash256) -> VexResult<bool> {
        let wtx = self
            .db
            .begin_write()
            .map_err(|e| VexError::Storage(format!("begin_write: {e}")))?;
        let removed;
        {
            let mut table = wtx
                .open_table(TABLE_OBJECTS)
                .map_err(|e| VexError::Storage(format!("table: {e}")))?;
            removed = table
                .remove(hash.as_bytes())
                .map_err(|e| VexError::Storage(format!("remove: {e}")))?
                .is_some();
        }
        wtx.commit()
            .map_err(|e| VexError::Storage(format!("commit: {e}")))?;
        Ok(removed)
    }

    fn list_hashes(&self) -> VexResult<Vec<Hash256>> {
        let rtx = self
            .db
            .begin_read()
            .map_err(|e| VexError::Storage(format!("begin_read: {e}")))?;
        let table = rtx
            .open_table(TABLE_OBJECTS)
            .map_err(|e| VexError::Storage(format!("table: {e}")))?;
        let mut out = Vec::new();
        let iter = table
            .iter()
            .map_err(|e| VexError::Storage(format!("iter: {e}")))?;
        for kv in iter {
            let (k, _v) = kv.map_err(|e| VexError::Storage(format!("iter: {e}")))?;
            out.push(Hash256::from_bytes(*k.value()));
        }
        Ok(out)
    }
}

#[derive(Debug, Clone)]
pub struct RedbRefBackend {
    db: std::sync::Arc<Database>,
}

impl RedbRefBackend {
    pub fn new(db: std::sync::Arc<Database>) -> Self {
        Self { db }
    }
}

impl RefBackend for RedbRefBackend {
    fn get(&self, name: &str) -> VexResult<Option<Hash256>> {
        let rtx = self
            .db
            .begin_read()
            .map_err(|e| VexError::Storage(format!("begin_read: {e}")))?;
        let table = rtx
            .open_table(TABLE_REFS)
            .map_err(|e| VexError::Storage(format!("table: {e}")))?;
        let Some(val) = table
            .get(name)
            .map_err(|e| VexError::Storage(format!("get: {e}")))?
        else {
            return Ok(None);
        };
        Ok(Some(Hash256::from_bytes(*val.value())))
    }

    fn set(&self, name: &str, target: Hash256) -> VexResult<()> {
        let wtx = self
            .db
            .begin_write()
            .map_err(|e| VexError::Storage(format!("begin_write: {e}")))?;
        {
            let mut table = wtx
                .open_table(TABLE_REFS)
                .map_err(|e| VexError::Storage(format!("table: {e}")))?;
            table
                .insert(name, target.as_bytes())
                .map_err(|e| VexError::Storage(format!("insert: {e}")))?;
        }
        wtx.commit()
            .map_err(|e| VexError::Storage(format!("commit: {e}")))?;
        Ok(())
    }

    fn compare_and_set(
        &self,
        name: &str,
        expected: Option<Hash256>,
        target: Hash256,
    ) -> VexResult<bool> {
        let wtx = self
            .db
            .begin_write()
            .map_err(|e| VexError::Storage(format!("begin_write: {e}")))?;
        let applied;
        {
            let mut table = wtx
                .open_table(TABLE_REFS)
                .map_err(|e| VexError::Storage(format!("table: {e}")))?;
            let current = table
                .get(name)
                .map_err(|e| VexError::Storage(format!("get: {e}")))?
                .map(|v| Hash256::from_bytes(*v.value()));
            if current == expected {
                table
                    .insert(name, target.as_bytes())
                    .map_err(|e| VexError::Storage(format!("insert: {e}")))?;
                applied = true;
            } else {
                applied = false;
            }
        }
        wtx.commit()
            .map_err(|e| VexError::Storage(format!("commit: {e}")))?;
        Ok(applied)
    }

    fn delete(&self, name: &str) -> VexResult<bool> {
        let wtx = self
            .db
            .begin_write()
            .map_err(|e| VexError::Storage(format!("begin_write: {e}")))?;
        let removed;
        {
            let mut table = wtx
                .open_table(TABLE_REFS)
                .map_err(|e| VexError::Storage(format!("table: {e}")))?;
            removed = table
                .remove(name)
                .map_err(|e| VexError::Storage(format!("remove: {e}")))?
                .is_some();
        }
        wtx.commit()
            .map_err(|e| VexError::Storage(format!("commit: {e}")))?;
        Ok(removed)
    }

    fn list(&self) -> VexResult<Vec<(String, Hash256)>> {
        let rtx = self
            .db
            .begin_read()
            .map_err(|e| VexError::Storage(format!("begin_read: {e}")))?;
        let table = rtx
            .open_table(TABLE_REFS)
            .map_err(|e| VexError::Storage(format!("table: {e}")))?;
        let mut out = Vec::new();
        let iter = table
            .iter()
            .map_err(|e| VexError::Storage(format!("iter: {e}")))?;
        for kv in iter {
            let (k, v) = kv.map_err(|e| VexError::Storage(format!("iter: {e}")))?;
            out.push((k.value().to_string(), Hash256::from_bytes(*v.value())));
        }
        Ok(out)
    }
}
