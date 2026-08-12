//! SQLite connection pool construction.
//!
//! Implements the concurrency posture from spec §3.4: WAL journal mode,
//! `synchronous = NORMAL`, a 5s busy timeout so brief write contention waits
//! rather than erroring, foreign keys enforced per-connection (SQLite does
//! not default to this), and a small pool (~5 connections) since SQLite
//! serializes writes internally regardless of pool size.

use std::env;
use std::str::FromStr;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteSynchronous};
use sqlx::SqlitePool;

/// Name of the environment variable used to override the SQLite database
/// file path. Falls back to [`DbConfig::default`]'s path when unset.
pub const DB_PATH_ENV_VAR: &str = "SHAREPAY_DB_PATH";

/// Default SQLite database file, relative to the process's working
/// directory, used when `SHAREPAY_DB_PATH` is not set.
pub const DEFAULT_DB_PATH: &str = "sharepay.db";

/// Configuration for constructing the pool. Currently just the DB file
/// path, but kept as a struct so future options (e.g. max_connections
/// overrides for tests) have an obvious home.
#[derive(Debug, Clone)]
pub struct DbConfig {
    pub db_path: String,
    pub max_connections: u32,
}

impl DbConfig {
    /// Reads configuration from the environment, falling back to sane
    /// defaults (`sharepay.db` in the working directory, 5 max
    /// connections per spec §3.4).
    pub fn from_env() -> Self {
        let db_path =
            env::var(DB_PATH_ENV_VAR).unwrap_or_else(|_| DEFAULT_DB_PATH.to_string());
        Self {
            db_path,
            max_connections: 5,
        }
    }

    /// An in-memory, per-connection-unique database — useful for tests
    /// that want full isolation without touching disk. Note: because each
    /// pooled connection to `:memory:` is a *distinct* database, this only
    /// makes sense when paired with `max_connections(1)`.
    pub fn in_memory() -> Self {
        Self {
            db_path: ":memory:".to_string(),
            max_connections: 1,
        }
    }
}

impl Default for DbConfig {
    fn default() -> Self {
        Self {
            db_path: DEFAULT_DB_PATH.to_string(),
            max_connections: 5,
        }
    }
}

/// Builds the `SqliteConnectOptions` with the pragmas mandated by spec
/// §3.4, applied per-connection via builder methods (not manual per-query
/// `PRAGMA` statements).
fn connect_options(config: &DbConfig) -> Result<SqliteConnectOptions, sqlx::Error> {
    let mut opts = if config.db_path == ":memory:" {
        SqliteConnectOptions::from_str("sqlite::memory:")?
    } else {
        SqliteConnectOptions::from_str(&format!("sqlite:{}", config.db_path))?
            .create_if_missing(true)
    };

    opts = opts
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_millis(5000))
        .foreign_keys(true);

    Ok(opts)
}

/// Constructs the pool and runs pending migrations (`migrations/`,
/// embedded at compile time). This is the single entry point callers
/// (main.rs, tests) should use to obtain a ready-to-use `SqlitePool`.
pub async fn init_pool(config: &DbConfig) -> Result<SqlitePool, sqlx::Error> {
    let options = connect_options(config)?;

    let pool = SqlitePoolOptions::new()
        .max_connections(config.max_connections)
        .connect_with(options)
        .await?;

    sqlx::migrate!("./migrations").run(&pool).await?;

    Ok(pool)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn in_memory_pool_runs_migrations_and_enforces_foreign_keys() {
        let pool = init_pool(&DbConfig::in_memory()).await.unwrap();

        // Foreign keys should be ON: inserting an item for a non-existent
        // bill must fail.
        let result = sqlx::query("INSERT INTO items (bill_id, name, price, sort_order) VALUES ('nonexistent', 'x', 100, 0)")
            .execute(&pool)
            .await;
        assert!(result.is_err(), "expected FK violation to be enforced");
    }
}
