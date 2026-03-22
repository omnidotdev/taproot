use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Complete introspected schema containing all discovered database objects.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntrospectedSchema {
    pub name: String,
    pub tables: Vec<TableInfo>,
    pub enums: Vec<EnumInfo>,
    pub composite_types: Vec<CompositeTypeInfo>,
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
