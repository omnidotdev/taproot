use async_trait::async_trait;
use sqlx::postgres::{PgPool, PgRow};
use sqlx::{Column, Row, TypeInfo};

use crate::error::{Error, Result};

/// A database row represented as a JSON object.
pub type JsonRow = serde_json::Value;

/// Per-request RLS context for setting Postgres role and claims
#[derive(Debug, Clone)]
pub struct RlsContext {
    /// Postgres role to SET LOCAL ROLE to (e.g. "authenticated")
    pub role: Option<String>,
    /// JSON string of JWT claims to SET LOCAL
    pub claims_json: Option<String>,
    /// Postgres setting name for claims (default: "request.jwt.claims")
    pub claims_setting: String,
}

impl Default for RlsContext {
    fn default() -> Self {
        Self {
            role: None,
            claims_json: None,
            claims_setting: "request.jwt.claims".into(),
        }
    }
}

impl RlsContext {
    /// Generate SET LOCAL SQL statements for this context
    #[must_use]
    pub fn set_local_statements(&self) -> Vec<String> {
        let mut stmts = Vec::new();

        if let Some(role) = &self.role {
            let escaped = role.replace('\'', "''");
            stmts.push(format!("SET LOCAL ROLE '{escaped}'"));
        }

        if let Some(claims) = &self.claims_json {
            let escaped_setting = self.claims_setting.replace('\'', "''");
            let escaped_claims = claims.replace('\'', "''");
            stmts.push(format!(
                "SET LOCAL \"{escaped_setting}\" = '{escaped_claims}'"
            ));
        }

        stmts
    }
}

/// Abstraction over database access for introspection and query execution.
#[async_trait]
pub trait Database: Send + Sync + 'static {
    /// Execute a parameterized query and return rows as JSON.
    async fn query(&self, sql: &str, params: &[serde_json::Value]) -> Result<Vec<JsonRow>>;

    /// Execute a parameterized statement and return the number of affected rows.
    async fn execute(&self, sql: &str, params: &[serde_json::Value]) -> Result<u64>;

    /// Execute a raw query with no parameters (used for introspection).
    async fn query_raw(&self, sql: &str) -> Result<Vec<JsonRow>>;

    /// Execute a query within a transaction that has RLS context set via SET LOCAL.
    /// Falls back to regular query if rls is None or has no statements.
    async fn query_with_rls(
        &self,
        sql: &str,
        params: &[serde_json::Value],
        _rls: Option<&RlsContext>,
    ) -> Result<Vec<JsonRow>> {
        self.query(sql, params).await
    }
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

    async fn query_with_rls(
        &self,
        sql: &str,
        params: &[serde_json::Value],
        rls: Option<&RlsContext>,
    ) -> Result<Vec<JsonRow>> {
        let Some(rls) = rls else {
            return self.query(sql, params).await;
        };

        let set_stmts = rls.set_local_statements();
        if set_stmts.is_empty() {
            return self.query(sql, params).await;
        }

        // Run in a transaction with SET LOCAL
        let mut tx = self.pool.begin().await?;

        for stmt in &set_stmts {
            sqlx::query(stmt).execute(&mut *tx).await?;
        }

        let mut query = sqlx::query(sql);
        for param in params {
            query = bind_json_param(query, param);
        }

        let rows = query.fetch_all(&mut *tx).await?;
        let result: Result<Vec<JsonRow>> = rows.iter().map(row_to_json).collect();

        tx.commit().await?;

        result
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rls_context_default_is_empty() {
        let ctx = RlsContext::default();
        assert!(ctx.role.is_none());
        assert!(ctx.claims_json.is_none());
        assert_eq!(ctx.claims_setting, "request.jwt.claims");
    }

    #[test]
    fn rls_context_with_role() {
        let ctx = RlsContext {
            role: Some("authenticated".into()),
            claims_json: Some(r#"{"sub":"user-1"}"#.into()),
            claims_setting: "request.jwt.claims".into(),
        };
        assert_eq!(ctx.role.as_deref(), Some("authenticated"));
    }

    #[test]
    fn rls_context_generates_set_statements() {
        let ctx = RlsContext {
            role: Some("authenticated".into()),
            claims_json: Some(r#"{"sub":"user-1"}"#.into()),
            claims_setting: "request.jwt.claims".into(),
        };
        let stmts = ctx.set_local_statements();
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains("SET LOCAL ROLE"));
        assert!(stmts[1].contains("SET LOCAL"));
        assert!(stmts[1].contains("request.jwt.claims"));
    }

    #[test]
    fn rls_context_empty_generates_no_statements() {
        let ctx = RlsContext::default();
        let stmts = ctx.set_local_statements();
        assert!(stmts.is_empty());
    }

    #[test]
    fn rls_context_escapes_single_quotes() {
        let ctx = RlsContext {
            role: Some("it's a role".into()),
            claims_json: None,
            claims_setting: "request.jwt.claims".into(),
        };
        let stmts = ctx.set_local_statements();
        assert_eq!(stmts.len(), 1);
        assert!(
            stmts[0].contains("it''s a role"),
            "should escape quotes: {}",
            stmts[0]
        );
    }
}
