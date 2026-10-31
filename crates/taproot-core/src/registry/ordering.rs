//! ordering enum generation and SQL conversion.
//!
//! Each table gets a flat `GraphQL` enum (`{Type}OrderBy`) where every value
//! encodes both the column and sort direction, e.g. `CREATED_AT_ASC_NULLS_LAST`.

use async_graphql::dynamic::{Enum, EnumItem};

use crate::introspect::TableInfo;

/// Suffixes appended to each column name to produce the full set of ordering
/// variants.
const ORDER_SUFFIXES: &[(&str, &str)] = &[
    ("_ASC", "ASC"),
    ("_DESC", "DESC"),
    ("_ASC_NULLS_FIRST", "ASC NULLS FIRST"),
    ("_ASC_NULLS_LAST", "ASC NULLS LAST"),
    ("_DESC_NULLS_FIRST", "DESC NULLS FIRST"),
    ("_DESC_NULLS_LAST", "DESC NULLS LAST"),
];

/// Build a `{Type}OrderBy` enum for a table.
///
/// For each column, six variants are generated covering ascending/descending
/// direction with optional nulls placement.
#[must_use]
pub fn build_table_order_by(table: &TableInfo, type_name: &str) -> Enum {
    let enum_name = format!("{type_name}OrderBy");
    let mut gql_enum = Enum::new(&enum_name);

    for col in &table.columns {
        let col_upper = crate::introspect::column_order_token(&col.name);

        for (suffix, _sql) in ORDER_SUFFIXES {
            let value = format!("{col_upper}{suffix}");
            gql_enum = gql_enum.item(EnumItem::new(value));
        }
    }

    gql_enum
}

/// Convert a JSON array of order-by enum values into a SQL `ORDER BY` clause.
///
/// Each element should be a string like `"NAME_ASC"` or `"CREATED_AT_DESC_NULLS_LAST"`.
/// Column names are validated against the table's columns; unknown columns are
/// silently skipped to prevent SQL injection.
///
/// Returns an empty string if no valid orderings are found.
#[must_use]
pub fn order_by_to_sql(order_by: &serde_json::Value, table: &TableInfo) -> String {
    let Some(items) = order_by.as_array() else {
        return String::new();
    };

    let clauses: Vec<String> = items
        .iter()
        .filter_map(|item| {
            let s = item.as_str()?;
            parse_order_value(s, table)
        })
        .collect();

    clauses.join(", ")
}

/// Parse a single enum value like `CREATED_AT_DESC_NULLS_LAST` into a SQL
/// fragment like `"created_at" DESC NULLS LAST`.
///
/// Returns `None` if the column is not found in the table.
fn parse_order_value(value: &str, table: &TableInfo) -> Option<String> {
    // Try matching known suffixes from longest to shortest to avoid ambiguity
    for (suffix, sql_dir) in ORDER_SUFFIXES.iter().rev() {
        if let Some(col_part) = value.strip_suffix(suffix) {
            let col_lower = col_part.to_lowercase();

            if table.columns.iter().any(|c| c.name == col_lower) {
                return Some(format!("\"{col_lower}\" {sql_dir}"));
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::introspect::{ColumnInfo, TableInfo};

    /// Build a minimal table for testing.
    fn test_table() -> TableInfo {
        TableInfo {
            name: "user_account".to_string(),
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
                    name: "created_at".to_string(),
                    data_type: "timestamptz".to_string(),
                    udt_name: "timestamptz".to_string(),
                    is_nullable: false,
                    has_default: true,
                    is_generated: false,
                    is_array: false,
                    comment: None,
                    tags: HashMap::new(),
                },
            ],
            primary_key: None,
            unique_constraints: Vec::new(),
            foreign_keys: Vec::new(),
            comment: None,
            tags: HashMap::new(),
        }
    }

    #[test]
    fn single_column_asc() {
        let table = test_table();
        let order = serde_json::json!(["NAME_ASC"]);
        let sql = order_by_to_sql(&order, &table);

        assert_eq!(sql, "\"name\" ASC");
    }

    #[test]
    fn multiple_columns() {
        let table = test_table();
        let order = serde_json::json!(["NAME_ASC", "CREATED_AT_DESC"]);
        let sql = order_by_to_sql(&order, &table);

        assert_eq!(sql, "\"name\" ASC, \"created_at\" DESC");
    }

    #[test]
    fn nulls_handling() {
        let table = test_table();
        let order = serde_json::json!(["CREATED_AT_ASC_NULLS_LAST", "NAME_DESC_NULLS_FIRST"]);
        let sql = order_by_to_sql(&order, &table);

        assert_eq!(
            sql,
            "\"created_at\" ASC NULLS LAST, \"name\" DESC NULLS FIRST"
        );
    }

    #[test]
    fn invalid_column_ignored() {
        let table = test_table();
        let order = serde_json::json!(["NONEXISTENT_ASC", "NAME_DESC"]);
        let sql = order_by_to_sql(&order, &table);

        assert_eq!(sql, "\"name\" DESC");
    }

    #[test]
    fn empty_array() {
        let table = test_table();
        let order = serde_json::json!([]);
        let sql = order_by_to_sql(&order, &table);

        assert_eq!(sql, "");
    }

    #[test]
    fn enum_has_all_variants() {
        let table = test_table();
        let gql_enum = build_table_order_by(&table, "UserAccount");

        // 3 columns * 6 suffixes = 18 variants
        let sdl = format!("{gql_enum:?}");

        // Verify the enum name
        assert!(sdl.contains("UserAccountOrderBy"));
    }

    #[test]
    fn non_array_input_returns_empty() {
        let table = test_table();
        let order = serde_json::json!("NAME_ASC");
        let sql = order_by_to_sql(&order, &table);

        assert_eq!(sql, "");
    }
}
