//! Aggregate types and SQL generation for connection-level aggregations.
//!
//! Generates `count`, `sum`, `avg`, `min`, `max` aggregate fields on each
//! connection type, a standard aggregate behavior. Aggregates
//! apply to the full filtered set, ignoring pagination.

use std::fmt::Write;

use async_graphql::dynamic::{Field, FieldFuture, FieldValue, Object, TypeRef};

use crate::introspect::ColumnInfo;

use super::scalars::map_udt_to_graphql;

/// UDT names considered numeric (eligible for sum/avg)
const NUMERIC_UDTS: &[&str] = &[
    "int2",
    "int4",
    "int8",
    "float4",
    "float8",
    "numeric",
    "decimal",
    "smallint",
    "integer",
    "bigint",
    "real",
    "double precision",
    "smallserial",
    "serial",
    "bigserial",
    "money",
];

/// Non-numeric UDT names that Postgres supports `min`/`max` on. Combined with
/// the numeric set this forms an allowlist: a type is eligible for min/max only
/// if it appears here or is numeric. An allowlist (rather than a denylist) is
/// deliberate: Postgres has no `min`/`max` aggregate for many types (`uuid`,
/// `bool`, `json`/`jsonb`, geometric, `bytea`, arrays, composites, ...), and
/// emitting `MIN(col)` for any of them fails the whole query, whereas omitting
/// a min/max field for an unrecognized-but-orderable type is harmless.
const ORDERABLE_UDTS: &[&str] = &[
    "text",
    "varchar",
    "bpchar",
    "char",
    "name",
    "citext",
    "date",
    "time",
    "timetz",
    "timestamp",
    "timestamptz",
    "interval",
    "inet",
    "cidr",
    "macaddr",
    "macaddr8",
];

/// Check whether a column's UDT is numeric
#[must_use]
fn is_numeric(col: &ColumnInfo) -> bool {
    NUMERIC_UDTS.contains(&col.udt_name.as_str())
}

/// Check whether a column's UDT is orderable (eligible for min/max). Allowlist:
/// numeric types plus scalar string, date/time, and network types.
#[must_use]
fn is_orderable(col: &ColumnInfo) -> bool {
    is_numeric(col) || ORDERABLE_UDTS.contains(&col.udt_name.as_str())
}

/// Quote a SQL identifier, escaping embedded double quotes
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Build the top-level `{Type}Aggregates` object type
#[must_use]
pub fn build_aggregates_type(type_name: &str, columns: &[ColumnInfo]) -> Object {
    #[allow(clippy::similar_names)]
    let aggregates_type_name = format!("{type_name}Aggregates");
    let sum_type_name = format!("{type_name}SumAggregates");
    let avg_type_name = format!("{type_name}AvgAggregates");
    let min_type_name = format!("{type_name}MinAggregates");
    let max_type_name = format!("{type_name}MaxAggregates");

    let has_numeric = columns.iter().any(|c| is_numeric(c) && !c.is_array);
    let has_orderable = columns.iter().any(|c| is_orderable(c) && !c.is_array);

    let mut obj = Object::new(&aggregates_type_name).field(Field::new(
        "count",
        TypeRef::named_nn(TypeRef::INT),
        |ctx| {
            FieldFuture::new(async move {
                let parent = ctx.parent_value.try_to_value()?;
                let value = extract_field(parent, "count");

                Ok(Some(FieldValue::value(value)))
            })
        },
    ));

    if has_numeric {
        obj = obj
            .field(build_sub_field("sum", &sum_type_name))
            .field(build_sub_field("avg", &avg_type_name));
    }

    if has_orderable {
        obj = obj
            .field(build_sub_field("min", &min_type_name))
            .field(build_sub_field("max", &max_type_name));
    }

    obj
}

/// Build a `{Type}SumAggregates` object type (one Float field per numeric column)
#[must_use]
pub fn build_sum_type(type_name: &str, columns: &[ColumnInfo]) -> Option<Object> {
    let fields: Vec<&ColumnInfo> = columns
        .iter()
        .filter(|c| is_numeric(c) && !c.is_array)
        .collect();

    if fields.is_empty() {
        return None;
    }

    let obj_name = format!("{type_name}SumAggregates");
    let mut obj = Object::new(&obj_name);

    for col in &fields {
        let field_name = crate::introspect::column_field_name(&col.name);
        obj = obj.field(build_scalar_extract_field(&field_name, TypeRef::FLOAT));
    }

    Some(obj)
}

/// Build a `{Type}AvgAggregates` object type (one Float field per numeric column)
#[must_use]
pub fn build_avg_type(type_name: &str, columns: &[ColumnInfo]) -> Option<Object> {
    let fields: Vec<&ColumnInfo> = columns
        .iter()
        .filter(|c| is_numeric(c) && !c.is_array)
        .collect();

    if fields.is_empty() {
        return None;
    }

    let obj_name = format!("{type_name}AvgAggregates");
    let mut obj = Object::new(&obj_name);

    for col in &fields {
        let field_name = crate::introspect::column_field_name(&col.name);
        obj = obj.field(build_scalar_extract_field(&field_name, TypeRef::FLOAT));
    }

    Some(obj)
}

/// Build a `{Type}MinAggregates` object type (field per orderable column, original type)
#[must_use]
pub fn build_min_type(type_name: &str, columns: &[ColumnInfo]) -> Option<Object> {
    build_minmax_type(type_name, "Min", columns)
}

/// Build a `{Type}MaxAggregates` object type (field per orderable column, original type)
#[must_use]
pub fn build_max_type(type_name: &str, columns: &[ColumnInfo]) -> Option<Object> {
    build_minmax_type(type_name, "Max", columns)
}

/// Build a min or max aggregates object type
fn build_minmax_type(type_name: &str, suffix: &str, columns: &[ColumnInfo]) -> Option<Object> {
    let fields: Vec<&ColumnInfo> = columns
        .iter()
        .filter(|c| is_orderable(c) && !c.is_array)
        .collect();

    if fields.is_empty() {
        return None;
    }

    let obj_name = format!("{type_name}{suffix}Aggregates");
    let mut obj = Object::new(&obj_name);

    for col in &fields {
        let field_name = crate::introspect::column_field_name(&col.name);
        let (gql_type, _) = map_udt_to_graphql(&col.udt_name);
        obj = obj.field(build_scalar_extract_field(&field_name, gql_type));
    }

    Some(obj)
}

/// Build the aggregate SQL SELECT clause for a table.
///
/// Returns the SELECT ... FROM portion without a WHERE clause. The caller
/// appends the WHERE clause using the same filter as the list query.
#[must_use]
pub fn build_aggregate_sql(schema: &str, table: &str, columns: &[ColumnInfo]) -> String {
    let numeric_cols: Vec<&ColumnInfo> = columns
        .iter()
        .filter(|c| is_numeric(c) && !c.is_array)
        .collect();

    let orderable_cols: Vec<&ColumnInfo> = columns
        .iter()
        .filter(|c| is_orderable(c) && !c.is_array)
        .collect();

    let mut sql = String::from("SELECT COUNT(*) AS count");

    if !numeric_cols.is_empty() {
        // SUM
        let _ = write!(sql, ", json_build_object(");
        for (i, col) in numeric_cols.iter().enumerate() {
            if i > 0 {
                let _ = write!(sql, ", ");
            }
            let field_name = crate::introspect::column_field_name(&col.name);
            let _ = write!(
                sql,
                "'{field_name}', SUM({col})",
                col = quote_ident(&col.name)
            );
        }
        let _ = write!(sql, ") AS sum");

        // AVG
        let _ = write!(sql, ", json_build_object(");
        for (i, col) in numeric_cols.iter().enumerate() {
            if i > 0 {
                let _ = write!(sql, ", ");
            }
            let field_name = crate::introspect::column_field_name(&col.name);
            let _ = write!(
                sql,
                "'{field_name}', AVG({col})",
                col = quote_ident(&col.name)
            );
        }
        let _ = write!(sql, ") AS avg");
    }

    if !orderable_cols.is_empty() {
        // MIN
        let _ = write!(sql, ", json_build_object(");
        for (i, col) in orderable_cols.iter().enumerate() {
            if i > 0 {
                let _ = write!(sql, ", ");
            }
            let field_name = crate::introspect::column_field_name(&col.name);
            let _ = write!(
                sql,
                "'{field_name}', MIN({col})",
                col = quote_ident(&col.name)
            );
        }
        let _ = write!(sql, ") AS min");

        // MAX
        let _ = write!(sql, ", json_build_object(");
        for (i, col) in orderable_cols.iter().enumerate() {
            if i > 0 {
                let _ = write!(sql, ", ");
            }
            let field_name = crate::introspect::column_field_name(&col.name);
            let _ = write!(
                sql,
                "'{field_name}', MAX({col})",
                col = quote_ident(&col.name)
            );
        }
        let _ = write!(sql, ") AS max");
    }

    let _ = write!(
        sql,
        " FROM {schema}.{table} t",
        schema = quote_ident(schema),
        table = quote_ident(table)
    );

    sql
}

/// Build a field that resolves a sub-aggregate object (sum, avg, min, max)
fn build_sub_field(name: &str, type_ref: &str) -> Field {
    let key = name.to_owned();

    Field::new(name, TypeRef::named(type_ref), move |ctx| {
        let key = key.clone();

        FieldFuture::new(async move {
            let parent = ctx.parent_value.try_to_value()?;
            let value = extract_field(parent, &key);

            match value {
                async_graphql::Value::Null => Ok(None),
                v => Ok(Some(FieldValue::value(v))),
            }
        })
    })
}

/// Build a field that extracts a scalar value by name from the parent object
fn build_scalar_extract_field(name: &str, type_ref: &str) -> Field {
    let key = name.to_owned();

    Field::new(name, TypeRef::named(type_ref), move |ctx| {
        let key = key.clone();

        FieldFuture::new(async move {
            let parent = ctx.parent_value.try_to_value()?;
            let value = extract_field(parent, &key);

            match value {
                async_graphql::Value::Null => Ok(None),
                v => Ok(Some(FieldValue::value(v))),
            }
        })
    })
}

/// Extract a field from a parent GraphQL value object by key
fn extract_field(parent: &async_graphql::Value, key: &str) -> async_graphql::Value {
    if let async_graphql::Value::Object(map) = parent {
        map.get(key).cloned().unwrap_or(async_graphql::Value::Null)
    } else {
        async_graphql::Value::Null
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn make_column(name: &str, udt_name: &str) -> ColumnInfo {
        ColumnInfo {
            name: name.to_owned(),
            data_type: String::new(),
            udt_name: udt_name.to_owned(),
            is_nullable: false,
            has_default: false,
            is_generated: false,
            is_array: false,
            comment: None,
            tags: HashMap::new(),
        }
    }

    #[test]
    fn numeric_detection() {
        assert!(is_numeric(&make_column("x", "int4")));
        assert!(is_numeric(&make_column("x", "float8")));
        assert!(is_numeric(&make_column("x", "numeric")));
        assert!(is_numeric(&make_column("x", "money")));
        assert!(!is_numeric(&make_column("x", "text")));
        assert!(!is_numeric(&make_column("x", "uuid")));
        assert!(!is_numeric(&make_column("x", "bool")));
    }

    #[test]
    fn orderable_detection() {
        assert!(is_orderable(&make_column("x", "int4")));
        assert!(is_orderable(&make_column("x", "text")));
        assert!(is_orderable(&make_column("x", "timestamptz")));
        assert!(!is_orderable(&make_column("x", "json")));
        assert!(!is_orderable(&make_column("x", "jsonb")));
    }

    #[test]
    fn aggregate_sql_basic() {
        let columns = vec![
            make_column("id", "int4"),
            make_column("amount", "numeric"),
            make_column("name", "text"),
        ];

        let sql = build_aggregate_sql("public", "orders", &columns);

        assert!(sql.contains("COUNT(*) AS count"));
        assert!(sql.contains("SUM(\"id\")"));
        assert!(sql.contains("SUM(\"amount\")"));
        assert!(sql.contains("AVG(\"id\")"));
        assert!(sql.contains("AVG(\"amount\")"));
        assert!(sql.contains("MIN(\"id\")"));
        assert!(sql.contains("MIN(\"amount\")"));
        assert!(sql.contains("MIN(\"name\")"));
        assert!(sql.contains("MAX(\"id\")"));
        assert!(sql.contains("MAX(\"amount\")"));
        assert!(sql.contains("MAX(\"name\")"));
        assert!(sql.contains("FROM \"public\".\"orders\" t"));
        // text columns should NOT appear in sum/avg
        assert!(!sql.contains("SUM(\"name\")"));
        assert!(!sql.contains("AVG(\"name\")"));
    }

    #[test]
    fn aggregate_sql_no_numeric_columns() {
        let columns = vec![
            make_column("name", "text"),
            make_column("email", "varchar"),
        ];

        let sql = build_aggregate_sql("public", "users", &columns);

        assert!(sql.contains("COUNT(*) AS count"));
        assert!(!sql.contains("SUM"));
        assert!(!sql.contains("AVG"));
        assert!(sql.contains("MIN"));
        assert!(sql.contains("MAX"));
    }

    #[test]
    fn aggregate_sql_excludes_non_orderable_from_minmax() {
        let columns = vec![
            make_column("id", "int4"),
            make_column("metadata", "jsonb"),
        ];

        let sql = build_aggregate_sql("public", "items", &columns);

        assert!(sql.contains("MIN(\"id\")"));
        assert!(sql.contains("MAX(\"id\")"));
        assert!(!sql.contains("MIN(\"metadata\")"));
        assert!(!sql.contains("MAX(\"metadata\")"));
    }

    #[test]
    fn aggregate_sql_camel_case_keys() {
        let columns = vec![make_column("total_amount", "numeric")];

        let sql = build_aggregate_sql("public", "orders", &columns);

        assert!(sql.contains("'totalAmount'"));
    }

    #[test]
    fn aggregate_sql_escapes_identifiers() {
        let columns = vec![make_column("user\"name", "text")];

        let sql = build_aggregate_sql("my\"schema", "my\"table", &columns);

        assert!(sql.contains("\"my\"\"schema\""));
        assert!(sql.contains("\"my\"\"table\""));
        assert!(sql.contains("\"user\"\"name\""));
    }
}
