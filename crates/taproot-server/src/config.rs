use std::path::Path;

use serde::{Deserialize, Serialize};
use taproot_core::registry::types::SchemaOptions;

/// Top-level configuration for the Taproot server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Database connection settings.
    #[serde(default)]
    pub database: DatabaseConfig,
    /// HTTP server settings.
    #[serde(default)]
    pub server: ServerConfig,
    /// `GraphQL` schema generation settings.
    #[serde(default)]
    pub schema: SchemaOptions,
}

/// Database connection configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseConfig {
    /// Postgres connection URL.
    pub url: String,
    /// Maximum connection pool size.
    #[serde(default = "default_pool_size")]
    pub pool_size: u32,
    /// Schemas to introspect.
    #[serde(default = "default_schemas")]
    pub schemas: Vec<String>,
    /// Tables to exclude from introspection.
    #[serde(default)]
    pub exclude_tables: Vec<String>,
}

/// HTTP server configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Listen address in `host:port` format.
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Enable the `GraphiQL` playground at `GET /`.
    #[serde(default = "default_playground")]
    pub playground: bool,
}

impl Config {
    /// Load configuration from a YAML file.
    ///
    /// # Errors
    /// Returns an error if the file cannot be read or parsed.
    pub fn from_file(path: &Path) -> anyhow::Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        let config: Self = serde_yaml::from_str(&contents)?;
        Ok(config)
    }
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            pool_size: default_pool_size(),
            schemas: default_schemas(),
            exclude_tables: Vec::new(),
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            playground: default_playground(),
        }
    }
}

const fn default_pool_size() -> u32 {
    20
}

fn default_schemas() -> Vec<String> {
    vec!["public".to_string()]
}

fn default_listen() -> String {
    "0.0.0.0:4001".to_string()
}

const fn default_playground() -> bool {
    true
}
