pub mod bridge;
pub mod compile;
pub mod executor;
pub mod plan;

pub use bridge::{condition_to_filter, filter_to_filter, order_to_exprs};
pub use compile::{compile_filter, compile_plan, compile_plan_count};
pub use executor::{execute_batch_lookup, execute_plan, execute_plan_count};
pub use plan::{FilterExpr, MutationKind, OrderByExpr, OrderDirection, PlanNode};

#[cfg(test)]
mod tests {
    use crate::plugin::Plugin;

    use super::compile::compile_plan;
    use super::plan::{FilterExpr, PlanNode};

    struct TenantIsolationPlugin;

    impl Plugin for TenantIsolationPlugin {
        fn name(&self) -> &'static str {
            "tenant-isolation"
        }

        fn on_plan(&self, plan: &mut PlanNode) {
            if let PlanNode::Scan { filter, table, .. } = plan
                && table == "users"
            {
                let tenant_filter = FilterExpr::Raw {
                    sql: "t.\"tenant_id\" = current_setting('app.tenant_id')".into(),
                    params: vec![],
                };
                *filter = Some(match filter.take() {
                    Some(existing) => FilterExpr::And(vec![existing, tenant_filter]),
                    None => tenant_filter,
                });
            }
        }
    }

    #[test]
    fn plugin_injects_filter_via_on_plan() {
        let mut plan = PlanNode::Scan {
            schema: "public".into(),
            table: "users".into(),
            filter: None,
            order_by: None,
            limit: None,
            offset: None,
            selections: vec![],
        };

        let plugin = TenantIsolationPlugin;
        plugin.on_plan(&mut plan);

        let (sql, _) = compile_plan(&plan);
        assert!(
            sql.contains("current_setting('app.tenant_id')"),
            "SQL should contain tenant filter: {sql}"
        );
    }

    #[test]
    fn plugin_composes_with_existing_filter() {
        let mut plan = PlanNode::Scan {
            schema: "public".into(),
            table: "users".into(),
            filter: Some(FilterExpr::Eq {
                column: "active".into(),
                value: serde_json::json!(true), cast: None,
            }),
            order_by: None,
            limit: None,
            offset: None,
            selections: vec![],
        };

        let plugin = TenantIsolationPlugin;
        plugin.on_plan(&mut plan);

        let (sql, params) = compile_plan(&plan);
        assert!(sql.contains("\"active\" = $1"), "should keep original filter: {sql}");
        assert!(sql.contains("current_setting"), "should add tenant filter: {sql}");
        assert_eq!(params.len(), 1);
    }

    #[test]
    fn on_plan_skips_non_matching_tables() {
        let mut plan = PlanNode::Scan {
            schema: "public".into(),
            table: "posts".into(),
            filter: None,
            order_by: None,
            limit: None,
            offset: None,
            selections: vec![],
        };

        let plugin = TenantIsolationPlugin;
        plugin.on_plan(&mut plan);

        let (sql, _) = compile_plan(&plan);
        assert!(
            !sql.contains("current_setting"),
            "posts table should not get tenant filter: {sql}"
        );
    }
}
