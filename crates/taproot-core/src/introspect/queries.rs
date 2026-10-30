use std::collections::HashMap;

use crate::db::Database;
use crate::error::{Error, Result};

use super::types::{
    ColumnInfo, CompositeTypeInfo, EnumInfo, ForeignKey, FunctionInfo, IntrospectOptions,
    IntrospectedSchema, PrimaryKey, TableInfo, UniqueConstraint,
};

/// Introspect the database and return schema metadata for each requested schema.
///
/// Queries `information_schema` and `pg_catalog` to discover tables, views,
/// columns, constraints, enums, and composite types.
///
/// # Errors
/// Returns an error if any database query fails or if the results cannot be parsed.
pub async fn introspect(
    db: &dyn Database,
    options: &IntrospectOptions,
) -> Result<Vec<IntrospectedSchema>> {
    let mut schemas = Vec::new();

    for schema_name in &options.schemas {
        let tables = fetch_tables(db, schema_name, &options.exclude_tables).await?;
        let enums = fetch_enums(db, schema_name).await?;
        let composite_types = fetch_composite_types(db, schema_name).await?;
        let functions = fetch_functions(db, schema_name).await?;

        schemas.push(IntrospectedSchema {
            name: schema_name.clone(),
            tables,
            enums,
            composite_types,
            functions,
        });
    }

    Ok(schemas)
}

/// Fetch all tables and views in a schema, including columns and constraints.
async fn fetch_tables(
    db: &dyn Database,
    schema: &str,
    exclude: &[String],
) -> Result<Vec<TableInfo>> {
    let sql = format!(
        "SELECT
            t.table_name,
            t.table_type,
            obj_description(
                (quote_ident(t.table_schema) || '.' || quote_ident(t.table_name))::regclass
            ) AS table_comment
        FROM information_schema.tables t
        WHERE t.table_schema = '{schema}'
            AND t.table_type IN ('BASE TABLE', 'VIEW')
        ORDER BY t.table_name"
    );

    let rows = db.query_raw(&sql).await?;
    let mut tables = Vec::new();

    for row in &rows {
        let name = str_field(row, "table_name")?;

        if exclude.contains(&name) {
            continue;
        }

        let table_type = str_field(row, "table_type")?;
        let is_view = table_type == "VIEW";
        let comment = opt_str_field(row, "table_comment");

        let columns = fetch_columns(db, schema, &name).await?;
        let primary_key = fetch_primary_key(db, schema, &name).await?;
        let unique_constraints = fetch_unique_constraints(db, schema, &name).await?;
        let foreign_keys = fetch_foreign_keys(db, schema, &name).await?;

        tables.push(TableInfo {
            name,
            schema: schema.to_string(),
            is_view,
            columns,
            primary_key,
            unique_constraints,
            foreign_keys,
            comment,
            tags: HashMap::new(),
        });
    }

    Ok(tables)
}

/// Fetch column metadata for a specific table.
async fn fetch_columns(
    db: &dyn Database,
    schema: &str,
    table: &str,
) -> Result<Vec<ColumnInfo>> {
    let sql = format!(
        "SELECT
            c.column_name,
            c.data_type,
            c.udt_name,
            c.is_nullable,
            c.column_default,
            c.is_generated,
            c.ordinal_position,
            col_description(
                (quote_ident(c.table_schema) || '.' || quote_ident(c.table_name))::regclass,
                c.ordinal_position
            ) AS column_comment
        FROM information_schema.columns c
        WHERE c.table_schema = '{schema}'
            AND c.table_name = '{table}'
        ORDER BY c.ordinal_position"
    );

    let rows = db.query_raw(&sql).await?;
    let mut columns = Vec::new();

    for row in &rows {
        let data_type = str_field(row, "data_type")?;
        let udt_name = str_field(row, "udt_name")?;
        let is_array = data_type == "ARRAY" || udt_name.starts_with('_');

        columns.push(ColumnInfo {
            name: str_field(row, "column_name")?,
            data_type,
            udt_name,
            is_nullable: str_field(row, "is_nullable")? == "YES",
            has_default: !row
                .get("column_default")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .is_empty()
                && !row.get("column_default").is_none_or(serde_json::Value::is_null),
            is_generated: str_field(row, "is_generated").unwrap_or_default() == "ALWAYS",
            is_array,
            comment: opt_str_field(row, "column_comment"),
            tags: HashMap::new(),
        });
    }

    Ok(columns)
}

/// Fetch the primary key constraint for a table, if any.
async fn fetch_primary_key(
    db: &dyn Database,
    schema: &str,
    table: &str,
) -> Result<Option<PrimaryKey>> {
    let sql = format!(
        "SELECT kcu.column_name
        FROM information_schema.table_constraints tc
        JOIN information_schema.key_column_usage kcu
            ON tc.constraint_name = kcu.constraint_name
            AND tc.table_schema = kcu.table_schema
        WHERE tc.constraint_type = 'PRIMARY KEY'
            AND tc.table_schema = '{schema}'
            AND tc.table_name = '{table}'
        ORDER BY kcu.ordinal_position"
    );

    let rows = db.query_raw(&sql).await?;

    if rows.is_empty() {
        return Ok(None);
    }

    let columns: Vec<String> = rows
        .iter()
        .map(|row| str_field(row, "column_name"))
        .collect::<Result<_>>()?;

    Ok(Some(PrimaryKey { columns }))
}

/// Fetch unique constraints for a table (excluding primary keys).
async fn fetch_unique_constraints(
    db: &dyn Database,
    schema: &str,
    table: &str,
) -> Result<Vec<UniqueConstraint>> {
    let sql = format!(
        "SELECT
            tc.constraint_name,
            kcu.column_name
        FROM information_schema.table_constraints tc
        JOIN information_schema.key_column_usage kcu
            ON tc.constraint_name = kcu.constraint_name
            AND tc.table_schema = kcu.table_schema
        WHERE tc.constraint_type = 'UNIQUE'
            AND tc.table_schema = '{schema}'
            AND tc.table_name = '{table}'
        ORDER BY tc.constraint_name, kcu.ordinal_position"
    );

    let rows = db.query_raw(&sql).await?;

    // Group columns by constraint name
    let mut constraints: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();

    for row in &rows {
        let name = str_field(row, "constraint_name")?;
        let column = str_field(row, "column_name")?;
        constraints.entry(name).or_default().push(column);
    }

    Ok(constraints
        .into_iter()
        .map(|(name, columns)| UniqueConstraint { name, columns })
        .collect())
}

/// Fetch foreign key relationships for a table.
async fn fetch_foreign_keys(
    db: &dyn Database,
    schema: &str,
    table: &str,
) -> Result<Vec<ForeignKey>> {
    let sql = format!(
        "SELECT
            tc.constraint_name,
            kcu.column_name,
            ccu.table_schema AS foreign_schema,
            ccu.table_name AS foreign_table,
            ccu.column_name AS foreign_column
        FROM information_schema.table_constraints tc
        JOIN information_schema.key_column_usage kcu
            ON tc.constraint_name = kcu.constraint_name
            AND tc.table_schema = kcu.table_schema
        JOIN information_schema.constraint_column_usage ccu
            ON tc.constraint_name = ccu.constraint_name
            AND tc.table_schema = ccu.table_schema
        WHERE tc.constraint_type = 'FOREIGN KEY'
            AND tc.table_schema = '{schema}'
            AND tc.table_name = '{table}'
        ORDER BY tc.constraint_name, kcu.ordinal_position"
    );

    let rows = db.query_raw(&sql).await?;

    // Group by constraint name
    let mut fks: std::collections::BTreeMap<String, ForeignKey> =
        std::collections::BTreeMap::new();

    for row in &rows {
        let name = str_field(row, "constraint_name")?;
        let column = str_field(row, "column_name")?;
        let foreign_column = str_field(row, "foreign_column")?;

        let entry = fks.entry(name.clone()).or_insert_with(|| ForeignKey {
            name,
            columns: Vec::new(),
            foreign_table: str_field(row, "foreign_table").unwrap_or_default(),
            foreign_schema: str_field(row, "foreign_schema").unwrap_or_default(),
            foreign_columns: Vec::new(),
        });

        entry.columns.push(column);
        entry.foreign_columns.push(foreign_column);
    }

    Ok(fks.into_values().collect())
}

/// Fetch all enum types in a schema.
async fn fetch_enums(db: &dyn Database, schema: &str) -> Result<Vec<EnumInfo>> {
    let sql = format!(
        "SELECT
            t.typname AS enum_name,
            n.nspname AS enum_schema,
            e.enumlabel AS enum_value
        FROM pg_type t
        JOIN pg_enum e ON t.oid = e.enumtypid
        JOIN pg_namespace n ON t.typnamespace = n.oid
        WHERE n.nspname = '{schema}'
        ORDER BY t.typname, e.enumsortorder"
    );

    let rows = db.query_raw(&sql).await?;

    // Group values by enum name
    let mut enums: std::collections::BTreeMap<String, EnumInfo> =
        std::collections::BTreeMap::new();

    for row in &rows {
        let name = str_field(row, "enum_name")?;
        let value = str_field(row, "enum_value")?;

        let entry = enums.entry(name.clone()).or_insert_with(|| EnumInfo {
            name,
            schema: str_field(row, "enum_schema").unwrap_or_default(),
            values: Vec::new(),
        });

        entry.values.push(value);
    }

    Ok(enums.into_values().collect())
}

/// Fetch all composite types in a schema.
async fn fetch_composite_types(
    db: &dyn Database,
    schema: &str,
) -> Result<Vec<CompositeTypeInfo>> {
    let sql = format!(
        "SELECT
            t.typname AS type_name,
            n.nspname AS type_schema,
            a.attname AS field_name,
            pg_catalog.format_type(a.atttypid, a.atttypmod) AS data_type,
            a.attnotnull AS not_null,
            a.attnum AS position
        FROM pg_type t
        JOIN pg_namespace n ON t.typnamespace = n.oid
        JOIN pg_class c ON t.typrelid = c.oid
        JOIN pg_attribute a ON c.oid = a.attrelid AND a.attnum > 0 AND NOT a.attisdropped
        WHERE n.nspname = '{schema}'
            AND t.typtype = 'c'
            AND c.relkind = 'c'
        ORDER BY t.typname, a.attnum"
    );

    let rows = db.query_raw(&sql).await?;

    // Group fields by type name
    let mut types: std::collections::BTreeMap<String, CompositeTypeInfo> =
        std::collections::BTreeMap::new();

    for row in &rows {
        let type_name = str_field(row, "type_name")?;
        let data_type = str_field(row, "data_type")?;

        let entry = types
            .entry(type_name.clone())
            .or_insert_with(|| CompositeTypeInfo {
                name: type_name,
                schema: str_field(row, "type_schema").unwrap_or_default(),
                fields: Vec::new(),
            });

        let not_null = row
            .get("not_null")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        entry.fields.push(ColumnInfo {
            name: str_field(row, "field_name")?,
            data_type: data_type.clone(),
            udt_name: data_type,
            is_nullable: !not_null,
            has_default: false,
            is_generated: false,
            is_array: false,
            comment: None,
            tags: HashMap::new(),
        });
    }

    Ok(types.into_values().collect())
}

/// Fetch SQL functions in a schema, identifying computed fields for tables.
async fn fetch_functions(db: &dyn Database, schema: &str) -> Result<Vec<FunctionInfo>> {
    let sql = format!(
        "SELECT
            p.proname AS name,
            n.nspname AS schema,
            pg_catalog.format_type(p.prorettype, NULL) AS return_type,
            p.proretset AS returns_set,
            p.provolatile AS volatility,
            pg_catalog.pg_get_function_arguments(p.oid) AS arg_string,
            COALESCE(
                (SELECT c.relname FROM pg_catalog.pg_class c
                 JOIN pg_catalog.pg_type t ON t.typrelid = c.oid
                 WHERE t.oid = p.proargtypes[0]
                 AND c.relnamespace = n.oid),
                NULL
            ) AS computed_table
        FROM pg_catalog.pg_proc p
        JOIN pg_catalog.pg_namespace n ON p.pronamespace = n.oid
        WHERE n.nspname = '{schema}'
          AND p.prokind = 'f'"
    );

    let rows = db.query_raw(&sql).await?;
    let mut functions = Vec::new();

    for row in &rows {
        let name = str_field(row, "name")?;
        let func_schema = str_field(row, "schema")?;
        let return_type = str_field(row, "return_type")?;

        let returns_set = row
            .get("returns_set")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        let volatility_str = str_field(row, "volatility")?;
        let volatility = volatility_str.chars().next().unwrap_or('v');

        let arg_string = opt_str_field(row, "arg_string").unwrap_or_default();
        let computed_table = opt_str_field(row, "computed_table");

        // Parse additional arguments (skip the first one if this is a computed field)
        let arguments = parse_extra_arguments(&arg_string, computed_table.is_some());

        functions.push(FunctionInfo {
            name,
            schema: func_schema,
            return_type,
            returns_set,
            volatility,
            arguments,
            computed_table,
        });
    }

    Ok(functions)
}

/// Parse the argument string from `pg_get_function_arguments` into (name, type) pairs.
///
/// If `skip_first` is true, the first argument (the table row type) is omitted.
fn parse_extra_arguments(arg_string: &str, skip_first: bool) -> Vec<(String, String)> {
    if arg_string.is_empty() {
        return Vec::new();
    }

    // Split on commas, but respect parentheses (e.g. `numeric(10,2)`)
    let parts = split_arg_string(arg_string);

    let skip = usize::from(skip_first);
    parts
        .into_iter()
        .skip(skip)
        .filter_map(|part| {
            let part = part.trim();
            if part.is_empty() {
                return None;
            }

            // Format is typically "name type" or just "type"
            // Handle IN/OUT/INOUT/VARIADIC prefixes
            let part = part
                .strip_prefix("IN ")
                .or_else(|| part.strip_prefix("INOUT "))
                .or_else(|| part.strip_prefix("OUT "))
                .or_else(|| part.strip_prefix("VARIADIC "))
                .unwrap_or(part);

            // Split into name and type — last space-separated token group is the type
            // but types can be multi-word (e.g. "double precision", "character varying")
            let tokens: Vec<&str> = part.splitn(2, ' ').collect();
            if tokens.len() == 2 {
                Some((tokens[0].to_string(), tokens[1].to_string()))
            } else {
                // Unnamed parameter
                Some((String::new(), tokens[0].to_string()))
            }
        })
        .collect()
}

/// Split an argument string on top-level commas (respecting parentheses).
fn split_arg_string(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth = 0;

    for ch in s.chars() {
        match ch {
            '(' => {
                depth += 1;
                current.push(ch);
            }
            ')' => {
                depth -= 1;
                current.push(ch);
            }
            ',' if depth == 0 => {
                parts.push(current.clone());
                current.clear();
            }
            _ => current.push(ch),
        }
    }

    if !current.is_empty() {
        parts.push(current);
    }

    parts
}

/// Extract a required string field from a JSON row.
fn str_field(row: &serde_json::Value, field: &str) -> Result<String> {
    row.get(field)
        .and_then(serde_json::Value::as_str)
        .map(String::from)
        .ok_or_else(|| Error::Introspection(format!("missing field `{field}` in query result")))
}

/// Extract an optional string field from a JSON row.
fn opt_str_field(row: &serde_json::Value, field: &str) -> Option<String> {
    row.get(field)
        .and_then(serde_json::Value::as_str)
        .map(String::from)
}
