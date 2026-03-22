use std::collections::HashMap;
use std::sync::Arc;

use crate::db::Database;
use crate::error::Result;

use super::compile::{compile_plan, compile_plan_count};
use super::plan::PlanNode;

/// Execute a plan node against the database
///
/// # Errors
/// Returns an error if the database query fails.
pub async fn execute_plan(
    plan: &PlanNode,
    db: &Arc<dyn Database>,
) -> Result<Vec<serde_json::Value>> {
    let (sql, params) = compile_plan(plan);
    if sql.is_empty() {
        return Ok(vec![]);
    }
    let rows = db.query(&sql, &params).await?;

    // Unwrap `row_to_json` wrappers produced by `SELECT row_to_json(t.*)`
    Ok(rows
        .into_iter()
        .map(|row| {
            row.get("row_to_json")
                .cloned()
                .unwrap_or(row)
        })
        .collect())
}

/// Execute a plan node as a COUNT query (for pagination)
///
/// # Errors
/// Returns an error if the database query fails.
pub async fn execute_plan_count(
    plan: &PlanNode,
    db: &Arc<dyn Database>,
) -> Result<i64> {
    let (sql, params) = compile_plan_count(plan);
    if sql.is_empty() {
        return Ok(0);
    }
    let rows = db.query(&sql, &params).await?;
    let count = rows
        .first()
        .and_then(|row| row.get("count"))
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    Ok(count)
}

/// Execute a `BatchLookup` plan and group results by key column
///
/// # Errors
/// Returns an error if the database query fails.
pub async fn execute_batch_lookup(
    plan: &PlanNode,
    db: &Arc<dyn Database>,
) -> Result<HashMap<String, Vec<serde_json::Value>>> {
    let PlanNode::BatchLookup { key_column, .. } = plan else {
        return Ok(HashMap::new());
    };

    let rows = execute_plan(plan, db).await?;
    let mut grouped: HashMap<String, Vec<serde_json::Value>> = HashMap::new();

    for row in rows {
        let key = row
            .get(key_column.as_str())
            .map(|v| match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_default();
        grouped.entry(key).or_default().push(row);
    }

    Ok(grouped)
}

#[cfg(test)]
mod tests {
    #[test]
    fn extract_lateral_from_row() {
        let row = serde_json::json!({
            "id": "user-1",
            "name": "Alice",
            "__lateral_posts": [
                {"id": "post-1", "title": "Hello"},
                {"id": "post-2", "title": "World"},
            ]
        });

        let lateral = row.get("__lateral_posts");
        assert!(lateral.is_some());
        let arr = lateral.unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 2);
    }

    #[test]
    fn lateral_empty_array_when_no_children() {
        let row = serde_json::json!({
            "id": "user-1",
            "name": "Alice",
            "__lateral_posts": []
        });

        let lateral = row.get("__lateral_posts");
        assert!(lateral.is_some());
        let arr = lateral.unwrap().as_array().unwrap();
        assert!(arr.is_empty());
    }
}
