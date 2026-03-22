use super::tags::parse_smart_tags;
use super::Plugin;
use crate::introspect::{IntrospectedSchema, TableInfo};

/// Plugin that reads `@tag` directives from Postgres `COMMENT ON` statements.
///
/// Supported tags:
/// - `@omit` — exclude table/column entirely, or `@omit create,update,delete` for operations
/// - `@name newName` — rename the table or column in the GraphQL schema
/// - `@deprecated reason` — mark a field as deprecated
#[derive(Debug, Default)]
pub struct SmartCommentsPlugin;

impl Plugin for SmartCommentsPlugin {
    fn name(&self) -> &'static str {
        "smart-comments"
    }

    fn on_introspect(&self, schema: &mut IntrospectedSchema) {
        for table in &mut schema.tables {
            // Parse table comment
            if let Some(comment) = &table.comment {
                let parsed = parse_smart_tags(comment);

                // Store all tags on the table
                table.tags = parsed.tags;

                // @name renames the table
                if let Some(new_name) = table.tags.get("name")
                    && !new_name.is_empty()
                {
                    table.name = new_name.clone();
                }

                // Replace comment with just the description (strip tags)
                table.comment = parsed.description;
            }

            // Parse column comments
            for col in &mut table.columns {
                if let Some(comment) = &col.comment {
                    let parsed = parse_smart_tags(comment);

                    // Store all tags on the column
                    col.tags = parsed.tags;

                    // @name renames the column
                    if let Some(new_name) = col.tags.get("name")
                        && !new_name.is_empty()
                    {
                        col.name = new_name.clone();
                    }

                    // Replace comment with just the description
                    col.comment = parsed.description;
                }
            }

            // Remove columns with bare @omit
            table.columns.retain(|col| {
                let Some(omit) = col.tags.get("omit") else {
                    return true;
                };
                // Bare @omit (empty value) = remove entirely
                !omit.is_empty()
            });
        }
    }

    fn filter_table(&self, table: &TableInfo) -> bool {
        let Some(omit) = table.tags.get("omit") else {
            return true;
        };
        // Bare @omit (empty value) = omit entire table
        !omit.is_empty()
    }
}
