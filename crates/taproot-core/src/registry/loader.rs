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
}
