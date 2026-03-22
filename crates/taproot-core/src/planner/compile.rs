use std::fmt::Write;

use serde_json::Value;

use super::{FilterExpr, OrderByExpr, OrderDirection, PlanNode};

/// Quote a SQL identifier, escaping embedded double quotes
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Compile a binary comparison operator (Eq, Ne, Gt, etc.)
fn compile_binop(column: &str, op: &str, value: &Value, param_offset: usize) -> (String, Vec<Value>) {
    let sql = format!("{} {op} ${param_offset}", quote_ident(column));
    (sql, vec![value.clone()])
}

/// Compile a `FilterExpr` tree into a parameterized SQL WHERE fragment
///
/// `param_offset` is 1-based (SQL uses `$1`, `$2`, ...). Returns the SQL
/// string and a vector of parameter values whose positions correspond to the
/// placeholders emitted in the SQL.
#[must_use]
pub fn compile_filter(expr: &FilterExpr, param_offset: usize) -> (String, Vec<Value>) {
    match expr {
        FilterExpr::Eq { column, value } => compile_binop(column, "=", value, param_offset),
        FilterExpr::Ne { column, value } => compile_binop(column, "!=", value, param_offset),
        FilterExpr::Gt { column, value } => compile_binop(column, ">", value, param_offset),
        FilterExpr::Gte { column, value } => compile_binop(column, ">=", value, param_offset),
        FilterExpr::Lt { column, value } => compile_binop(column, "<", value, param_offset),
        FilterExpr::Lte { column, value } => compile_binop(column, "<=", value, param_offset),

        FilterExpr::In { column, values } => {
            let placeholders: Vec<String> = (0..values.len())
                .map(|i| format!("${}", param_offset + i))
                .collect();
            let sql = format!("{} IN ({})", quote_ident(column), placeholders.join(", "));
            (sql, values.clone())
        }

        FilterExpr::Like { column, pattern } => {
            let sql = format!("{} LIKE ${param_offset}", quote_ident(column));
            (sql, vec![Value::String(pattern.clone())])
        }

        FilterExpr::ILike { column, pattern } => {
            let sql = format!("{} ILIKE ${param_offset}", quote_ident(column));
            (sql, vec![Value::String(pattern.clone())])
        }

        FilterExpr::IsNull { column, is_null } => {
            let suffix = if *is_null { "IS NULL" } else { "IS NOT NULL" };
            let sql = format!("{} {suffix}", quote_ident(column));
            (sql, vec![])
        }

        FilterExpr::And(children) => compile_compound(children, "AND", param_offset),
        FilterExpr::Or(children) => compile_compound(children, "OR", param_offset),

        FilterExpr::Not(inner) => {
            let (inner_sql, params) = compile_filter(inner, param_offset);
            (format!("NOT ({inner_sql})"), params)
        }

        FilterExpr::Raw { sql, params } => (sql.clone(), params.clone()),
    }
}

/// Compile a compound expression (AND / OR) by recursively compiling children
/// and tracking the running param offset
fn compile_compound(children: &[FilterExpr], joiner: &str, param_offset: usize) -> (String, Vec<Value>) {
    let mut fragments = Vec::with_capacity(children.len());
    let mut all_params = Vec::new();
    let mut offset = param_offset;

    for child in children {
        let (sql, params) = compile_filter(child, offset);
        offset += params.len();
        fragments.push(sql);
        all_params.extend(params);
    }

    let sql = format!("({})", fragments.join(&format!(" {joiner} ")));
    (sql, all_params)
}

/// Compile an `OrderDirection` to its SQL representation
const fn compile_direction(dir: OrderDirection) -> &'static str {
    match dir {
        OrderDirection::Asc => "ASC",
        OrderDirection::Desc => "DESC",
        OrderDirection::AscNullsFirst => "ASC NULLS FIRST",
        OrderDirection::AscNullsLast => "ASC NULLS LAST",
        OrderDirection::DescNullsFirst => "DESC NULLS FIRST",
        OrderDirection::DescNullsLast => "DESC NULLS LAST",
    }
}

/// Compile an ORDER BY clause from a list of order expressions
fn compile_order_by(exprs: &[OrderByExpr]) -> String {
    let parts: Vec<String> = exprs
        .iter()
        .map(|e| format!("{} {}", quote_ident(&e.column), compile_direction(e.direction)))
        .collect();
    parts.join(", ")
}

/// Compile a `PlanNode` into a parameterized SQL query
#[must_use]
pub fn compile_plan(plan: &PlanNode) -> (String, Vec<Value>) {
    match plan {
        PlanNode::Scan {
            schema,
            table,
            filter,
            order_by,
            limit,
            offset,
            ..
        } => {
            let mut sql = format!(
                "SELECT row_to_json(t.*) FROM {}.{} t",
                quote_ident(schema),
                quote_ident(table),
            );

            let params = filter.as_ref().map_or_else(Vec::new, |expr| {
                let (where_sql, where_params) = compile_filter(expr, 1);
                let _ = write!(sql, " WHERE {where_sql}");
                where_params
            });

            if let Some(order_exprs) = order_by
                && !order_exprs.is_empty()
            {
                let _ = write!(sql, " ORDER BY {}", compile_order_by(order_exprs));
            }

            if let Some(lim) = limit {
                let _ = write!(sql, " LIMIT {lim}");
            }

            if let Some(off) = offset {
                let _ = write!(sql, " OFFSET {off}");
            }

            (sql, params)
        }

        PlanNode::BatchLookup {
            schema,
            table,
            key_column,
            keys,
            ..
        } => {
            let placeholders: Vec<String> = (1..=keys.len())
                .map(|i| format!("${i}"))
                .collect();
            let sql = format!(
                "SELECT row_to_json(t.*) FROM {}.{} t WHERE {} IN ({})",
                quote_ident(schema),
                quote_ident(table),
                quote_ident(key_column),
                placeholders.join(", "),
            );
            (sql, keys.clone())
        }

        // Other node types are not yet supported
        _ => (String::new(), Vec::new()),
    }
}

/// Compile a `PlanNode` into a `SELECT COUNT(*)` query (for connection pagination)
#[must_use]
pub fn compile_plan_count(plan: &PlanNode) -> (String, Vec<Value>) {
    match plan {
        PlanNode::Scan {
            schema,
            table,
            filter,
            ..
        } => {
            let mut sql = format!(
                "SELECT COUNT(*) AS count FROM {}.{} t",
                quote_ident(schema),
                quote_ident(table),
            );

            let params = filter.as_ref().map_or_else(Vec::new, |expr| {
                let (where_sql, where_params) = compile_filter(expr, 1);
                let _ = write!(sql, " WHERE {where_sql}");
                where_params
            });

            (sql, params)
        }

        // Other node types are not yet supported
        _ => (String::new(), Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compile_eq() {
        let expr = FilterExpr::Eq { column: "name".into(), value: serde_json::json!("Alice") };
        let (sql, params) = compile_filter(&expr, 1);
        assert_eq!(sql, "\"name\" = $1");
        assert_eq!(params, vec![serde_json::json!("Alice")]);
    }

    #[test]
    fn compile_in() {
        let expr = FilterExpr::In { column: "id".into(), values: vec![serde_json::json!("a"), serde_json::json!("b")] };
        let (sql, params) = compile_filter(&expr, 1);
        assert_eq!(sql, "\"id\" IN ($1, $2)");
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn compile_is_null() {
        let expr = FilterExpr::IsNull { column: "deleted_at".into(), is_null: true };
        let (sql, params) = compile_filter(&expr, 1);
        assert_eq!(sql, "\"deleted_at\" IS NULL");
        assert!(params.is_empty());
    }

    #[test]
    fn compile_is_not_null() {
        let expr = FilterExpr::IsNull { column: "deleted_at".into(), is_null: false };
        let (sql, params) = compile_filter(&expr, 1);
        assert_eq!(sql, "\"deleted_at\" IS NOT NULL");
        assert!(params.is_empty());
    }

    #[test]
    fn compile_and() {
        let expr = FilterExpr::And(vec![
            FilterExpr::Eq { column: "a".into(), value: serde_json::json!(1) },
            FilterExpr::Eq { column: "b".into(), value: serde_json::json!(2) },
        ]);
        let (sql, params) = compile_filter(&expr, 1);
        assert_eq!(sql, "(\"a\" = $1 AND \"b\" = $2)");
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn compile_or() {
        let expr = FilterExpr::Or(vec![
            FilterExpr::Eq { column: "x".into(), value: serde_json::json!(1) },
            FilterExpr::Eq { column: "y".into(), value: serde_json::json!(2) },
        ]);
        let (sql, params) = compile_filter(&expr, 1);
        assert_eq!(sql, "(\"x\" = $1 OR \"y\" = $2)");
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn compile_not() {
        let expr = FilterExpr::Not(Box::new(FilterExpr::Eq { column: "active".into(), value: serde_json::json!(false) }));
        let (sql, params) = compile_filter(&expr, 1);
        assert_eq!(sql, "NOT (\"active\" = $1)");
        assert_eq!(params.len(), 1);
    }

    #[test]
    fn compile_like() {
        let expr = FilterExpr::Like { column: "name".into(), pattern: "%test%".into() };
        let (sql, params) = compile_filter(&expr, 1);
        assert_eq!(sql, "\"name\" LIKE $1");
        assert_eq!(params, vec![serde_json::json!("%test%")]);
    }

    #[test]
    fn compile_param_offset() {
        let expr = FilterExpr::Eq { column: "x".into(), value: serde_json::json!(42) };
        let (sql, _) = compile_filter(&expr, 5);
        assert_eq!(sql, "\"x\" = $5");
    }

    #[test]
    fn compile_raw() {
        let expr = FilterExpr::Raw { sql: "current_setting('app.tenant_id') = tenant_id".into(), params: vec![] };
        let (sql, params) = compile_filter(&expr, 1);
        assert_eq!(sql, "current_setting('app.tenant_id') = tenant_id");
        assert!(params.is_empty());
    }

    #[test]
    fn compile_comparison_operators() {
        for (expr, op) in [
            (FilterExpr::Gt { column: "a".into(), value: serde_json::json!(1) }, ">"),
            (FilterExpr::Gte { column: "a".into(), value: serde_json::json!(1) }, ">="),
            (FilterExpr::Lt { column: "a".into(), value: serde_json::json!(1) }, "<"),
            (FilterExpr::Lte { column: "a".into(), value: serde_json::json!(1) }, "<="),
            (FilterExpr::Ne { column: "a".into(), value: serde_json::json!(1) }, "!="),
        ] {
            let (sql, _) = compile_filter(&expr, 1);
            assert_eq!(sql, format!("\"a\" {op} $1"));
        }
    }

    #[test]
    fn compile_scan_simple() {
        let plan = PlanNode::Scan {
            schema: "public".into(),
            table: "users".into(),
            filter: None,
            order_by: None,
            limit: None,
            offset: None,
            selections: vec![PlanNode::Column { name: "id".into() }],
        };
        let (sql, params) = compile_plan(&plan);
        assert_eq!(sql, "SELECT row_to_json(t.*) FROM \"public\".\"users\" t");
        assert!(params.is_empty());
    }

    #[test]
    fn compile_scan_with_filter() {
        let plan = PlanNode::Scan {
            schema: "public".into(),
            table: "users".into(),
            filter: Some(FilterExpr::Eq { column: "id".into(), value: serde_json::json!("abc") }),
            order_by: None,
            limit: None,
            offset: None,
            selections: vec![],
        };
        let (sql, params) = compile_plan(&plan);
        assert_eq!(sql, "SELECT row_to_json(t.*) FROM \"public\".\"users\" t WHERE \"id\" = $1");
        assert_eq!(params, vec![serde_json::json!("abc")]);
    }

    #[test]
    fn compile_scan_with_order_limit_offset() {
        let plan = PlanNode::Scan {
            schema: "public".into(),
            table: "users".into(),
            filter: None,
            order_by: Some(vec![
                OrderByExpr { column: "name".into(), direction: OrderDirection::Asc },
                OrderByExpr { column: "id".into(), direction: OrderDirection::DescNullsLast },
            ]),
            limit: Some(10),
            offset: Some(20),
            selections: vec![],
        };
        let (sql, params) = compile_plan(&plan);
        assert_eq!(
            sql,
            "SELECT row_to_json(t.*) FROM \"public\".\"users\" t ORDER BY \"name\" ASC, \"id\" DESC NULLS LAST LIMIT 10 OFFSET 20"
        );
        assert!(params.is_empty());
    }

    #[test]
    fn compile_batch_lookup() {
        let plan = PlanNode::BatchLookup {
            schema: "public".into(),
            table: "users".into(),
            key_column: "id".into(),
            keys: vec![serde_json::json!("a"), serde_json::json!("b"), serde_json::json!("c")],
            selections: vec![],
        };
        let (sql, params) = compile_plan(&plan);
        assert_eq!(
            sql,
            "SELECT row_to_json(t.*) FROM \"public\".\"users\" t WHERE \"id\" IN ($1, $2, $3)"
        );
        assert_eq!(params.len(), 3);
    }

    #[test]
    fn compile_scan_count() {
        let plan = PlanNode::Scan {
            schema: "public".into(),
            table: "posts".into(),
            filter: Some(FilterExpr::Eq { column: "author_id".into(), value: serde_json::json!("u1") }),
            order_by: None,
            limit: None,
            offset: None,
            selections: vec![],
        };
        let (sql, params) = compile_plan_count(&plan);
        assert_eq!(
            sql,
            "SELECT COUNT(*) AS count FROM \"public\".\"posts\" t WHERE \"author_id\" = $1"
        );
        assert_eq!(params.len(), 1);
    }
}
