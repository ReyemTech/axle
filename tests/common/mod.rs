//! Shared fixtures for store contract tests.
//!
//! `store_matrix!(name, |store: Arc<dyn Store>| async move { ... })` expands to a module
//! `name` with one `#[tokio::test]` per backend:
//! - `name::sqlite` — a fresh SQLite file in the temp dir (WAL, pooled connections, so
//!   concurrent tests race for real), always run; the files are removed even on panic;
//! - `name::postgres` — only with `--features pg-tests`; uses `AXL_TEST_PG_URL` and a
//!   freshly created schema per test, dropped afterwards even when the body panics, so
//!   tests run in parallel.
//!
//! Bodies run on a multi-threaded runtime inside a spawned task (so they must be `Send`).
#![allow(dead_code)]

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use axle::store::Store;
use axle::store::sqlite::SqliteStore;
use tokio::task::JoinError;

macro_rules! store_matrix {
    ($name:ident, $body:expr) => {
        mod $name {
            #[allow(unused_imports)]
            use super::*;

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn sqlite() {
                let (store, _file) = crate::common::sqlite_store().await;
                crate::common::run_body(($body)(store)).await;
            }

            #[cfg(feature = "pg-tests")]
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn postgres() {
                let (store, schema) = crate::common::pg_store().await;
                let outcome = crate::common::catch_body(($body)(store)).await;
                schema.drop().await;
                crate::common::resume(outcome);
            }
        }
    };
}

/// Runs a test body in its own task so a panic is caught rather than unwinding past cleanup.
pub async fn catch_body<F>(body: F) -> Result<(), JoinError>
where
    F: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(body).await
}

/// Re-raises a panic captured by `catch_body` (after cleanup has run).
pub fn resume(outcome: Result<(), JoinError>) {
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
}

pub async fn run_body<F>(body: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    resume(catch_body(body).await);
}

/// A fresh token row (sessions, tasks and messages reference the writer's token).
pub async fn token(store: &dyn Store) -> axle::domain::TokenId {
    let id = axle::domain::TokenId::new();
    store
        .insert_token(
            id,
            "test",
            &format!("test-hash-{id}"),
            time::macros::datetime!(2026-01-01 0:00 UTC),
        )
        .await
        .expect("insert test token");
    id
}

/// A temp SQLite database file (plus `-wal` / `-shm`), deleted on drop.
pub struct TempDb(PathBuf);

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
        }
    }
}

pub async fn sqlite_store() -> (Arc<dyn Store>, TempDb) {
    let path = std::env::temp_dir().join(format!("axle-test-{}.db", uuid::Uuid::now_v7()));
    let store = SqliteStore::connect(&format!("sqlite://{}", path.display()))
        .await
        .expect("open temp-file sqlite store");
    (Arc::new(store), TempDb(path))
}

#[cfg(feature = "pg-tests")]
pub use pg::pg_store;

#[cfg(feature = "pg-tests")]
mod pg {
    use std::str::FromStr;
    use std::sync::Arc;

    use axle::store::Store;
    use axle::store::postgres::PostgresStore;
    use sqlx::AssertSqlSafe;
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

    /// A per-test Postgres schema; `drop` removes it (the macro calls it whether or not the
    /// test body panicked).
    pub struct TestSchema {
        admin: PgConnectOptions,
        name: String,
    }

    impl TestSchema {
        /// A pool of its own on this test's schema, for driving raw interleavings.
        pub async fn pool(&self) -> sqlx::PgPool {
            PgPoolOptions::new()
                .max_connections(2)
                .connect_with(
                    self.admin
                        .clone()
                        .options([("search_path", self.name.as_str())]),
                )
                .await
                .expect("connect raw test pool")
        }

        pub async fn drop(self) {
            let pool = PgPoolOptions::new()
                .max_connections(1)
                .connect_with(self.admin)
                .await
                .expect("connect to drop test schema");
            sqlx::query(AssertSqlSafe(format!(
                "DROP SCHEMA \"{}\" CASCADE",
                self.name
            )))
            .execute(&pool)
            .await
            .expect("drop test schema");
            pool.close().await;
        }
    }

    pub async fn pg_store() -> (Arc<dyn Store>, TestSchema) {
        let url = std::env::var("AXL_TEST_PG_URL")
            .expect("AXL_TEST_PG_URL must be set when running with --features pg-tests");
        let admin = PgConnectOptions::from_str(&url).expect("parse AXL_TEST_PG_URL");
        let name = format!("axl_test_{}", uuid::Uuid::now_v7().simple());

        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(admin.clone())
            .await
            .expect("connect to AXL_TEST_PG_URL");
        sqlx::query(AssertSqlSafe(format!("CREATE SCHEMA \"{name}\"")))
            .execute(&pool)
            .await
            .expect("create test schema");
        pool.close().await;

        let scoped = admin.clone().options([("search_path", name.as_str())]);
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect_with(scoped)
            .await
            .expect("connect with test search_path");
        let store = PostgresStore::from_pool(pool)
            .await
            .expect("migrate test schema");
        (Arc::new(store), TestSchema { admin, name })
    }
}
