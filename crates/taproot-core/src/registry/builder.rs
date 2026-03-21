use std::collections::HashSet;
use std::sync::Arc;

use async_graphql::dynamic::{
    Enum, EnumItem, Field, FieldFuture, FieldValue, InputValue, Object, Scalar, Schema, TypeRef,
};

use crate::db::Database;
use crate::error::{Error, Result};
use crate::introspect::{ColumnInfo, IntrospectedSchema};

use super::scalars::{map_udt_to_graphql, needs_custom_scalar};
use super::types::SchemaOptions;

/// Build a dynamic `async_graphql` schema from introspected database metadata.
///
/// Each table becomes a `GraphQL` object type, with singular and list root query
/// fields generated automatically based on column types and primary keys.
///
/// # Errors
/// Returns an error if the schema fails validation.
pub fn build_schema(
    schemas: &[IntrospectedSchema],
    db: &Arc<dyn Database>,
    options: &SchemaOptions,
) -> Result<Schema> {
    let mut custom_scalars: HashSet<&'static str> = HashSet::new();
    let mut query = Object::new("Query");

    // Collect all enum type names for lookup during column mapping
    let mut enum_names: HashSet<String> = HashSet::new();
    let mut schema_builder = Schema::build("Query", None, None);

    // Register enum types
    for introspected in schemas {
        for enum_info in &introspected.enums {
            let gql_name = to_pascal_case(&enum_info.name);
            enum_names.insert(enum_info.name.clone());

            let mut gql_enum = Enum::new(&gql_name);
            for value in &enum_info.values {
                gql_enum = gql_enum.item(EnumItem::new(value));
            }

            schema_builder = schema_builder.register(gql_enum);
        }
    }

    // Register custom scalars and build object types
    for introspected in schemas {
        for table in &introspected.tables {
            let type_name = to_pascal_case(&table.name);

            // Collect custom scalars needed by this table's columns
            for col in &table.columns {
                let (gql_type, _) = resolve_column_type(col, &enum_names);
                if let Some(scalar) = needs_custom_scalar(&gql_type) {
                    custom_scalars.insert(scalar);
                }
            }

            // Build the object type
            let object = build_object_type(&type_name, &table.columns, &enum_names);
            schema_builder = schema_builder.register(object);

            // Singular query (by single-column primary key)
            if let Some(pk) = &table.primary_key
                && pk.columns.len() == 1
            {
                let pk_col_name = &pk.columns[0];
                if let Some(pk_col) = table.columns.iter().find(|c| &c.name == pk_col_name) {
                    let field =
                        build_singular_query(table, &type_name, pk_col, &enum_names, db);
                    query = query.field(field);
                }
            }

            // List query
            let list_field = build_list_query(table, &type_name, options, db);
            query = query.field(list_field);
        }
    }

    // Register collected custom scalars
    for scalar_name in &custom_scalars {
        schema_builder = schema_builder.register(Scalar::new(*scalar_name));
    }

    schema_builder = schema_builder.register(query);

    if options.federation {
        schema_builder = schema_builder.enable_federation();
    }

    schema_builder
        .finish()
        .map_err(|e| Error::Schema(e.to_string()))
}

/// Build a `GraphQL` object type from a table's columns.
fn build_object_type(
    type_name: &str,
    columns: &[ColumnInfo],
    enum_names: &HashSet<String>,
) -> Object {
    let mut object = Object::new(type_name);

    for col in columns {
        let field_name = to_camel_case(&col.name);
        let type_ref = column_to_type_ref(col, enum_names);

        let col_name = col.name.clone();
        let field = Field::new(&field_name, type_ref, move |ctx| {
            let col_name = col_name.clone();
            FieldFuture::new(async move {
                let parent = ctx.parent_value.try_to_value()?;

                let value = if let async_graphql::Value::Object(map) = parent {
                    map.get(col_name.as_str()).cloned()
                } else {
                    None
                };

                match value {
                    Some(async_graphql::Value::Null) | None => Ok(None),
                    Some(v) => Ok(Some(FieldValue::value(v))),
                }
            })
        });

        object = object.field(field);
    }

    object
}

/// Build a singular query field: `user(id: UUID!): User`
fn build_singular_query(
    table: &crate::introspect::TableInfo,
    type_name: &str,
    pk_col: &ColumnInfo,
    enum_names: &HashSet<String>,
    db: &Arc<dyn Database>,
) -> Field {
    let field_name = to_camel_case(&table.name);
    let (gql_type, _is_array) = resolve_column_type(pk_col, enum_names);
    let pk_arg_type = TypeRef::named_nn(gql_type);
    let pk_arg_name = to_camel_case(&pk_col.name);

    let schema_name = table.schema.clone();
    let table_name = table.name.clone();
    let pk_col_name = pk_col.name.clone();
    let db = Arc::clone(db);
    let arg_name = pk_arg_name.clone();

    Field::new(
        field_name,
        TypeRef::named(type_name),
        move |ctx| {
            let db = Arc::clone(&db);
            let schema_name = schema_name.clone();
            let table_name = table_name.clone();
            let pk_col_name = pk_col_name.clone();
            let arg_name = arg_name.clone();

            FieldFuture::new(async move {
                let pk_value = ctx.args.try_get(&arg_name)?;
                let pk_json = value_accessor_to_json(&pk_value);

                let sql = format!(
                    "SELECT row_to_json(t.*) FROM {schema}.{table} t WHERE {col} = $1",
                    schema = quote_ident(&schema_name),
                    table = quote_ident(&table_name),
                    col = quote_ident(&pk_col_name),
                );

                let rows = db.query(&sql, &[pk_json]).await.map_err(|e| {
                    async_graphql::Error::new(format!("database error: {e}"))
                })?;

                Ok(rows
                    .into_iter()
                    .next()
                    .map(|row| row_json_to_field_value(&row)))
            })
        },
    )
    .argument(InputValue::new(pk_arg_name, pk_arg_type))
}

/// Build a list query field: `users: [User!]!`
fn build_list_query(
    table: &crate::introspect::TableInfo,
    type_name: &str,
    options: &SchemaOptions,
    db: &Arc<dyn Database>,
) -> Field {
    let field_name = pluralize(&to_camel_case(&table.name));
    let list_type = TypeRef::named_nn_list_nn(type_name);

    let schema_name = table.schema.clone();
    let table_name = table.name.clone();
    let max_page_size = options.max_page_size;
    let db = Arc::clone(db);

    Field::new(field_name, list_type, move |_ctx| {
        let db = Arc::clone(&db);
        let schema_name = schema_name.clone();
        let table_name = table_name.clone();

        FieldFuture::new(async move {
            let sql = format!(
                "SELECT row_to_json(t.*) FROM {schema}.{table} t LIMIT {limit}",
                schema = quote_ident(&schema_name),
                table = quote_ident(&table_name),
                limit = max_page_size,
            );

            let rows = db.query_raw(&sql).await.map_err(|e| {
                async_graphql::Error::new(format!("database error: {e}"))
            })?;

            let values: Vec<FieldValue<'_>> = rows
                .iter()
                .map(|row| row_json_to_field_value(row))
                .collect();

            Ok(Some(FieldValue::list(values)))
        })
    })
}

/// Resolve a column's `udt_name` to a `GraphQL` type name, accounting for enums.
fn resolve_column_type(col: &ColumnInfo, enum_names: &HashSet<String>) -> (String, bool) {
    let base_udt = col.udt_name.strip_prefix('_').unwrap_or(&col.udt_name);

    if enum_names.contains(base_udt) {
        let gql_name = to_pascal_case(base_udt);
        (gql_name, col.is_array)
    } else {
        let (gql_type, is_array_from_udt) = map_udt_to_graphql(&col.udt_name);
        (gql_type.to_string(), col.is_array || is_array_from_udt)
    }
}

/// Convert a column to a full `TypeRef`, respecting nullability and array status.
fn column_to_type_ref(col: &ColumnInfo, enum_names: &HashSet<String>) -> TypeRef {
    let (gql_type, is_array) = resolve_column_type(col, enum_names);

    if is_array {
        if col.is_nullable {
            TypeRef::named_nn_list(gql_type)
        } else {
            TypeRef::named_nn_list_nn(gql_type)
        }
    } else if col.is_nullable {
        TypeRef::named(gql_type)
    } else {
        TypeRef::named_nn(gql_type)
    }
}

/// Unwrap a `row_to_json` result into a `FieldValue` backed by the `GraphQL`
/// value object.
///
/// The row result from `SELECT row_to_json(t.*)` is a JSON object wrapped in
/// another layer — we extract the inner object.
fn row_json_to_field_value(row: &serde_json::Value) -> FieldValue<'static> {
    // `row_to_json` returns the row as a JSON object. When fetched via our
    // `Database::query` / `query_raw`, the result is already a JSON object with
    // a single `row_to_json` key, or the object itself.
    let obj = row
        .get("row_to_json")
        .cloned()
        .unwrap_or_else(|| row.clone());

    FieldValue::value(serde_json_to_gql_value(&obj))
}

/// Convert a `serde_json::Value` to an `async_graphql::Value`.
fn serde_json_to_gql_value(value: &serde_json::Value) -> async_graphql::Value {
    match value {
        serde_json::Value::Null => async_graphql::Value::Null,
        serde_json::Value::Bool(b) => async_graphql::Value::from(*b),
        serde_json::Value::Number(n) => n.as_i64().map_or_else(
            || {
                n.as_f64().map_or_else(
                    || async_graphql::Value::String(n.to_string()),
                    async_graphql::Value::from,
                )
            },
            async_graphql::Value::from,
        ),
        serde_json::Value::String(s) => async_graphql::Value::String(s.clone()),
        serde_json::Value::Array(arr) => {
            async_graphql::Value::List(arr.iter().map(serde_json_to_gql_value).collect())
        }
        serde_json::Value::Object(map) => {
            let obj: async_graphql::indexmap::IndexMap<async_graphql::Name, async_graphql::Value> =
                map.iter()
                    .map(|(k, v)| (async_graphql::Name::new(k), serde_json_to_gql_value(v)))
                    .collect();
            async_graphql::Value::Object(obj)
        }
    }
}

/// Convert a `ValueAccessor` argument to a `serde_json::Value` for parameter
/// binding.
fn value_accessor_to_json(
    accessor: &async_graphql::dynamic::ValueAccessor<'_>,
) -> serde_json::Value {
    gql_value_to_json(accessor.as_value())
}

/// Convert an `async_graphql::Value` to a `serde_json::Value`.
fn gql_value_to_json(value: &async_graphql::Value) -> serde_json::Value {
    match value {
        async_graphql::Value::Null | async_graphql::Value::Binary(_) => serde_json::Value::Null,
        async_graphql::Value::Number(n) => {
            serde_json::Value::Number(serde_json::Number::from(n.as_i64().unwrap_or_default()))
        }
        async_graphql::Value::String(s) => serde_json::Value::String(s.clone()),
        async_graphql::Value::Boolean(b) => serde_json::Value::Bool(*b),
        async_graphql::Value::Enum(name) => serde_json::Value::String(name.to_string()),
        async_graphql::Value::List(list) => {
            serde_json::Value::Array(list.iter().map(gql_value_to_json).collect())
        }
        async_graphql::Value::Object(map) => {
            let obj: serde_json::Map<String, serde_json::Value> = map
                .iter()
                .map(|(k, v)| (k.to_string(), gql_value_to_json(v)))
                .collect();
            serde_json::Value::Object(obj)
        }
    }
}

/// Convert `snake_case` to `PascalCase`.
#[must_use]
pub fn to_pascal_case(s: &str) -> String {
    s.split('_')
        .map(|word| {
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |c| {
                let mut result = c.to_uppercase().to_string();
                result.extend(chars);
                result
            })
        })
        .collect()
}

/// Convert `snake_case` to `camelCase`.
#[must_use]
pub fn to_camel_case(s: &str) -> String {
    let pascal = to_pascal_case(s);
    let mut chars = pascal.chars();
    chars.next().map_or_else(String::new, |c| {
        let mut result = c.to_lowercase().to_string();
        result.extend(chars);
        result
    })
}

/// Naive pluralization — append "s" or handle common suffixes.
#[must_use]
pub fn pluralize(s: &str) -> String {
    if s.ends_with('s') || s.ends_with("sh") || s.ends_with("ch") || s.ends_with('x') {
        format!("{s}es")
    } else if s.ends_with('y')
        && !s.ends_with("ey")
        && !s.ends_with("ay")
        && !s.ends_with("oy")
    {
        format!("{}ies", &s[..s.len() - 1])
    } else {
        format!("{s}s")
    }
}

/// Quote a SQL identifier to prevent injection.
fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pascal_case_conversion() {
        assert_eq!(to_pascal_case("user_account"), "UserAccount");
        assert_eq!(to_pascal_case("id"), "Id");
        assert_eq!(to_pascal_case("created_at"), "CreatedAt");
    }

    #[test]
    fn camel_case_conversion() {
        assert_eq!(to_camel_case("user_account"), "userAccount");
        assert_eq!(to_camel_case("id"), "id");
        assert_eq!(to_camel_case("created_at"), "createdAt");
    }

    #[test]
    fn pluralization_rules() {
        assert_eq!(pluralize("user"), "users");
        assert_eq!(pluralize("status"), "statuses");
        assert_eq!(pluralize("category"), "categories");
        assert_eq!(pluralize("key"), "keys");
        assert_eq!(pluralize("day"), "days");
    }

    #[test]
    fn quote_ident_escapes() {
        assert_eq!(quote_ident("table"), "\"table\"");
        assert_eq!(quote_ident("my\"table"), "\"my\"\"table\"");
    }

    #[test]
    fn json_to_gql_roundtrip() {
        let json = serde_json::json!({
            "name": "Alice",
            "age": 30,
            "active": true,
        });

        let gql = serde_json_to_gql_value(&json);
        assert!(matches!(gql, async_graphql::Value::Object(_)));
    }

    #[test]
    fn resolve_enum_column() {
        let mut enum_names = HashSet::new();
        enum_names.insert("status".to_string());

        let col = ColumnInfo {
            name: "status".to_string(),
            data_type: "USER-DEFINED".to_string(),
            udt_name: "status".to_string(),
            is_nullable: false,
            has_default: false,
            is_generated: false,
            is_array: false,
            comment: None,
        };

        let (gql_type, is_array) = resolve_column_type(&col, &enum_names);
        assert_eq!(gql_type, "Status");
        assert!(!is_array);
    }
}
