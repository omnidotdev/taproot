pub mod smart_comments;
pub mod tags;

pub use smart_comments::SmartCommentsPlugin;

use async_graphql::dynamic::Field;

use crate::introspect::{IntrospectedSchema, TableInfo};
use crate::planner::PlanNode;

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

    /// Called after the query plan is built, before SQL execution.
    ///
    /// Plugins can modify the plan tree — inject filters (e.g. RLS/tenant
    /// isolation), rewrite nodes, or add logging markers
    fn on_plan(&self, _plan: &mut PlanNode) {}

    /// Filter tables/views from the schema.
    ///
    /// Return `false` to exclude a table entirely (no type, no queries, no
    /// mutations). Called once per table after `on_introspect`
    fn filter_table(&self, _table: &TableInfo) -> bool {
        true
    }

    /// Contribute custom root `Query` fields (the Taproot equivalent of
    /// a schema-extension hook).
    ///
    /// Returned fields are added to the generated `Query` type. A field's
    /// resolver can run SQL via `ctx.data::<Arc<dyn Database>>()` (the pooled
    /// database handle is registered in the schema data) together with the
    /// per-request `ctx.data::<RlsContext>()`. `schemas` is the introspected
    /// metadata, so a plugin can tailor fields to the live schema
    fn query_fields(&self, _schemas: &[IntrospectedSchema]) -> Vec<Field> {
        Vec::new()
    }

    /// Contribute custom root `Mutation` fields. See [`Plugin::query_fields`]
    /// for how a resolver reaches the database
    fn mutation_fields(&self, _schemas: &[IntrospectedSchema]) -> Vec<Field> {
        Vec::new()
    }
}
