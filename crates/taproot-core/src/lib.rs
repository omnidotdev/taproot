pub mod db;
pub mod embed;
pub mod error;
pub mod introspect;
pub mod planner;
pub mod plugin;
pub mod registry;
pub mod subscription;

#[cfg(feature = "lattice")]
pub mod lattice;

pub use embed::{TaprootSubgraph, TaprootSubgraphOptions};
pub use introspect::{introspect, IntrospectOptions, IntrospectedSchema};
#[cfg(feature = "lattice")]
pub use lattice::TaprootExecutor;
pub use planner::{
    compile_filter, compile_plan, compile_plan_count, execute_batch_lookup, execute_plan,
    execute_plan_count, FilterExpr, MutationKind, OrderByExpr, OrderDirection, PlanNode,
};
pub use plugin::{Plugin, SmartCommentsPlugin};
pub use registry::{build_schema, SchemaOptions};
