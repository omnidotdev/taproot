use std::sync::Arc;

use crate::db::Database;
use crate::error::Result;
use crate::introspect::{introspect, IntrospectOptions};
use crate::plugin::Plugin;
use crate::registry::build_schema;
use crate::registry::types::SchemaOptions;

/// A taproot subgraph ready for embedding in a federation gateway.
pub struct TaprootSubgraph {
    schema: async_graphql::dynamic::Schema,
}

impl TaprootSubgraph {
    /// Build a taproot subgraph from a database connection.
    ///
    /// # Errors
    /// Returns an error if introspection or schema building fails.
    pub async fn build(db: Arc<dyn Database>, options: TaprootSubgraphOptions) -> Result<Self> {
        let introspect_opts = IntrospectOptions {
            schemas: options.schemas,
            exclude_tables: options.exclude_tables,
            smart_comments: true,
        };

        let schemas = introspect(db.as_ref(), &introspect_opts).await?;

        let schema_opts = SchemaOptions {
            federation: true,
            subscriptions: false,
            ..Default::default()
        };

        let schema = build_schema(&schemas, &db, &schema_opts, &options.plugins, None)?;

        Ok(Self { schema })
    }

    /// Execute a GraphQL query against this subgraph.
    pub async fn execute(
        &self,
        query: &str,
        operation_name: Option<&str>,
        variables: Option<serde_json::Value>,
    ) -> serde_json::Value {
        let mut request = async_graphql::Request::new(query);

        if let Some(name) = operation_name {
            request = request.operation_name(name);
        }

        if let Some(vars) = variables {
            request = request.variables(async_graphql::Variables::from_json(vars));
        }

        let response = self.schema.execute(request).await;

        let mut result = serde_json::Map::new();

        if let Ok(data) = response.data.into_json() {
            result.insert("data".into(), data);
        }

        if !response.errors.is_empty() {
            let errors: Vec<serde_json::Value> = response
                .errors
                .iter()
                .map(|e| serde_json::json!({"message": e.message}))
                .collect();
            result.insert("errors".into(), serde_json::Value::Array(errors));
        }

        serde_json::Value::Object(result)
    }

    /// Return the federation SDL for this subgraph.
    #[must_use]
    pub fn sdl(&self) -> String {
        use async_graphql::SDLExportOptions;

        self.schema
            .sdl_with_options(SDLExportOptions::new().federation())
    }

    /// Return a reference to the underlying async-graphql schema.
    #[must_use]
    pub const fn schema(&self) -> &async_graphql::dynamic::Schema {
        &self.schema
    }
}

/// Options for building a taproot subgraph.
pub struct TaprootSubgraphOptions {
    /// Postgres schemas to introspect.
    pub schemas: Vec<String>,
    /// Tables to exclude.
    pub exclude_tables: Vec<String>,
    /// Plugins to apply.
    pub plugins: Vec<Arc<dyn Plugin>>,
}

impl Default for TaprootSubgraphOptions {
    fn default() -> Self {
        Self {
            schemas: vec!["public".into()],
            exclude_tables: Vec::new(),
            plugins: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_options() {
        let opts = TaprootSubgraphOptions::default();
        assert_eq!(opts.schemas, vec!["public".to_string()]);
        assert!(opts.exclude_tables.is_empty());
        assert!(opts.plugins.is_empty());
    }
}
