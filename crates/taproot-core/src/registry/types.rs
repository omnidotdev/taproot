use serde::{Deserialize, Serialize};

/// Options controlling how the GraphQL schema is generated from introspected
/// database metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaOptions {
    /// Enable Relay-style cursor pagination on list queries
    #[serde(default = "default_relay_pagination")]
    pub relay_pagination: bool,
    /// Default number of rows returned by list queries
    #[serde(default = "default_page_size")]
    pub default_page_size: usize,
    /// Maximum number of rows a client may request
    #[serde(default = "default_max_page_size")]
    pub max_page_size: usize,
    /// Enable Apollo Federation support
    #[serde(default)]
    pub federation: bool,
    /// Enable GraphQL subscriptions
    #[serde(default)]
    pub subscriptions: bool,
    /// Maximum query depth (0 = unlimited)
    #[serde(default = "default_max_depth")]
    pub max_depth: usize,
    /// Maximum query complexity (0 = unlimited)
    #[serde(default = "default_max_complexity")]
    pub max_complexity: usize,
}

const fn default_relay_pagination() -> bool {
    true
}

const fn default_page_size() -> usize {
    25
}

const fn default_max_page_size() -> usize {
    100
}

const fn default_max_depth() -> usize {
    15
}

const fn default_max_complexity() -> usize {
    1000
}

impl Default for SchemaOptions {
    fn default() -> Self {
        Self {
            relay_pagination: default_relay_pagination(),
            default_page_size: default_page_size(),
            max_page_size: default_max_page_size(),
            federation: false,
            subscriptions: false,
            max_depth: default_max_depth(),
            max_complexity: default_max_complexity(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_values() {
        let opts = SchemaOptions::default();
        assert!(opts.relay_pagination);
        assert_eq!(opts.default_page_size, 25);
        assert_eq!(opts.max_page_size, 100);
        assert!(!opts.federation);
        assert!(!opts.subscriptions);
        assert_eq!(opts.max_depth, 15);
        assert_eq!(opts.max_complexity, 1000);
    }

    #[test]
    fn partial_deserialize_fills_defaults() {
        // A config that sets only some fields must fill the rest with the
        // documented defaults, not with type-zero values
        let opts: SchemaOptions =
            serde_json::from_str(r#"{ "default_page_size": 50, "federation": true }"#).unwrap();
        assert_eq!(opts.default_page_size, 50);
        assert!(opts.federation);
        // Unset fields keep their real defaults
        assert_eq!(opts.max_page_size, 100);
        assert!(opts.relay_pagination);
        assert_eq!(opts.max_depth, 15);
        assert_eq!(opts.max_complexity, 1000);
    }
}
