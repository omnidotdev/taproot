use async_trait::async_trait;
use sqlx::postgres::{PgPool, PgRow};
use sqlx::{Column, Row, TypeInfo};

use crate::error::{Error, Result};

/// A database row represented as a JSON object.
pub type JsonRow = serde_json::Value;

/// Abstraction over database access for introspection and query execution.
#[async_trait]
pub trait Database: Send + Sync + 'static {
    /// Execute a parameterized query and return rows as JSON.
    async fn query(&self, sql: &str, params: &[serde_json::Value]) -> Result<Vec<JsonRow>>;

    /// Execute a parameterized statement and return the number of affected rows.
    async fn execute(&self, sql: &str, params: &[serde_json::Value]) -> Result<u64>;

    /// Execute a raw query with no parameters (used for introspection).
    async fn query_raw(&self, sql: &str) -> Result<Vec<JsonRow>>;
}

/// Database backend powered by sqlx with a Postgres connection pool.
#[derive(Debug, Clone)]
pub struct SqlxBackend {
    pool: PgPool,
}

impl SqlxBackend {
    /// Connect to a Postgres database using the given URL.
    ///
    /// # Errors
    /// Returns an error if the connection cannot be established.
    pub async fn connect(url: &str) -> Result<Self> {
        let pool = PgPool::connect(url).await?;
        Ok(Self { pool })
    }

    /// Create a backend from an existing connection pool.
    #[must_use]
    pub const fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Return a reference to the underlying connection pool.
    #[must_use]
    pub const fn pool(&self) -> &PgPool {
        &self.pool
    }
}

#[async_trait]
impl Database for SqlxBackend {
    async fn query(&self, sql: &str, params: &[serde_json::Value]) -> Result<Vec<JsonRow>> {
        let mut query = sqlx::query(sql);

        for param in params {
            query = bind_json_param(query, param);
        }

        let rows = query.fetch_all(&self.pool).await?;

        rows.iter().map(row_to_json).collect()
    }

    async fn execute(&self, sql: &str, params: &[serde_json::Value]) -> Result<u64> {
        let mut query = sqlx::query(sql);

        for param in params {
            query = bind_json_param(query, param);
        }

        let result = query.execute(&self.pool).await?;
        Ok(result.rows_affected())
    }

    async fn query_raw(&self, sql: &str) -> Result<Vec<JsonRow>> {
        let rows = sqlx::query(sql).fetch_all(&self.pool).await?;
        rows.iter().map(row_to_json).collect()
    }
}

/// Bind a JSON value to a sqlx query as the appropriate Postgres type.
fn bind_json_param<'q>(
    query: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    value: &'q serde_json::Value,
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    match value {
        serde_json::Value::Null => query.bind(None::<String>),
        serde_json::Value::Bool(b) => query.bind(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                query.bind(i)
            } else if let Some(f) = n.as_f64() {
                query.bind(f)
            } else {
                query.bind(n.to_string())
            }
        }
        serde_json::Value::String(s) => {
            // Try UUID first — Postgres requires typed UUID params
            if let Ok(uuid) = uuid::Uuid::parse_str(s) {
                query.bind(uuid)
            } else {
                query.bind(s.as_str())
            }
        }
        // Arrays and objects are bound as JSON
        other => query.bind(other.clone()),
    }
}

/// Convert a `PgRow` into a JSON object by inspecting column metadata.
///
/// # Errors
/// Returns an error if a column value cannot be decoded.
fn row_to_json(row: &PgRow) -> Result<serde_json::Value> {
    let mut map = serde_json::Map::new();

    for col in row.columns() {
        let name = col.name();
        let type_name = col.type_info().name();

        let value: serde_json::Value = match type_name {
            "BOOL" => row
                .try_get::<Option<bool>, _>(name)
                .map(|v| v.map_or(serde_json::Value::Null, serde_json::Value::Bool))
                .map_err(|e| Error::Database(e.to_string()))?,

            "INT2" | "SMALLINT" | "SMALLSERIAL" => row
                .try_get::<Option<i16>, _>(name)
                .map(|v| v.map_or(serde_json::Value::Null, |n| serde_json::json!(n)))
                .map_err(|e| Error::Database(e.to_string()))?,

            "INT4" | "INT" | "SERIAL" => row
                .try_get::<Option<i32>, _>(name)
                .map(|v| v.map_or(serde_json::Value::Null, |n| serde_json::json!(n)))
                .map_err(|e| Error::Database(e.to_string()))?,

            "INT8" | "BIGINT" | "BIGSERIAL" => row
                .try_get::<Option<i64>, _>(name)
                .map(|v| v.map_or(serde_json::Value::Null, |n| serde_json::json!(n)))
                .map_err(|e| Error::Database(e.to_string()))?,

            "FLOAT4" | "REAL" => row
                .try_get::<Option<f32>, _>(name)
                .map(|v| v.map_or(serde_json::Value::Null, |n| serde_json::json!(n)))
                .map_err(|e| Error::Database(e.to_string()))?,

            "FLOAT8" | "DOUBLE PRECISION" => row
                .try_get::<Option<f64>, _>(name)
                .map(|v| v.map_or(serde_json::Value::Null, |n| serde_json::json!(n)))
                .map_err(|e| Error::Database(e.to_string()))?,

            "JSON" | "JSONB" => row
                .try_get::<Option<serde_json::Value>, _>(name)
                .map(|v| v.unwrap_or(serde_json::Value::Null))
                .map_err(|e| Error::Database(e.to_string()))?,

            "UUID" => row
                .try_get::<Option<uuid::Uuid>, _>(name)
                .map(|v| {
                    v.map_or(serde_json::Value::Null, |u| {
                        serde_json::Value::String(u.to_string())
                    })
                })
                .map_err(|e| Error::Database(e.to_string()))?,

            "TIMESTAMPTZ" => row
                .try_get::<Option<chrono::DateTime<chrono::Utc>>, _>(name)
                .map(|v| {
                    v.map_or(serde_json::Value::Null, |t| {
                        serde_json::Value::String(t.to_rfc3339())
                    })
                })
                .map_err(|e| Error::Database(e.to_string()))?,

            "TIMESTAMP" => row
                .try_get::<Option<chrono::NaiveDateTime>, _>(name)
                .map(|v| {
                    v.map_or(serde_json::Value::Null, |t| {
                        serde_json::Value::String(t.to_string())
                    })
                })
                .map_err(|e| Error::Database(e.to_string()))?,

            "DATE" => row
                .try_get::<Option<chrono::NaiveDate>, _>(name)
                .map(|v| {
                    v.map_or(serde_json::Value::Null, |d| {
                        serde_json::Value::String(d.to_string())
                    })
                })
                .map_err(|e| Error::Database(e.to_string()))?,

            "NUMERIC" => row
                .try_get::<Option<bigdecimal::BigDecimal>, _>(name)
                .map(|v| {
                    v.map_or(serde_json::Value::Null, |d| {
                        serde_json::Value::String(d.to_string())
                    })
                })
                .map_err(|e| Error::Database(e.to_string()))?,

            // Fall back to string representation for all other types
            _ => row
                .try_get::<Option<String>, _>(name)
                .map(|v| {
                    v.map_or(serde_json::Value::Null, serde_json::Value::String)
                })
                .map_err(|e| Error::Database(e.to_string()))?,
        };

        map.insert(name.to_string(), value);
    }

    Ok(serde_json::Value::Object(map))
}
