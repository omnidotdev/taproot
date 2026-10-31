use serde_json::Value;

use crate::introspect::ColumnInfo;

use super::plan::{FilterExpr, OrderByExpr, OrderDirection};

/// Escape `%` and `_` characters for use in SQL LIKE patterns
fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// Convert a single column's `{Scalar}Filter` operator object into
/// `FilterExpr`s, using the standard connection-filter operator
/// names
fn parse_filter_ops(col: &ColumnInfo, ops: &Value, schema: &str) -> Vec<FilterExpr> {
    let Some(obj) = ops.as_object() else {
        return Vec::new();
    };

    if col.is_array {
        return parse_array_ops(col, obj);
    }

    let column = col.name.as_str();
    let suffix = col.pg_cast_suffix(schema);
    let cast = if suffix.is_empty() { None } else { Some(suffix) };

    let cmp = |make: fn(String, Value, Option<String>) -> FilterExpr, val: &Value| {
        Some(make(column.to_string(), val.clone(), cast.clone()))
    };
    let like = |pattern: String| FilterExpr::Like { column: column.to_string(), pattern };
    let insensitive = |pattern: String| FilterExpr::ILike { column: column.to_string(), pattern };

    let mut filters = Vec::new();

    for (op, val) in obj {
        let expr = match op.as_str() {
            "equalTo" => cmp(|c, v, k| FilterExpr::Eq { column: c, value: v, cast: k }, val),
            "notEqualTo" | "distinctFrom" => {
                cmp(|c, v, k| FilterExpr::Ne { column: c, value: v, cast: k }, val)
            }
            "greaterThan" => cmp(|c, v, k| FilterExpr::Gt { column: c, value: v, cast: k }, val),
            "greaterThanOrEqualTo" => {
                cmp(|c, v, k| FilterExpr::Gte { column: c, value: v, cast: k }, val)
            }
            "lessThan" => cmp(|c, v, k| FilterExpr::Lt { column: c, value: v, cast: k }, val),
            "lessThanOrEqualTo" => {
                cmp(|c, v, k| FilterExpr::Lte { column: c, value: v, cast: k }, val)
            }
            "in" => val.as_array().map(|arr| FilterExpr::In {
                column: column.to_string(),
                values: arr.clone(),
                cast: cast.clone(),
            }),
            "notIn" => val.as_array().map(|arr| {
                FilterExpr::Not(Box::new(FilterExpr::In {
                    column: column.to_string(),
                    values: arr.clone(),
                    cast: cast.clone(),
                }))
            }),
            "like" => val.as_str().map(|s| like(s.to_string())),
            "likeInsensitive" => val.as_str().map(|s| insensitive(s.to_string())),
            "includes" => val.as_str().map(|s| like(format!("%{}%", escape_like(s)))),
            "includesInsensitive" => val.as_str().map(|s| insensitive(format!("%{}%", escape_like(s)))),
            "startsWith" => val.as_str().map(|s| like(format!("{}%", escape_like(s)))),
            "startsWithInsensitive" => val.as_str().map(|s| insensitive(format!("{}%", escape_like(s)))),
            "endsWith" => val.as_str().map(|s| like(format!("%{}", escape_like(s)))),
            "endsWithInsensitive" => val.as_str().map(|s| insensitive(format!("%{}", escape_like(s)))),
            "isNull" => val.as_bool().map(|b| FilterExpr::IsNull {
                column: column.to_string(),
                is_null: b,
            }),
            _ => None,
        };

        if let Some(e) = expr {
            filters.push(e);
        }
    }

    filters
}

/// Convert a `{Scalar}ListFilter` operator object (for an array column) into
/// `FilterExpr`s. Supports `isNull` and the `any*` membership operators, which
/// compile to `<value> <op> ANY(column)`.
fn parse_array_ops(col: &ColumnInfo, ops: &serde_json::Map<String, Value>) -> Vec<FilterExpr> {
    let column = col.name.clone();
    // Cast the scalar operand for date/time element types; other element types
    // (uuid, int, text, bool) already bind to a compatible type
    let elem = col.udt_name.strip_prefix('_').unwrap_or(&col.udt_name);
    let cast = matches!(
        elem,
        "date" | "time" | "timetz" | "timestamp" | "timestamptz" | "interval"
    )
    .then(|| format!("::{elem}"));

    let mut filters = Vec::new();
    for (op, val) in ops {
        let sql_op = match op.as_str() {
            "anyEqualTo" => "=",
            "anyNotEqualTo" => "<>",
            "anyLessThan" => "<",
            "anyLessThanOrEqualTo" => "<=",
            "anyGreaterThan" => ">",
            "anyGreaterThanOrEqualTo" => ">=",
            "isNull" => {
                if let Some(b) = val.as_bool() {
                    filters.push(FilterExpr::IsNull {
                        column: column.clone(),
                        is_null: b,
                    });
                }
                continue;
            }
            _ => continue,
        };
        filters.push(FilterExpr::ArrayAny {
            column: column.clone(),
            op: sql_op.to_string(),
            value: val.clone(),
            cast: cast.clone(),
        });
    }
    filters
}

/// Combine a list of filter expressions into a single expression,
/// using AND when there are multiple
fn combine_filters(mut filters: Vec<FilterExpr>) -> Option<FilterExpr> {
    match filters.len() {
        0 => None,
        1 => filters.pop(),
        _ => Some(FilterExpr::And(filters)),
    }
}

/// Convert a GraphQL condition JSON object into a `FilterExpr` tree
///
/// Supports column-level operators (`eq`, `ne`, `gt`, `gte`, `lt`, `lte`,
/// `in`, `notIn`, `like`, `ilike`, `startsWith`, `endsWith`, `isNull`) and
/// logical combinators (`and`, `or`, `not`). Unknown columns are silently
/// skipped
#[must_use]
pub fn condition_to_filter(
    condition: &Value,
    columns: &[ColumnInfo],
    schema: &str,
) -> Option<FilterExpr> {
    let obj = condition.as_object()?;

    // `condition` is a flat set of per-column equality checks of the form
    // `{ field: value }`. Richer querying goes through `filter`
    let mut filters = Vec::new();
    for (key, val) in obj {
        if let Some(col) = columns
            .iter()
            .find(|c| crate::introspect::column_field_name(&c.name) == key.as_str())
        {
            let suffix = col.pg_cast_suffix(schema);
            let cast = if suffix.is_empty() { None } else { Some(suffix) };
            filters.push(FilterExpr::Eq {
                column: col.name.clone(),
                value: val.clone(),
                cast,
            });
        }
    }

    combine_filters(filters)
}

/// Convert a GraphQL `filter` JSON object into a `FilterExpr` tree.
///
/// Supports per-column `{Scalar}Filter` operator objects (see
/// [`parse_filter_ops`]) and the `and`, `or`, `not` combinators (the
/// standard connection-filter shape).
#[must_use]
pub fn filter_to_filter(
    filter: &Value,
    columns: &[ColumnInfo],
    schema: &str,
) -> Option<FilterExpr> {
    let obj = filter.as_object()?;

    let mut filters = Vec::new();

    for (key, val) in obj {
        match key.as_str() {
            "and" => {
                if let Some(arr) = val.as_array() {
                    let sub: Vec<FilterExpr> = arr
                        .iter()
                        .filter_map(|item| filter_to_filter(item, columns, schema))
                        .collect();
                    if !sub.is_empty() {
                        filters.push(FilterExpr::And(sub));
                    }
                }
            }
            "or" => {
                if let Some(arr) = val.as_array() {
                    let sub: Vec<FilterExpr> = arr
                        .iter()
                        .filter_map(|item| filter_to_filter(item, columns, schema))
                        .collect();
                    if !sub.is_empty() {
                        filters.push(FilterExpr::Or(sub));
                    }
                }
            }
            "not" => {
                if let Some(inner) = filter_to_filter(val, columns, schema) {
                    filters.push(FilterExpr::Not(Box::new(inner)));
                }
            }
            column_name => {
                // Skip unknown columns
                if let Some(col) = columns
                    .iter()
                    .find(|c| crate::introspect::column_field_name(&c.name) == column_name)
                {
                    filters.extend(parse_filter_ops(col, val, schema));
                }
            }
        }
    }

    combine_filters(filters)
}

/// Direction suffixes ordered longest-first for greedy matching
const DIRECTION_SUFFIXES: &[(&str, OrderDirection)] = &[
    ("_DESC_NULLS_FIRST", OrderDirection::DescNullsFirst),
    ("_DESC_NULLS_LAST", OrderDirection::DescNullsLast),
    ("_ASC_NULLS_FIRST", OrderDirection::AscNullsFirst),
    ("_ASC_NULLS_LAST", OrderDirection::AscNullsLast),
    ("_DESC", OrderDirection::Desc),
    ("_ASC", OrderDirection::Asc),
];

/// Convert a GraphQL `orderBy` JSON array of `SCREAMING_SNAKE` enum values
/// into a list of `OrderByExpr`
///
/// Each enum value like `"NAME_ASC"` or `"CREATED_AT_DESC_NULLS_LAST"` is
/// parsed by trying direction suffixes longest-first. The column part is
/// lowercased to produce the corresponding database column name. Unknown
/// columns are silently skipped
#[must_use]
pub fn order_to_exprs(order_by: &Value, columns: &[ColumnInfo]) -> Vec<OrderByExpr> {
    let Some(arr) = order_by.as_array() else {
        return Vec::new();
    };

    arr.iter()
        .filter_map(|item| {
            let s = item.as_str()?;

            // Try each suffix longest-first
            let (col_part, direction) = DIRECTION_SUFFIXES
                .iter()
                .find_map(|(suffix, dir)| {
                    s.strip_suffix(suffix).map(|col| (col, *dir))
                })?;

            // Match the enum's column token against each column's order token
            // (e.g. ROW_ID -> the `id` column), skipping unknown columns
            let column = columns
                .iter()
                .find(|c| crate::introspect::column_order_token(&c.name) == col_part)
                .map(|c| c.name.clone())?;

            Some(OrderByExpr { column, direction })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::introspect::ColumnInfo;

    fn text_col(name: &str) -> ColumnInfo {
        ColumnInfo {
            name: name.to_string(),
            data_type: "text".to_string(),
            udt_name: "text".to_string(),
            is_nullable: false,
            has_default: false,
            is_generated: false,
            is_array: false,
            comment: None,
            tags: HashMap::new(),
        }
    }

    fn test_columns() -> Vec<ColumnInfo> {
        vec![text_col("id"), text_col("name"), text_col("email")]
    }

    #[test]
    fn simple_eq_condition() {
        let condition = serde_json::json!({ "name": { "equalTo": "Alice" } });
        let result = condition_to_filter(&condition, &test_columns(), "public");
        assert!(result.is_some());
    }

    #[test]
    fn multiple_columns() {
        let condition = serde_json::json!({ "name": { "equalTo": "Alice" }, "email": { "like": "%example%" } });
        let result = condition_to_filter(&condition, &test_columns(), "public");
        assert!(result.is_some());
    }

    #[test]
    fn and_combinator() {
        let condition = serde_json::json!({ "and": [{ "name": { "equalTo": "Alice" } }, { "email": { "equalTo": "alice@example.com" } }] });
        let result = filter_to_filter(&condition, &test_columns(), "public");
        assert!(result.is_some());
    }

    #[test]
    fn or_combinator() {
        let condition = serde_json::json!({ "or": [{ "name": { "equalTo": "Alice" } }, { "name": { "equalTo": "Bob" } }] });
        let result = filter_to_filter(&condition, &test_columns(), "public");
        assert!(result.is_some());
    }

    #[test]
    fn not_combinator() {
        let condition = serde_json::json!({ "not": { "name": { "equalTo": "Alice" } } });
        let result = filter_to_filter(&condition, &test_columns(), "public");
        assert!(result.is_some());
    }

    #[test]
    fn is_null_filter() {
        let condition = serde_json::json!({ "email": { "isNull": true } });
        let result = condition_to_filter(&condition, &test_columns(), "public");
        assert!(result.is_some());
    }

    #[test]
    fn in_filter() {
        let condition = serde_json::json!({ "name": { "in": ["Alice", "Bob"] } });
        let result = condition_to_filter(&condition, &test_columns(), "public");
        assert!(result.is_some());
    }

    #[test]
    fn empty_condition_returns_none() {
        let condition = serde_json::json!({});
        let result = condition_to_filter(&condition, &test_columns(), "public");
        assert!(result.is_none());
    }

    #[test]
    fn unknown_column_ignored() {
        let condition = serde_json::json!({ "nonexistent": { "eq": "x" } });
        let result = condition_to_filter(&condition, &test_columns(), "public");
        assert!(result.is_none());
    }

    #[test]
    fn order_by_single_asc() {
        let order = serde_json::json!(["NAME_ASC"]);
        let result = order_to_exprs(&order, &test_columns());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].column, "name");
        assert!(matches!(result[0].direction, OrderDirection::Asc));
    }

    #[test]
    fn order_by_multiple() {
        let order = serde_json::json!(["NAME_DESC", "EMAIL_ASC_NULLS_LAST"]);
        let result = order_to_exprs(&order, &test_columns());
        assert_eq!(result.len(), 2);
        assert!(matches!(result[1].direction, OrderDirection::AscNullsLast));
    }

    #[test]
    fn order_by_unknown_column_skipped() {
        let order = serde_json::json!(["NONEXISTENT_ASC"]);
        let result = order_to_exprs(&order, &test_columns());
        assert!(result.is_empty());
    }

    #[test]
    fn order_by_empty_array() {
        let order = serde_json::json!([]);
        let result = order_to_exprs(&order, &test_columns());
        assert!(result.is_empty());
    }

    #[test]
    fn order_by_non_array_returns_empty() {
        let order = serde_json::json!("NAME_ASC");
        let result = order_to_exprs(&order, &test_columns());
        assert!(result.is_empty());
    }
}
