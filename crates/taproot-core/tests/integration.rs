use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use taproot_core::db::{Database, SqlxBackend};
use taproot_core::planner::{
    execute_batch_lookup, execute_plan, execute_plan_count, FilterExpr, OrderByExpr, OrderDirection,
    PlanNode,
};
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
        &format!(
            "CREATE TABLE \"{schema}\".comments (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                body TEXT NOT NULL,
                post_id UUID NOT NULL REFERENCES \"{schema}\".posts(id),
                author_id UUID NOT NULL REFERENCES \"{schema}\".users(id),
                created_at TIMESTAMPTZ NOT NULL DEFAULT now()
            )"
        ),
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
        &format!(
            "INSERT INTO \"{schema}\".comments (id, body, post_id, author_id) VALUES \
             ('20000000-0000-0000-0000-000000000001', 'Great post!', '10000000-0000-0000-0000-000000000001', '00000000-0000-0000-0000-000000000002'), \
             ('20000000-0000-0000-0000-000000000002', 'Thanks!', '10000000-0000-0000-0000-000000000001', '00000000-0000-0000-0000-000000000001'), \
             ('20000000-0000-0000-0000-000000000003', 'Nice draft', '10000000-0000-0000-0000-000000000002', '00000000-0000-0000-0000-000000000001')"
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

    // Delete comment on Draft Post first (FK constraint prevents deleting the post otherwise)
    let delete_comment = r#"
        mutation {
            deleteComment(input: {
                id: "20000000-0000-0000-0000-000000000003"
            }) {
                comment {
                    body
                }
            }
        }
    "#;

    let comment_result = gql_schema.execute(delete_comment).await;
    assert!(
        comment_result.errors.is_empty(),
        "delete comment errors: {:?}",
        comment_result.errors
    );

    // Delete Bob's post now that it has no referencing comments
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

    // Delete Bob's comment on First Post (FK constraint prevents deleting user otherwise)
    let delete_bob_comment = r#"
        mutation {
            deleteComment(input: {
                id: "20000000-0000-0000-0000-000000000001"
            }) {
                comment {
                    body
                }
            }
        }
    "#;

    let bob_comment_result = gql_schema.execute(delete_bob_comment).await;
    assert!(
        bob_comment_result.errors.is_empty(),
        "delete Bob's comment errors: {:?}",
        bob_comment_result.errors
    );

    // Now delete Bob (no more posts or comments referencing him)
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

#[tokio::test]
async fn plan_scan_all_users() {
    let Some(db) = connect().await else {
        return;
    };
    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let db_arc: Arc<dyn Database> = Arc::new(db.clone());

    let plan = PlanNode::Scan {
        schema: schema_name.clone(),
        table: "users".into(),
        filter: None,
        order_by: Some(vec![OrderByExpr {
            column: "name".into(),
            direction: OrderDirection::Asc,
        }]),
        limit: None,
        offset: None,
        selections: vec![],
    };

    let rows = execute_plan(&plan, &db_arc, None)
        .await
        .expect("scan should succeed");
    assert_eq!(rows.len(), 2, "should find both seeded users");

    teardown_test_schema(&db, &schema_name).await;
}

#[tokio::test]
async fn plan_scan_with_filter() {
    let Some(db) = connect().await else {
        return;
    };
    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let db_arc: Arc<dyn Database> = Arc::new(db.clone());

    let plan = PlanNode::Scan {
        schema: schema_name.clone(),
        table: "users".into(),
        filter: Some(FilterExpr::Eq {
            column: "name".into(),
            value: serde_json::json!("Alice"),
        }),
        order_by: None,
        limit: None,
        offset: None,
        selections: vec![],
    };

    let rows = execute_plan(&plan, &db_arc, None)
        .await
        .expect("scan should succeed");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], "Alice");

    teardown_test_schema(&db, &schema_name).await;
}

#[tokio::test]
async fn plan_scan_count() {
    let Some(db) = connect().await else {
        return;
    };
    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let db_arc: Arc<dyn Database> = Arc::new(db.clone());

    let plan = PlanNode::Scan {
        schema: schema_name.clone(),
        table: "posts".into(),
        filter: None,
        order_by: None,
        limit: None,
        offset: None,
        selections: vec![],
    };

    let count = execute_plan_count(&plan, &db_arc, None)
        .await
        .expect("count should succeed");
    assert_eq!(count, 2, "should count both seeded posts");

    teardown_test_schema(&db, &schema_name).await;
}

#[tokio::test]
async fn plan_batch_lookup() {
    let Some(db) = connect().await else {
        return;
    };
    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let db_arc: Arc<dyn Database> = Arc::new(db.clone());

    // Batch lookup both users by their known IDs
    let batch_plan = PlanNode::BatchLookup {
        schema: schema_name.clone(),
        table: "users".into(),
        key_column: "id".into(),
        keys: vec![
            serde_json::json!("00000000-0000-0000-0000-000000000001"),
            serde_json::json!("00000000-0000-0000-0000-000000000002"),
        ],
        selections: vec![],
    };

    let grouped = execute_batch_lookup(&batch_plan, &db_arc, None)
        .await
        .expect("batch lookup should succeed");
    assert_eq!(grouped.len(), 2, "should have 2 groups (one per user)");

    teardown_test_schema(&db, &schema_name).await;
}

#[tokio::test]
async fn plan_scan_with_pagination() {
    let Some(db) = connect().await else {
        return;
    };
    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let db_arc: Arc<dyn Database> = Arc::new(db.clone());

    // Page 1: first user
    let plan = PlanNode::Scan {
        schema: schema_name.clone(),
        table: "users".into(),
        filter: None,
        order_by: Some(vec![OrderByExpr {
            column: "name".into(),
            direction: OrderDirection::Asc,
        }]),
        limit: Some(1),
        offset: Some(0),
        selections: vec![],
    };

    let page1 = execute_plan(&plan, &db_arc, None)
        .await
        .expect("scan should succeed");
    assert_eq!(page1.len(), 1, "page 1 should have 1 row");
    assert_eq!(page1[0]["name"], "Alice", "first alphabetically");

    // Page 2: second user
    let plan_p2 = PlanNode::Scan {
        schema: schema_name.clone(),
        table: "users".into(),
        filter: None,
        order_by: Some(vec![OrderByExpr {
            column: "name".into(),
            direction: OrderDirection::Asc,
        }]),
        limit: Some(1),
        offset: Some(1),
        selections: vec![],
    };

    let page2 = execute_plan(&plan_p2, &db_arc, None)
        .await
        .expect("scan should succeed");
    assert_eq!(page2.len(), 1, "page 2 should have 1 row");
    assert_eq!(page2[0]["name"], "Bob", "second alphabetically");

    teardown_test_schema(&db, &schema_name).await;
}

// Plugin that filters to only Alice
struct OnlyAlicePlugin;

#[allow(clippy::unnecessary_literal_bound)]
impl Plugin for OnlyAlicePlugin {
    fn name(&self) -> &str {
        "only-alice"
    }
    fn on_plan(&self, plan: &mut PlanNode) {
        if let PlanNode::Scan { filter, table, .. } = plan
            && table == "users"
        {
            let alice_filter = FilterExpr::Eq {
                column: "name".into(),
                value: serde_json::json!("Alice"),
            };
            *filter = Some(match filter.take() {
                Some(existing) => FilterExpr::And(vec![existing, alice_filter]),
                None => alice_filter,
            });
        }
    }
}

#[tokio::test]
async fn plan_with_plugin_on_plan_hook() {
    let Some(db) = connect().await else {
        return;
    };
    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let db_arc: Arc<dyn Database> = Arc::new(db.clone());

    let mut plan = PlanNode::Scan {
        schema: schema_name.clone(),
        table: "users".into(),
        filter: None,
        order_by: None,
        limit: None,
        offset: None,
        selections: vec![],
    };

    // Apply plugin
    let plugin = OnlyAlicePlugin;
    plugin.on_plan(&mut plan);

    let rows = execute_plan(&plan, &db_arc, None)
        .await
        .expect("scan should succeed");
    assert_eq!(rows.len(), 1, "plugin should filter to only Alice");
    assert_eq!(rows[0]["name"], "Alice");

    teardown_test_schema(&db, &schema_name).await;
}

// Plugin that injects a name="Alice" filter into any Scan on the users table,
// used to prove on_plan hooks apply during real GraphQL execution
struct OnlyAliceQueryPlugin;

#[allow(clippy::unnecessary_literal_bound)]
impl Plugin for OnlyAliceQueryPlugin {
    fn name(&self) -> &str {
        "only-alice-query"
    }
    fn on_plan(&self, plan: &mut PlanNode) {
        if let PlanNode::Scan { filter, table, .. } = plan
            && table == "users"
        {
            let alice_filter = FilterExpr::Eq {
                column: "name".into(),
                value: serde_json::json!("Alice"),
            };
            *filter = Some(match filter.take() {
                Some(existing) => FilterExpr::And(vec![existing, alice_filter]),
                None => alice_filter,
            });
        }
    }
}

#[tokio::test]
async fn on_plan_hook_filters_real_graphql_queries() {
    let Some(db) = connect().await else {
        return;
    };
    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    // Build schema WITH the on_plan plugin
    let options = IntrospectOptions {
        schemas: vec![schema_name.clone()],
        exclude_tables: Vec::new(),
        smart_comments: true,
    };
    let introspected = introspect(&db, &options).await.unwrap();
    let db_arc: Arc<dyn Database> = Arc::new(db.clone());
    let plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(OnlyAliceQueryPlugin)];
    let gql_schema =
        build_schema(&introspected, &db_arc, &SchemaOptions::default(), &plugins).unwrap();

    // List query should return only Alice
    let list_result = gql_schema
        .execute("{ users { nodes { name } totalCount } }")
        .await;
    assert!(
        list_result.errors.is_empty(),
        "list query errors: {:?}",
        list_result.errors
    );
    let list_data = list_result.data.into_json().expect("failed to serialize");
    let conn = list_data.get("users").expect("users missing");
    let nodes = conn["nodes"].as_array().expect("nodes missing");
    assert_eq!(nodes.len(), 1, "plugin should filter to only Alice");
    assert_eq!(nodes[0]["name"], "Alice");

    // Singular query for Bob should return null (filtered out by plugin)
    let bob_result = gql_schema
        .execute(r#"{ user(id: "00000000-0000-0000-0000-000000000002") { name } }"#)
        .await;
    assert!(
        bob_result.errors.is_empty(),
        "singular query errors: {:?}",
        bob_result.errors
    );
    let bob_data = bob_result.data.into_json().expect("failed to serialize");
    assert!(
        bob_data["user"].is_null(),
        "Bob should be filtered out by on_plan hook"
    );

    teardown_test_schema(&db, &schema_name).await;
}

#[tokio::test]
async fn belongs_to_batched_via_dataloader() {
    let Some(db) = connect().await else {
        return;
    };
    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let gql_schema = build_test_schema(&db, &schema_name).await;

    // Query all posts with their authors — triggers belongs-to DataLoader batching
    let result = gql_schema
        .execute("{ posts { nodes { title userByAuthorId { name email } } } }")
        .await;

    assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
    let data = result.data.into_json().expect("should serialize");
    let nodes = data["posts"]["nodes"]
        .as_array()
        .expect("should be array");

    // Both posts should have their authors resolved
    for node in nodes {
        assert!(
            !node["userByAuthorId"].is_null(),
            "author should be resolved for: {}",
            node["title"]
        );
    }

    let first_post = nodes
        .iter()
        .find(|n| n["title"] == "First Post")
        .unwrap();
    assert_eq!(first_post["userByAuthorId"]["name"], "Alice");

    let draft_post = nodes
        .iter()
        .find(|n| n["title"] == "Draft Post")
        .unwrap();
    assert_eq!(draft_post["userByAuthorId"]["name"], "Bob");

    teardown_test_schema(&db, &schema_name).await;
}

// Plugin that filters posts to only published ones
struct PublishedOnlyPlugin;

#[allow(clippy::unnecessary_literal_bound)]
impl Plugin for PublishedOnlyPlugin {
    fn name(&self) -> &str {
        "published-only"
    }
    fn on_plan(&self, plan: &mut PlanNode) {
        let filter_slot = match plan {
            PlanNode::Scan { filter, table, .. } if table == "posts" => filter,
            PlanNode::LateralJoin {
                filter,
                child_table,
                ..
            } if child_table == "posts" => filter,
            _ => return,
        };

        let published_filter = FilterExpr::Eq {
            column: "published".into(),
            value: serde_json::json!(true),
        };
        *filter_slot = Some(match filter_slot.take() {
            Some(existing) => FilterExpr::And(vec![existing, published_filter]),
            None => published_filter,
        });
    }
}

#[tokio::test]
async fn has_many_with_on_plan_hook() {
    let Some(db) = connect().await else {
        return;
    };
    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let introspected = introspect(
        &db,
        &IntrospectOptions {
            schemas: vec![schema_name.clone()],
            exclude_tables: Vec::new(),
            smart_comments: true,
        },
    )
    .await
    .expect("introspection should succeed");

    let db_arc: Arc<dyn Database> = Arc::new(db.clone());
    let plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(PublishedOnlyPlugin)];

    let gql_schema = build_schema(
        &introspected,
        &db_arc,
        &SchemaOptions::default(),
        &plugins,
    )
    .expect("schema build should succeed");

    // Query Alice's posts — plugin should filter to only published
    let result = gql_schema
        .execute(
            r#"{ user(id: "00000000-0000-0000-0000-000000000001") { name postsByAuthorId { nodes { title published } totalCount } } }"#,
        )
        .await;

    assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
    let data = result.data.into_json().expect("should serialize");

    let posts = &data["user"]["postsByAuthorId"];
    let total = posts["totalCount"].as_i64().unwrap();
    assert_eq!(total, 1, "only published posts should be counted");

    let nodes = posts["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 1, "only published posts should be returned");
    assert_eq!(nodes[0]["title"], "First Post");

    teardown_test_schema(&db, &schema_name).await;
}

#[tokio::test]
async fn federation_sdl_contains_key_directives() {
    let Some(db) = connect().await else {
        return;
    };
    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let introspected = introspect(
        &db,
        &IntrospectOptions {
            schemas: vec![schema_name.clone()],
            ..Default::default()
        },
    )
    .await
    .expect("introspection should succeed");

    let db_arc: Arc<dyn Database> = Arc::new(db.clone());
    let options = SchemaOptions {
        federation: true,
        ..Default::default()
    };

    let gql_schema =
        build_schema(&introspected, &db_arc, &options, &[]).expect("schema build should succeed");

    let sdl = gql_schema
        .sdl_with_options(async_graphql::SDLExportOptions::new().federation());

    teardown_test_schema(&db, &schema_name).await;

    // Types with PKs should have @key directives
    assert!(sdl.contains("@key"), "@key directive missing from SDL");
    assert!(sdl.contains("User"), "User type missing from SDL");
    assert!(sdl.contains("Post"), "Post type missing from SDL");
}

#[tokio::test]
async fn federation_entities_resolves_by_key() {
    let Some(db) = connect().await else {
        return;
    };
    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let introspected = introspect(
        &db,
        &IntrospectOptions {
            schemas: vec![schema_name.clone()],
            ..Default::default()
        },
    )
    .await
    .expect("introspection should succeed");

    let db_arc: Arc<dyn Database> = Arc::new(db.clone());
    let options = SchemaOptions {
        federation: true,
        ..Default::default()
    };

    let gql_schema =
        build_schema(&introspected, &db_arc, &options, &[]).expect("schema build should succeed");

    // Resolve Alice by her entity key
    let query = r#"
        {
            _entities(representations: [
                { __typename: "User", id: "00000000-0000-0000-0000-000000000001" }
            ]) {
                ... on User { name email }
            }
        }
    "#;

    let result = gql_schema.execute(query).await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        result.errors.is_empty(),
        "_entities query errors: {:?}",
        result.errors
    );

    let data = result.data.into_json().expect("failed to serialize result");
    let entities = data["_entities"]
        .as_array()
        .expect("_entities should be an array");
    assert_eq!(entities.len(), 1);
    assert_eq!(entities[0]["name"], "Alice");
    assert_eq!(entities[0]["email"], "alice@example.com");
}

#[tokio::test]
async fn federation_entities_resolves_multiple() {
    let Some(db) = connect().await else {
        return;
    };
    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let introspected = introspect(
        &db,
        &IntrospectOptions {
            schemas: vec![schema_name.clone()],
            ..Default::default()
        },
    )
    .await
    .expect("introspection should succeed");

    let db_arc: Arc<dyn Database> = Arc::new(db.clone());
    let options = SchemaOptions {
        federation: true,
        ..Default::default()
    };

    let gql_schema =
        build_schema(&introspected, &db_arc, &options, &[]).expect("schema build should succeed");

    // Resolve both Alice and Bob in a single _entities call
    let query = r#"
        {
            _entities(representations: [
                { __typename: "User", id: "00000000-0000-0000-0000-000000000001" },
                { __typename: "User", id: "00000000-0000-0000-0000-000000000002" }
            ]) {
                ... on User { name email }
            }
        }
    "#;

    let result = gql_schema.execute(query).await;

    teardown_test_schema(&db, &schema_name).await;

    assert!(
        result.errors.is_empty(),
        "_entities query errors: {:?}",
        result.errors
    );

    let data = result.data.into_json().expect("failed to serialize result");
    let entities = data["_entities"]
        .as_array()
        .expect("_entities should be an array");
    assert_eq!(entities.len(), 2, "expected 2 resolved entities");

    // Order should match the representations order
    assert_eq!(entities[0]["name"], "Alice");
    assert_eq!(entities[0]["email"], "alice@example.com");
    assert_eq!(entities[1]["name"], "Bob");
    assert_eq!(entities[1]["email"], "bob@example.com");
}

#[tokio::test]
async fn has_many_via_lateral_join() {
    let Some(db) = connect().await else {
        return;
    };
    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let gql_schema = build_test_schema(&db, &schema_name).await;

    let result = gql_schema
        .execute(r"{ users(first: 10, orderBy: [NAME_ASC]) { edges { node { name postsByAuthorId(first: 10) { edges { node { title } } } } } } }")
        .await;

    assert!(result.errors.is_empty(), "errors: {:?}", result.errors);

    let data = result.data.into_json().expect("should serialize");
    let edges = data["users"]["edges"].as_array().expect("should be array");
    assert_eq!(edges.len(), 2);

    // Alice should have 1 post
    let alice = &edges[0]["node"];
    assert_eq!(alice["name"], "Alice");
    let alice_posts = alice["postsByAuthorId"]["edges"]
        .as_array()
        .expect("array");
    assert_eq!(alice_posts.len(), 1);
    assert_eq!(alice_posts[0]["node"]["title"], "First Post");

    // Bob should have 1 post
    let bob = &edges[1]["node"];
    assert_eq!(bob["name"], "Bob");
    let bob_posts = bob["postsByAuthorId"]["edges"]
        .as_array()
        .expect("array");
    assert_eq!(bob_posts.len(), 1);
    assert_eq!(bob_posts[0]["node"]["title"], "Draft Post");

    teardown_test_schema(&db, &schema_name).await;
}

#[tokio::test]
async fn has_many_lateral_respects_pagination() {
    let Some(db) = connect().await else {
        return;
    };
    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    // Add more posts for Alice
    let extra_posts = format!(
        "INSERT INTO \"{schema_name}\".posts (title, content, author_id, published) VALUES \
         ('Post 2', 'Content 2', '00000000-0000-0000-0000-000000000001', true), \
         ('Post 3', 'Content 3', '00000000-0000-0000-0000-000000000001', true)"
    );
    db.query_raw(&extra_posts)
        .await
        .expect("insert extra posts");

    let gql_schema = build_test_schema(&db, &schema_name).await;

    // Request only first 2 posts
    let result = gql_schema
        .execute(
            r"{ users(first: 10, orderBy: [NAME_ASC]) { edges { node { name postsByAuthorId(first: 2) { edges { node { title } } } } } } }",
        )
        .await;

    assert!(result.errors.is_empty(), "errors: {:?}", result.errors);

    let data = result.data.into_json().expect("should serialize");
    let edges = data["users"]["edges"].as_array().expect("array");

    // Find Alice (has 3 posts, should only get 2)
    let alice = edges
        .iter()
        .find(|e| e["node"]["name"] == "Alice")
        .expect("Alice");
    let alice_posts = alice["node"]["postsByAuthorId"]["edges"]
        .as_array()
        .expect("array");
    assert_eq!(alice_posts.len(), 2, "should respect first:2 pagination");

    teardown_test_schema(&db, &schema_name).await;
}

#[tokio::test]
async fn on_plan_hooks_apply_to_lateral_joins() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let introspected = introspect(
        &db,
        &IntrospectOptions {
            schemas: vec![schema_name.clone()],
            ..Default::default()
        },
    )
    .await
    .expect("introspection");

    let db_arc: Arc<dyn Database> = Arc::new(db.clone());

    let options = SchemaOptions::default();
    let plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(PublishedOnlyPlugin)];

    let gql_schema =
        build_schema(&introspected, &db_arc, &options, &plugins).expect("schema build");

    // Query users with posts — plugin should filter to published only
    let result = gql_schema
        .execute(
            r"{ users(first: 10, orderBy: [NAME_ASC]) { edges { node { name postsByAuthorId(first: 10) { edges { node { title } } } } } } }",
        )
        .await;

    assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
    let data = result.data.into_json().expect("serialize");
    let edges = data["users"]["edges"].as_array().expect("array");

    // Alice has 1 published post ("First Post")
    let alice = edges
        .iter()
        .find(|e| e["node"]["name"] == "Alice")
        .expect("Alice");
    let alice_posts = alice["node"]["postsByAuthorId"]["edges"]
        .as_array()
        .expect("array");
    assert_eq!(alice_posts.len(), 1);
    assert_eq!(alice_posts[0]["node"]["title"], "First Post");

    // Bob has 0 published posts (his post is unpublished)
    let bob = edges
        .iter()
        .find(|e| e["node"]["name"] == "Bob")
        .expect("Bob");
    let bob_posts = bob["node"]["postsByAuthorId"]["edges"]
        .as_array()
        .expect("array");
    assert!(
        bob_posts.is_empty(),
        "Bob's unpublished post should be filtered out"
    );

    teardown_test_schema(&db, &schema_name).await;
}

#[tokio::test]
async fn deep_nested_has_many_via_lateral_join() {
    let Some(db) = connect().await else {
        return;
    };

    let schema_name = test_schema();
    setup_test_schema(&db, &schema_name).await;

    let gql_schema = build_test_schema(&db, &schema_name).await;

    // Query 3 levels deep: users -> posts -> comments
    let result = gql_schema
        .execute(
            r"{
                users(first: 10, orderBy: [NAME_ASC]) {
                    edges {
                        node {
                            name
                            postsByAuthorId(first: 10) {
                                edges {
                                    node {
                                        title
                                        commentsByPostId(first: 10) {
                                            edges {
                                                node {
                                                    body
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }",
        )
        .await;

    assert!(result.errors.is_empty(), "errors: {:?}", result.errors);

    let data = result.data.into_json().expect("should serialize");
    let edges = data["users"]["edges"].as_array().expect("array");
    assert_eq!(edges.len(), 2);

    // Alice's "First Post" should have 2 comments
    let alice = edges
        .iter()
        .find(|e| e["node"]["name"] == "Alice")
        .expect("Alice");
    let alice_posts = alice["node"]["postsByAuthorId"]["edges"]
        .as_array()
        .expect("array");
    assert_eq!(alice_posts.len(), 1);
    assert_eq!(alice_posts[0]["node"]["title"], "First Post");

    let first_post_comments = alice_posts[0]["node"]["commentsByPostId"]["edges"]
        .as_array()
        .expect("array");
    assert_eq!(
        first_post_comments.len(),
        2,
        "First Post should have 2 comments"
    );

    // Bob's "Draft Post" should have 1 comment
    let bob = edges
        .iter()
        .find(|e| e["node"]["name"] == "Bob")
        .expect("Bob");
    let bob_posts = bob["node"]["postsByAuthorId"]["edges"]
        .as_array()
        .expect("array");
    assert_eq!(bob_posts.len(), 1);
    assert_eq!(bob_posts[0]["node"]["title"], "Draft Post");

    let draft_comments = bob_posts[0]["node"]["commentsByPostId"]["edges"]
        .as_array()
        .expect("array");
    assert_eq!(
        draft_comments.len(),
        1,
        "Draft Post should have 1 comment"
    );
    assert_eq!(draft_comments[0]["node"]["body"], "Nice draft");

    teardown_test_schema(&db, &schema_name).await;
}
