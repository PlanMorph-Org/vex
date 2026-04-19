//! Postgres-backed [`RefBackend`] with transactional compare-and-set.
//!
//! Available with the `postgres-backend` feature. Designed for the cloud
//! deployment where many `vex push` calls may race for the same branch.
//!
//! Schema (one row per `(repo_id, name)`):
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS vex_refs (
//!     repo_id    UUID  NOT NULL,
//!     name       TEXT  NOT NULL,
//!     target     BYTEA NOT NULL,                  -- 32 bytes
//!     updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
//!     PRIMARY KEY (repo_id, name)
//! );
//! ```
//!
//! The schema is created on demand by [`PostgresRefBackend::ensure_schema`].

use std::sync::Arc;

use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::Row;
use tokio::runtime::Runtime;
use uuid::Uuid;
use vex_utils::{Hash256, VexError, VexResult};

use crate::backend::RefBackend;

#[derive(Debug, Clone)]
pub struct PostgresConfig {
    /// Standard `postgres://user:pass@host/db` URL.
    pub url: String,
    /// Logical scope inside the table — every repository is a UUID.
    pub repo_id: Uuid,
    /// Connection pool cap. Default 4.
    pub max_connections: u32,
}

impl PostgresConfig {
    pub fn new(url: impl Into<String>, repo_id: Uuid) -> Self {
        Self {
            url: url.into(),
            repo_id,
            max_connections: 4,
        }
    }
}

#[derive(Debug)]
pub struct PostgresRefBackend {
    pool: PgPool,
    repo_id: Uuid,
    rt: Arc<Runtime>,
}

impl PostgresRefBackend {
    pub fn new(cfg: PostgresConfig) -> VexResult<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("vex-pg")
            .build()
            .map_err(|e| VexError::Storage(format!("tokio: {e}")))?;
        Self::with_runtime(cfg, Arc::new(rt))
    }

    #[allow(clippy::needless_pass_by_value)]
    pub fn with_runtime(cfg: PostgresConfig, rt: Arc<Runtime>) -> VexResult<Self> {
        let pool = rt.block_on(async {
            PgPoolOptions::new()
                .max_connections(cfg.max_connections)
                .connect(&cfg.url)
                .await
                .map_err(|e| VexError::Storage(format!("pg connect: {e}")))
        })?;
        let me = Self {
            pool,
            repo_id: cfg.repo_id,
            rt,
        };
        me.ensure_schema()?;
        Ok(me)
    }

    pub fn ensure_schema(&self) -> VexResult<()> {
        let pool = self.pool.clone();
        self.rt.block_on(async move {
            sqlx::query(
                "CREATE TABLE IF NOT EXISTS vex_refs (
                    repo_id    UUID  NOT NULL,
                    name       TEXT  NOT NULL,
                    target     BYTEA NOT NULL,
                    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
                    PRIMARY KEY (repo_id, name)
                )",
            )
            .execute(&pool)
            .await
            .map_err(|e| VexError::Storage(format!("pg ddl: {e}")))?;
            Ok::<_, VexError>(())
        })
    }
}

fn hash_to_bytes(h: Hash256) -> Vec<u8> {
    h.as_bytes().to_vec()
}

fn bytes_to_hash(b: &[u8]) -> VexResult<Hash256> {
    if b.len() != 32 {
        return Err(VexError::Storage(format!(
            "ref target wrong length: {}",
            b.len()
        )));
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(b);
    Ok(Hash256::from_bytes(arr))
}

impl RefBackend for PostgresRefBackend {
    fn get(&self, name: &str) -> VexResult<Option<Hash256>> {
        let pool = self.pool.clone();
        let repo_id = self.repo_id;
        let name = name.to_string();
        self.rt.block_on(async move {
            let row = sqlx::query("SELECT target FROM vex_refs WHERE repo_id = $1 AND name = $2")
                .bind(repo_id)
                .bind(&name)
                .fetch_optional(&pool)
                .await
                .map_err(|e| VexError::Storage(format!("pg select: {e}")))?;
            match row {
                Some(r) => {
                    let bytes: Vec<u8> = r
                        .try_get("target")
                        .map_err(|e| VexError::Storage(format!("pg col: {e}")))?;
                    Ok(Some(bytes_to_hash(&bytes)?))
                }
                None => Ok(None),
            }
        })
    }

    fn set(&self, name: &str, target: Hash256) -> VexResult<()> {
        let pool = self.pool.clone();
        let repo_id = self.repo_id;
        let name = name.to_string();
        let target = hash_to_bytes(target);
        self.rt.block_on(async move {
            sqlx::query(
                "INSERT INTO vex_refs (repo_id, name, target, updated_at)
                 VALUES ($1, $2, $3, now())
                 ON CONFLICT (repo_id, name)
                 DO UPDATE SET target = EXCLUDED.target, updated_at = now()",
            )
            .bind(repo_id)
            .bind(&name)
            .bind(&target)
            .execute(&pool)
            .await
            .map_err(|e| VexError::Storage(format!("pg upsert: {e}")))?;
            Ok::<_, VexError>(())
        })
    }

    fn compare_and_set(
        &self,
        name: &str,
        expected: Option<Hash256>,
        target: Hash256,
    ) -> VexResult<bool> {
        let pool = self.pool.clone();
        let repo_id = self.repo_id;
        let name = name.to_string();
        let target_bytes = hash_to_bytes(target);
        let expected_bytes = expected.map(hash_to_bytes);

        self.rt.block_on(async move {
            let mut tx = pool
                .begin()
                .await
                .map_err(|e| VexError::Storage(format!("pg begin: {e}")))?;

            // Lock the row (or absence) to avoid races against concurrent CAS.
            let current: Option<Vec<u8>> = sqlx::query_scalar(
                "SELECT target FROM vex_refs WHERE repo_id = $1 AND name = $2 FOR UPDATE",
            )
            .bind(repo_id)
            .bind(&name)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| VexError::Storage(format!("pg lock: {e}")))?;

            let matches = match (&current, &expected_bytes) {
                (None, None) => true,
                (Some(c), Some(e)) => c == e,
                _ => false,
            };
            if !matches {
                tx.rollback()
                    .await
                    .map_err(|e| VexError::Storage(format!("pg rollback: {e}")))?;
                return Ok(false);
            }

            sqlx::query(
                "INSERT INTO vex_refs (repo_id, name, target, updated_at)
                 VALUES ($1, $2, $3, now())
                 ON CONFLICT (repo_id, name)
                 DO UPDATE SET target = EXCLUDED.target, updated_at = now()",
            )
            .bind(repo_id)
            .bind(&name)
            .bind(&target_bytes)
            .execute(&mut *tx)
            .await
            .map_err(|e| VexError::Storage(format!("pg cas write: {e}")))?;

            tx.commit()
                .await
                .map_err(|e| VexError::Storage(format!("pg commit: {e}")))?;
            Ok(true)
        })
    }

    fn delete(&self, name: &str) -> VexResult<bool> {
        let pool = self.pool.clone();
        let repo_id = self.repo_id;
        let name = name.to_string();
        self.rt.block_on(async move {
            let res = sqlx::query("DELETE FROM vex_refs WHERE repo_id = $1 AND name = $2")
                .bind(repo_id)
                .bind(&name)
                .execute(&pool)
                .await
                .map_err(|e| VexError::Storage(format!("pg delete: {e}")))?;
            Ok(res.rows_affected() > 0)
        })
    }

    fn list(&self) -> VexResult<Vec<(String, Hash256)>> {
        let pool = self.pool.clone();
        let repo_id = self.repo_id;
        self.rt.block_on(async move {
            let rows =
                sqlx::query("SELECT name, target FROM vex_refs WHERE repo_id = $1 ORDER BY name")
                    .bind(repo_id)
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| VexError::Storage(format!("pg list: {e}")))?;
            let mut out = Vec::with_capacity(rows.len());
            for r in rows {
                let name: String = r
                    .try_get("name")
                    .map_err(|e| VexError::Storage(format!("pg col: {e}")))?;
                let target: Vec<u8> = r
                    .try_get("target")
                    .map_err(|e| VexError::Storage(format!("pg col: {e}")))?;
                out.push((name, bytes_to_hash(&target)?));
            }
            Ok(out)
        })
    }
}
