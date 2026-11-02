//! Relay Global Object Identification.
//!
//! Encodes and decodes the opaque `nodeId: ID!` that every table type exposes,
//! enabling the `node(nodeId:)` root field to refetch any object by its global
//! id. The wire format is URL-safe base64 (no padding) of a JSON array whose
//! first element is the GraphQL type name and whose remaining elements are the
//! row's primary-key values in column order: `[typeName, pk1, pk2, ...]`.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::Value;

/// Error decoding a global node id.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NodeIdError {
    /// The id was not valid base64, not valid JSON, or not a non-empty array
    /// whose first element is the type name
    #[error("malformed node id")]
    Malformed,
}

/// Encode a global node id from a type name and its primary-key values.
#[must_use]
pub fn encode_node_id(type_name: &str, pk_values: &[Value]) -> String {
    let mut parts: Vec<Value> = Vec::with_capacity(pk_values.len() + 1);
    parts.push(Value::String(type_name.to_string()));
    parts.extend(pk_values.iter().cloned());

    URL_SAFE_NO_PAD.encode(Value::Array(parts).to_string())
}

/// Decode a global node id into its type name and primary-key values.
///
/// # Errors
///
/// Returns [`NodeIdError::Malformed`] when the input is not valid base64, does
/// not decode to a JSON array, or the array is empty or lacks a string type
/// name in the first position.
pub fn decode_node_id(node_id: &str) -> Result<(String, Vec<Value>), NodeIdError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(node_id)
        .map_err(|_| NodeIdError::Malformed)?;

    let parsed: Value = serde_json::from_slice(&bytes).map_err(|_| NodeIdError::Malformed)?;

    let Value::Array(mut parts) = parsed else {
        return Err(NodeIdError::Malformed);
    };

    if parts.is_empty() {
        return Err(NodeIdError::Malformed);
    }

    let Value::String(type_name) = parts.remove(0) else {
        return Err(NodeIdError::Malformed);
    };

    Ok((type_name, parts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trips_single_pk() {
        let id = encode_node_id("User", &[json!("00000000-0000-0000-0000-000000000001")]);
        let (type_name, pks) = decode_node_id(&id).expect("decode");
        assert_eq!(type_name, "User");
        assert_eq!(pks, vec![json!("00000000-0000-0000-0000-000000000001")]);
    }

    #[test]
    fn round_trips_composite_pk() {
        let id = encode_node_id("UserRole", &[json!(7), json!("admin")]);
        let (type_name, pks) = decode_node_id(&id).expect("decode");
        assert_eq!(type_name, "UserRole");
        assert_eq!(pks, vec![json!(7), json!("admin")]);
    }

    #[test]
    fn round_trips_type_with_no_pk_values() {
        let id = encode_node_id("Query", &[]);
        let (type_name, pks) = decode_node_id(&id).expect("decode");
        assert_eq!(type_name, "Query");
        assert!(pks.is_empty());
    }

    #[test]
    fn id_is_opaque_base64url() {
        let id = encode_node_id("User", &[json!(1)]);
        // URL-safe, unpadded: no '+', '/', or '=' that would need escaping
        assert!(!id.contains('+') && !id.contains('/') && !id.contains('='));
    }

    #[test]
    fn rejects_non_base64() {
        assert_eq!(decode_node_id("not valid base64!!"), Err(NodeIdError::Malformed));
    }

    #[test]
    fn rejects_non_array_payload() {
        let not_array = URL_SAFE_NO_PAD.encode("{\"a\":1}");
        assert_eq!(decode_node_id(&not_array), Err(NodeIdError::Malformed));
    }

    #[test]
    fn rejects_empty_array() {
        let empty = URL_SAFE_NO_PAD.encode("[]");
        assert_eq!(decode_node_id(&empty), Err(NodeIdError::Malformed));
    }

    #[test]
    fn rejects_non_string_type_name() {
        let bad = URL_SAFE_NO_PAD.encode("[1,2]");
        assert_eq!(decode_node_id(&bad), Err(NodeIdError::Malformed));
    }
}
