use std::collections::HashMap;

/// Info about a federation entity type, mapping GraphQL type to database table
#[derive(Debug, Clone)]
pub struct EntityInfo {
    pub schema: String,
    pub table: String,
    /// Primary key columns: (`camelCase` arg name, `snake_case` column name)
    pub pk_columns: Vec<(String, String)>,
}

/// Maps GraphQL type names to their entity info for federation resolution
#[derive(Debug, Clone, Default)]
pub struct EntityMap {
    entities: HashMap<String, EntityInfo>,
}

impl EntityMap {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, type_name: &str, info: EntityInfo) {
        self.entities.insert(type_name.to_string(), info);
    }

    #[must_use]
    pub fn get(&self, type_name: &str) -> Option<&EntityInfo> {
        self.entities.get(type_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_map_stores_table_info() {
        let mut map = EntityMap::new();
        map.register(
            "User",
            EntityInfo {
                schema: "public".into(),
                table: "users".into(),
                pk_columns: vec![("id".into(), "id".into())],
            },
        );
        let info = map.get("User");
        assert!(info.is_some());
        assert_eq!(info.unwrap().table, "users");
    }

    #[test]
    fn entity_map_returns_none_for_unknown() {
        let map = EntityMap::new();
        assert!(map.get("Unknown").is_none());
    }
}
