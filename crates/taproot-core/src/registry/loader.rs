use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_graphql::dataloader::{DataLoader, Loader};

use crate::db::{Database, RlsContext};
use crate::planner::{execute_batch_lookup, PlanNode};

/// Key identifying a specific belongs-to relationship loader
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct LoaderKey {
    pub schema: String,
    pub table: String,
    pub key_column: String,
}

impl LoaderKey {
    #[must_use]
    pub fn new(schema: &str, table: &str, key_column: &str) -> Self {
        Self {
            schema: schema.to_string(),
            table: table.to_string(),
            key_column: key_column.to_string(),
        }
    }
}

/// Normalize a JSON value to a string key for `DataLoader` lookup
#[must_use]
pub fn normalize_key(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

/// Loader that batches belongs-to lookups into a single `WHERE key IN (...)` query
pub struct BelongsToLoader {
    db: Arc<dyn Database>,
    schema: String,
    table: String,
    key_column: String,
    rls: Option<RlsContext>,
}

impl BelongsToLoader {
    #[must_use]
    pub fn new(
        db: Arc<dyn Database>,
        schema: String,
        table: String,
        key_column: String,
        rls: Option<RlsContext>,
    ) -> Self {
        Self {
            db,
            schema,
            table,
            key_column,
            rls,
        }
    }
}

impl Loader<String> for BelongsToLoader {
    type Value = serde_json::Value;
    type Error = Arc<async_graphql::Error>;

    async fn load(&self, keys: &[String]) -> Result<HashMap<String, Self::Value>, Self::Error> {
        if keys.is_empty() {
            return Ok(HashMap::new());
        }

        let plan = PlanNode::BatchLookup {
            schema: self.schema.clone(),
            table: self.table.clone(),
            key_column: self.key_column.clone(),
            keys: keys.iter().map(|k| serde_json::json!(k)).collect(),
            selections: vec![],
        };

        let grouped = execute_batch_lookup(&plan, &self.db, self.rls.as_ref())
            .await
            .map_err(|e| Arc::new(async_graphql::Error::new(format!("database error: {e}"))))?;

        // For belongs-to, each key maps to at most one row
        let mut result = HashMap::with_capacity(grouped.len());
        for (key, mut rows) in grouped {
            if let Some(row) = rows.pop() {
                let value = row.get("row_to_json").cloned().unwrap_or(row);
                result.insert(key, value);
            }
        }

        Ok(result)
    }
}

/// Registry that lazily creates and caches `DataLoader` instances per relationship
#[derive(Clone)]
pub struct LoaderRegistry {
    db: Arc<dyn Database>,
    rls: Option<RlsContext>,
    loaders: Arc<Mutex<HashMap<LoaderKey, Arc<DataLoader<BelongsToLoader>>>>>,
}

impl LoaderRegistry {
    #[must_use]
    pub fn new(db: Arc<dyn Database>, rls: Option<RlsContext>) -> Self {
        Self {
            db,
            rls,
            loaders: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Get or create a `DataLoader` for the given relationship
    ///
    /// # Panics
    /// Panics if the internal loader mutex is poisoned
    pub fn get_belongs_to(&self, key: &LoaderKey) -> Arc<DataLoader<BelongsToLoader>> {
        let mut loaders = self.loaders.lock().expect("loader registry poisoned");

        if let Some(loader) = loaders.get(key) {
            return Arc::clone(loader);
        }

        let loader = BelongsToLoader::new(
            Arc::clone(&self.db),
            key.schema.clone(),
            key.table.clone(),
            key.key_column.clone(),
            self.rls.clone(),
        );

        let data_loader = Arc::new(DataLoader::new(loader, tokio::spawn));
        loaders.insert(key.clone(), Arc::clone(&data_loader));
        data_loader
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loader_key_creation() {
        let key = LoaderKey::new("public", "users", "id");
        assert_eq!(key.schema, "public");
        assert_eq!(key.table, "users");
        assert_eq!(key.key_column, "id");
    }

    #[test]
    fn normalize_key_handles_strings() {
        assert_eq!(normalize_key(&serde_json::json!("abc")), "abc");
    }

    #[test]
    fn normalize_key_handles_numbers() {
        assert_eq!(normalize_key(&serde_json::json!(42)), "42");
    }

    #[test]
    fn normalize_key_handles_null() {
        assert_eq!(normalize_key(&serde_json::Value::Null), "null");
    }

    #[tokio::test]
    async fn belongs_to_loader_batches_into_single_query() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use async_trait::async_trait;

        use crate::db::JsonRow;
        use crate::error::Result as DbResult;

        // Database double that counts how many queries it runs
        struct CountingBackend {
            count: Arc<AtomicUsize>,
            rows: Vec<JsonRow>,
        }

        #[async_trait]
        impl Database for CountingBackend {
            async fn query(&self, _sql: &str, _params: &[serde_json::Value]) -> DbResult<Vec<JsonRow>> {
                self.count.fetch_add(1, Ordering::SeqCst);
                Ok(self.rows.clone())
            }
            async fn execute(&self, _sql: &str, _params: &[serde_json::Value]) -> DbResult<u64> {
                Ok(0)
            }
            async fn query_raw(&self, _sql: &str) -> DbResult<Vec<JsonRow>> {
                Ok(vec![])
            }
        }

        let count = Arc::new(AtomicUsize::new(0));
        let rows = vec![
            serde_json::json!({ "id": "k1", "name": "A" }),
            serde_json::json!({ "id": "k2", "name": "B" }),
            serde_json::json!({ "id": "k3", "name": "C" }),
        ];
        let db: Arc<dyn Database> = Arc::new(CountingBackend {
            count: Arc::clone(&count),
            rows,
        });

        let loader =
            BelongsToLoader::new(db, "public".into(), "users".into(), "id".into(), None);

        let keys = vec!["k1".to_string(), "k2".to_string(), "k3".to_string()];
        let result = loader.load(&keys).await.expect("load");

        // Three keys are resolved in a SINGLE batched query, not N+1
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "belongs-to loader must batch all keys into one query"
        );
        assert_eq!(result.len(), 3);
    }
}
