//! Mutation input type generation and SQL building for create, update, and
//! delete operations.
//!
//! Follows `PostGraphile` v5 conventions for input and payload types.

use std::collections::HashSet;

use async_graphql::dynamic::{InputObject, InputValue, TypeRef};

use crate::introspect::{ColumnInfo, TableInfo};

use super::builder::{to_camel_case, to_pascal_case};
use super::scalars::map_udt_to_graphql;

/// Build a `Create{Type}Input` with all non-generated columns.
///
/// Required fields are non-nullable columns without defaults. All other
/// non-generated columns are optional.
#[must_use]
#[allow(clippy::implicit_hasher)]
pub fn build_create_input(
    table: &TableInfo,
    type_name: &str,
    enum_names: &HashSet<String>,
) -> InputObject {
    let input_name = format!("Create{type_name}Input");
    let mut input = InputObject::new(&input_name);

    input = input.field(InputValue::new(
        "clientMutationId",
        TypeRef::named(TypeRef::STRING),
    ));

    for col in &table.columns {
        if col.is_generated {
            continue;
        }

        let field_name = to_camel_case(&col.name);
        let type_ref = create_input_type_ref(col, enum_names);
        input = input.field(InputValue::new(field_name, type_ref));
    }

    input
}

/// Build an `Update{Type}Input` with PK field(s) required and a `patch` field.
#[must_use]
#[allow(clippy::implicit_hasher)]
pub fn build_update_input(
    _table: &TableInfo,
    type_name: &str,
    pk_col: &ColumnInfo,
    enum_names: &HashSet<String>,
) -> InputObject {
    let input_name = format!("Update{type_name}Input");
    let patch_type_name = format!("Update{type_name}Patch");
    let mut input = InputObject::new(&input_name);

    input = input.field(InputValue::new(
        "clientMutationId",
        TypeRef::named(TypeRef::STRING),
    ));

    // PK field is always required
    let pk_field_name = to_camel_case(&pk_col.name);
    let (gql_type, _) = resolve_column_type_for_input(pk_col, enum_names);
    input = input.field(InputValue::new(pk_field_name, TypeRef::named_nn(gql_type)));

    // Patch field
    input = input.field(InputValue::new("patch", TypeRef::named_nn(&patch_type_name)));

    input
}

/// Build an `Update{Type}Patch` with all non-PK, non-generated columns optional.
#[must_use]
#[allow(clippy::implicit_hasher)]
pub fn build_update_patch(
    table: &TableInfo,
    type_name: &str,
    pk_col: &ColumnInfo,
    enum_names: &HashSet<String>,
) -> InputObject {
    let patch_name = format!("Update{type_name}Patch");
    let mut patch = InputObject::new(&patch_name);

    for col in &table.columns {
        if col.is_generated || col.name == pk_col.name {
            continue;
        }

        let field_name = to_camel_case(&col.name);
        let (gql_type, _) = resolve_column_type_for_input(col, enum_names);
        // All patch fields are optional
        patch = patch.field(InputValue::new(field_name, TypeRef::named(gql_type)));
    }

    patch
}

/// Build a `Delete{Type}Input` with PK field(s).
#[must_use]
#[allow(clippy::implicit_hasher)]
pub fn build_delete_input(
    _table: &TableInfo,
    type_name: &str,
    pk_col: &ColumnInfo,
    enum_names: &HashSet<String>,
) -> InputObject {
    let input_name = format!("Delete{type_name}Input");
    let mut input = InputObject::new(&input_name);

    input = input.field(InputValue::new(
        "clientMutationId",
        TypeRef::named(TypeRef::STRING),
    ));

    let pk_field_name = to_camel_case(&pk_col.name);
    let (gql_type, _) = resolve_column_type_for_input(pk_col, enum_names);
    input = input.field(InputValue::new(pk_field_name, TypeRef::named_nn(gql_type)));

    input
}

/// Resolve a column's type for use in input types.
fn resolve_column_type_for_input(col: &ColumnInfo, enum_names: &HashSet<String>) -> (String, bool) {
    let base_udt = col.udt_name.strip_prefix('_').unwrap_or(&col.udt_name);

    if enum_names.contains(base_udt) {
        let gql_name = to_pascal_case(base_udt);
        (gql_name, col.is_array)
    } else {
        let (gql_type, is_array_from_udt) = map_udt_to_graphql(&col.udt_name);
        (gql_type.to_string(), col.is_array || is_array_from_udt)
    }
}

/// Build the `TypeRef` for a column in a create input.
///
/// Required = non-nullable and no default. Everything else is optional.
fn create_input_type_ref(col: &ColumnInfo, enum_names: &HashSet<String>) -> TypeRef {
    let (gql_type, _) = resolve_column_type_for_input(col, enum_names);
    let required = !col.is_nullable && !col.has_default;

    if required {
        TypeRef::named_nn(gql_type)
    } else {
        TypeRef::named(gql_type)
    }
}

/// Generate an `INSERT` statement with `RETURNING row_to_json(...)`.
///
/// Returns the SQL string and parameter values.
///
/// # Panics
/// Panics if `input` is not a JSON object.
#[must_use]
pub fn create_to_sql(
    input: &serde_json::Value,
    table: &TableInfo,
    schema: &str,
) -> (String, Vec<serde_json::Value>) {
    let obj = input.as_object().expect("input must be an object");
    let mut columns = Vec::new();
    let mut placeholders = Vec::new();
    let mut params = Vec::new();
    let mut idx = 1usize;

    for col in &table.columns {
        if col.is_generated {
            continue;
        }

        let field_name = to_camel_case(&col.name);

        // Skip clientMutationId
        if field_name == "clientMutationId" {
            continue;
        }

        if let Some(value) = obj.get(&field_name) {
            columns.push(quote_ident(&col.name));

            // Enum columns need explicit casts from text
            if col.data_type == "USER-DEFINED" {
                placeholders.push(format!(
                    "${idx}::{}.{}",
                    quote_ident(schema),
                    quote_ident(&col.udt_name),
                ));
            } else {
                placeholders.push(format!("${idx}"));
            }

            params.push(value.clone());
            idx += 1;
        }
    }

    let sql = format!(
        "WITH inserted AS (\
            INSERT INTO {schema}.{table} ({columns}) \
            VALUES ({placeholders}) \
            RETURNING *\
        ) SELECT row_to_json(inserted.*) FROM inserted",
        schema = quote_ident(schema),
        table = quote_ident(&table.name),
        columns = columns.join(", "),
        placeholders = placeholders.join(", "),
    );

    (sql, params)
}

/// Generate an `UPDATE` statement with `RETURNING row_to_json(...)`.
///
/// Returns the SQL string and parameter values.
///
/// # Panics
/// Panics if `patch` is not a JSON object.
#[must_use]
pub fn update_to_sql(
    pk_col_name: &str,
    pk_value: &serde_json::Value,
    patch: &serde_json::Value,
    table: &TableInfo,
    schema: &str,
) -> (String, Vec<serde_json::Value>) {
    let patch_obj = patch.as_object().expect("patch must be an object");
    let mut set_clauses = Vec::new();
    let mut params = Vec::new();
    let mut idx = 1usize;

    for col in &table.columns {
        if col.is_generated || col.name == pk_col_name {
            continue;
        }

        let field_name = to_camel_case(&col.name);
        if let Some(value) = patch_obj.get(&field_name) {
            // Enum columns need explicit casts from text
            if col.data_type == "USER-DEFINED" {
                set_clauses.push(format!(
                    "{} = ${idx}::{}.{}",
                    quote_ident(&col.name),
                    quote_ident(schema),
                    quote_ident(&col.udt_name),
                ));
            } else {
                set_clauses.push(format!("{} = ${idx}", quote_ident(&col.name)));
            }
            params.push(value.clone());
            idx += 1;
        }
    }

    params.push(pk_value.clone());

    let sql = format!(
        "WITH updated AS (\
            UPDATE {schema}.{table} \
            SET {sets} \
            WHERE {pk} = ${pk_idx} \
            RETURNING *\
        ) SELECT row_to_json(updated.*) FROM updated",
        schema = quote_ident(schema),
        table = quote_ident(&table.name),
        sets = set_clauses.join(", "),
        pk = quote_ident(pk_col_name),
        pk_idx = idx,
    );

    (sql, params)
}

/// Generate a `DELETE` statement with `RETURNING row_to_json(...)`.
///
/// Returns the SQL string and parameter values.
#[must_use]
pub fn delete_to_sql(
    pk_col_name: &str,
    pk_value: &serde_json::Value,
    table: &TableInfo,
    schema: &str,
) -> (String, Vec<serde_json::Value>) {
    let params = vec![pk_value.clone()];

    let sql = format!(
        "WITH deleted AS (\
            DELETE FROM {schema}.{table} \
            WHERE {pk} = $1 \
            RETURNING *\
        ) SELECT row_to_json(deleted.*) FROM deleted",
        schema = quote_ident(schema),
        table = quote_ident(&table.name),
        pk = quote_ident(pk_col_name),
    );

    (sql, params)
}

/// Quote a SQL identifier to prevent injection.
fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::introspect::{ColumnInfo, PrimaryKey, TableInfo};

    fn test_table() -> TableInfo {
        TableInfo {
            name: "users".to_string(),
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
                    name: "email".to_string(),
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
                    data_type: "timestamp with time zone".to_string(),
                    udt_name: "timestamptz".to_string(),
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
    fn create_sql_generation() {
        let table = test_table();
        let input = serde_json::json!({
            "name": "Alice",
            "email": "alice@example.com"
        });

        let (sql, params) = create_to_sql(&input, &table, "public");

        assert!(sql.contains("INSERT INTO"));
        assert!(sql.contains("\"name\""));
        assert!(sql.contains("\"email\""));
        assert!(sql.contains("RETURNING"));
        assert_eq!(params.len(), 2);
        assert_eq!(params[0], serde_json::json!("Alice"));
        assert_eq!(params[1], serde_json::json!("alice@example.com"));
    }

    #[test]
    fn update_sql_generation() {
        let table = test_table();
        let pk_value = serde_json::json!("00000000-0000-0000-0000-000000000001");
        let patch = serde_json::json!({
            "name": "Bob"
        });

        let (sql, params) = update_to_sql("id", &pk_value, &patch, &table, "public");

        assert!(sql.contains("UPDATE"));
        assert!(sql.contains("SET"));
        assert!(sql.contains("\"name\" = $1"));
        assert!(sql.contains("WHERE \"id\" = $2"));
        assert!(sql.contains("RETURNING"));
        assert_eq!(params.len(), 2);
        assert_eq!(params[0], serde_json::json!("Bob"));
    }

    #[test]
    fn delete_sql_generation() {
        let table = test_table();
        let pk_value = serde_json::json!("00000000-0000-0000-0000-000000000001");

        let (sql, params) = delete_to_sql("id", &pk_value, &table, "public");

        assert!(sql.contains("DELETE FROM"));
        assert!(sql.contains("WHERE \"id\" = $1"));
        assert!(sql.contains("RETURNING"));
        assert_eq!(params.len(), 1);
    }

    #[test]
    fn create_sql_skips_client_mutation_id() {
        let table = test_table();
        let input = serde_json::json!({
            "clientMutationId": "abc",
            "name": "Alice",
            "email": "alice@example.com"
        });

        let (sql, params) = create_to_sql(&input, &table, "public");

        assert!(!sql.contains("clientMutationId"));
        assert_eq!(params.len(), 2);
    }
}
