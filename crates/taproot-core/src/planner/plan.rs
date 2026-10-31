use serde_json::Value;

/// A node in the query execution plan tree
#[derive(Debug, Clone)]
pub enum PlanNode {
    /// Root or nested table scan with optional filters, ordering, pagination
    Scan {
        schema: String,
        table: String,
        filter: Option<FilterExpr>,
        order_by: Option<Vec<OrderByExpr>>,
        limit: Option<i64>,
        offset: Option<i64>,
        selections: Vec<Self>,
    },
    /// Batched entity lookup: `WHERE key_column = ANY($1)`
    BatchLookup {
        schema: String,
        table: String,
        key_column: String,
        keys: Vec<Value>,
        selections: Vec<Self>,
    },
    /// Lateral join for has-many relationships
    LateralJoin {
        parent_key: String,
        child_schema: String,
        child_table: String,
        child_key: String,
        filter: Option<FilterExpr>,
        order_by: Option<Vec<OrderByExpr>>,
        limit: Option<i64>,
        offset: Option<i64>,
        selections: Vec<Self>,
    },
    /// Direct column selection
    Column {
        name: String,
    },
    /// Mutation operation
    Mutation {
        kind: MutationKind,
        schema: String,
        table: String,
        returning: Vec<Self>,
    },
}

/// A filter expression that compiles to a SQL WHERE clause
#[derive(Debug, Clone)]
pub enum FilterExpr {
    Eq { column: String, value: Value, cast: Option<String> },
    Ne { column: String, value: Value, cast: Option<String> },
    Gt { column: String, value: Value, cast: Option<String> },
    Gte { column: String, value: Value, cast: Option<String> },
    Lt { column: String, value: Value, cast: Option<String> },
    Lte { column: String, value: Value, cast: Option<String> },
    In { column: String, values: Vec<Value>, cast: Option<String> },
    Like { column: String, pattern: String },
    ILike { column: String, pattern: String },
    IsNull { column: String, is_null: bool },
    /// `<value> <op> ANY(<column>)` for array columns, e.g. `$1 = ANY("tags")`.
    /// `op` is a scalar comparison operator (`=`, `<>`, `<`, `<=`, `>`, `>=`)
    ArrayAny { column: String, op: String, value: Value, cast: Option<String> },
    And(Vec<Self>),
    Or(Vec<Self>),
    Not(Box<Self>),
    /// Raw SQL fragment with parameters (for plugin-injected clauses)
    Raw { sql: String, params: Vec<Value> },
}

/// Ordering direction for a column
#[derive(Debug, Clone)]
pub struct OrderByExpr {
    pub column: String,
    pub direction: OrderDirection,
}

/// Sort direction with nulls handling
#[derive(Debug, Clone, Copy)]
pub enum OrderDirection {
    Asc,
    Desc,
    AscNullsFirst,
    AscNullsLast,
    DescNullsFirst,
    DescNullsLast,
}

/// Mutation operation type
#[derive(Debug, Clone)]
pub enum MutationKind {
    Create {
        input: Value,
    },
    Update {
        /// Primary key columns: `(column_name, value)` pairs
        pk_columns: Vec<(String, Value)>,
        patch: Value,
    },
    Delete {
        /// Primary key columns: `(column_name, value)` pairs
        pk_columns: Vec<(String, Value)>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_node_defaults() {
        let node = PlanNode::Scan {
            schema: "public".into(),
            table: "users".into(),
            filter: None,
            order_by: None,
            limit: None,
            offset: None,
            selections: vec![PlanNode::Column { name: "id".into() }],
        };
        assert!(matches!(node, PlanNode::Scan { .. }));
    }

    #[test]
    fn scan_with_filter() {
        let node = PlanNode::Scan {
            schema: "public".into(),
            table: "users".into(),
            filter: Some(FilterExpr::Eq {
                column: "id".into(),
                value: serde_json::json!("abc-123"), cast: None,
            }),
            order_by: None,
            limit: None,
            offset: None,
            selections: vec![],
        };
        assert!(matches!(node, PlanNode::Scan { filter: Some(_), .. }));
    }

    #[test]
    fn batch_lookup_node() {
        let node = PlanNode::BatchLookup {
            schema: "public".into(),
            table: "users".into(),
            key_column: "id".into(),
            keys: vec![serde_json::json!("a"), serde_json::json!("b")],
            selections: vec![PlanNode::Column {
                name: "name".into(),
            }],
        };
        assert!(matches!(node, PlanNode::BatchLookup { .. }));
    }

    #[test]
    fn lateral_join_node() {
        let node = PlanNode::LateralJoin {
            parent_key: "id".into(),
            child_schema: "public".into(),
            child_table: "posts".into(),
            child_key: "author_id".into(),
            filter: None,
            order_by: None,
            limit: None,
            offset: None,
            selections: vec![],
        };
        assert!(matches!(node, PlanNode::LateralJoin { .. }));
    }

    #[test]
    fn filter_expr_compound() {
        let expr = FilterExpr::And(vec![
            FilterExpr::Eq {
                column: "name".into(),
                value: serde_json::json!("Alice"), cast: None,
            },
            FilterExpr::IsNull {
                column: "deleted_at".into(),
                is_null: true,
            },
        ]);
        assert!(matches!(expr, FilterExpr::And(_)));
    }
}
