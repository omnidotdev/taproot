use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use taproot_core::db::{Database, SqlxBackend};
use taproot_core::registry::types::SchemaOptions;
use taproot_core::{IntrospectOptions, Plugin, build_schema, introspect};

/// Unique schema name per test run to avoid collisions.
fn test_schema() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock error")
        .subsec_nanos();
    let tid = std::thread::current().id();

    format!("taproot_test_{nanos}_{tid:?}")
        .replace(['(', ')'], "")
        .to_lowercase()
}

/// Set up test tables with enum, FK, comments, and seed data.
async fn setup_test_schema(db: &dyn Database, schema: &str) {
    let ddl = [
        &format!("CREATE SCHEMA IF NOT EXISTS \"{schema}\""),
        &format!("CREATE TYPE \"{schema}\".user_role AS ENUM ('admin', 'editor', 'viewer')"),
        &format!(
            "CREATE TABLE \"{schema}\".users (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                name TEXT NOT NULL,
                email TEXT NOT NULL UNIQUE,
                role \"{schema}\".user_role NOT NULL DEFAULT 'viewer',
                created_at TIMESTAMPTZ NOT NULL DEFAULT now()
            )"
        ),
        &format!("COMMENT ON TABLE \"{schema}\".users IS 'Application users'"),
        &format!("COMMENT ON COLUMN \"{schema}\".users.email IS 'Unique email address'"),
        &format!(
            "CREATE TABLE \"{schema}\".posts (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                title TEXT NOT NULL,
                content TEXT,
                author_id UUID NOT NULL REFERENCES \"{schema}\".users(id),
                published BOOLEAN NOT NULL DEFAULT false,
                created_at TIMESTAMPTZ NOT NULL DEFAULT now()
            )"
        ),
        &format!("COMMENT ON TABLE \"{schema}\".posts IS 'Blog posts'"),
        &format!("COMMENT ON COLUMN \"{schema}\".posts.title IS 'Post title'"),
    ];

    for stmt in &ddl {
        db.query_raw(stmt).await.expect("DDL statement failed");
    }

    // Seed data
    let seed = [
        &format!(
            "INSERT INTO \"{schema}\".users (id, name, email, role) VALUES \
             ('00000000-0000-0000-0000-000000000001', 'Alice', 'alice@example.com', 'admin'), \
             ('00000000-0000-0000-0000-000000000002', 'Bob', 'bob@example.com', 'editor')"
        ) as &str,
        &format!(
            "INSERT INTO \"{schema}\".posts (id, title, content, author_id, published) VALUES \
             ('10000000-0000-0000-0000-000000000001', 'First Post', 'Hello world', '00000000-0000-0000-0000-000000000001', true), \
             ('10000000-0000-0000-0000-000000000002', 'Draft Post', 'Work in progress', '00000000-0000-0000-0000-000000000002', false)"
        ),
    ];

    for stmt in &seed {
        db.query_raw(stmt).await.expect("Seed statement failed");
    }
}

/// Drop the test schema on cleanup.
async fn teardown_test_schema(db: &dyn Database, schema: &str) {
    let _ = db
        .query_raw(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .await;
}

/// Connect to the test database, returning `None` if `DATABASE_URL` is unset.
async fn connect() -> Option<SqlxBackend> {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL not set -- skipping integration tests");
        return None;
    };

    Some(
        SqlxBackend::connect(&url)
            .await
            .expect("failed to connect to test database"),
    )
}

/// Build a GQL schema from the test database for reuse across tests.
async fn build_test_schema(
    db: &SqlxBackend,
    schema_name: &str,
) -> async_graphql::dynamic::Schema {
    let options = IntrospectOptions {
        schemas: vec![schema_name.to_string()],
        exclude_tables: Vec::new(),
        smart_comments: true,
    };

    let introspected = introspect(db, &options)
        .await
        .expect("introspection failed");

    let db_arc: Arc<dyn Database> = Arc::new(db.clone());
    let schema_opts = SchemaOptions::default();

    build_schema(&introspected, &db_arc, &schema_opts, &[]).expect("schema build failed")
}

#[tokio::test]
async fn introspection_discovers_tables_and_constraints() {
    let Some(db) = connect().await else {
        return;
    };

    let schema = test_schema();
    setup_test_schema(&db, &schema).await;

    let options = IntrospectOptions {
        schemas: vec![schema.clone()],
        exclude_tables: Vec::new(),
        smart_comments: true,
    };

    let result = introspect(&db, &options).await;
    teardown_test_schema(&db, &schema).await;

    let schemas = result.expect("introspection failed");
    assert_eq!(schemas.len(), 1);

    let introspected = &schemas[0];

    // Tables discovered
    let table_names: Vec<&str> = introspected.tables.iter().map(|t| t.name.as_str()).collect();
    assert!(table_names.contains(&"users"), "users table not found");
    assert!(table_names.contains(&"posts"), "posts table not found");

    // Users table columns
    let users = introspected
        .tables
        .iter()
        .find(|t| t.name == "users")
        .expect("users table missing");
    assert_eq!(users.columns.len(), 5, "users should have 5 columns");
    assert_eq!(
        users.comment.as_deref(),
        Some("Application users"),
        "table comment mismatch"
    );

    // Primary key detected
    let pk = users.primary_key.as_ref().expect("users PK missing");
    assert_eq!(pk.columns, vec!["id"]);

    // Unique constraint on email
    assert!(
        users
            .unique_constraints
            .iter()
            .any(|uc| uc.columns.contains(&"email".to_string())),
        "email unique constraint not found"
    );

    // Posts FK to users
    let posts = introspected
        .tables
        .iter()
        .find(|t| t.name == "posts")
        .expect("posts table missing");
    assert!(!posts.foreign_keys.is_empty(), "posts FK not found");

    let fk = &posts.foreign_keys[0];
    assert_eq!(fk.foreign_table, "users");
    assert_eq!(fk.columns, vec!["author_id"]);
    assert_eq!(fk.foreign_columns, vec!["id"]);

    // Column comment
    let email_col = users
        .columns
        .iter()
        .find(|c| c.name == "email")
        .expect("email column missing");
    assert_eq!(email_col.comment.as_deref(), Some("Unique email address"));

    // Enum detected
    assert!(!introspected.enums.is_empty(), "enums not found");
    let role_enum = introspected
        .enums
        .iter()
        .find(|e| e.name == "user_role")
        .expect("user_role enum missing");
    assert_eq!(role_enum.values, vec!["admin", "editor", "viewer"]);
}

#[tokio::test]
async fn build_schema_from_introspected_metadata() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let gql_schema = build_test_schema(&db, &schema_name).await;

    teardown_test_schema(&db, &schema_name).await;

    let sdl = gql_schema.sdl();

    assert!(sdl.contains("User"), "User type missing from SDL");
    assert!(sdl.contains("Post"), "Post type missing from SDL");
    assert!(sdl.contains("UserRole"), "UserRole enum missing from SDL");
    assert!(sdl.contains("users"), "users query field missing from SDL");
    assert!(sdl.contains("posts"), "posts query field missing from SDL");

    // Connection types registered
    assert!(
        sdl.contains("UserConnection"),
        "UserConnection type missing from SDL"
    );
    assert!(
        sdl.contains("PostConnection"),
        "PostConnection type missing from SDL"
    );
    assert!(sdl.contains("PageInfo"), "PageInfo type missing from SDL");

    // Condition and ordering types registered
    assert!(
        sdl.contains("UserCondition"),
        "UserCondition type missing from SDL"
    );
    assert!(
        sdl.contains("UserOrderBy"),
        "UserOrderBy enum missing from SDL"
    );
}

#[tokio::test]
async fn execute_list_query() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let gql_schema = build_test_schema(&db, &schema_name).await;

    // List queries now return connections
    let result = gql_schema
        .execute("{ users { nodes { name email } totalCount } }")
        .await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        result.errors.is_empty(),
        "query errors: {:?}",
        result.errors
    );

    let data = result.data.into_json().expect("failed to serialize result");
    let users_conn = data.get("users").expect("users field missing");

    // Verify total count
    assert_eq!(
        users_conn.get("totalCount").and_then(serde_json::Value::as_i64),
        Some(2),
        "expected totalCount of 2"
    );

    // Verify nodes
    let nodes = users_conn
        .get("nodes")
        .and_then(|v| v.as_array())
        .expect("nodes field missing or not an array");
    assert_eq!(nodes.len(), 2);

    let names: Vec<&str> = nodes
        .iter()
        .filter_map(|u| u.get("name").and_then(|v| v.as_str()))
        .collect();
    assert!(names.contains(&"Alice"));
    assert!(names.contains(&"Bob"));
}

#[tokio::test]
async fn execute_list_query_with_edges() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let gql_schema = build_test_schema(&db, &schema_name).await;

    let result = gql_schema
        .execute("{ users { edges { node { name } cursor } pageInfo { hasNextPage hasPreviousPage } } }")
        .await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        result.errors.is_empty(),
        "query errors: {:?}",
        result.errors
    );

    let data = result.data.into_json().expect("failed to serialize result");
    let users_conn = data.get("users").expect("users field missing");

    let edges = users_conn
        .get("edges")
        .and_then(|v| v.as_array())
        .expect("edges field missing");
    assert_eq!(edges.len(), 2);

    // Each edge has a cursor
    for edge in edges {
        assert!(
            edge.get("cursor").and_then(|v| v.as_str()).is_some(),
            "edge missing cursor"
        );
        assert!(edge.get("node").is_some(), "edge missing node");
    }

    // Page info
    let page_info = users_conn.get("pageInfo").expect("pageInfo missing");
    assert_eq!(page_info.get("hasNextPage"), Some(&serde_json::json!(false)));
    assert_eq!(
        page_info.get("hasPreviousPage"),
        Some(&serde_json::json!(false))
    );
}

#[tokio::test]
async fn execute_singular_query_by_id() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let gql_schema = build_test_schema(&db, &schema_name).await;

    let query = r#"{ user(id: "00000000-0000-0000-0000-000000000001") { name } }"#;
    let result = gql_schema.execute(query).await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        result.errors.is_empty(),
        "singular query errors: {:?}",
        result.errors
    );

    let data = result.data.into_json().expect("failed to serialize result");
    let user = data.get("user").expect("user field missing");
    assert_eq!(
        user.get("name").and_then(|v| v.as_str()),
        Some("Alice"),
        "expected Alice for user with id ...0001"
    );
}

#[tokio::test]
async fn connection_query_with_pagination() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let gql_schema = build_test_schema(&db, &schema_name).await;

    // First page: request 1 item
    let result = gql_schema
        .execute("{ users(first: 1) { nodes { name } edges { cursor } totalCount pageInfo { hasNextPage hasPreviousPage endCursor } } }")
        .await;

    assert!(
        result.errors.is_empty(),
        "first page errors: {:?}",
        result.errors
    );

    let data = result.data.into_json().expect("failed to serialize");
    let conn = data.get("users").expect("users missing");

    assert_eq!(conn["totalCount"], 2);

    let nodes = conn["nodes"].as_array().expect("nodes missing");
    assert_eq!(nodes.len(), 1, "expected 1 node on first page");

    let page_info = &conn["pageInfo"];
    assert_eq!(page_info["hasNextPage"], true);
    assert_eq!(page_info["hasPreviousPage"], false);

    // Second page: use the cursor from the first page
    let end_cursor = page_info["endCursor"]
        .as_str()
        .expect("endCursor missing");

    let second_query = format!(
        r#"{{ users(first: 1, after: "{end_cursor}") {{ nodes {{ name }} totalCount pageInfo {{ hasNextPage hasPreviousPage }} }} }}"#
    );

    let result2 = gql_schema.execute(&second_query).await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        result2.errors.is_empty(),
        "second page errors: {:?}",
        result2.errors
    );

    let data2 = result2.data.into_json().expect("failed to serialize");
    let conn2 = data2.get("users").expect("users missing");

    let nodes2 = conn2["nodes"].as_array().expect("nodes missing");
    assert_eq!(nodes2.len(), 1, "expected 1 node on second page");

    let page_info2 = &conn2["pageInfo"];
    assert_eq!(page_info2["hasNextPage"], false);
    assert_eq!(page_info2["hasPreviousPage"], true);
}

#[tokio::test]
async fn filter_query() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let gql_schema = build_test_schema(&db, &schema_name).await;

    // Filter by name equality
    let query = r#"{ users(condition: { name: { eq: "Alice" } }) { nodes { name email } totalCount } }"#;
    let result = gql_schema.execute(query).await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        result.errors.is_empty(),
        "filter query errors: {:?}",
        result.errors
    );

    let data = result.data.into_json().expect("failed to serialize");
    let conn = data.get("users").expect("users missing");

    assert_eq!(conn["totalCount"], 1);

    let nodes = conn["nodes"].as_array().expect("nodes missing");
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0]["name"], "Alice");
    assert_eq!(nodes[0]["email"], "alice@example.com");
}

#[tokio::test]
async fn order_by_query() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let gql_schema = build_test_schema(&db, &schema_name).await;

    // Order by name descending
    let query = "{ users(orderBy: [NAME_DESC]) { nodes { name } } }";
    let result = gql_schema.execute(query).await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        result.errors.is_empty(),
        "order query errors: {:?}",
        result.errors
    );

    let data = result.data.into_json().expect("failed to serialize");
    let conn = data.get("users").expect("users missing");
    let nodes = conn["nodes"].as_array().expect("nodes missing");

    assert_eq!(nodes.len(), 2);
    assert_eq!(nodes[0]["name"], "Bob", "expected Bob first when DESC");
    assert_eq!(nodes[1]["name"], "Alice", "expected Alice second when DESC");
}

#[tokio::test]
async fn unique_constraint_query() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let gql_schema = build_test_schema(&db, &schema_name).await;

    let result = gql_schema
        .execute(r#"{ userByEmail(email: "alice@example.com") { name } }"#)
        .await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        result.errors.is_empty(),
        "unique constraint query errors: {:?}",
        result.errors
    );

    let data = result.data.into_json().expect("failed to serialize result");
    assert_eq!(
        data["userByEmail"]["name"], "Alice",
        "expected Alice for email alice@example.com"
    );
}

#[tokio::test]
async fn create_mutation() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let gql_schema = build_test_schema(&db, &schema_name).await;

    let mutation = r#"
        mutation {
            createUser(input: {
                name: "Charlie",
                email: "charlie@example.com",
                role: viewer
            }) {
                clientMutationId
                user {
                    name
                    email
                    role
                }
            }
        }
    "#;

    let result = gql_schema.execute(mutation).await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        result.errors.is_empty(),
        "create mutation errors: {:?}",
        result.errors
    );

    let data = result.data.into_json().expect("failed to serialize result");
    let payload = &data["createUser"];
    assert_eq!(payload["user"]["name"], "Charlie");
    assert_eq!(payload["user"]["email"], "charlie@example.com");
}

#[tokio::test]
async fn update_mutation() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let gql_schema = build_test_schema(&db, &schema_name).await;

    let mutation = r#"
        mutation {
            updateUser(input: {
                id: "00000000-0000-0000-0000-000000000001",
                patch: {
                    name: "Alice Updated"
                }
            }) {
                user {
                    name
                    email
                }
            }
        }
    "#;

    let result = gql_schema.execute(mutation).await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        result.errors.is_empty(),
        "update mutation errors: {:?}",
        result.errors
    );

    let data = result.data.into_json().expect("failed to serialize result");
    let payload = &data["updateUser"];
    assert_eq!(payload["user"]["name"], "Alice Updated");
    assert_eq!(payload["user"]["email"], "alice@example.com");
}

#[tokio::test]
async fn delete_mutation() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let gql_schema = build_test_schema(&db, &schema_name).await;

    // Delete a user with no posts referencing them (delete Bob's posts first)
    let delete_post = r#"
        mutation {
            deletePost(input: {
                id: "10000000-0000-0000-0000-000000000002"
            }) {
                post {
                    title
                }
            }
        }
    "#;

    let post_result = gql_schema.execute(delete_post).await;
    assert!(
        post_result.errors.is_empty(),
        "delete post errors: {:?}",
        post_result.errors
    );

    // Now delete Bob
    let delete_user = r#"
        mutation {
            deleteUser(input: {
                id: "00000000-0000-0000-0000-000000000002"
            }) {
                user {
                    name
                }
            }
        }
    "#;

    let result = gql_schema.execute(delete_user).await;
    assert!(
        result.errors.is_empty(),
        "delete user mutation errors: {:?}",
        result.errors
    );

    let data = result.data.into_json().expect("failed to serialize result");
    assert_eq!(data["deleteUser"]["user"]["name"], "Bob");

    // Verify user is gone
    let verify = r#"{ user(id: "00000000-0000-0000-0000-000000000002") { name } }"#;
    let verify_result = gql_schema.execute(verify).await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        verify_result.errors.is_empty(),
        "verify query errors: {:?}",
        verify_result.errors
    );

    let verify_data = verify_result.data.into_json().expect("failed to serialize");
    assert!(
        verify_data["user"].is_null(),
        "expected user to be null after deletion"
    );
}

#[tokio::test]
async fn belongs_to_relationship() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let gql_schema = build_test_schema(&db, &schema_name).await;

    let result = gql_schema
        .execute("{ posts { nodes { title userByAuthorId { name } } } }")
        .await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        result.errors.is_empty(),
        "belongs-to query errors: {:?}",
        result.errors
    );

    let data = result.data.into_json().expect("failed to serialize result");
    let nodes = data["posts"]["nodes"]
        .as_array()
        .expect("nodes missing");

    // Each post should have its author resolved
    for node in nodes {
        let author = &node["userByAuthorId"];
        assert!(
            author.get("name").is_some(),
            "expected author name on post"
        );
    }

    // Verify specific relationships
    let first_post = nodes
        .iter()
        .find(|n| n["title"] == "First Post")
        .expect("First Post not found");
    assert_eq!(first_post["userByAuthorId"]["name"], "Alice");

    let draft_post = nodes
        .iter()
        .find(|n| n["title"] == "Draft Post")
        .expect("Draft Post not found");
    assert_eq!(draft_post["userByAuthorId"]["name"], "Bob");
}

#[tokio::test]
async fn has_many_relationship() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let gql_schema = build_test_schema(&db, &schema_name).await;

    let query =
        r#"{ user(id: "00000000-0000-0000-0000-000000000001") { name postsByAuthorId { nodes { title } totalCount } } }"#;
    let result = gql_schema.execute(query).await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        result.errors.is_empty(),
        "has-many query errors: {:?}",
        result.errors
    );

    let data = result.data.into_json().expect("failed to serialize result");
    let user = data.get("user").expect("user field missing");

    assert_eq!(user["name"], "Alice");

    let posts_conn = &user["postsByAuthorId"];
    assert_eq!(
        posts_conn["totalCount"], 1,
        "expected Alice to have 1 post"
    );

    let post_nodes = posts_conn["nodes"]
        .as_array()
        .expect("nodes missing");
    assert_eq!(post_nodes.len(), 1);
    assert_eq!(post_nodes[0]["title"], "First Post");
}

/// Plugin that filters out the posts table
struct OmitPosts;

#[allow(clippy::unnecessary_literal_bound)]
impl Plugin for OmitPosts {
    fn name(&self) -> &str {
        "omit-posts"
    }
    fn filter_table(&self, table: &taproot_core::introspect::TableInfo) -> bool {
        table.name != "posts"
    }
}

#[tokio::test]
async fn plugin_filters_tables() {
    let Some(db) = connect().await else {
        return;
    };
    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let options = IntrospectOptions {
        schemas: vec![schema_name.clone()],
        exclude_tables: Vec::new(),
        smart_comments: true,
    };
    let introspected = introspect(&db, &options).await.unwrap();
    let db_arc: Arc<dyn Database> = Arc::new(db.clone());

    let plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(OmitPosts)];
    let schema =
        build_schema(&introspected, &db_arc, &SchemaOptions::default(), &plugins).unwrap();
    let sdl = schema.sdl();

    teardown_test_schema(&db, &schema_name).await;

    assert!(!sdl.contains("Post"), "Post type should be filtered out");
    assert!(sdl.contains("User"), "User type should remain");
}
