//! Mutation input type generation and SQL building for create, update, and
//! delete operations.
//!
//! Follows `PostGraphile` v5 conventions for input and payload types.

use std::collections::HashSet;

use async_graphql::dynamic::{InputObject, InputValue, TypeRef};

use crate::introspect::{ColumnInfo, TableInfo};

use super::builder::to_pascal_case;
use super::scalars::map_udt_to_graphql;

/// Descriptor for a nested relationship field on a create input type.
#[derive(Debug)]
pub struct NestedRelField {
    /// GraphQL field name (e.g. `userByAuthorId`, `commentsByPostId`)
    pub field_name: String,
    /// GraphQL input type name for this nested field
    pub input_type_name: String,
}

/// Runtime metadata for resolving a nested relationship during a create
/// mutation.
#[derive(Clone, Debug)]
pub struct NestedRelInfo {
    /// camelCase field name on the input (e.g. `userByAuthorId`)
    pub field_name: String,
    /// Direction of the relationship
    pub direction: NestedRelDirection,
    /// Related table info for SQL generation
    pub related_table: TableInfo,
    /// Related table's schema name
    pub related_schema: String,
}

/// Direction of a nested relationship.
#[derive(Clone, Debug)]
pub enum NestedRelDirection {
    /// FK column(s) are on this table, pointing to related table's PK
    BelongsTo {
        /// Local FK column names (e.g. `["author_id"]`)
        fk_columns: Vec<String>,
        /// Related table PK column names (e.g. `["id"]`)
        related_pk_columns: Vec<String>,
    },
    /// FK column(s) are on the child table, pointing to this table's PK
    HasMany {
        /// Child table's FK column names pointing to this table
        child_fk_columns: Vec<String>,
        /// This table's PK column names
        parent_pk_columns: Vec<String>,
    },
}

/// Build a nested belongs-to input type with `create` and `connect` fields.
///
/// For example, `PostUserByAuthorIdInput` has:
/// - `create: CreateUserInput` (optional, excluded when `omit_create` is true)
/// - `connect: PostUserByAuthorIdConnectInput` (optional)
///
/// When the related table has `@omit(create)`, pass `omit_create: true` to
/// exclude the `create` field, leaving only `connect`.
#[must_use]
pub fn build_nested_belongs_to_input(
    container_type_name: &str,
    related_create_input: &str,
    connect_input_name: &str,
    omit_create: bool,
) -> InputObject {
    let mut input = InputObject::new(container_type_name);

    if !omit_create {
        input = input.field(InputValue::new(
            "create",
            TypeRef::named(related_create_input),
        ));
    }

    input = input.field(InputValue::new(
        "connect",
        TypeRef::named(connect_input_name),
    ));

    input
}

/// Build a connect input type with the PK fields of the related table as
/// required fields.
///
/// For example, `PostUserByAuthorIdConnectInput` has `id: UUID!`.
#[must_use]
#[allow(clippy::implicit_hasher)]
pub fn build_nested_connect_input(
    connect_input_name: &str,
    pk_cols: &[ColumnInfo],
    enum_names: &HashSet<String>,
) -> InputObject {
    let mut input = InputObject::new(connect_input_name);

    for pk_col in pk_cols {
        let field_name = crate::introspect::column_field_name(&pk_col.name);
        let (gql_type, _) = resolve_column_type_for_input(pk_col, enum_names);
        input = input.field(InputValue::new(field_name, TypeRef::named_nn(gql_type)));
    }

    input
}

/// Build a nested has-many input type with a `create` array field.
///
/// For example, `PostCommentsByPostIdInput` has:
/// - `create: [CreateCommentInput]` (optional list)
#[must_use]
pub fn build_nested_has_many_input(
    container_type_name: &str,
    child_create_input: &str,
) -> InputObject {
    let mut input = InputObject::new(container_type_name);

    input = input.field(InputValue::new(
        "create",
        TypeRef::named_list(child_create_input),
    ));

    input
}

/// Build a `{Type}OnConflict` input type for upsert support.
///
/// Contains `constraint` (the Postgres constraint name) and `updateColumns`
/// (which columns to SET on conflict).
#[must_use]
pub fn build_on_conflict_input(type_name: &str) -> InputObject {
    let input_name = format!("{type_name}OnConflict");
    let mut input = InputObject::new(&input_name);

    input = input.field(InputValue::new(
        "constraint",
        TypeRef::named_nn(TypeRef::STRING),
    ));

    input = input.field(InputValue::new(
        "updateColumns",
        TypeRef::named_nn_list_nn(TypeRef::STRING),
    ));

    input
}

/// Build a `Create{Type}Input` with all non-generated columns.
///
/// Required fields are non-nullable columns without defaults. All other
/// non-generated columns are optional. Includes an optional `onConflict`
/// field for upsert support. Nested relationship fields are appended from
/// the provided descriptors.
#[must_use]
#[allow(clippy::implicit_hasher)]
pub fn build_create_input(
    table: &TableInfo,
    type_name: &str,
    enum_names: &HashSet<String>,
    nested_fields: &[NestedRelField],
    optional_columns: &HashSet<String>,
) -> InputObject {
    let input_name = format!("Create{type_name}Input");
    let on_conflict_type = format!("{type_name}OnConflict");
    let mut input = InputObject::new(&input_name);

    input = input.field(InputValue::new(
        "clientMutationId",
        TypeRef::named(TypeRef::STRING),
    ));

    for col in &table.columns {
        if col.is_generated {
            continue;
        }

        let field_name = crate::introspect::column_field_name(&col.name);
        // A foreign-key column covered by a belongs-to nested relationship is
        // always optional: the caller may supply the related row via the nested
        // `create`/`connect` field instead of the raw key, and the resolver
        // fills the key from it
        let type_ref = if optional_columns.contains(&col.name) {
            let (gql_type, _) = resolve_column_type_for_input(col, enum_names);
            TypeRef::named(gql_type)
        } else {
            create_input_type_ref(col, enum_names)
        };
        input = input.field(InputValue::new(field_name, type_ref));
    }

    // Nested relationship fields (belongs-to and has-many)
    for nested in nested_fields {
        input = input.field(InputValue::new(
            &nested.field_name,
            TypeRef::named(&nested.input_type_name),
        ));
    }

    input = input.field(InputValue::new(
        "onConflict",
        TypeRef::named(&on_conflict_type),
    ));

    input
}

/// Build an `Update{Type}Input` with PK field(s) required and a `patch` field.
#[must_use]
#[allow(clippy::implicit_hasher)]
pub fn build_update_input(
    _table: &TableInfo,
    type_name: &str,
    pk_cols: &[ColumnInfo],
    enum_names: &HashSet<String>,
) -> InputObject {
    let input_name = format!("Update{type_name}Input");
    let patch_type_name = format!("Update{type_name}Patch");
    let mut input = InputObject::new(&input_name);

    input = input.field(InputValue::new(
        "clientMutationId",
        TypeRef::named(TypeRef::STRING),
    ));

    // All PK fields are required
    for pk_col in pk_cols {
        let pk_field_name = crate::introspect::column_field_name(&pk_col.name);
        let (gql_type, _) = resolve_column_type_for_input(pk_col, enum_names);
        input = input.field(InputValue::new(pk_field_name, TypeRef::named_nn(gql_type)));
    }

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
    pk_cols: &[ColumnInfo],
    enum_names: &HashSet<String>,
) -> InputObject {
    let patch_name = format!("Update{type_name}Patch");
    let mut patch = InputObject::new(&patch_name);

    let pk_names: HashSet<&str> = pk_cols.iter().map(|c| c.name.as_str()).collect();

    for col in &table.columns {
        if col.is_generated || pk_names.contains(col.name.as_str()) {
            continue;
        }

        let field_name = crate::introspect::column_field_name(&col.name);
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
    pk_cols: &[ColumnInfo],
    enum_names: &HashSet<String>,
) -> InputObject {
    let input_name = format!("Delete{type_name}Input");
    let mut input = InputObject::new(&input_name);

    input = input.field(InputValue::new(
        "clientMutationId",
        TypeRef::named(TypeRef::STRING),
    ));

    for pk_col in pk_cols {
        let pk_field_name = crate::introspect::column_field_name(&pk_col.name);
        let (gql_type, _) = resolve_column_type_for_input(pk_col, enum_names);
        input = input.field(InputValue::new(pk_field_name, TypeRef::named_nn(gql_type)));
    }

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
/// When `onConflict` is present in the input, generates an upsert using
/// `ON CONFLICT ON CONSTRAINT ... DO UPDATE SET ...`.
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

    // Collect valid column names for validation
    let valid_columns: HashSet<&str> = table.columns.iter().map(|c| c.name.as_str()).collect();

    for col in &table.columns {
        if col.is_generated {
            continue;
        }

        let field_name = crate::introspect::column_field_name(&col.name);

        // Skip meta fields
        if field_name == "clientMutationId" || field_name == "onConflict" {
            continue;
        }

        if let Some(value) = obj.get(&field_name) {
            columns.push(quote_ident(&col.name));
            placeholders.push(format!("${idx}{}", col.pg_cast_suffix(schema)));
            params.push(value.clone());
            idx += 1;
        }
    }

    let on_conflict_clause = obj
        .get("onConflict")
        .and_then(serde_json::Value::as_object)
        .map(|oc| {
            let constraint = oc
                .get("constraint")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();

            let update_columns: Vec<&str> = oc
                .get("updateColumns")
                .and_then(serde_json::Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(serde_json::Value::as_str)
                        .filter(|col| valid_columns.contains(col))
                        .collect()
                })
                .unwrap_or_default();

            let set_clauses: Vec<String> = update_columns
                .iter()
                .map(|col| {
                    format!(
                        "{ident} = EXCLUDED.{ident}",
                        ident = quote_ident(col),
                    )
                })
                .collect();

            format!(
                " ON CONFLICT ON CONSTRAINT {constraint} DO UPDATE SET {sets}",
                constraint = quote_ident(constraint),
                sets = set_clauses.join(", "),
            )
        })
        .unwrap_or_default();

    let sql = format!(
        "WITH inserted AS (\
            INSERT INTO {schema}.{table} ({columns}) \
            VALUES ({placeholders}){on_conflict} \
            RETURNING *\
        ) SELECT row_to_json(inserted.*) FROM inserted",
        schema = quote_ident(schema),
        table = quote_ident(&table.name),
        columns = columns.join(", "),
        placeholders = placeholders.join(", "),
        on_conflict = on_conflict_clause,
    );

    (sql, params)
}

/// Generate an `UPDATE` statement with `RETURNING row_to_json(...)`.
///
/// `pk_columns` contains `(column_name, value)` pairs for the WHERE clause.
///
/// Returns the SQL string and parameter values.
///
/// # Panics
/// Panics if `patch` is not a JSON object.
#[must_use]
pub fn update_to_sql(
    pk_columns: &[(String, serde_json::Value)],
    patch: &serde_json::Value,
    table: &TableInfo,
    schema: &str,
) -> (String, Vec<serde_json::Value>) {
    let patch_obj = patch.as_object().expect("patch must be an object");
    let pk_names: HashSet<&str> = pk_columns.iter().map(|(n, _)| n.as_str()).collect();
    let mut set_clauses = Vec::new();
    let mut params = Vec::new();
    let mut idx = 1usize;

    for col in &table.columns {
        if col.is_generated || pk_names.contains(col.name.as_str()) {
            continue;
        }

        let field_name = crate::introspect::column_field_name(&col.name);
        if let Some(value) = patch_obj.get(&field_name) {
            set_clauses.push(format!(
                "{} = ${idx}{}",
                quote_ident(&col.name),
                col.pg_cast_suffix(schema),
            ));
            params.push(value.clone());
            idx += 1;
        }
    }

    let mut where_clauses = Vec::new();
    for (col_name, value) in pk_columns {
        where_clauses.push(format!("{} = ${idx}", quote_ident(col_name)));
        params.push(value.clone());
        idx += 1;
    }

    let sql = format!(
        "WITH updated AS (\
            UPDATE {schema}.{table} \
            SET {sets} \
            WHERE {where_clause} \
            RETURNING *\
        ) SELECT row_to_json(updated.*) FROM updated",
        schema = quote_ident(schema),
        table = quote_ident(&table.name),
        sets = set_clauses.join(", "),
        where_clause = where_clauses.join(" AND "),
    );

    (sql, params)
}

/// Generate a `DELETE` statement with `RETURNING row_to_json(...)`.
///
/// `pk_columns` contains `(column_name, value)` pairs for the WHERE clause.
///
/// Returns the SQL string and parameter values.
#[must_use]
pub fn delete_to_sql(
    pk_columns: &[(String, serde_json::Value)],
    table: &TableInfo,
    schema: &str,
) -> (String, Vec<serde_json::Value>) {
    let mut params = Vec::new();
    let mut where_clauses = Vec::new();

    for (idx, (col_name, value)) in pk_columns.iter().enumerate() {
        where_clauses.push(format!("{} = ${}", quote_ident(col_name), idx + 1));
        params.push(value.clone());
    }

    let sql = format!(
        "WITH deleted AS (\
            DELETE FROM {schema}.{table} \
            WHERE {where_clause} \
            RETURNING *\
        ) SELECT row_to_json(deleted.*) FROM deleted",
        schema = quote_ident(schema),
        table = quote_ident(&table.name),
        where_clause = where_clauses.join(" AND "),
    );

    (sql, params)
}

/// Build a `Create{Types}BulkInput` with `objects` array and optional
/// `clientMutationId`.
#[must_use]
pub fn build_bulk_create_input(type_name: &str, plural_type_name: &str) -> InputObject {
    let input_name = format!("Create{plural_type_name}BulkInput");
    let create_input_type = format!("Create{type_name}Input");
    let mut input = InputObject::new(&input_name);

    input = input.field(InputValue::new(
        "clientMutationId",
        TypeRef::named(TypeRef::STRING),
    ));

    input = input.field(InputValue::new(
        "objects",
        TypeRef::named_nn_list_nn(&create_input_type),
    ));

    input
}

/// Build a `Update{Types}BulkInput` with `where` condition, `patch`, and
/// optional `clientMutationId`.
#[must_use]
pub fn build_bulk_update_input(type_name: &str, plural_type_name: &str) -> InputObject {
    let input_name = format!("Update{plural_type_name}BulkInput");
    let condition_type = format!("{type_name}Condition");
    let patch_type = format!("Update{type_name}Patch");
    let mut input = InputObject::new(&input_name);

    input = input.field(InputValue::new(
        "clientMutationId",
        TypeRef::named(TypeRef::STRING),
    ));

    input = input.field(InputValue::new("where", TypeRef::named_nn(&condition_type)));

    input = input.field(InputValue::new("patch", TypeRef::named_nn(&patch_type)));

    input
}

/// Build a `Delete{Types}BulkInput` with `where` condition and optional
/// `clientMutationId`.
#[must_use]
pub fn build_bulk_delete_input(type_name: &str, plural_type_name: &str) -> InputObject {
    let input_name = format!("Delete{plural_type_name}BulkInput");
    let condition_type = format!("{type_name}Condition");
    let mut input = InputObject::new(&input_name);

    input = input.field(InputValue::new(
        "clientMutationId",
        TypeRef::named(TypeRef::STRING),
    ));

    input = input.field(InputValue::new("where", TypeRef::named_nn(&condition_type)));

    input
}

/// Generate a bulk `UPDATE` statement with a pre-compiled WHERE clause.
///
/// Returns the SQL string and parameter values.
///
/// # Panics
/// Panics if `patch` is not a JSON object.
#[must_use]
pub fn bulk_update_to_sql(
    patch: &serde_json::Value,
    filter_sql: &str,
    filter_params: &[serde_json::Value],
    table: &TableInfo,
    schema: &str,
) -> (String, Vec<serde_json::Value>) {
    let patch_obj = patch.as_object().expect("patch must be an object");
    let mut set_clauses = Vec::new();
    let mut params = Vec::new();
    let mut idx = 1usize;

    for col in &table.columns {
        if col.is_generated {
            continue;
        }

        let field_name = crate::introspect::column_field_name(&col.name);
        if let Some(value) = patch_obj.get(&field_name) {
            set_clauses.push(format!(
                "{} = ${idx}{}",
                quote_ident(&col.name),
                col.pg_cast_suffix(schema),
            ));
            params.push(value.clone());
            idx += 1;
        }
    }

    // Re-number filter placeholders to start after set params
    let renumbered_filter = renumber_placeholders(filter_sql, idx);
    params.extend_from_slice(filter_params);

    let sql = format!(
        "WITH updated AS (\
            UPDATE {schema}.{table} \
            SET {sets} \
            WHERE {where_clause} \
            RETURNING *\
        ) SELECT row_to_json(updated.*) FROM updated",
        schema = quote_ident(schema),
        table = quote_ident(&table.name),
        sets = set_clauses.join(", "),
        where_clause = renumbered_filter,
    );

    (sql, params)
}

/// Generate a bulk `DELETE` statement with a pre-compiled WHERE clause.
///
/// Returns the SQL string and parameter values.
#[must_use]
pub fn bulk_delete_to_sql(
    filter_sql: &str,
    filter_params: &[serde_json::Value],
    table: &TableInfo,
    schema: &str,
) -> (String, Vec<serde_json::Value>) {
    // Filter placeholders already start at $1
    let sql = format!(
        "WITH deleted AS (\
            DELETE FROM {schema}.{table} \
            WHERE {where_clause} \
            RETURNING *\
        ) SELECT row_to_json(deleted.*) FROM deleted",
        schema = quote_ident(schema),
        table = quote_ident(&table.name),
        where_clause = filter_sql,
    );

    (sql, filter_params.to_vec())
}

/// Re-number `$N` placeholders in a SQL fragment, offsetting by `start`.
///
/// For example, `renumber_placeholders("$1 AND $2", 3)` produces `"$3 AND $4"`.
fn renumber_placeholders(sql: &str, start: usize) -> String {
    let mut result = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '$' {
            let mut num_str = String::new();
            while let Some(&digit) = chars.peek() {
                if digit.is_ascii_digit() {
                    num_str.push(digit);
                    chars.next();
                } else {
                    break;
                }
            }
            result.push('$');
            if let Ok(n) = num_str.parse::<usize>() {
                result.push_str(&(n + start - 1).to_string());
            } else {
                result.push_str(&num_str);
            }
        } else {
            result.push(ch);
        }
    }

    result
}

/// Quote a SQL identifier to prevent injection.
pub(crate) fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::introspect::{ColumnInfo, PrimaryKey, TableInfo};

    fn composite_pk_table() -> TableInfo {
        TableInfo {
            name: "user_roles".to_string(),
            schema: "public".to_string(),
            is_view: false,
            columns: vec![
                ColumnInfo {
                    name: "user_id".to_string(),
                    data_type: "uuid".to_string(),
                    udt_name: "uuid".to_string(),
                    is_nullable: false,
                    has_default: false,
                    is_generated: false,
                    is_array: false,
                    comment: None,
                    tags: HashMap::new(),
                },
                ColumnInfo {
                    name: "role_id".to_string(),
                    data_type: "uuid".to_string(),
                    udt_name: "uuid".to_string(),
                    is_nullable: false,
                    has_default: false,
                    is_generated: false,
                    is_array: false,
                    comment: None,
                    tags: HashMap::new(),
                },
                ColumnInfo {
                    name: "granted_at".to_string(),
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
                columns: vec!["user_id".to_string(), "role_id".to_string()],
            }),
            unique_constraints: Vec::new(),
            foreign_keys: Vec::new(),
            comment: None,
            tags: HashMap::new(),
        }
    }

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
        let pk_columns = vec![
            ("id".to_string(), serde_json::json!("00000000-0000-0000-0000-000000000001")),
        ];
        let patch = serde_json::json!({
            "name": "Bob"
        });

        let (sql, params) = update_to_sql(&pk_columns, &patch, &table, "public");

        assert!(sql.contains("UPDATE"));
        assert!(sql.contains("SET"));
        assert!(sql.contains("\"name\" = $1"));
        assert!(sql.contains("WHERE \"id\" = $2"));
        assert!(sql.contains("RETURNING"));
        assert_eq!(params.len(), 2);
        assert_eq!(params[0], serde_json::json!("Bob"));
    }

    #[test]
    fn update_sql_composite_pk() {
        let table = composite_pk_table();
        let pk_columns = vec![
            ("user_id".to_string(), serde_json::json!("u1")),
            ("role_id".to_string(), serde_json::json!("r1")),
        ];
        let patch = serde_json::json!({
            "grantedAt": "2026-01-01"
        });

        let (sql, params) = update_to_sql(&pk_columns, &patch, &table, "public");

        assert!(sql.contains("\"granted_at\" = $1"));
        assert!(sql.contains("\"user_id\" = $2"));
        assert!(sql.contains("AND"));
        assert!(sql.contains("\"role_id\" = $3"));
        assert_eq!(params.len(), 3);
    }

    #[test]
    fn delete_sql_generation() {
        let table = test_table();
        let pk_columns = vec![
            ("id".to_string(), serde_json::json!("00000000-0000-0000-0000-000000000001")),
        ];

        let (sql, params) = delete_to_sql(&pk_columns, &table, "public");

        assert!(sql.contains("DELETE FROM"));
        assert!(sql.contains("WHERE \"id\" = $1"));
        assert!(sql.contains("RETURNING"));
        assert_eq!(params.len(), 1);
    }

    #[test]
    fn delete_sql_composite_pk() {
        let table = composite_pk_table();
        let pk_columns = vec![
            ("user_id".to_string(), serde_json::json!("u1")),
            ("role_id".to_string(), serde_json::json!("r1")),
        ];

        let (sql, params) = delete_to_sql(&pk_columns, &table, "public");

        assert!(sql.contains("\"user_id\" = $1"));
        assert!(sql.contains("AND"));
        assert!(sql.contains("\"role_id\" = $2"));
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn create_sql_with_on_conflict() {
        let table = test_table();
        let input = serde_json::json!({
            "name": "Alice",
            "email": "alice@example.com",
            "onConflict": {
                "constraint": "users_email_key",
                "updateColumns": ["name"]
            }
        });

        let (sql, params) = create_to_sql(&input, &table, "public");

        assert!(sql.contains("ON CONFLICT ON CONSTRAINT \"users_email_key\""));
        assert!(sql.contains("DO UPDATE SET \"name\" = EXCLUDED.\"name\""));
        assert!(sql.contains("INSERT INTO"));
        assert!(sql.contains("RETURNING"));
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn create_sql_on_conflict_filters_invalid_columns() {
        let table = test_table();
        let input = serde_json::json!({
            "name": "Alice",
            "email": "alice@example.com",
            "onConflict": {
                "constraint": "users_email_key",
                "updateColumns": ["name", "nonexistent_col"]
            }
        });

        let (sql, _params) = create_to_sql(&input, &table, "public");

        assert!(sql.contains("DO UPDATE SET \"name\" = EXCLUDED.\"name\""));
        assert!(!sql.contains("nonexistent_col"));
    }

    #[test]
    fn create_sql_without_on_conflict_unchanged() {
        let table = test_table();
        let input = serde_json::json!({
            "name": "Alice",
            "email": "alice@example.com"
        });

        let (sql, params) = create_to_sql(&input, &table, "public");

        assert!(!sql.contains("ON CONFLICT"));
        assert!(sql.contains("INSERT INTO"));
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn bulk_update_sql_generation() {
        let table = test_table();
        let patch = serde_json::json!({ "name": "Updated" });
        let filter_sql = "\"email\" = $1";
        let filter_params = vec![serde_json::json!("alice@example.com")];

        let (sql, params) = bulk_update_to_sql(&patch, filter_sql, &filter_params, &table, "public");

        assert!(sql.contains("UPDATE"));
        assert!(sql.contains("SET"));
        assert!(sql.contains("\"name\" = $1"));
        // Filter placeholder should be renumbered to $2
        assert!(sql.contains("\"email\" = $2"));
        assert!(sql.contains("RETURNING"));
        assert_eq!(params.len(), 2);
        assert_eq!(params[0], serde_json::json!("Updated"));
        assert_eq!(params[1], serde_json::json!("alice@example.com"));
    }

    #[test]
    fn bulk_delete_sql_generation() {
        let table = test_table();
        let filter_sql = "\"name\" = $1";
        let filter_params = vec![serde_json::json!("Alice")];

        let (sql, params) = bulk_delete_to_sql(filter_sql, &filter_params, &table, "public");

        assert!(sql.contains("DELETE FROM"));
        assert!(sql.contains("WHERE \"name\" = $1"));
        assert!(sql.contains("RETURNING"));
        assert_eq!(params.len(), 1);
        assert_eq!(params[0], serde_json::json!("Alice"));
    }

    #[test]
    fn renumber_placeholders_offsets_correctly() {
        assert_eq!(
            super::renumber_placeholders("$1 AND $2", 3),
            "$3 AND $4"
        );
    }

    #[test]
    fn renumber_placeholders_single() {
        assert_eq!(super::renumber_placeholders("$1", 5), "$5");
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
