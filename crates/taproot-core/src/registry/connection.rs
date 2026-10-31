//! Relay-style cursor pagination types
//!
//! Provides cursor encoding/decoding, connection/edge/page-info type builders,
//! and a response builder that assembles the JSON structure resolvers expect.

use async_graphql::dynamic::{Field, FieldFuture, FieldValue, Object, TypeRef};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};

/// Prefix used in cursor values before the offset
const CURSOR_PREFIX: &str = "cursor:";

/// Encode an offset into a base64url cursor string.
#[must_use]
pub fn encode_cursor(offset: usize) -> String {
    URL_SAFE_NO_PAD.encode(format!("{CURSOR_PREFIX}{offset}"))
}

/// Decode a base64url cursor string back into an offset.
///
/// Returns `None` if the cursor is malformed or missing the expected prefix.
#[must_use]
pub fn decode_cursor(cursor: &str) -> Option<usize> {
    let bytes = URL_SAFE_NO_PAD.decode(cursor).ok()?;
    let decoded = String::from_utf8(bytes).ok()?;
    let offset_str = decoded.strip_prefix(CURSOR_PREFIX)?;

    offset_str.parse().ok()
}

/// A resolved pagination window: how many leading rows to skip and how many to
/// take, computed from the Relay connection arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// Number of leading rows to skip (the window's absolute start offset)
    pub skip: usize,
    /// Number of rows to take
    pub take: usize,
}

/// The Relay pagination arguments for a connection field. `after`/`before` are
/// already decoded from cursors into absolute offsets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PageArgs {
    pub first: Option<usize>,
    pub after: Option<usize>,
    pub last: Option<usize>,
    pub before: Option<usize>,
    pub offset: Option<usize>,
}

/// Compute the pagination window from the Relay connection arguments against a
/// known `total_count`.
///
/// Uses the offset-cursor model: `after`/`before` cursors decode to absolute
/// offsets. `after` sets the lower bound (exclusive), `before` the upper bound
/// (exclusive), `offset` skips further forward from the lower bound, then
/// `first` trims from the start of the range and `last` from the end (Relay
/// applies `first` before `last`). When neither `first` nor `last` is given,
/// `default_page_size` is applied forward. `first`/`last` are capped at
/// `max_page_size`.
#[must_use]
pub fn compute_window(
    args: PageArgs,
    default_page_size: usize,
    max_page_size: usize,
    total_count: i64,
) -> Window {
    let PageArgs {
        first,
        after,
        last,
        before,
        offset,
    } = args;
    let total = usize::try_from(total_count).unwrap_or(0);

    let mut lo = after.map_or(0, |a| a.saturating_add(1));
    let mut hi = before.map_or(total, |b| b.min(total));

    // Additional forward skip, relative to the lower bound
    lo = lo.saturating_add(offset.unwrap_or(0));
    lo = lo.min(hi);

    match (first, last) {
        (Some(f), last_opt) => {
            let f = f.min(max_page_size);
            hi = hi.min(lo.saturating_add(f));
            if let Some(l) = last_opt {
                let l = l.min(max_page_size);
                lo = lo.max(hi.saturating_sub(l));
            }
        }
        (None, Some(l)) => {
            let l = l.min(max_page_size);
            lo = lo.max(hi.saturating_sub(l));
        }
        (None, None) => {
            let n = default_page_size.min(max_page_size);
            hi = hi.min(lo.saturating_add(n));
        }
    }

    Window {
        skip: lo,
        take: hi.saturating_sub(lo),
    }
}

/// Build the shared `PageInfo` object type.
///
/// Fields: `hasNextPage`, `hasPreviousPage`, `startCursor`, `endCursor`
#[must_use]
pub fn build_page_info_type() -> Object {
    Object::new("PageInfo")
        .field(Field::new(
            "hasNextPage",
            TypeRef::named_nn(TypeRef::BOOLEAN),
            |ctx| {
                FieldFuture::new(async move {
                    let parent = ctx.parent_value.try_to_value()?;
                    let value = extract_field(parent, "hasNextPage");

                    Ok(Some(FieldValue::value(value)))
                })
            },
        ))
        .field(Field::new(
            "hasPreviousPage",
            TypeRef::named_nn(TypeRef::BOOLEAN),
            |ctx| {
                FieldFuture::new(async move {
                    let parent = ctx.parent_value.try_to_value()?;
                    let value = extract_field(parent, "hasPreviousPage");

                    Ok(Some(FieldValue::value(value)))
                })
            },
        ))
        .field(Field::new(
            "startCursor",
            TypeRef::named(TypeRef::STRING),
            |ctx| {
                FieldFuture::new(async move {
                    let parent = ctx.parent_value.try_to_value()?;
                    let value = extract_field(parent, "startCursor");

                    match value {
                        async_graphql::Value::Null => Ok(None),
                        v => Ok(Some(FieldValue::value(v))),
                    }
                })
            },
        ))
        .field(Field::new(
            "endCursor",
            TypeRef::named(TypeRef::STRING),
            |ctx| {
                FieldFuture::new(async move {
                    let parent = ctx.parent_value.try_to_value()?;
                    let value = extract_field(parent, "endCursor");

                    match value {
                        async_graphql::Value::Null => Ok(None),
                        v => Ok(Some(FieldValue::value(v))),
                    }
                })
            },
        ))
}

/// Build a `{Type}Connection` object type.
///
/// Fields: `edges`, `nodes`, `pageInfo`, `totalCount`
#[must_use]
pub fn build_connection_type(type_name: &str) -> Object {
    let edge_type = format!("{type_name}Edge");
    let connection_name = format!("{type_name}Connection");

    Object::new(&connection_name)
        .field(Field::new(
            "edges",
            TypeRef::named_nn_list_nn(&edge_type),
            |ctx| {
                FieldFuture::new(async move {
                    let parent = ctx.parent_value.try_to_value()?;
                    let values = extract_list(parent, "edges");

                    Ok(Some(FieldValue::list(values)))
                })
            },
        ))
        .field(Field::new(
            "nodes",
            TypeRef::named_nn_list_nn(type_name),
            |ctx| {
                FieldFuture::new(async move {
                    let parent = ctx.parent_value.try_to_value()?;
                    let values = extract_list(parent, "nodes");

                    Ok(Some(FieldValue::list(values)))
                })
            },
        ))
        .field(Field::new(
            "pageInfo",
            TypeRef::named_nn("PageInfo"),
            |ctx| {
                FieldFuture::new(async move {
                    let parent = ctx.parent_value.try_to_value()?;
                    let value = extract_field(parent, "pageInfo");

                    Ok(Some(FieldValue::value(value)))
                })
            },
        ))
        .field(Field::new(
            "totalCount",
            TypeRef::named_nn(TypeRef::INT),
            |ctx| {
                FieldFuture::new(async move {
                    let parent = ctx.parent_value.try_to_value()?;
                    let value = extract_field(parent, "totalCount");

                    Ok(Some(FieldValue::value(value)))
                })
            },
        ))
}

/// Build a `{Type}Edge` object type.
///
/// Fields: `node`, `cursor`
#[must_use]
pub fn build_edge_type(type_name: &str) -> Object {
    let edge_name = format!("{type_name}Edge");

    Object::new(&edge_name)
        .field(Field::new("node", TypeRef::named_nn(type_name), |ctx| {
            FieldFuture::new(async move {
                let parent = ctx.parent_value.try_to_value()?;
                let value = extract_field(parent, "node");

                Ok(Some(FieldValue::value(value)))
            })
        }))
        .field(Field::new(
            "cursor",
            TypeRef::named_nn(TypeRef::STRING),
            |ctx| {
                FieldFuture::new(async move {
                    let parent = ctx.parent_value.try_to_value()?;
                    let value = extract_field(parent, "cursor");

                    Ok(Some(FieldValue::value(value)))
                })
            },
        ))
}

/// Build the JSON structure that connection resolvers expect.
///
/// Takes raw query result rows and assembles edges (with cursors), a nodes
/// shortcut, page info, and total count into a single JSON value.
#[must_use]
pub fn build_connection_response(
    rows: Vec<serde_json::Value>,
    offset: usize,
    total_count: i64,
) -> serde_json::Value {
    let edges: Vec<serde_json::Value> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            serde_json::json!({
                "node": row,
                "cursor": encode_cursor(offset + i),
            })
        })
        .collect();

    let nodes = rows;

    let has_previous_page = offset > 0;
    let has_next_page = (offset + edges.len()) < usize::try_from(total_count).unwrap_or(0);

    let start_cursor = if edges.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::Value::String(encode_cursor(offset))
    };

    let end_cursor = if edges.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::Value::String(encode_cursor(offset + edges.len() - 1))
    };

    serde_json::json!({
        "edges": edges,
        "nodes": nodes,
        "pageInfo": {
            "hasNextPage": has_next_page,
            "hasPreviousPage": has_previous_page,
            "startCursor": start_cursor,
            "endCursor": end_cursor,
        },
        "totalCount": total_count,
    })
}

/// Extract a field from a parent `GraphQL` value object by key
fn extract_field(parent: &async_graphql::Value, key: &str) -> async_graphql::Value {
    if let async_graphql::Value::Object(map) = parent {
        map.get(key).cloned().unwrap_or(async_graphql::Value::Null)
    } else {
        async_graphql::Value::Null
    }
}

/// Extract a list field from a parent `GraphQL` value, returning individual
/// field values for use with `FieldValue::list`
fn extract_list(parent: &async_graphql::Value, key: &str) -> Vec<FieldValue<'static>> {
    if let async_graphql::Value::Object(map) = parent
        && let Some(async_graphql::Value::List(items)) = map.get(key)
    {
        return items
            .iter()
            .map(|v| FieldValue::value(v.clone()))
            .collect();
    }

    vec![]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(
        first: Option<usize>,
        after: Option<usize>,
        last: Option<usize>,
        before: Option<usize>,
        offset: Option<usize>,
        total: i64,
    ) -> (usize, usize) {
        let w = compute_window(
            PageArgs {
                first,
                after,
                last,
                before,
                offset,
            },
            25,
            100,
            total,
        );
        (w.skip, w.take)
    }

    #[test]
    fn window_default_forward() {
        assert_eq!(win(None, None, None, None, None, 100), (0, 25));
    }

    #[test]
    fn window_first() {
        assert_eq!(win(Some(10), None, None, None, None, 100), (0, 10));
    }

    #[test]
    fn window_after_then_first() {
        // after cursor at offset 9 -> start at 10
        assert_eq!(win(Some(10), Some(9), None, None, None, 100), (10, 10));
    }

    #[test]
    fn window_last_of_total() {
        assert_eq!(win(None, None, Some(10), None, None, 100), (90, 10));
    }

    #[test]
    fn window_before_then_last() {
        // before cursor at offset 50 -> upper bound 50, last 10 -> [40, 50)
        assert_eq!(win(None, None, Some(10), Some(50), None, 100), (40, 10));
    }

    #[test]
    fn window_offset_then_first() {
        assert_eq!(win(Some(10), None, None, None, Some(5), 100), (5, 10));
    }

    #[test]
    fn window_first_capped_at_max() {
        // first 1000 capped at max_page_size 100
        assert_eq!(win(Some(1000), None, None, None, None, 500), (0, 100));
    }

    #[test]
    fn window_last_exceeds_total() {
        assert_eq!(win(None, None, Some(10), None, None, 3), (0, 3));
    }

    #[test]
    fn window_after_past_end() {
        assert_eq!(win(Some(10), Some(500), None, None, None, 100), (100, 0));
    }

    #[test]
    fn window_first_zero() {
        assert_eq!(win(Some(0), None, None, None, None, 100), (0, 0));
    }

    #[test]
    fn window_first_then_last_combined() {
        // first 20 -> [0,20); last 5 -> [15,20)
        assert_eq!(win(Some(20), None, Some(5), None, None, 100), (15, 5));
    }

    #[test]
    fn cursor_roundtrip() {
        for offset in [0, 1, 42, 999, 100_000] {
            let encoded = encode_cursor(offset);
            let decoded = decode_cursor(&encoded);
            assert_eq!(decoded, Some(offset), "roundtrip failed for offset {offset}");
        }
    }

    #[test]
    fn decode_invalid_cursor() {
        // Not valid base64
        assert_eq!(decode_cursor("!!!invalid!!!"), None);

        // Valid base64 but wrong prefix
        let no_prefix = URL_SAFE_NO_PAD.encode("offset:42");
        assert_eq!(decode_cursor(&no_prefix), None);

        // Valid base64, correct prefix, but non-numeric offset
        let bad_number = URL_SAFE_NO_PAD.encode("cursor:abc");
        assert_eq!(decode_cursor(&bad_number), None);

        // Empty string
        assert_eq!(decode_cursor(""), None);
    }

    #[test]
    fn connection_response_structure() {
        let rows = vec![
            serde_json::json!({"id": 1, "name": "Alice"}),
            serde_json::json!({"id": 2, "name": "Bob"}),
        ];

        let response = build_connection_response(rows, 0, 5);

        // Check top-level keys
        assert!(response.get("edges").is_some());
        assert!(response.get("nodes").is_some());
        assert!(response.get("pageInfo").is_some());
        assert_eq!(response["totalCount"], 5);

        // Check edges
        let edges = response["edges"].as_array().unwrap();
        assert_eq!(edges.len(), 2);
        assert_eq!(edges[0]["node"]["name"], "Alice");
        assert!(edges[0]["cursor"].is_string());
        assert_eq!(edges[1]["node"]["name"], "Bob");

        // Check nodes shortcut matches edge nodes
        let nodes = response["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0]["name"], "Alice");
        assert_eq!(nodes[1]["name"], "Bob");

        // Check page info
        let page_info = &response["pageInfo"];
        assert_eq!(page_info["hasNextPage"], true);
        assert_eq!(page_info["hasPreviousPage"], false);
        assert!(page_info["startCursor"].is_string());
        assert!(page_info["endCursor"].is_string());

        // Verify cursors decode correctly
        let start = page_info["startCursor"].as_str().unwrap();
        let end = page_info["endCursor"].as_str().unwrap();
        assert_eq!(decode_cursor(start), Some(0));
        assert_eq!(decode_cursor(end), Some(1));
    }

    #[test]
    fn last_page_detection() {
        let rows = vec![
            serde_json::json!({"id": 3}),
            serde_json::json!({"id": 4}),
        ];

        // Offset 2, total 4 — this is the last page
        let response = build_connection_response(rows, 2, 4);
        let page_info = &response["pageInfo"];

        assert_eq!(page_info["hasNextPage"], false);
        assert_eq!(page_info["hasPreviousPage"], true);
    }

    #[test]
    fn empty_result_set() {
        let response = build_connection_response(vec![], 0, 0);

        let edges = response["edges"].as_array().unwrap();
        assert!(edges.is_empty());

        let nodes = response["nodes"].as_array().unwrap();
        assert!(nodes.is_empty());

        let page_info = &response["pageInfo"];
        assert_eq!(page_info["hasNextPage"], false);
        assert_eq!(page_info["hasPreviousPage"], false);
        assert!(page_info["startCursor"].is_null());
        assert!(page_info["endCursor"].is_null());
    }
}
