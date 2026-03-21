use serde::{Deserialize, Serialize};

/// Options controlling how the GraphQL schema is generated from introspected
/// database metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaOptions {
    /// Enable Relay-style cursor pagination on list queries
    pub relay_pagination: bool,
    /// Default number of rows returned by list queries
    pub default_page_size: usize,
    /// Maximum number of rows a client may request
    pub max_page_size: usize,
    /// Enable Apollo Federation support
    pub federation: bool,
}

impl Default for SchemaOptions {
    fn default() -> Self {
        Self {
            relay_pagination: true,
            default_page_size: 25,
            max_page_size: 100,
            federation: false,
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
    }
}
