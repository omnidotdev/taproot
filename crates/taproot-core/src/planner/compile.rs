use std::fmt::Write;

use serde_json::Value;

use super::{FilterExpr, OrderByExpr, OrderDirection, PlanNode};

/// Quote a SQL identifier, escaping embedded double quotes
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Compile a binary comparison operator (Eq, Ne, Gt, etc.)
fn compile_binop(
    column: &str,
    op: &str,
    value: &Value,
    param_offset: usize,
    cast: Option<&str>,
) -> (String, Vec<Value>) {
    let cast = cast.unwrap_or_default();
    let sql = format!("{} {op} ${param_offset}{cast}", quote_ident(column));
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
        FilterExpr::Eq { column, value, cast } => {
            compile_binop(column, "=", value, param_offset, cast.as_deref())
        }
        FilterExpr::Ne { column, value, cast } => {
            compile_binop(column, "!=", value, param_offset, cast.as_deref())
        }
        FilterExpr::Gt { column, value, cast } => {
            compile_binop(column, ">", value, param_offset, cast.as_deref())
        }
        FilterExpr::Gte { column, value, cast } => {
            compile_binop(column, ">=", value, param_offset, cast.as_deref())
        }
        FilterExpr::Lt { column, value, cast } => {
            compile_binop(column, "<", value, param_offset, cast.as_deref())
        }
        FilterExpr::Lte { column, value, cast } => {
            compile_binop(column, "<=", value, param_offset, cast.as_deref())
        }

        FilterExpr::In { column, values, cast } => {
            let cast = cast.as_deref().unwrap_or_default();
            let placeholders: Vec<String> = (0..values.len())
                .map(|i| format!("${}{cast}", param_offset + i))
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

        FilterExpr::ArrayAny { column, op, value, cast } => {
            let cast = cast.as_deref().unwrap_or_default();
            let sql = format!("${param_offset}{cast} {op} ANY({})", quote_ident(column));
            (sql, vec![value.clone()])
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

/// Append optional filter, ORDER BY, LIMIT, and OFFSET clauses to a SQL buffer
fn append_tail_clauses(
    buf: &mut String,
    filter: Option<&FilterExpr>,
    order_by: Option<&Vec<OrderByExpr>>,
    limit: Option<i64>,
    offset: Option<i64>,
    param_offset: &mut usize,
    params: &mut Vec<Value>,
) {
    if let Some(cf) = filter {
        let (filter_sql, filter_params) = compile_filter(cf, *param_offset);
        *param_offset += filter_params.len();
        params.extend(filter_params);
        let _ = write!(buf, " AND {filter_sql}");
    }

    if let Some(order_exprs) = order_by
        && !order_exprs.is_empty()
    {
        let _ = write!(buf, " ORDER BY {}", compile_order_by(order_exprs));
    }

    if let Some(lim) = limit {
        let _ = write!(buf, " LIMIT {lim}");
    }

    if let Some(off) = offset {
        let _ = write!(buf, " OFFSET {off}");
    }
}

/// Compile a single `LateralJoin` node into a `LEFT JOIN LATERAL (...)` clause
///
/// Handles both leaf laterals (no children) and nested laterals (with
/// `LateralJoin` children in `selections`) by recursing.
///
/// - `parent_alias`: the alias of the parent table (e.g. `"t"`, `"c0"`)
/// - `prefix`: the lateral join alias prefix (e.g. `"lj0"`, `"lj0_0"`)
/// - `param_offset`: running 1-based parameter counter, mutated in place
fn compile_lateral_clause(
    lateral: &PlanNode,
    parent_alias: &str,
    prefix: &str,
    param_offset: &mut usize,
) -> (String, Vec<Value>) {
    let PlanNode::LateralJoin {
        parent_key,
        child_schema,
        child_table,
        child_key,
        filter: child_filter,
        order_by: child_order_by,
        limit: child_limit,
        offset: child_offset,
        selections,
    } = lateral
    else {
        return (String::new(), Vec::new());
    };

    let mut all_params: Vec<Value> = Vec::new();

    // Collect nested lateral children
    let nested_laterals: Vec<_> = selections
        .iter()
        .filter(|s| matches!(s, PlanNode::LateralJoin { .. }))
        .collect();

    let mut clause = String::new();

    if nested_laterals.is_empty() {
        // Leaf lateral: simple aggregation directly from the child table
        let _ = write!(
            clause,
            " LEFT JOIN LATERAL (SELECT COALESCE(json_agg(row_to_json(c.*)), '[]'::json) AS rows FROM {}.{} c WHERE c.{} = {}.{}",
            quote_ident(child_schema),
            quote_ident(child_table),
            quote_ident(child_key),
            parent_alias,
            quote_ident(parent_key),
        );

        append_tail_clauses(
            &mut clause,
            child_filter.as_ref(),
            child_order_by.as_ref(),
            *child_limit,
            *child_offset,
            param_offset,
            &mut all_params,
        );

        let _ = write!(clause, ") {prefix} ON true");
    } else {
        // Nested lateral: wrap in a subquery that includes its own lateral joins
        let child_alias = prefix.replacen("lj", "c", 1);
        let sub_alias = format!("sub_{prefix}");

        // Build nested lateral clauses and select columns
        let mut nested_select = format!("{child_alias}.*");
        let mut nested_lateral_clauses = String::new();

        for (i, nested) in nested_laterals.iter().enumerate() {
            let nested_prefix = format!("{prefix}_{i}");
            if let PlanNode::LateralJoin {
                child_table: nested_child_table,
                ..
            } = nested
            {
                let _ = write!(
                    nested_select,
                    ", {nested_prefix}.rows AS \"__lateral_{nested_child_table}\""
                );
            }

            let (nested_clause, nested_params) =
                compile_lateral_clause(nested, &child_alias, &nested_prefix, param_offset);
            all_params.extend(nested_params);
            nested_lateral_clauses.push_str(&nested_clause);
        }

        // Build the inner FROM clause
        let mut inner_from = format!(
            "SELECT {nested_select} FROM {}.{} {child_alias}{nested_lateral_clauses} WHERE {child_alias}.{} = {}.{}",
            quote_ident(child_schema),
            quote_ident(child_table),
            quote_ident(child_key),
            parent_alias,
            quote_ident(parent_key),
        );

        append_tail_clauses(
            &mut inner_from,
            child_filter.as_ref(),
            child_order_by.as_ref(),
            *child_limit,
            *child_offset,
            param_offset,
            &mut all_params,
        );

        let _ = write!(
            clause,
            " LEFT JOIN LATERAL (SELECT COALESCE(json_agg(row_to_json({sub_alias}.*)), '[]'::json) AS rows FROM ({inner_from}) {sub_alias}) {prefix} ON true"
        );
    }

    (clause, all_params)
}

/// Compile a `Scan` node that contains `LateralJoin` children into a
/// `SELECT row_to_json(sub.*)` query with `LEFT JOIN LATERAL` subqueries
fn compile_scan_with_laterals(
    schema: &str,
    table: &str,
    filter: Option<&FilterExpr>,
    order_by: Option<&Vec<OrderByExpr>>,
    limit: Option<i64>,
    offset: Option<i64>,
    laterals: &[&PlanNode],
) -> (String, Vec<Value>) {
    let mut all_params: Vec<Value> = Vec::new();
    let mut param_offset: usize = 1;

    // Build SELECT columns: t.* plus lateral aliases
    let mut select_cols = String::from("t.*");
    for (i, lateral) in laterals.iter().enumerate() {
        if let PlanNode::LateralJoin { child_table, .. } = lateral {
            let _ = write!(select_cols, ", lj{i}.rows AS \"__lateral_{child_table}\"");
        }
    }

    // Build lateral join clauses
    let mut lateral_clauses = String::new();
    for (i, lateral) in laterals.iter().enumerate() {
        let prefix = format!("lj{i}");
        let (clause, params) =
            compile_lateral_clause(lateral, "t", &prefix, &mut param_offset);
        all_params.extend(params);
        lateral_clauses.push_str(&clause);
    }

    // Build inner query
    let mut inner = format!(
        "SELECT {select_cols} FROM {}.{} t{lateral_clauses}",
        quote_ident(schema),
        quote_ident(table),
    );

    if let Some(expr) = filter {
        let (where_sql, where_params) = compile_filter(expr, param_offset);
        all_params.extend(where_params);
        let _ = write!(inner, " WHERE {where_sql}");
    }

    if let Some(order_exprs) = order_by
        && !order_exprs.is_empty()
    {
        let _ = write!(inner, " ORDER BY {}", compile_order_by(order_exprs));
    }

    if let Some(lim) = limit {
        let _ = write!(inner, " LIMIT {lim}");
    }

    if let Some(off) = offset {
        let _ = write!(inner, " OFFSET {off}");
    }

    let sql = format!("SELECT row_to_json(sub.*) FROM ({inner}) sub");
    (sql, all_params)
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
            selections,
        } => {
            // Collect lateral join children
            let laterals: Vec<_> = selections
                .iter()
                .filter(|s| matches!(s, PlanNode::LateralJoin { .. }))
                .collect();

            if !laterals.is_empty() {
                return compile_scan_with_laterals(
                    schema,
                    table,
                    filter.as_ref(),
                    order_by.as_ref(),
                    *limit,
                    *offset,
                    &laterals,
                );
            }

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
        let expr = FilterExpr::Eq { column: "name".into(), value: serde_json::json!("Alice"), cast: None };
        let (sql, params) = compile_filter(&expr, 1);
        assert_eq!(sql, "\"name\" = $1");
        assert_eq!(params, vec![serde_json::json!("Alice")]);
    }

    #[test]
    fn compile_in() {
        let expr = FilterExpr::In { column: "id".into(), values: vec![serde_json::json!("a"), serde_json::json!("b")], cast: None };
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
            FilterExpr::Eq { column: "a".into(), value: serde_json::json!(1), cast: None },
            FilterExpr::Eq { column: "b".into(), value: serde_json::json!(2), cast: None },
        ]);
        let (sql, params) = compile_filter(&expr, 1);
        assert_eq!(sql, "(\"a\" = $1 AND \"b\" = $2)");
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn compile_or() {
        let expr = FilterExpr::Or(vec![
            FilterExpr::Eq { column: "x".into(), value: serde_json::json!(1), cast: None },
            FilterExpr::Eq { column: "y".into(), value: serde_json::json!(2), cast: None },
        ]);
        let (sql, params) = compile_filter(&expr, 1);
        assert_eq!(sql, "(\"x\" = $1 OR \"y\" = $2)");
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn compile_not() {
        let expr = FilterExpr::Not(Box::new(FilterExpr::Eq { column: "active".into(), value: serde_json::json!(false), cast: None }));
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
        let expr = FilterExpr::Eq { column: "x".into(), value: serde_json::json!(42), cast: None };
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
            (FilterExpr::Gt { column: "a".into(), value: serde_json::json!(1), cast: None }, ">"),
            (FilterExpr::Gte { column: "a".into(), value: serde_json::json!(1), cast: None }, ">="),
            (FilterExpr::Lt { column: "a".into(), value: serde_json::json!(1), cast: None }, "<"),
            (FilterExpr::Lte { column: "a".into(), value: serde_json::json!(1), cast: None }, "<="),
            (FilterExpr::Ne { column: "a".into(), value: serde_json::json!(1), cast: None }, "!="),
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
            filter: Some(FilterExpr::Eq { column: "id".into(), value: serde_json::json!("abc"), cast: None }),
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
    fn compile_lateral_join_simple() {
        let plan = PlanNode::Scan {
            schema: "public".into(),
            table: "users".into(),
            filter: None,
            order_by: None,
            limit: None,
            offset: None,
            selections: vec![PlanNode::LateralJoin {
                parent_key: "id".into(),
                child_schema: "public".into(),
                child_table: "posts".into(),
                child_key: "author_id".into(),
                filter: None,
                order_by: None,
                limit: None,
                offset: None,
                selections: vec![],
            }],
        };
        let (sql, params) = compile_plan(&plan);
        assert!(sql.contains("LATERAL"), "SQL should contain LATERAL: {sql}");
        assert!(sql.contains("json_agg"), "SQL should aggregate child rows: {sql}");
        assert!(sql.contains("\"author_id\" = t.\"id\""), "SQL should join on FK=PK: {sql}");
        assert!(params.is_empty());
    }

    #[test]
    fn compile_lateral_join_with_filter() {
        let plan = PlanNode::Scan {
            schema: "public".into(),
            table: "users".into(),
            filter: None,
            order_by: None,
            limit: None,
            offset: None,
            selections: vec![PlanNode::LateralJoin {
                parent_key: "id".into(),
                child_schema: "public".into(),
                child_table: "posts".into(),
                child_key: "author_id".into(),
                filter: Some(FilterExpr::Eq {
                    column: "published".into(),
                    value: serde_json::json!(true), cast: None,
                }),
                order_by: None,
                limit: None,
                offset: None,
                selections: vec![],
            }],
        };
        let (sql, params) = compile_plan(&plan);
        assert!(sql.contains("\"published\" = $1"), "SQL should contain child filter: {sql}");
        assert_eq!(params.len(), 1);
    }

    #[test]
    fn compile_lateral_join_with_order_limit() {
        let plan = PlanNode::Scan {
            schema: "public".into(),
            table: "users".into(),
            filter: None,
            order_by: None,
            limit: None,
            offset: None,
            selections: vec![PlanNode::LateralJoin {
                parent_key: "id".into(),
                child_schema: "public".into(),
                child_table: "posts".into(),
                child_key: "author_id".into(),
                filter: None,
                order_by: Some(vec![OrderByExpr {
                    column: "created_at".into(),
                    direction: OrderDirection::Desc,
                }]),
                limit: Some(10),
                offset: Some(5),
                selections: vec![],
            }],
        };
        let (sql, params) = compile_plan(&plan);
        assert!(sql.contains("ORDER BY"), "SQL should contain ORDER BY: {sql}");
        assert!(sql.contains("LIMIT 10"), "SQL should contain LIMIT: {sql}");
        assert!(sql.contains("OFFSET 5"), "SQL should contain OFFSET: {sql}");
        assert!(params.is_empty());
    }

    #[test]
    fn compile_lateral_join_with_parent_filter() {
        let plan = PlanNode::Scan {
            schema: "public".into(),
            table: "users".into(),
            filter: Some(FilterExpr::Eq {
                column: "active".into(),
                value: serde_json::json!(true), cast: None,
            }),
            order_by: None,
            limit: None,
            offset: None,
            selections: vec![PlanNode::LateralJoin {
                parent_key: "id".into(),
                child_schema: "public".into(),
                child_table: "posts".into(),
                child_key: "author_id".into(),
                filter: None,
                order_by: None,
                limit: None,
                offset: None,
                selections: vec![],
            }],
        };
        let (sql, params) = compile_plan(&plan);
        assert!(sql.contains("\"active\" = $1"), "Parent filter should be present: {sql}");
        assert!(sql.contains("LATERAL"), "Lateral join should still be present: {sql}");
        assert_eq!(params.len(), 1);
    }

    #[test]
    fn compile_lateral_join_multiple() {
        let plan = PlanNode::Scan {
            schema: "public".into(),
            table: "users".into(),
            filter: None,
            order_by: None,
            limit: None,
            offset: None,
            selections: vec![
                PlanNode::LateralJoin {
                    parent_key: "id".into(),
                    child_schema: "public".into(),
                    child_table: "posts".into(),
                    child_key: "author_id".into(),
                    filter: None,
                    order_by: None,
                    limit: None,
                    offset: None,
                    selections: vec![],
                },
                PlanNode::LateralJoin {
                    parent_key: "id".into(),
                    child_schema: "public".into(),
                    child_table: "comments".into(),
                    child_key: "user_id".into(),
                    filter: None,
                    order_by: None,
                    limit: None,
                    offset: None,
                    selections: vec![],
                },
            ],
        };
        let (sql, params) = compile_plan(&plan);
        assert!(sql.contains("lj0"), "SQL should contain lj0 alias: {sql}");
        assert!(sql.contains("lj1"), "SQL should contain lj1 alias: {sql}");
        assert!(sql.contains("\"__lateral_posts\""), "SQL should contain __lateral_posts column: {sql}");
        assert!(sql.contains("\"__lateral_comments\""), "SQL should contain __lateral_comments column: {sql}");
        assert!(params.is_empty());
    }

    #[test]
    fn compile_lateral_join_child_and_parent_filter() {
        let plan = PlanNode::Scan {
            schema: "public".into(),
            table: "users".into(),
            filter: Some(FilterExpr::Eq {
                column: "active".into(),
                value: serde_json::json!(true), cast: None,
            }),
            order_by: None,
            limit: None,
            offset: None,
            selections: vec![PlanNode::LateralJoin {
                parent_key: "id".into(),
                child_schema: "public".into(),
                child_table: "posts".into(),
                child_key: "author_id".into(),
                filter: Some(FilterExpr::Eq {
                    column: "published".into(),
                    value: serde_json::json!(true), cast: None,
                }),
                order_by: None,
                limit: None,
                offset: None,
                selections: vec![],
            }],
        };
        let (sql, params) = compile_plan(&plan);
        // Child filter is compiled first (inside the lateral subquery)
        assert!(sql.contains("\"published\" = $1"), "Child filter should get $1: {sql}");
        // Parent filter is compiled after lateral joins
        assert!(sql.contains("\"active\" = $2"), "Parent filter should get $2: {sql}");
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn compile_nested_lateral_join() {
        let plan = PlanNode::Scan {
            schema: "public".into(),
            table: "users".into(),
            filter: None,
            order_by: None,
            limit: None,
            offset: None,
            selections: vec![PlanNode::LateralJoin {
                parent_key: "id".into(),
                child_schema: "public".into(),
                child_table: "posts".into(),
                child_key: "author_id".into(),
                filter: None,
                order_by: None,
                limit: None,
                offset: None,
                selections: vec![PlanNode::LateralJoin {
                    parent_key: "id".into(),
                    child_schema: "public".into(),
                    child_table: "comments".into(),
                    child_key: "post_id".into(),
                    filter: None,
                    order_by: None,
                    limit: None,
                    offset: None,
                    selections: vec![],
                }],
            }],
        };
        let (sql, params) = compile_plan(&plan);

        let lateral_count = sql.matches("LATERAL").count();
        assert!(lateral_count >= 2, "Should have at least 2 LATERAL clauses: {sql}");
        assert!(sql.contains("\"posts\""), "Should reference posts: {sql}");
        assert!(sql.contains("\"comments\""), "Should reference comments: {sql}");
        let agg_count = sql.matches("json_agg").count();
        assert!(agg_count >= 2, "Should have at least 2 json_agg: {sql}");
        assert!(sql.contains("__lateral_posts"), "Should have lateral key for posts: {sql}");
        assert!(sql.contains("__lateral_comments"), "Should have lateral key for comments: {sql}");
        assert!(params.is_empty());
    }

    #[test]
    fn compile_nested_lateral_with_filters() {
        let plan = PlanNode::Scan {
            schema: "public".into(),
            table: "users".into(),
            filter: Some(FilterExpr::Eq {
                column: "active".into(),
                value: serde_json::json!(true), cast: None,
            }),
            order_by: None,
            limit: None,
            offset: None,
            selections: vec![PlanNode::LateralJoin {
                parent_key: "id".into(),
                child_schema: "public".into(),
                child_table: "posts".into(),
                child_key: "author_id".into(),
                filter: Some(FilterExpr::Eq {
                    column: "published".into(),
                    value: serde_json::json!(true), cast: None,
                }),
                order_by: None,
                limit: None,
                offset: None,
                selections: vec![PlanNode::LateralJoin {
                    parent_key: "id".into(),
                    child_schema: "public".into(),
                    child_table: "comments".into(),
                    child_key: "post_id".into(),
                    filter: Some(FilterExpr::Eq {
                        column: "approved".into(),
                        value: serde_json::json!(true), cast: None,
                    }),
                    order_by: None,
                    limit: None,
                    offset: None,
                    selections: vec![],
                }],
            }],
        };
        let (sql, params) = compile_plan(&plan);
        assert_eq!(params.len(), 3, "Should have 3 filter params: {sql}");
        assert!(sql.contains("\"published\""), "Should filter posts: {sql}");
        assert!(sql.contains("\"approved\""), "Should filter comments: {sql}");
        assert!(sql.contains("\"active\""), "Should filter users: {sql}");
    }

    #[test]
    fn compile_scan_count() {
        let plan = PlanNode::Scan {
            schema: "public".into(),
            table: "posts".into(),
            filter: Some(FilterExpr::Eq { column: "author_id".into(), value: serde_json::json!("u1"), cast: None }),
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
