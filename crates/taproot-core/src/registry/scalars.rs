//! Map Postgres `udt_name` values to `GraphQL` type references.
//!
//! Custom scalars that don't exist in the base `GraphQL` spec (UUID, `DateTime`,
//! etc.) must be explicitly registered in the schema — use [`needs_custom_scalar`]
//! to check whether a type name requires registration.

/// Map a Postgres `udt_name` to a `GraphQL` type name.
///
/// Array types (prefixed with `_`) are stripped before lookup and the result is
/// indicated via the returned boolean (`true` = array).
#[must_use]
pub fn map_udt_to_graphql(udt_name: &str) -> (&'static str, bool) {
    let (base, is_array) = udt_name
        .strip_prefix('_')
        .map_or((udt_name, false), |inner| (inner, true));

    #[allow(clippy::match_same_arms)]
    let graphql_type = match base {
        // Explicit string types listed for clarity, fallback also returns String
        "text" | "varchar" | "character varying" | "bpchar" | "character" | "name" | "citext"
        | "char" => "String",
        "int2" | "smallint" | "smallserial" | "int4" | "int" | "integer" | "serial" => "Int",
        "int8" | "bigint" | "bigserial" => "BigInt",
        "float4" | "real" | "float8" | "double precision" => "Float",
        "bool" | "boolean" => "Boolean",
        "uuid" => "UUID",
        "timestamptz" | "timestamp" | "timestamp with time zone"
        | "timestamp without time zone" => "DateTime",
        "date" => "Date",
        "json" | "jsonb" => "JSON",
        "numeric" | "decimal" => "BigDecimal",
        _ => "String",
    };

    (graphql_type, is_array)
}

/// Return the custom scalar name that must be registered for a `GraphQL` type,
/// if any. Built-in scalars (`String`, `Int`, `Float`, `Boolean`) return `None`.
#[must_use]
pub fn needs_custom_scalar(graphql_type: &str) -> Option<&'static str> {
    match graphql_type {
        "UUID" => Some("UUID"),
        "DateTime" => Some("DateTime"),
        "Date" => Some("Date"),
        "JSON" => Some("JSON"),
        "BigInt" => Some("BigInt"),
        "BigDecimal" => Some("BigDecimal"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_common_types() {
        assert_eq!(map_udt_to_graphql("text"), ("String", false));
        assert_eq!(map_udt_to_graphql("varchar"), ("String", false));
        assert_eq!(map_udt_to_graphql("int4"), ("Int", false));
        assert_eq!(map_udt_to_graphql("int8"), ("BigInt", false));
        assert_eq!(map_udt_to_graphql("float8"), ("Float", false));
        assert_eq!(map_udt_to_graphql("bool"), ("Boolean", false));
        assert_eq!(map_udt_to_graphql("uuid"), ("UUID", false));
        assert_eq!(map_udt_to_graphql("timestamptz"), ("DateTime", false));
        assert_eq!(map_udt_to_graphql("date"), ("Date", false));
        assert_eq!(map_udt_to_graphql("jsonb"), ("JSON", false));
        assert_eq!(map_udt_to_graphql("numeric"), ("BigDecimal", false));
    }

    #[test]
    fn map_array_types() {
        assert_eq!(map_udt_to_graphql("_text"), ("String", true));
        assert_eq!(map_udt_to_graphql("_uuid"), ("UUID", true));
        assert_eq!(map_udt_to_graphql("_int4"), ("Int", true));
    }

    #[test]
    fn unknown_falls_back_to_string() {
        assert_eq!(map_udt_to_graphql("geometry"), ("String", false));
    }

    #[test]
    fn custom_scalar_detection() {
        assert_eq!(needs_custom_scalar("UUID"), Some("UUID"));
        assert_eq!(needs_custom_scalar("DateTime"), Some("DateTime"));
        assert_eq!(needs_custom_scalar("String"), None);
        assert_eq!(needs_custom_scalar("Int"), None);
        assert_eq!(needs_custom_scalar("Boolean"), None);
    }
}
