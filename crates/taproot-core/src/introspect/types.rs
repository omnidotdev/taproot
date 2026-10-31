use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Complete introspected schema containing all discovered database objects.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntrospectedSchema {
    pub name: String,
    pub tables: Vec<TableInfo>,
    pub enums: Vec<EnumInfo>,
    pub composite_types: Vec<CompositeTypeInfo>,
    #[serde(default)]
    pub functions: Vec<FunctionInfo>,
}

/// Metadata about a database table or view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableInfo {
    pub name: String,
    pub schema: String,
    pub is_view: bool,
    pub columns: Vec<ColumnInfo>,
    pub primary_key: Option<PrimaryKey>,
    pub unique_constraints: Vec<UniqueConstraint>,
    pub foreign_keys: Vec<ForeignKey>,
    pub comment: Option<String>,
    /// Smart tags parsed from Postgres `COMMENT ON` directives
    #[serde(default)]
    pub tags: HashMap<String, String>,
}

/// Metadata about a single column.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub udt_name: String,
    pub is_nullable: bool,
    pub has_default: bool,
    pub is_generated: bool,
    pub is_array: bool,
    pub comment: Option<String>,
    /// Smart tags parsed from Postgres `COMMENT ON` directives
    #[serde(default)]
    pub tags: HashMap<String, String>,
}

impl ColumnInfo {
    /// SQL cast suffix (e.g. `::"schema"."status"`) to apply to a parameter
    /// placeholder bound for this column, or an empty string when none is needed.
    ///
    /// A parameter is bound as text (or a native scalar), and Postgres will not
    /// implicitly coerce text into an enum, composite, or date/time type, so
    /// those get an explicit cast. Types whose bound representation is already
    /// compatible (text, numeric, bool, uuid) need no cast. Array columns are
    /// left uncast (array parameter binding is not yet supported)
    #[must_use]
    pub fn pg_cast_suffix(&self, schema: &str) -> String {
        if self.is_array {
            return String::new();
        }
        let quote = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
        if self.data_type == "USER-DEFINED" {
            return format!("::{}.{}", quote(schema), quote(&self.udt_name));
        }
        if matches!(
            self.udt_name.as_str(),
            "date" | "time" | "timetz" | "timestamp" | "timestamptz" | "interval"
        ) {
            return format!("::{}", self.udt_name);
        }
        String::new()
    }

    /// GraphQL field name for this column (see [`column_field_name`])
    #[must_use]
    pub fn field_name(&self) -> String {
        column_field_name(&self.name)
    }
}

/// `camelCase` a `snake_case` identifier
fn camel_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut upper_next = false;
    for (i, c) in s.chars().enumerate() {
        if c == '_' {
            upper_next = true;
        } else if i == 0 {
            out.extend(c.to_lowercase());
        } else if upper_next {
            out.extend(c.to_uppercase());
            upper_next = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// GraphQL field name for a database column.
///
/// A column literally named `id` is exposed as `rowId` (matching
/// a common convention that reserves `id` for a global identifier); every
/// other column is camelCased
#[must_use]
pub fn column_field_name(column: &str) -> String {
    if column == "id" {
        "rowId".to_string()
    } else {
        camel_case(column)
    }
}

/// `SCREAMING_SNAKE_CASE` form of a column's GraphQL field name, used for
/// order-by enum values (`id` -> `ROW_ID`, `created_at` -> `CREATED_AT`).
#[must_use]
pub fn column_order_token(column: &str) -> String {
    let field = column_field_name(column);
    let mut out = String::with_capacity(field.len() + 4);
    for c in field.chars() {
        if c.is_ascii_uppercase() && !out.is_empty() {
            out.push('_');
        }
        out.push(c.to_ascii_uppercase());
    }
    out
}

/// Primary key constraint for a table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrimaryKey {
    pub columns: Vec<String>,
}

/// Unique constraint on one or more columns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UniqueConstraint {
    pub name: String,
    pub columns: Vec<String>,
}

/// Foreign key relationship between tables.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForeignKey {
    pub name: String,
    pub columns: Vec<String>,
    pub foreign_table: String,
    pub foreign_schema: String,
    pub foreign_columns: Vec<String>,
}

/// Metadata about a Postgres enum type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnumInfo {
    pub name: String,
    pub schema: String,
    pub values: Vec<String>,
}

/// Metadata about a Postgres composite type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompositeTypeInfo {
    pub name: String,
    pub schema: String,
    pub fields: Vec<ColumnInfo>,
}

/// A Postgres function discovered during introspection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionInfo {
    /// Function name
    pub name: String,
    /// Schema the function lives in
    pub schema: String,
    /// Return type UDT name
    pub return_type: String,
    /// Whether the function returns SETOF
    pub returns_set: bool,
    /// Function volatility: 'i' (immutable), 's' (stable), 'v' (volatile)
    pub volatility: char,
    /// Additional parameters after the first table-row param (name, UDT type)
    pub arguments: Vec<(String, String)>,
    /// The table name this is a computed field for (if first arg is a table row type)
    pub computed_table: Option<String>,
}

/// Options controlling which schemas and tables are introspected.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntrospectOptions {
    /// Schemas to introspect.
    pub schemas: Vec<String>,
    /// Tables to exclude from introspection.
    pub exclude_tables: Vec<String>,
    /// Whether to parse smart comments for directives.
    pub smart_comments: bool,
}

impl Default for IntrospectOptions {
    fn default() -> Self {
        Self {
            schemas: vec!["public".to_string()],
            exclude_tables: Vec::new(),
            smart_comments: true,
        }
    }
}
