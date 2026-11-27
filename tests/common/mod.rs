//! Shared fixtures for store contract tests.
//!
//! `store_matrix!(name, |store: Arc<dyn Store>| async move { ... })` expands to a module
//! `name` with one `#[tokio::test]` per backend:
//! - `name::sqlite` — fresh in-memory SQLite (`sqlite::memory:`), always run;
//! - `name::postgres` — only with `--features pg-tests`; uses `COORD_TEST_PG_URL` and a
//!   freshly created schema per test (dropped afterwards), so tests run in parallel.
#![allow(dead_code)]

use std::sync::Arc;

use coord::store::Store;
use coord::store::sqlite::SqliteStore;

macro_rules! store_matrix {
    ($name:ident, $body:expr) => {
        mod $name {
            #[allow(unused_imports)]
            use super::*;

            #[tokio::test]
            async fn sqlite() {
                let store = crate::common::sqlite_store().await;
                ($body)(store).await;
            }

            #[cfg(feature = "pg-tests")]
            #[tokio::test]
            async fn postgres() {
                let (store, schema) = crate::common::pg_store().await;
                ($body)(store).await;
                schema.drop().await;
            }
        }
    };
}

pub async fn sqlite_store() -> Arc<dyn Store> {
    Arc::new(
        SqliteStore::connect("sqlite::memory:")
            .await
            .expect("open in-memory sqlite store"),
    )
}

#[cfg(feature = "pg-tests")]
pub use pg::pg_store;

#[cfg(feature = "pg-tests")]
mod pg {
    use std::str::FromStr;
    use std::sync::Arc;

    use coord::store::Store;
    use coord::store::postgres::PostgresStore;
    use sqlx::AssertSqlSafe;
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

    /// A per-test Postgres schema; `drop` removes it once the test body succeeded.
    pub struct TestSchema {
        admin: PgConnectOptions,
        name: String,
    }

    impl TestSchema {
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
        let url = std::env::var("COORD_TEST_PG_URL")
            .expect("COORD_TEST_PG_URL must be set when running with --features pg-tests");
        let admin = PgConnectOptions::from_str(&url).expect("parse COORD_TEST_PG_URL");
        let name = format!("coord_test_{}", uuid::Uuid::now_v7().simple());

        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(admin.clone())
            .await
            .expect("connect to COORD_TEST_PG_URL");
        sqlx::query(AssertSqlSafe(format!("CREATE SCHEMA \"{name}\"")))
            .execute(&pool)
            .await
            .expect("create test schema");
        pool.close().await;

        let scoped = admin.clone().options([("search_path", name.as_str())]);
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect_with(scoped)
            .await
            .expect("connect with test search_path");
        let store = PostgresStore::from_pool(pool)
            .await
            .expect("migrate test schema");
        (Arc::new(store), TestSchema { admin, name })
    }
}
