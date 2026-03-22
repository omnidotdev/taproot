pub mod tags;

use crate::introspect::{IntrospectedSchema, TableInfo};

/// Extension point for customizing Taproot's schema generation pipeline.
///
/// Plugins receive mutable access to the introspected schema before GraphQL
/// types are built, allowing them to rename fields, omit tables, or attach
/// metadata that the builder reads downstream.
///
/// All methods have default no-op implementations so plugins only need to
/// override the hooks they care about.
pub trait Plugin: Send + Sync {
    /// Human-readable name for logging
    fn name(&self) -> &str;

    /// Called after introspection completes, before the GraphQL schema is built.
    ///
    /// Plugins can mutate the schema representation — rename tables/columns,
    /// remove items, or attach metadata via the comment field
    fn on_introspect(&self, _schema: &mut IntrospectedSchema) {}

    /// Filter tables/views from the schema.
    ///
    /// Return `false` to exclude a table entirely (no type, no queries, no
    /// mutations). Called once per table after `on_introspect`
    fn filter_table(&self, _table: &TableInfo) -> bool {
        true
    }
}
