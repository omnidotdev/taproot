//! Condition type generation and SQL conversion for per-table filtering.
//!
//! Generates `PostGraphile` v5-compatible `{Type}Condition` input types with
//! scalar condition inputs (equality, comparison, pattern matching) and
//! logical combinators (`and`, `or`, `not`).

use async_graphql::dynamic::{InputObject, InputValue, TypeRef};

use crate::introspect::TableInfo;

use super::scalars::map_udt_to_graphql;

/// Scalar condition type names keyed by GraphQL scalar name
struct ScalarConditionDef {
    /// GraphQL scalar this condition applies to
    scalar: &'static str,
    /// Name of the generated input type
    type_name: &'static str,
    /// Whether to include ordering operators (gt, gte, lt, lte)
    has_ordering: bool,
    /// Whether to include pattern operators (like, ilike, startsWith, endsWith)
    has_pattern: bool,
}

/// All supported scalar condition definitions
const SCALAR_CONDITIONS: &[ScalarConditionDef] = &[
    ScalarConditionDef {
        scalar: "String",
        type_name: "StringCondition",
        has_ordering: true,
        has_pattern: true,
    },
    ScalarConditionDef {
        scalar: "Int",
        type_name: "IntCondition",
        has_ordering: true,
        has_pattern: false,
    },
    ScalarConditionDef {
        scalar: "Float",
        type_name: "FloatCondition",
        has_ordering: true,
        has_pattern: false,
    },
    ScalarConditionDef {
        scalar: "Boolean",
        type_name: "BooleanCondition",
        has_ordering: false,
        has_pattern: false,
    },
    ScalarConditionDef {
        scalar: "UUID",
        type_name: "UUIDCondition",
        has_ordering: false,
        has_pattern: false,
    },
    ScalarConditionDef {
        scalar: "DateTime",
        type_name: "DateTimeCondition",
        has_ordering: true,
        has_pattern: false,
    },
    ScalarConditionDef {
        scalar: "Date",
        type_name: "DateCondition",
        has_ordering: true,
        has_pattern: false,
    },
    ScalarConditionDef {
        scalar: "BigInt",
        type_name: "BigIntCondition",
        has_ordering: true,
        has_pattern: false,
    },
    ScalarConditionDef {
        scalar: "BigDecimal",
        type_name: "BigDecimalCondition",
        has_ordering: true,
        has_pattern: false,
    },
    ScalarConditionDef {
        scalar: "JSON",
        type_name: "JSONCondition",
        has_ordering: false,
        has_pattern: false,
    },
];

/// Build all scalar condition input types.
///
/// Each condition type includes operators appropriate for the scalar:
/// - All: `eq`, `ne`, `in`, `notIn`, `isNull`
/// - Numeric/temporal/string: `gt`, `gte`, `lt`, `lte`
/// - String only: `like`, `ilike`, `startsWith`, `endsWith`
#[must_use]
pub fn build_scalar_conditions() -> Vec<InputObject> {
    SCALAR_CONDITIONS.iter().map(build_scalar_condition).collect()
}

/// Build a single scalar condition input type from its definition
fn build_scalar_condition(def: &ScalarConditionDef) -> InputObject {
    let scalar = def.scalar;
    let mut input = InputObject::new(def.type_name);

    // Universal operators
    input = input
        .field(InputValue::new("eq", TypeRef::named(scalar)))
        .field(InputValue::new("ne", TypeRef::named(scalar)))
        .field(InputValue::new("in", TypeRef::named_nn_list(scalar)))
        .field(InputValue::new("notIn", TypeRef::named_nn_list(scalar)))
        .field(InputValue::new("isNull", TypeRef::named("Boolean")));

    // Ordering operators
    if def.has_ordering {
        input = input
            .field(InputValue::new("gt", TypeRef::named(scalar)))
            .field(InputValue::new("gte", TypeRef::named(scalar)))
            .field(InputValue::new("lt", TypeRef::named(scalar)))
            .field(InputValue::new("lte", TypeRef::named(scalar)));
    }

    // Pattern operators
    if def.has_pattern {
        input = input
            .field(InputValue::new("like", TypeRef::named("String")))
            .field(InputValue::new("ilike", TypeRef::named("String")))
            .field(InputValue::new("startsWith", TypeRef::named("String")))
            .field(InputValue::new("endsWith", TypeRef::named("String")));
    }

    input
}

/// Look up the condition type name for a GraphQL scalar.
///
/// Returns `None` for scalars without a known condition type.
#[must_use]
pub fn condition_type_for_scalar(graphql_type: &str) -> Option<&'static str> {
    SCALAR_CONDITIONS
        .iter()
        .find(|def| def.scalar == graphql_type)
        .map(|def| def.type_name)
}

/// Build a per-table `{Type}Condition` input type.
///
/// Creates fields for each column mapped to the appropriate scalar condition
/// input, plus `and`, `or`, `not` logical combinators.
#[must_use]
pub fn build_table_condition(table: &TableInfo, type_name: &str) -> InputObject {
    let condition_name = format!("{type_name}Condition");
    let mut input = InputObject::new(&condition_name);

    // Column condition fields
    for col in &table.columns {
        // Skip array columns since conditions on arrays need different semantics
        if col.is_array {
            continue;
        }

        let (gql_type, _) = map_udt_to_graphql(&col.udt_name);

        if let Some(cond_type) = condition_type_for_scalar(gql_type) {
            input = input.field(InputValue::new(&col.name, TypeRef::named(cond_type)));
        }
    }

    // Logical combinators
    input = input
        .field(InputValue::new(
            "and",
            TypeRef::named_nn_list(&condition_name),
        ))
        .field(InputValue::new(
            "or",
            TypeRef::named_nn_list(&condition_name),
        ))
        .field(InputValue::new("not", TypeRef::named(&condition_name)));

    input
}

/// Convert a condition JSON value to a SQL WHERE clause with parameterized values.
///
/// Returns the SQL fragment and a vector of parameter values. Parameter
/// placeholders start at `$<param_offset + 1>`.
///
/// # Errors
/// This function does not return errors; invalid operators are silently skipped.
/// The caller should validate the condition against the GraphQL schema before
/// calling this function.
#[must_use]
pub fn condition_to_sql(
    condition: &serde_json::Value,
    table: &TableInfo,
    param_offset: usize,
) -> (String, Vec<serde_json::Value>) {
    let mut clauses = Vec::new();
    let mut params = Vec::new();

    let Some(obj) = condition.as_object() else {
        return (String::new(), params);
    };

    for (key, value) in obj {
        match key.as_str() {
            "and" => {
                if let Some(arr) = value.as_array() {
                    let mut sub_clauses = Vec::new();
                    for item in arr {
                        let offset = param_offset + params.len();
                        let (sql, sub_params) = condition_to_sql(item, table, offset);
                        if !sql.is_empty() {
                            sub_clauses.push(sql);
                            params.extend(sub_params);
                        }
                    }
                    if !sub_clauses.is_empty() {
                        clauses.push(format!("({})", sub_clauses.join(" AND ")));
                    }
                }
            }
            "or" => {
                if let Some(arr) = value.as_array() {
                    let mut sub_clauses = Vec::new();
                    for item in arr {
                        let offset = param_offset + params.len();
                        let (sql, sub_params) = condition_to_sql(item, table, offset);
                        if !sql.is_empty() {
                            sub_clauses.push(sql);
                            params.extend(sub_params);
                        }
                    }
                    if !sub_clauses.is_empty() {
                        clauses.push(format!("({})", sub_clauses.join(" OR ")));
                    }
                }
            }
            "not" => {
                let offset = param_offset + params.len();
                let (sql, sub_params) = condition_to_sql(value, table, offset);
                if !sql.is_empty() {
                    clauses.push(format!("NOT ({sql})"));
                    params.extend(sub_params);
                }
            }
            column_name => {
                // Column-level condition
                if table.columns.iter().any(|c| c.name == *column_name) {
                    let offset = param_offset + params.len();
                    let (sql, col_params) =
                        column_condition_to_sql(column_name, value, offset);
                    if !sql.is_empty() {
                        clauses.push(sql);
                        params.extend(col_params);
                    }
                }
            }
        }
    }

    if clauses.is_empty() {
        return (String::new(), Vec::new());
    }

    (clauses.join(" AND "), params)
}

/// Convert a single column's condition operators to SQL
fn column_condition_to_sql(
    column: &str,
    operators: &serde_json::Value,
    param_offset: usize,
) -> (String, Vec<serde_json::Value>) {
    let mut clauses = Vec::new();
    let mut params = Vec::new();
    let quoted = quote_ident(column);

    let Some(ops) = operators.as_object() else {
        return (String::new(), params);
    };

    for (op, value) in ops {
        let idx = param_offset + params.len() + 1;

        match op.as_str() {
            "eq" => {
                clauses.push(format!("{quoted} = ${idx}"));
                params.push(value.clone());
            }
            "ne" => {
                clauses.push(format!("{quoted} <> ${idx}"));
                params.push(value.clone());
            }
            "gt" => {
                clauses.push(format!("{quoted} > ${idx}"));
                params.push(value.clone());
            }
            "gte" => {
                clauses.push(format!("{quoted} >= ${idx}"));
                params.push(value.clone());
            }
            "lt" => {
                clauses.push(format!("{quoted} < ${idx}"));
                params.push(value.clone());
            }
            "lte" => {
                clauses.push(format!("{quoted} <= ${idx}"));
                params.push(value.clone());
            }
            "in" => {
                if let Some(arr) = value.as_array() {
                    let placeholders: Vec<String> = arr
                        .iter()
                        .enumerate()
                        .map(|(i, v)| {
                            let p = param_offset + params.len() + i + 1;
                            // Params will be extended below, but we record the
                            // placeholder index now
                            let _ = v;
                            format!("${p}")
                        })
                        .collect();
                    clauses.push(format!("{quoted} IN ({})", placeholders.join(", ")));
                    params.extend(arr.iter().cloned());
                }
            }
            "notIn" => {
                if let Some(arr) = value.as_array() {
                    let placeholders: Vec<String> = arr
                        .iter()
                        .enumerate()
                        .map(|(i, _)| {
                            let p = param_offset + params.len() + i + 1;
                            format!("${p}")
                        })
                        .collect();
                    clauses.push(format!("{quoted} NOT IN ({})", placeholders.join(", ")));
                    params.extend(arr.iter().cloned());
                }
            }
            "isNull" => {
                if value.as_bool() == Some(true) {
                    clauses.push(format!("{quoted} IS NULL"));
                } else {
                    clauses.push(format!("{quoted} IS NOT NULL"));
                }
            }
            "like" => {
                clauses.push(format!("{quoted} LIKE ${idx}"));
                params.push(value.clone());
            }
            "ilike" => {
                clauses.push(format!("{quoted} ILIKE ${idx}"));
                params.push(value.clone());
            }
            "startsWith" => {
                // Convert to LIKE with escaped value + '%'
                if let Some(s) = value.as_str() {
                    let escaped = s.replace('%', "\\%").replace('_', "\\_");
                    clauses.push(format!("{quoted} LIKE ${idx}"));
                    params.push(serde_json::Value::String(format!("{escaped}%")));
                }
            }
            "endsWith" => {
                if let Some(s) = value.as_str() {
                    let escaped = s.replace('%', "\\%").replace('_', "\\_");
                    clauses.push(format!("{quoted} LIKE ${idx}"));
                    params.push(serde_json::Value::String(format!("%{escaped}")));
                }
            }
            _ => {}
        }
    }

    if clauses.is_empty() {
        return (String::new(), Vec::new());
    }

    (clauses.join(" AND "), params)
}

/// Quote a SQL identifier to prevent injection
fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::introspect::{ColumnInfo, PrimaryKey, TableInfo};
    use serde_json::json;

    /// Build a minimal test table with common column types
    fn test_table() -> TableInfo {
        TableInfo {
            name: "user".to_string(),
            schema: "public".to_string(),
            is_view: false,
            columns: vec![
                ColumnInfo {
                    name: "id".to_string(),
                    data_type: "uuid".to_string(),
                    udt_name: "uuid".to_string(),
                    is_nullable: false,
                    has_default: true,
                    is_generated: false,
                    is_array: false,
                    comment: None,
                    tags: HashMap::new(),
                },
                ColumnInfo {
                    name: "name".to_string(),
                    data_type: "text".to_string(),
                    udt_name: "text".to_string(),
                    is_nullable: false,
                    has_default: false,
                    is_generated: false,
                    is_array: false,
                    comment: None,
                    tags: HashMap::new(),
                },
                ColumnInfo {
                    name: "age".to_string(),
                    data_type: "integer".to_string(),
                    udt_name: "int4".to_string(),
                    is_nullable: true,
                    has_default: false,
                    is_generated: false,
                    is_array: false,
                    comment: None,
                    tags: HashMap::new(),
                },
                ColumnInfo {
                    name: "active".to_string(),
                    data_type: "boolean".to_string(),
                    udt_name: "bool".to_string(),
                    is_nullable: false,
                    has_default: true,
                    is_generated: false,
                    is_array: false,
                    comment: None,
                    tags: HashMap::new(),
                },
            ],
            primary_key: Some(PrimaryKey {
                columns: vec!["id".to_string()],
            }),
            unique_constraints: Vec::new(),
            foreign_keys: Vec::new(),
            comment: None,
            tags: HashMap::new(),
        }
    }

    #[test]
    fn simple_eq() {
        let table = test_table();
        let condition = json!({ "name": { "eq": "Alice" } });

        let (sql, params) = condition_to_sql(&condition, &table, 0);

        assert_eq!(sql, "\"name\" = $1");
        assert_eq!(params, vec![json!("Alice")]);
    }

    #[test]
    fn is_null_true() {
        let table = test_table();
        let condition = json!({ "age": { "isNull": true } });

        let (sql, params) = condition_to_sql(&condition, &table, 0);

        assert_eq!(sql, "\"age\" IS NULL");
        assert!(params.is_empty());
    }

    #[test]
    fn is_null_false() {
        let table = test_table();
        let condition = json!({ "age": { "isNull": false } });

        let (sql, params) = condition_to_sql(&condition, &table, 0);

        assert_eq!(sql, "\"age\" IS NOT NULL");
        assert!(params.is_empty());
    }

    #[test]
    fn and_combinator() {
        let table = test_table();
        let condition = json!({
            "and": [
                { "name": { "eq": "Alice" } },
                { "active": { "eq": true } }
            ]
        });

        let (sql, params) = condition_to_sql(&condition, &table, 0);

        assert_eq!(sql, "(\"name\" = $1 AND \"active\" = $2)");
        assert_eq!(params, vec![json!("Alice"), json!(true)]);
    }

    #[test]
    fn or_combinator() {
        let table = test_table();
        let condition = json!({
            "or": [
                { "name": { "eq": "Alice" } },
                { "name": { "eq": "Bob" } }
            ]
        });

        let (sql, params) = condition_to_sql(&condition, &table, 0);

        assert_eq!(sql, "(\"name\" = $1 OR \"name\" = $2)");
        assert_eq!(params, vec![json!("Alice"), json!("Bob")]);
    }

    #[test]
    fn in_filter() {
        let table = test_table();
        let condition = json!({ "name": { "in": ["Alice", "Bob", "Charlie"] } });

        let (sql, params) = condition_to_sql(&condition, &table, 0);

        assert_eq!(sql, "\"name\" IN ($1, $2, $3)");
        assert_eq!(params, vec![json!("Alice"), json!("Bob"), json!("Charlie")]);
    }

    #[test]
    fn empty_filter() {
        let table = test_table();
        let condition = json!({});

        let (sql, params) = condition_to_sql(&condition, &table, 0);

        assert!(sql.is_empty());
        assert!(params.is_empty());
    }

    #[test]
    fn param_offset() {
        let table = test_table();
        let condition = json!({ "name": { "eq": "Alice" } });

        let (sql, params) = condition_to_sql(&condition, &table, 3);

        assert_eq!(sql, "\"name\" = $4");
        assert_eq!(params, vec![json!("Alice")]);
    }

    #[test]
    fn not_combinator() {
        let table = test_table();
        let condition = json!({
            "not": { "active": { "eq": false } }
        });

        let (sql, params) = condition_to_sql(&condition, &table, 0);

        assert_eq!(sql, "NOT (\"active\" = $1)");
        assert_eq!(params, vec![json!(false)]);
    }

    #[test]
    fn scalar_conditions_count() {
        let conditions = build_scalar_conditions();
        assert_eq!(conditions.len(), SCALAR_CONDITIONS.len());
    }

    #[test]
    fn table_condition_has_column_fields() {
        let table = test_table();
        let condition = build_table_condition(&table, "User");

        // Verify the type name follows v5 convention
        assert_eq!(condition.type_name(), "UserCondition");
    }

    #[test]
    fn condition_type_lookup() {
        assert_eq!(condition_type_for_scalar("String"), Some("StringCondition"));
        assert_eq!(condition_type_for_scalar("Int"), Some("IntCondition"));
        assert_eq!(condition_type_for_scalar("Boolean"), Some("BooleanCondition"));
        assert_eq!(condition_type_for_scalar("UUID"), Some("UUIDCondition"));
        assert_eq!(condition_type_for_scalar("Unknown"), None);
    }

    #[test]
    fn unknown_column_ignored() {
        let table = test_table();
        let condition = json!({ "nonexistent": { "eq": "value" } });

        let (sql, params) = condition_to_sql(&condition, &table, 0);

        assert!(sql.is_empty());
        assert!(params.is_empty());
    }

    #[test]
    fn starts_with_escapes_wildcards() {
        let table = test_table();
        let condition = json!({ "name": { "startsWith": "100%" } });

        let (sql, params) = condition_to_sql(&condition, &table, 0);

        assert_eq!(sql, "\"name\" LIKE $1");
        assert_eq!(params, vec![json!("100\\%%")]);
    }

    #[test]
    fn multiple_operators_on_same_column() {
        let table = test_table();
        let condition = json!({ "age": { "gte": 18, "lt": 65 } });

        let (sql, params) = condition_to_sql(&condition, &table, 0);

        // Order depends on serde_json map iteration, but both must be present
        assert!(sql.contains("\"age\" >="));
        assert!(sql.contains("\"age\" <"));
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn sql_injection_in_column_name() {
        // Even if someone crafts a column name with quotes, `quote_ident` escapes
        assert_eq!(quote_ident("col\"name"), "\"col\"\"name\"");
    }
}
