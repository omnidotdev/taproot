use crate::db::Database;
use crate::error::Result;

use super::types::{IntrospectOptions, IntrospectedSchema};

/// Introspect the database and return schema metadata.
///
/// # Errors
/// Returns an error if the database queries fail or results cannot be parsed.
pub async fn introspect(
    _db: &dyn Database,
    _options: &IntrospectOptions,
) -> Result<Vec<IntrospectedSchema>> {
    todo!("implemented in Task 3")
}
