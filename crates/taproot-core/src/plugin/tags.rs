use std::collections::HashMap;

/// Parsed smart tags from a Postgres comment string
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SmartTags {
    /// Human-readable description (text before any `@tags`)
    pub description: Option<String>,
    /// Parsed tags: `@tagName` maps to value (empty string if boolean/no-value tag)
    pub tags: HashMap<String, String>,
}

impl SmartTags {
    /// Check if a tag is present
    #[must_use]
    pub fn has(&self, tag: &str) -> bool {
        self.tags.contains_key(tag)
    }

    /// Get a tag's value, returning `None` if tag is absent
    #[must_use]
    pub fn get(&self, tag: &str) -> Option<&str> {
        self.tags.get(tag).map(String::as_str)
    }
}

/// Parse a single tag body (the text after `@`) into a `(name, value)` pair.
///
/// Supports both the space form (`omit create,update`) and the parenthesized
/// form (`omit(create)`). A bare tag (`omit`) yields an empty value
fn parse_tag(rest: &str) -> (String, String) {
    if let Some(open) = rest.find('(') {
        let name = &rest[..open];
        let trimmed = rest.trim_end();
        if !name.contains(' ') && trimmed.ends_with(')') {
            let value = &rest[open + 1..trimmed.len() - 1];
            return (name.to_string(), value.trim().to_string());
        }
    }

    let mut parts = rest.splitn(2, ' ');
    let name = parts.next().unwrap_or_default().to_string();
    let value = parts.next().map(str::trim).unwrap_or_default().to_string();
    (name, value)
}

/// Parse a Postgres comment string into smart tags.
///
/// Tags are lines starting with `@`. Everything before the first tag line
/// is the description. Tag format: `@name` (boolean), `@name value`, or
/// `@name(value)`.
///
/// # Examples
///
/// ```
/// use taproot_core::plugin::tags::parse_smart_tags;
///
/// let result = parse_smart_tags("A user table\n@omit create,update\n@name users");
/// assert_eq!(result.description, Some("A user table".to_string()));
/// assert_eq!(result.get("omit"), Some("create,update"));
/// assert!(result.has("name"));
/// ```
#[must_use]
pub fn parse_smart_tags(comment: &str) -> SmartTags {
    let mut description_lines = Vec::new();
    let mut tags = HashMap::new();
    let mut in_tags = false;

    for line in comment.lines() {
        let trimmed = line.trim();

        if let Some(rest) = trimmed.strip_prefix('@') {
            in_tags = true;
            let (tag_name, tag_value) = parse_tag(rest);

            if !tag_name.is_empty() {
                tags.insert(tag_name, tag_value);
            }
        } else if !in_tags && !trimmed.is_empty() {
            description_lines.push(trimmed);
        }
    }

    let description = if description_lines.is_empty() {
        None
    } else {
        Some(description_lines.join("\n"))
    };

    SmartTags { description, tags }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn description_only() {
        let result = parse_smart_tags("A table for storing users");
        assert_eq!(
            result.description,
            Some("A table for storing users".to_string())
        );
        assert!(result.tags.is_empty());
    }

    #[test]
    fn tags_only() {
        let result = parse_smart_tags("@omit");
        assert_eq!(result.description, None);
        assert!(result.has("omit"));
        assert_eq!(result.get("omit"), Some(""));
    }

    #[test]
    fn tag_with_value() {
        let result = parse_smart_tags("@omit create,update,delete");
        assert_eq!(result.get("omit"), Some("create,update,delete"));
    }

    #[test]
    fn tag_with_parenthesized_value() {
        let result = parse_smart_tags("@omit(create)");
        assert_eq!(result.get("omit"), Some("create"));

        let multi = parse_smart_tags("@omit(create,update)");
        assert_eq!(multi.get("omit"), Some("create,update"));
    }

    #[test]
    fn description_and_tags() {
        let result = parse_smart_tags("The users table\n@omit create\n@name users");
        assert_eq!(result.description, Some("The users table".to_string()));
        assert_eq!(result.get("omit"), Some("create"));
        assert_eq!(result.get("name"), Some("users"));
    }

    #[test]
    fn empty_comment() {
        let result = parse_smart_tags("");
        assert_eq!(result, SmartTags::default());
    }

    #[test]
    fn whitespace_handling() {
        let result = parse_smart_tags("  Some description  \n  @omit   create  ");
        assert_eq!(result.description, Some("Some description".to_string()));
        assert_eq!(result.get("omit"), Some("create"));
    }

    #[test]
    fn has_and_get_helpers() {
        let result = parse_smart_tags("@deprecated\n@name users");
        assert!(result.has("deprecated"));
        assert!(!result.has("missing"));
        assert_eq!(result.get("deprecated"), Some(""));
        assert_eq!(result.get("name"), Some("users"));
        assert_eq!(result.get("missing"), None);
    }

    #[test]
    fn multiple_tags() {
        let result = parse_smart_tags("@omit\n@name users\n@foreignKey user_id");
        assert_eq!(result.tags.len(), 3);
        assert!(result.has("omit"));
        assert_eq!(result.get("name"), Some("users"));
        assert_eq!(result.get("foreignKey"), Some("user_id"));
    }

    #[test]
    fn multiline_description() {
        let result = parse_smart_tags("First line\nSecond line\n@tag value");
        assert_eq!(
            result.description,
            Some("First line\nSecond line".to_string())
        );
        assert_eq!(result.get("tag"), Some("value"));
    }

    #[test]
    fn description_lines_after_tags_ignored() {
        // Once tags start, non-tag lines are ignored
        let result = parse_smart_tags("Description\n@tag value\nstray line");
        assert_eq!(result.description, Some("Description".to_string()));
        assert_eq!(result.tags.len(), 1);
    }

    #[test]
    fn bare_at_sign_ignored() {
        // A lone `@` with no tag name should be ignored
        let result = parse_smart_tags("@");
        assert!(result.tags.is_empty());
    }
}
